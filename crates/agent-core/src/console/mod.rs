//! The local console: what the merchant's operations team uses.
//!
//! Served by the running agent on `127.0.0.1` (invariant 9), rendered on the
//! server (maud) with a little vanilla JavaScript for countdowns and hiding
//! revealed details. No build step.
//!
//! Pages: the dashboard (health, float, today against limits, pause), live
//! trades with deadline countdowns, the exception queue with its actions,
//! recent decisions, read-only settings, and users.
//!
//! Every page is behind a login ([`auth`]); every action checks the role
//! and the session's CSRF token; every response says `no-store` and carries
//! a strict Content-Security-Policy. Bank details appear only on a reveal,
//! which the engine records, and the page hides them again after 60 s.

// Handlers return `Result<_, Response>` so a check can hand back the page
// to show instead (a redirect to sign in, a 403). That is axum's idiom;
// boxing every early return would only add noise.
#![allow(clippy::result_large_err)]

pub mod auth;
mod pages;

use std::collections::HashMap;
use std::sync::Arc;

use axum::extract::{Form, Path, Query, State};
use axum::http::{header, HeaderMap, HeaderValue, StatusCode};
use axum::response::{IntoResponse, Redirect, Response};
use axum::routing::{get, post};
use axum::Router;
use chrono::{DateTime, Utc};

use crate::config::{Limits, TwoPerson};
use crate::engine::{Decision, Revealed};
use crate::machine::PersonAction;
use crate::money::Minor;
use crate::notify::BoxFuture;
use crate::store::{ManualTask, Store, TradeRecord};
use auth::{Role, Session, Sessions, TotpKey};

pub const COOKIE: &str = "ma_session";

/// What the console reads about the agent, in one go.
#[derive(Debug, Clone, Default)]
pub struct Snapshot {
	pub paused: bool,
	pub paused_by: Option<String>,
	pub paused_auto: bool,
	/// The most recent trades, newest first.
	pub trades: Vec<TradeRecord>,
	pub open_tasks: Vec<ManualTask>,
	/// The payout account's balance, if the rail can read it.
	pub balance: Option<Minor>,
	/// Payouts reserved or made today (not cancelled).
	pub today: Minor,
	pub last_tick: Option<DateTime<Utc>>,
	pub lease_holder: Option<String>,
}

/// What the console asks the agent to do. Object-safe, so the console
/// doesn't depend on the engine's type parameters.
pub trait AgentOps: Send + Sync {
	fn readiness(&self) -> BoxFuture<'_, crate::engine::Readiness>;
	fn metrics_text(&self) -> BoxFuture<'_, String>;
	fn snapshot(&self) -> BoxFuture<'_, Result<Snapshot, String>>;
	fn decide<'a>(
		&'a self,
		trade_id: &'a str,
		action: PersonAction,
		by: &'a str,
	) -> BoxFuture<'a, Result<Decision, String>>;
	fn reveal<'a>(
		&'a self,
		trade_id: &'a str,
		by: &'a str,
	) -> BoxFuture<'a, Result<Revealed, String>>;
	fn manual_paid<'a>(
		&'a self,
		trade_id: &'a str,
		reference: &'a str,
		by: &'a str,
	) -> BoxFuture<'a, Result<(), String>>;
	fn manual_failed<'a>(
		&'a self,
		trade_id: &'a str,
		by: &'a str,
	) -> BoxFuture<'a, Result<(), String>>;
	fn pause<'a>(&'a self, by: &'a str) -> BoxFuture<'a, Result<(), String>>;
	/// Resume, republishing the offers the pause withdrew. Returns what
	/// happened, for the person.
	fn resume<'a>(&'a self, by: &'a str) -> BoxFuture<'a, Result<String, String>>;
}

pub type Clock = Arc<dyn Fn() -> DateTime<Utc> + Send + Sync>;

pub struct Console {
	pub store: Arc<Store>,
	pub ops: Arc<dyn AgentOps>,
	pub sessions: Sessions,
	pub key: TotpKey,
	/// Read-only settings, as label → value. No secrets.
	pub settings: Vec<(String, String)>,
	pub limits: Option<Limits>,
	pub two_person: TwoPerson,
	pub clock: Clock,
}

