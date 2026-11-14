//! Any rail, served over the adapter protocol (see [`super::template`]).
//!
//! The reference implementation of the server side, in Rust: the tests run
//! the simulated bank behind it, so the template rail and the conformance
//! suite are checked against the very fault-injecting bank the engine is
//! tested with. Merchants write their own adapter (the Node example in
//! `examples/adapter/` is the one to copy); this one is not deployed.

use std::sync::Arc;

use axum::extract::{Path, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::Deserialize;
use serde_json::{json, Value};

use super::{AccountLookup, PayoutRail, PayoutRequest};
use crate::machine::{PayoutStatus, SendOutcome};
use crate::money::Minor;
use crate::pii::{AccountNumber, HolderName};
use crate::secrets::Secret;

struct Served<R> {
	rail: R,
	token: Secret,
	name: String,
}

pub fn router<R: PayoutRail + 'static>(rail: R, token: Secret, name: &str) -> Router {
	let state = Arc::new(Served {
		rail,
		token,
		name: name.to_string(),
	});
	Router::new()
		.route("/v1/info", get(info::<R>))
		.route("/v1/lookup", post(lookup::<R>))
		.route("/v1/payouts", post(send::<R>))
		.route("/v1/payouts/{key}", get(status::<R>))
		.route("/v1/balance", get(balance::<R>))
		.with_state(state)
}

fn authorised<R>(s: &Served<R>, headers: &HeaderMap) -> bool {
	let want = format!("Bearer {}", s.token.expose());
	let got = headers
		.get("authorization")
		.and_then(|v| v.to_str().ok())
		.unwrap_or_default();
	crate::console::auth::same_token(got, &want)
}

fn unauthorised() -> Response {
	(
		StatusCode::UNAUTHORIZED,
		Json(json!({"error": "unauthorised"})),
	)
		.into_response()
}

fn unavailable(e: impl std::fmt::Display) -> Response {
	(
		StatusCode::SERVICE_UNAVAILABLE,
		Json(json!({"error": e.to_string()})),
	)
		.into_response()
}

async fn info<R: PayoutRail>(State(s): State<Arc<Served<R>>>, headers: HeaderMap) -> Response {
	if !authorised(&s, &headers) {
		return unauthorised();
	}
	Json(json!({"protocol": super::template::PROTOCOL, "name": s.name, "idempotent_send": s.rail.idempotent_send()}))
		.into_response()
}

#[derive(Deserialize)]
struct LookupBody {
	bank_code: String,
	account_number: String,
}

async fn lookup<R: PayoutRail>(
	State(s): State<Arc<Served<R>>>,
	headers: HeaderMap,
	Json(b): Json<LookupBody>,
) -> Response {
	if !authorised(&s, &headers) {
		return unauthorised();
	}
	match s
		.rail
		.verify_account(&b.bank_code, &AccountNumber::new(b.account_number))
		.await
	{
		Ok(AccountLookup::Name(n)) => {
			Json(json!({"result": "found", "account_name": n.expose()})).into_response()
		}
		Ok(AccountLookup::NotFound) => Json(json!({"result": "not_found"})).into_response(),
		Ok(AccountLookup::Unsupported) => Json(json!({"result": "unsupported"})).into_response(),
		Err(e) => unavailable(e),
	}
}

#[derive(Deserialize)]
struct PayoutBody {
	key: String,
	trade_id: String,
	amount_minor: String,
	currency: String,
	bank_code: String,
	account_number: String,
	account_name: String,
	narration: String,
}

async fn send<R: PayoutRail>(
	State(s): State<Arc<Served<R>>>,
	headers: HeaderMap,
	Json(b): Json<PayoutBody>,
) -> Response {
	if !authorised(&s, &headers) {
		return unauthorised();
	}
	let Ok(amount) = b.amount_minor.parse::<u128>() else {
		return (
			StatusCode::BAD_REQUEST,
			Json(json!({"error": "amount_minor"})),
		)
			.into_response();
	};
	if b.currency != "NGN" {
		return Json(json!({"result": "rejected", "reason": "only NGN"})).into_response();
	}
	let account = AccountNumber::new(b.account_number);
	let name = HolderName::new(b.account_name);
	let out = s
		.rail
		.send(PayoutRequest {
			idempotency_key: &b.key,
			trade_id: &b.trade_id,
			amount: Minor(amount),
			bank_code: &b.bank_code,
			account: &account,
			name: &name,
			narration: &b.narration,
		})
		.await;
	match out {
		Ok(SendOutcome::Accepted { reference }) => {
			Json(json!({"result": "accepted", "reference": reference})).into_response()
		}
		Ok(SendOutcome::Rejected { reason }) => {
			Json(json!({"result": "rejected", "reason": reason})).into_response()
		}
		Ok(SendOutcome::Queued | SendOutcome::Unknown) => {
			Json(json!({"result": "unknown"})).into_response()
		}
		Err(e) => unavailable(e),
	}
}

async fn status<R: PayoutRail>(
	State(s): State<Arc<Served<R>>>,
	headers: HeaderMap,
	Path(key): Path<String>,
) -> Response {
	if !authorised(&s, &headers) {
		return unauthorised();
	}
	let body: Value = match s.rail.status(&key).await {
		Ok(PayoutStatus::Succeeded { reference }) => {
			json!({"result": "succeeded", "reference": reference})
		}
		Ok(PayoutStatus::Pending) => json!({"result": "pending"}),
		Ok(PayoutStatus::Failed { reason }) => json!({"result": "failed", "reason": reason}),
		Ok(PayoutStatus::NotFound) => json!({"result": "not_found"}),
		Ok(PayoutStatus::Unknown) => json!({"result": "unknown"}),
		Err(e) => return unavailable(e),
	};
	Json(body).into_response()
}

async fn balance<R: PayoutRail>(State(s): State<Arc<Served<R>>>, headers: HeaderMap) -> Response {
	if !authorised(&s, &headers) {
		return unauthorised();
	}
	match s.rail.balance().await {
		Ok(b) => {
			Json(json!({"currency": "NGN", "available_minor": b.map(|m| m.get().to_string())}))
				.into_response()
		}
		Err(e) => unavailable(e),
	}
}
