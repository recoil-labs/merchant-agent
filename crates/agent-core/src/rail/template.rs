//! The template rail: the merchant's own payout adapter, over HTTPS.
//!
//! The adapter is a small service the merchant runs in front of their bank
//! or payout API, in any language. The agent calls it with this protocol
//! (version 1, described for adapter writers in `docs/adapters.md`):
//!
//! | call | answer (HTTP 200, JSON) |
//! |---|---|
//! | `GET /v1/info` | `{protocol: 1, name, idempotent_send}` |
//! | `POST /v1/lookup` `{bank_code, account_number}` | `{result: "found", account_name}` · `{result: "not_found"}` · `{result: "unsupported"}` |
//! | `POST /v1/payouts` `{key, trade_id, amount_minor, currency, bank_code, account_number, account_name, narration}` | `{result: "accepted", reference}` · `{result: "rejected", reason}` · `{result: "unknown"}` |
//! | `GET /v1/payouts/{key}` | `{result: "succeeded", reference}` · `{result: "pending"}` · `{result: "failed", reason}` · `{result: "not_found"}` · `{result: "unknown"}` |
//! | `GET /v1/balance` | `{currency: "NGN", available_minor}` (`null` if it can't tell) |
//!
//! Every call carries `Authorization: Bearer <rail.credentials>`. Amounts
//! are minor units (kobo) as decimal strings.
//!
//! **Only a 200 with a known `result` is an answer.** Anything else (a
//! timeout, a 4xx or 5xx, a body that isn't the protocol) means "could not
//! ask", and the engine then asks by key. In particular a refusal must be
//! an explicit `rejected`: an error status never counts as "no money moved".
//!
//! **Idempotency.** The adapter says in `/v1/info` whether a payout sent
//! twice under one key is paid once. Only then will the agent ever resend a
//! key the adapter says it has never seen; otherwise a lost answer always
//! goes to a person. The conformance suite checks the claim.

use std::time::Duration;

use reqwest::{Method, StatusCode};
use serde::de::DeserializeOwned;
use serde::Deserialize;
use serde_json::json;

use super::{clean, AccountLookup, PayoutRail, PayoutRequest, RailError};
use crate::machine::{PayoutStatus, SendOutcome};
use crate::money::Minor;
use crate::pii::{AccountNumber, HolderName};
use crate::secrets::Secret;

pub const PROTOCOL: u32 = 1;

/// What the adapter says about itself.
#[derive(Debug, Clone, Deserialize)]
pub struct Info {
	pub protocol: u32,
	#[serde(default)]
	pub name: String,
	pub idempotent_send: bool,
}

pub struct TemplateRail {
	http: reqwest::Client,
	base: String,
	token: Secret,
	info: Info,
}

#[derive(Deserialize)]
#[serde(tag = "result", rename_all = "snake_case")]
enum Lookup {
	Found { account_name: String },
	NotFound,
	Unsupported,
}

#[derive(Deserialize)]
#[serde(tag = "result", rename_all = "snake_case")]
enum Sent {
	Accepted { reference: String },
	Rejected { reason: String },
	Unknown,
}

#[derive(Deserialize)]
#[serde(tag = "result", rename_all = "snake_case")]
enum Status {
	Succeeded { reference: String },
	Pending,
	Failed { reason: String },
	NotFound,
	Unknown,
}

#[derive(Deserialize)]
struct Balance {
	currency: String,
	available_minor: Option<String>,
}

/// A payout key the protocol can carry in a path: what the agent makes
/// (`rpma-ft-…`), nothing else.
fn valid_key(key: &str) -> bool {
	(8..=100).contains(&key.len())
		&& key
			.bytes()
			.all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-' || b == b'_')
}

/// A bank reference as the adapter gave it, if it is one: it is reported to
/// RecoilPay and shown to the customer, so it is kept exactly, never masked.
/// Up to 100 printable characters, no spaces inside.
fn bank_reference(r: &str) -> Option<String> {
	let r = r.trim();
	let ok = !r.is_empty() && r.len() <= 100 && r.chars().all(|c| c.is_ascii_graphic());
	ok.then(|| r.to_string())
}

impl TemplateRail {
	/// Connect: read `/v1/info` and check the protocol version.
	pub async fn connect(url: &str, token: Secret, timeout: Duration) -> Result<Self, String> {
		let http = reqwest::Client::builder()
			.timeout(timeout)
			.connect_timeout(timeout.min(Duration::from_secs(10)))
			.redirect(reqwest::redirect::Policy::none())
			.build()
			.map_err(|e| format!("could not build the adapter client: {e}"))?;
		let mut rail = Self {
			http,
			base: url.trim_end_matches('/').to_string(),
			token,
			info: Info {
				protocol: 0,
				name: String::new(),
				idempotent_send: false,
			},
		};
		let info: Info = rail
			.call(Method::GET, "/v1/info", None)
			.await
			.map_err(|e| format!("the adapter at {url}: {}", e.0))?;
		if info.protocol != PROTOCOL {
			return Err(format!(
				"the adapter at {url} speaks protocol {}, but this agent speaks {PROTOCOL}",
				info.protocol
			));
		}
		rail.info = info;
		Ok(rail)
	}