pub fn router(console: Arc<Console>) -> Router {
	Router::new()
		.route("/login", get(login_page).post(login))
		.route("/logout", post(logout))
		.route("/", get(dashboard))
		.route("/trades", get(trades))
		.route("/exceptions", get(exceptions))
		.route("/audit", get(audit))
		.route("/settings", get(settings))
		.route("/users", get(users))
		.route("/users/{name}/{action}", post(user_action))
		.route("/trades/{id}/{action}", post(trade_action))
		.route("/pause", post(pause))
		.route("/resume", post(resume))
		.route(
			"/static/console.js",
			get(|| async { asset("application/javascript", pages::JS) }),
		)
		.route(
			"/static/console.css",
			get(|| async { asset("text/css", pages::CSS) }),
		)
		.with_state(console)
}

// ── responses ────────────────────────────────────────────────────────────

fn secure(mut r: Response) -> Response {
	let h = r.headers_mut();
	h.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
	h.insert(
		header::CONTENT_SECURITY_POLICY,
		HeaderValue::from_static("default-src 'none'; script-src 'self'; style-src 'self'; img-src 'self'; form-action 'self'; frame-ancestors 'none'; base-uri 'none'"),
	);
	h.insert(header::X_FRAME_OPTIONS, HeaderValue::from_static("DENY"));
	h.insert(
		header::X_CONTENT_TYPE_OPTIONS,
		HeaderValue::from_static("nosniff"),
	);
	h.insert(
		header::REFERRER_POLICY,
		HeaderValue::from_static("no-referrer"),
	);
	r
}

fn html(status: StatusCode, markup: maud::Markup) -> Response {
	secure(
		(
			status,
			[(header::CONTENT_TYPE, "text/html; charset=utf-8")],
			markup.into_string(),
		)
			.into_response(),
	)
}

fn asset(kind: &'static str, body: &'static str) -> Response {
	let mut r = ([(header::CONTENT_TYPE, kind)], body).into_response();
	r.headers_mut()
		.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
	r
}

fn to(path: &str, msg: &str) -> Response {
	let encoded: String = msg
		.bytes()
		.map(|b| {
			if b.is_ascii_alphanumeric() {
				(b as char).to_string()
			} else {
				format!("%{b:02X}")
			}
		})
		.collect();
	secure(Redirect::to(&format!("{path}?msg={encoded}")).into_response())
}

fn cookie(token: &str) -> String {
	format!("{COOKIE}={token}; Path=/; HttpOnly; Secure; SameSite=Strict")
}

fn token_from(headers: &HeaderMap) -> Option<String> {
	headers
		.get_all(header::COOKIE)
		.iter()
		.filter_map(|v| v.to_str().ok())
		.flat_map(|v| v.split(';'))
		.filter_map(|kv| kv.trim().split_once('='))
		.find(|(k, _)| *k == COOKIE)
		.map(|(_, v)| v.to_string())
}

/// The signed-in session, or a redirect to the login page.
fn session(c: &Console, headers: &HeaderMap) -> Result<Session, Response> {
	token_from(headers)
		.and_then(|t| c.sessions.get(&t, (c.clock)()))
		.ok_or_else(|| to("/login", "Please sign in."))
}

fn require(s: &Session, role: Role) -> Result<(), Response> {
	if s.role >= role {
		Ok(())
	} else {
		Err(html(
			StatusCode::FORBIDDEN,
			pages::message(
				"Not allowed",
				&format!(
					"Your role is {}. This needs {} or above. Ask an admin.",
					s.role, role
				),
			),
		))
	}
}

/// A POST must carry this session's CSRF token.
fn csrf(s: &Session, form: &HashMap<String, String>) -> Result<(), Response> {
	match form.get("csrf") {
		Some(t) if auth::same_token(t, &s.csrf) => Ok(()),
		_ => Err(html(
			StatusCode::FORBIDDEN,
			pages::message("This form expired", "Reload the page and try again."),
		)),
	}
}

type Msg = Query<HashMap<String, String>>;

fn flash(q: &Msg) -> Option<&str> {
	q.get("msg").map(String::as_str)
}

// ── sign in and out ──────────────────────────────────────────────────────

async fn login_page(Query(q): Msg) -> Response {
	html(
		StatusCode::OK,
		pages::login(q.get("msg").map(String::as_str)),
	)
}

async fn login(State(c): State<Arc<Console>>, Form(f): Form<HashMap<String, String>>) -> Response {
	let get = |k: &str| f.get(k).cloned().unwrap_or_default();
	let (username, password, code) = (get("username"), get("password"), get("code"));
	let now = (c.clock)();
	// argon2 is deliberately slow: keep it off the async workers.
	let c2 = c.clone();
	let result = tokio::task::spawn_blocking(move || {
		auth::login(&c2.store, &c2.key, &username, &password, &code, now)
	})
	.await
	.unwrap_or_else(|e| Err(auth::LoginError::Unavailable(e.to_string())));
	match result {
		Ok((user, role)) => {
			let token = c.sessions.create(&user, role, now);
			tracing::info!(user = %user, role = %role, "console sign-in");
			let mut r = Redirect::to("/").into_response();
			r.headers_mut().insert(
				header::SET_COOKIE,
				HeaderValue::from_str(&cookie(&token)).expect("a cookie header"),
			);
			secure(r)
		}
		Err(e) => to("/login", &e.to_string()),
	}
}

async fn logout(
	State(c): State<Arc<Console>>,
	headers: HeaderMap,
	Form(f): Form<HashMap<String, String>>,
) -> Response {
	let s = match session(&c, &headers) {
		Ok(s) => s,
		Err(r) => return r,
	};
	if let Err(r) = csrf(&s, &f) {
		return r;
	}
	if let Some(t) = token_from(&headers) {
		c.sessions.end(&t);
	}
	let mut r = to("/login", "Signed out.");
	r.headers_mut().insert(
		header::SET_COOKIE,
		HeaderValue::from_str(&format!(
			"{COOKIE}=; Path=/; HttpOnly; Secure; SameSite=Strict; Max-Age=0"
		))
		.expect("a cookie header"),
	);
	r
}

// ── pages ────────────────────────────────────────────────────────────────

async fn snapshot_page(
	c: Arc<Console>,
	headers: HeaderMap,
	render: impl FnOnce(&Console, &Session, &Snapshot) -> maud::Markup,
) -> Response {
	let s = match session(&c, &headers) {
		Ok(s) => s,
		Err(r) => return r,
	};
	match c.ops.snapshot().await {
		Ok(snap) => html(StatusCode::OK, render(&c, &s, &snap)),
		Err(e) => html(
			StatusCode::SERVICE_UNAVAILABLE,
			pages::message("The agent can't answer", &e),
		),
	}
}

async fn dashboard(State(c): State<Arc<Console>>, headers: HeaderMap, q: Msg) -> Response {
	let now = (c.clock)();
	snapshot_page(c, headers, |c, s, snap| {
		pages::dashboard(c, s, snap, flash(&q), now)
	})
	.await
}

async fn trades(State(c): State<Arc<Console>>, headers: HeaderMap, q: Msg) -> Response {
	snapshot_page(c, headers, |c, s, snap| {
		pages::trades(c, s, snap, flash(&q))
	})
	.await
}

async fn exceptions(State(c): State<Arc<Console>>, headers: HeaderMap, q: Msg) -> Response {
	snapshot_page(c, headers, |c, s, snap| {
		pages::exceptions(c, s, snap, flash(&q))
	})
	.await
}

async fn settings(State(c): State<Arc<Console>>, headers: HeaderMap) -> Response {
	match session(&c, &headers) {
		Ok(s) => html(StatusCode::OK, pages::settings(&c, &s)),
		Err(r) => r,
	}
}

async fn audit(State(c): State<Arc<Console>>, headers: HeaderMap) -> Response {
	let s = match session(&c, &headers) {
		Ok(s) => s,
		Err(r) => return r,
	};
	match c.store.recent_decisions(200) {
		Ok(rows) => html(StatusCode::OK, pages::audit(&s, &rows)),
		Err(e) => html(
			StatusCode::SERVICE_UNAVAILABLE,
			pages::message("The agent can't answer", &e.to_string()),
		),
	}
}

async fn users(State(c): State<Arc<Console>>, headers: HeaderMap, q: Msg) -> Response {
	let s = match session(&c, &headers) {
		Ok(s) => s,
		Err(r) => return r,
	};
	if let Err(r) = require(&s, Role::Admin) {
		return r;
	}
	match c.store.users() {
		Ok(list) => html(StatusCode::OK, pages::users(&s, &list, flash(&q))),
		Err(e) => html(
			StatusCode::SERVICE_UNAVAILABLE,
			pages::message("The agent can't answer", &e.to_string()),
		),
	}
}

// ── actions ──────────────────────────────────────────────────────────────