	pub fn info(&self) -> &Info {
		&self.info
	}

	async fn call<T: DeserializeOwned>(
		&self,
		method: Method,
		path: &str,
		body: Option<serde_json::Value>,
	) -> Result<T, RailError> {
		let mut req = self
			.http
			.request(method, format!("{}{path}", self.base))
			.bearer_auth(self.token.expose());
		if let Some(b) = body {
			req = req.json(&b);
		}
		// The body we sent may hold an account number; reqwest's errors don't
		// carry it, but say only what kind of failure it was anyway.
		let res = req.send().await.map_err(|e| {
			RailError(if e.is_timeout() {
				"the adapter timed out".into()
			} else if e.is_connect() {
				"could not connect to the adapter".into()
			} else {
				"the request to the adapter failed".into()
			})
		})?;
		let status = res.status();
		if status == StatusCode::UNAUTHORIZED || status == StatusCode::FORBIDDEN {
			return Err(RailError(format!(
				"the adapter refused the token ({status})"
			)));
		}
		if status != StatusCode::OK {
			return Err(RailError(format!("the adapter answered {status}")));
		}
		let text = res
			.text()
			.await
			.map_err(|_| RailError("the adapter's answer was cut off".into()))?;
		serde_json::from_str(&text)
			.map_err(|_| RailError("the adapter's answer is not the protocol".into()))
	}
}

impl PayoutRail for TemplateRail {
	fn idempotent_send(&self) -> bool {
		self.info.idempotent_send
	}

	async fn verify_account(
		&self,
		bank_code: &str,
		account: &AccountNumber,
	) -> Result<AccountLookup, RailError> {
		let answer: Lookup = self
			.call(
				Method::POST,
				"/v1/lookup",
				Some(json!({"bank_code": bank_code, "account_number": account.expose()})),
			)
			.await?;
		Ok(match answer {
			Lookup::Found { account_name } => AccountLookup::Name(HolderName::new(account_name)),
			Lookup::NotFound => AccountLookup::NotFound,
			Lookup::Unsupported => AccountLookup::Unsupported,
		})
	}

	async fn send(&self, req: PayoutRequest<'_>) -> Result<SendOutcome, RailError> {
		if !valid_key(req.idempotency_key) {
			return Ok(SendOutcome::Rejected {
				reason: "the payout key can't be sent to the adapter".into(),
			});
		}
		let answer: Sent = self
			.call(
				Method::POST,
				"/v1/payouts",
				Some(json!({
					"key": req.idempotency_key,
					"trade_id": req.trade_id,
					"amount_minor": req.amount.get().to_string(),
					"currency": "NGN",
					"bank_code": req.bank_code,
					"account_number": req.account.expose(),
					"account_name": req.name.expose(),
					"narration": req.narration,
				})),
			)
			.await?;
		Ok(match answer {
			Sent::Accepted { reference } => match bank_reference(&reference) {
				Some(r) => SendOutcome::Accepted { reference: r },
				None => SendOutcome::Unknown,
			},
			Sent::Rejected { reason } => SendOutcome::Rejected {
				reason: clean(&reason),
			},
			Sent::Unknown => SendOutcome::Unknown,
		})
	}

	async fn status(&self, key: &str) -> Result<PayoutStatus, RailError> {
		if !valid_key(key) {
			return Ok(PayoutStatus::NotFound);
		}
		let answer: Status = self
			.call(Method::GET, &format!("/v1/payouts/{key}"), None)
			.await?;
		Ok(match answer {
			Status::Succeeded { reference } => match bank_reference(&reference) {
				Some(r) => PayoutStatus::Succeeded { reference: r },
				None => PayoutStatus::Unknown,
			},
			Status::Pending => PayoutStatus::Pending,
			Status::Failed { reason } => PayoutStatus::Failed {
				reason: clean(&reason),
			},
			Status::NotFound => PayoutStatus::NotFound,
			Status::Unknown => PayoutStatus::Unknown,
		})
	}

	async fn balance(&self) -> Result<Option<Minor>, RailError> {
		let b: Balance = self.call(Method::GET, "/v1/balance", None).await?;
		if !b.currency.eq_ignore_ascii_case("NGN") {
			return Err(RailError(format!(
				"the adapter's balance is in {}, not NGN",
				clean(&b.currency)
			)));
		}
		match b.available_minor {
			None => Ok(None),
			Some(s) => s.parse::<u128>().map(|v| Some(Minor(v))).map_err(|_| {
				RailError("the adapter's balance is not a whole number of kobo".into())
			}),
		}
	}
}