async fn trade_action(
	State(c): State<Arc<Console>>,
	headers: HeaderMap,
	Path((id, action)): Path<(String, String)>,
	Form(f): Form<HashMap<String, String>>,
) -> Response {
	let s = match session(&c, &headers) {
		Ok(s) => s,
		Err(r) => return r,
	};
	if let Err(r) = csrf(&s, &f) {
		return r;
	}
	let needs = match action.as_str() {
		"approve" | "repay" => Role::Approver,
		"mark-paid" | "nothing-sent" | "reveal" | "manual-paid" | "manual-failed" => Role::Operator,
		_ => {
			return html(
				StatusCode::NOT_FOUND,
				pages::message("No such action", "Go back and try again."),
			)
		}
	};
	if let Err(r) = require(&s, needs) {
		return r;
	}
	let reference = f
		.get("reference")
		.map(|r| r.trim().to_string())
		.unwrap_or_default();
	let by = s.username.as_str();
	let decided = |d: Result<Decision, String>| {
		match d {
		Ok(Decision::Applied(state)) => format!("Done. The trade is now: {}.", state.label()),
		Ok(Decision::NeedsSecondApprover) => {
			"Recorded. This payout is over the two-person threshold: a different approver must confirm it.".into()
		}
		Err(e) => format!("Not done: {e}"),
	}
	};
	let done = |r: Result<(), String>, ok: &str| match r {
		Ok(()) => ok.to_string(),
		Err(e) => format!("Not done: {e}"),
	};
	let msg = match action.as_str() {
		"approve" => decided(c.ops.decide(&id, PersonAction::Approve, by).await),
		"repay" => decided(c.ops.decide(&id, PersonAction::ApproveRepay, by).await),
		"nothing-sent" => decided(c.ops.decide(&id, PersonAction::NothingSent, by).await),
		"mark-paid" => decided(
			c.ops
				.decide(&id, PersonAction::MarkedPaid { reference }, by)
				.await,
		),
		"manual-paid" => done(
			c.ops.manual_paid(&id, &reference, by).await,
			"Recorded. The agent reports it to RecoilPay on its next pass.",
		),
		"manual-failed" => done(
			c.ops.manual_failed(&id, by).await,
			"Recorded. The trade now needs a person.",
		),
		"reveal" => {
			return match c.ops.reveal(&id, by).await {
				Ok(shown) => html(StatusCode::OK, pages::reveal(&s, &shown)),
				Err(e) => to("/exceptions", &format!("Not shown: {e}")),
			};
		}
		_ => unreachable!("checked above"),
	};
	to("/exceptions", &msg)
}

async fn pause(
	State(c): State<Arc<Console>>,
	headers: HeaderMap,
	Form(f): Form<HashMap<String, String>>,
) -> Response {
	let s = match session(&c, &headers) {
		Ok(s) => s,
		Err(r) => return r,
	};
	if let Err(r) = csrf(&s, &f).and_then(|()| require(&s, Role::Operator)) {
		return r;
	}
	match c.ops.pause(&s.username).await {
		Ok(()) => to("/", "Paused. No new payouts start, and your offers are withdrawn. Payouts already with the bank carry on."),
		Err(e) => to("/", &format!("Not paused: {e}")),
	}
}

async fn resume(
	State(c): State<Arc<Console>>,
	headers: HeaderMap,
	Form(f): Form<HashMap<String, String>>,
) -> Response {
	let s = match session(&c, &headers) {
		Ok(s) => s,
		Err(r) => return r,
	};
	if let Err(r) = csrf(&s, &f) {
		return r;
	}
	// The agent paused itself for a reason: resuming that needs an approver.
	let auto = c
		.ops
		.snapshot()
		.await
		.map(|snap| snap.paused_auto)
		.unwrap_or(true);
	let needs = if auto { Role::Approver } else { Role::Operator };
	if let Err(r) = require(&s, needs) {
		return r;
	}
	match c.ops.resume(&s.username).await {
		Ok(done) => to("/", &done),
		Err(e) => to("/", &format!("Not resumed: {e}")),
	}
}

async fn user_action(
	State(c): State<Arc<Console>>,
	headers: HeaderMap,
	Path((name, action)): Path<(String, String)>,
	Form(f): Form<HashMap<String, String>>,
) -> Response {
	let s = match session(&c, &headers) {
		Ok(s) => s,
		Err(r) => return r,
	};
	if let Err(r) = csrf(&s, &f).and_then(|()| require(&s, Role::Admin)) {
		return r;
	}
	if name == s.username {
		return to("/users", "You can't disable your own account.");
	}
	let disabled = match action.as_str() {
		"disable" => true,
		"enable" => false,
		_ => {
			return html(
				StatusCode::NOT_FOUND,
				pages::message("No such action", "Go back and try again."),
			)
		}
	};
	match c.store.set_user_disabled(&name, disabled, &s.username) {
		Ok(true) => {
			tracing::info!(admin = %s.username, user = %name, disabled, "console user changed");
			to("/users", if disabled { "Disabled." } else { "Enabled." })
		}
		Ok(false) => to("/users", "No such user."),
		Err(e) => to("/users", &format!("Not changed: {e}")),
	}
}
