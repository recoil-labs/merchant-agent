//! The console, driven in-process against the real engine.
//!
//! Auth: wrong password, wrong code, lockout, code replay, idle expiry,
//! CSRF. Roles: every action against every role. Then the exception queue
//! end to end: an approver signs in, finds a held trade and clears it, and
//! the engine pays it once.

mod common;
use common::*;

use std::sync::Arc;

use axum::body::Body;
use axum::http::{header, Request, StatusCode};
use chrono::Duration as ChronoDuration;
use merchant_agent_core::console::auth::{self, Role, Sessions, TotpKey, LOCKOUT};
use merchant_agent_core::console::{router, Console};
use merchant_agent_core::store::Store;
use tower::ServiceExt;

const PASSWORD: &str = "correct horse battery staple";
const PEPPER: &str = "test-pepper";

struct Harness {
	world: World,
	agent: Arc<Agent>,
	console: Arc<Console>,
	now: Arc<Mutex<chrono::DateTime<chrono::Utc>>>,
	_dir: tempfile::TempDir,
}

impl Harness {
	/// A held trade (the bank's name doesn't match), and one user per role.
	async fn new(bank_name: &str) -> Self {
		let dir = tempfile::tempdir().unwrap();
		let world = World::new();
		world.bank.inner.add_account("058", ACCOUNT, bank_name);
		let agent = Arc::new(world.agent(dir.path()).unwrap());
		agent.tick().await.unwrap();

		let key = || TotpKey::from_pepper(&Secret::new(PEPPER.into()));
		let now = Arc::new(Mutex::new(start_time()));
		for (name, role) in [
			("vic", Role::Viewer),
			("ola", Role::Operator),
			("ada", Role::Approver),
			("bayo", Role::Approver),
			("root", Role::Admin),
		] {
			auth::add_user(agent.store(), &key(), name, role, PASSWORD, start_time()).unwrap();
		}
		let clock_now = now.clone();
		let console = Arc::new(Console {
			store: agent.store().clone(),
			ops: agent.clone(),
			sessions: Sessions::default(),
			key: key(),
			settings: vec![("Mode".into(), "sandbox".into())],
			limits: None,
			two_person: merchant_agent_core::config::TwoPerson {
				enabled: false,
				approve_over: None,
			},
			clock: Arc::new(move || *clock_now.lock().unwrap()),
		});
		Self {
			world,
			agent,
			console,
			now,
			_dir: dir,
		}
	}

	fn advance(&self, d: ChronoDuration) {
		*self.now.lock().unwrap() += d;
	}

	fn code(&self, user: &str) -> String {
		auth::current_code(
			&self.console.store,
			&self.console.key,
			user,
			*self.now.lock().unwrap(),
		)
		.unwrap()
	}

	async fn send(&self, req: Request<Body>) -> axum::response::Response {
		router(self.console.clone()).oneshot(req).await.unwrap()
	}

	async fn post(
		&self,
		path: &str,
		cookie: Option<&str>,
		form: &[(&str, &str)],
	) -> axum::response::Response {
		let body: Vec<String> = form
			.iter()
			.map(|(k, v)| format!("{k}={}", urlencode(v)))
			.collect();
		let mut req =
			Request::post(path).header(header::CONTENT_TYPE, "application/x-www-form-urlencoded");
		if let Some(c) = cookie {
			req = req.header(header::COOKIE, c);
		}
		self.send(req.body(Body::from(body.join("&"))).unwrap())
			.await
	}

	async fn get(&self, path: &str, cookie: Option<&str>) -> axum::response::Response {
		let mut req = Request::get(path);
		if let Some(c) = cookie {
			req = req.header(header::COOKIE, c);
		}
		self.send(req.body(Body::empty()).unwrap()).await
	}

	/// Sign in; the session cookie and its CSRF token.
	async fn login(&self, user: &str) -> (String, String) {
		let code = self.code(user);
		let r = self
			.post(
				"/login",
				None,
				&[("username", user), ("password", PASSWORD), ("code", &code)],
			)
			.await;
		let cookie = r
			.headers()
			.get(header::SET_COOKIE)
			.unwrap_or_else(|| panic!("{user} should sign in: {:?}", location(&r)))
			.to_str()
			.unwrap()
			.split(';')
			.next()
			.unwrap()
			.to_string();
		let page = body(self.get("/", Some(&cookie)).await).await;
		let csrf = page
			.split("name=\"csrf\" value=\"")
			.nth(1)
			.unwrap()
			.split('"')
			.next()
			.unwrap()
			.to_string();
		// The next login in the same 30 s step must use a later code.
		self.advance(ChronoDuration::seconds(30));
		(cookie, csrf)
	}
}

fn urlencode(s: &str) -> String {
	s.bytes()
		.map(|b| {
			if b.is_ascii_alphanumeric() || b == b'-' || b == b'_' {
				(b as char).to_string()
			} else {
				format!("%{b:02X}")
			}
		})
		.collect()
}

fn location(r: &axum::response::Response) -> String {
	r.headers()
		.get(header::LOCATION)
		.map(|v| v.to_str().unwrap().to_string())
		.unwrap_or_default()
}

async fn body(r: axum::response::Response) -> String {
	let bytes = axum::body::to_bytes(r.into_body(), 1 << 20).await.unwrap();
	String::from_utf8(bytes.to_vec()).unwrap()
}

fn decoded(location: &str) -> String {
	let mut out = Vec::new();
	let b = location.as_bytes();
	let mut i = 0;
	while i < b.len() {
		if b[i] == b'%' && i + 2 < b.len() {
			out.push(
				u8::from_str_radix(std::str::from_utf8(&b[i + 1..i + 3]).unwrap(), 16).unwrap(),
			);
			i += 3;
		} else {
			out.push(b[i]);
			i += 1;
		}
	}
	String::from_utf8(out).unwrap()
}

// ── signing in ───────────────────────────────────────────────────────────

#[tokio::test]
async fn a_wrong_password_or_code_is_refused_without_saying_which() {
	let h = Harness::new("TEST PAYEE").await;
	let code = h.code("ola");
	for (pw, c) in [("wrong password!!", code.as_str()), (PASSWORD, "000000")] {
		let r = h
			.post(
				"/login",
				None,
				&[("username", "ola"), ("password", pw), ("code", c)],
			)
			.await;
		assert!(r.headers().get(header::SET_COOKIE).is_none());
		assert!(decoded(&location(&r)).contains("wrong username, password or code"));
	}
	let r = h
		.post(
			"/login",
			None,
			&[
				("username", "nobody"),
				("password", PASSWORD),
				("code", &code),
			],
		)
		.await;
	assert!(
		decoded(&location(&r)).contains("wrong username, password or code"),
		"no hint that the user doesn't exist"
	);
}

#[tokio::test]
async fn five_failures_lock_the_account_until_the_lockout_ends() {
	let h = Harness::new("TEST PAYEE").await;
	for i in 1..=5 {
		let r = h
			.post(
				"/login",
				None,
				&[
					("username", "ola"),
					("password", "nope nope nope"),
					("code", "123456"),
				],
			)
			.await;
		assert_eq!(
			decoded(&location(&r)).contains("locked"),
			i == 5,
			"attempt {i}"
		);
	}
	let code = h.code("ola");
	let r = h
		.post(
			"/login",
			None,
			&[("username", "ola"), ("password", PASSWORD), ("code", &code)],
		)
		.await;
	assert!(
		decoded(&location(&r)).contains("locked"),
		"even the right password, while locked"
	);
	h.advance(LOCKOUT + ChronoDuration::seconds(1));
	h.login("ola").await;
}

#[tokio::test]
async fn a_code_cannot_be_used_twice() {
	let h = Harness::new("TEST PAYEE").await;
	let code = h.code("ola");
	let form = [
		("username", "ola"),
		("password", PASSWORD),
		("code", code.as_str()),
	];
	assert!(h
		.post("/login", None, &form)
		.await
		.headers()
		.get(header::SET_COOKIE)
		.is_some());
	assert!(h
		.post("/login", None, &form)
		.await
		.headers()
		.get(header::SET_COOKIE)
		.is_none());
}

#[tokio::test]
async fn an_idle_session_expires_after_fifteen_minutes() {
	let h = Harness::new("TEST PAYEE").await;
	let (cookie, _) = h.login("vic").await;
	h.advance(ChronoDuration::minutes(14));
	assert_eq!(
		h.get("/", Some(&cookie)).await.status(),
		StatusCode::OK,
		"activity keeps it alive"
	);
	h.advance(ChronoDuration::minutes(15));
	let r = h.get("/", Some(&cookie)).await;
	assert!(
		location(&r).starts_with("/login"),
		"expired: back to sign in"
	);
}

#[tokio::test]
async fn pages_need_a_session_and_carry_strict_headers() {
	let h = Harness::new("TEST PAYEE").await;
	for page in [
		"/",
		"/trades",
		"/exceptions",
		"/audit",
		"/settings",
		"/users",
	] {
		assert!(
			location(&h.get(page, None).await).starts_with("/login"),
			"{page}"
		);
	}
	let (cookie, _) = h.login("vic").await;
	let r = h.get("/", Some(&cookie)).await;
	let headers = r.headers();
	assert_eq!(headers[header::CACHE_CONTROL], "no-store");
	assert!(headers[header::CONTENT_SECURITY_POLICY]
		.to_str()
		.unwrap()
		.contains("script-src 'self'"));
	assert_eq!(headers[header::X_FRAME_OPTIONS], "DENY");
	let set_cookie = h
		.post(
			"/login",
			None,
			&[
				("username", "ada"),
				("password", PASSWORD),
				("code", &h.code("ada")),
			],
		)
		.await
		.headers()[header::SET_COOKIE]
		.to_str()
		.unwrap()
		.to_string();
	for attr in ["HttpOnly", "Secure", "SameSite=Strict"] {
		assert!(set_cookie.contains(attr), "{set_cookie}");
	}
}

#[tokio::test]
async fn a_form_without_the_sessions_csrf_token_is_refused() {
	let h = Harness::new("TEST PAYEE").await;
	let (cookie, csrf) = h.login("ola").await;
	assert_eq!(
		h.post("/pause", Some(&cookie), &[]).await.status(),
		StatusCode::FORBIDDEN
	);
	assert_eq!(
		h.post("/pause", Some(&cookie), &[("csrf", "forged")])
			.await
			.status(),
		StatusCode::FORBIDDEN
	);
	let (_, other_csrf) = h.login("ada").await;
	assert_eq!(
		h.post("/pause", Some(&cookie), &[("csrf", &other_csrf)])
			.await
			.status(),
		StatusCode::FORBIDDEN,
		"another session's token is not this one's"
	);
	assert!(!h.agent.store().is_paused().unwrap());
	assert!(h
		.post("/pause", Some(&cookie), &[("csrf", &csrf)])
		.await
		.status()
		.is_redirection());
	assert!(h.agent.store().is_paused().unwrap());
}

// ── roles ────────────────────────────────────────────────────────────────

#[tokio::test]
async fn each_action_needs_its_role() {
	let h = Harness::new("SOMEONE ELSE").await;
	assert_eq!(state(&h.agent), AgentState::Held(HoldReason::NameMismatch));
	let path = format!("/trades/{TRADE_ID}/approve");

	let (vic, vic_csrf) = h.login("vic").await;
	assert_eq!(
		h.post("/pause", Some(&vic), &[("csrf", &vic_csrf)])
			.await
			.status(),
		StatusCode::FORBIDDEN,
		"a viewer can't pause"
	);
	assert_eq!(
		h.post(&path, Some(&vic), &[("csrf", &vic_csrf)])
			.await
			.status(),
		StatusCode::FORBIDDEN
	);
	assert_eq!(
		h.get("/users", Some(&vic)).await.status(),
		StatusCode::FORBIDDEN,
		"users are for admins"
	);

	let (ola, ola_csrf) = h.login("ola").await;
	assert_eq!(
		h.post(&path, Some(&ola), &[("csrf", &ola_csrf)])
			.await
			.status(),
		StatusCode::FORBIDDEN,
		"an operator can't approve"
	);
	assert_eq!(state(&h.agent), AgentState::Held(HoldReason::NameMismatch));

	let (root, _) = h.login("root").await;
	assert_eq!(h.get("/users", Some(&root)).await.status(), StatusCode::OK);
}

#[tokio::test]
async fn resuming_after_an_automatic_pause_needs_an_approver() {
	let h = Harness::new("TEST PAYEE").await;
	h.agent.pause("agent", true).await.unwrap();
	let (ola, ola_csrf) = h.login("ola").await;
	assert_eq!(
		h.post("/resume", Some(&ola), &[("csrf", &ola_csrf)])
			.await
			.status(),
		StatusCode::FORBIDDEN
	);
	assert!(h.agent.store().is_paused().unwrap());
	let (ada, ada_csrf) = h.login("ada").await;
	assert!(h
		.post("/resume", Some(&ada), &[("csrf", &ada_csrf)])
		.await
		.status()
		.is_redirection());
	assert!(!h.agent.store().is_paused().unwrap());
}

// ── the exception queue ──────────────────────────────────────────────────

#[tokio::test]
async fn an_approver_finds_and_clears_a_held_trade() {
	let h = Harness::new("SOMEONE ELSE").await;
	let (ada, csrf) = h.login("ada").await;

	let page = body(h.get("/exceptions", Some(&ada)).await).await;
	assert!(page.contains(TRADE_ID), "the held trade is in the queue");
	assert!(
		page.contains("doesn&#39;t match") || page.contains("doesn't match"),
		"with its reason in plain words"
	);
	assert!(
		page.contains("******0000") && !page.contains(ACCOUNT),
		"masked, never the account number"
	);
	assert!(page.contains(&format!("/trades/{TRADE_ID}/approve")));

	let r = h
		.post(
			&format!("/trades/{TRADE_ID}/approve"),
			Some(&ada),
			&[("csrf", &csrf)],
		)
		.await;
	assert!(
		decoded(&location(&r)).contains("Done"),
		"{}",
		decoded(&location(&r))
	);
	assert_eq!(
		h.post("/logout", Some(&ada), &[("csrf", &csrf)])
			.await
			.status(),
		StatusCode::SEE_OTHER
	);

	for _ in 0..5 {
		h.agent.tick().await.unwrap();
	}
	assert!(reported(&state(&h.agent)));
	assert_eq!(h.world.bank.inner.settled_payments().len(), 1);
	assert!(
		location(&h.get("/", Some(&ada)).await).starts_with("/login"),
		"signed out"
	);
}

#[tokio::test]
async fn invariant_11_pausing_from_the_console_withdraws_the_offers() {
	let h = Harness::new("TEST PAYEE").await;
	let (ola, csrf) = h.login("ola").await;
	h.post("/pause", Some(&ola), &[("csrf", &csrf)]).await;
	assert!(*h.world.platform.offers_paused.lock().unwrap());
	let (paused, by, auto, _) = h.agent.store().control().unwrap();
	assert_eq!((paused, by.as_deref(), auto), (true, Some("ola"), false));
}

// ── the manual rail's reveal ─────────────────────────────────────────────

#[tokio::test]
async fn a_reveal_is_recorded_shown_once_and_hidden_after_a_minute() {
	let dir = tempfile::tempdir().unwrap();
	let world = World::new();
	let store = Arc::new(Store::open(dir.path(), "manual").unwrap());
	let agent = Arc::new(
		merchant_agent_core::engine::Engine::start(
			store.clone(),
			world.platform.clone(),
			world.locks.clone(),
			checks(),
			merchant_agent_core::rail::manual::ManualRail::new(store.clone(), world.clock.clone()),
			world.clock.clone(),
			World::cfg(),
		)
		.unwrap(),
	);
	agent.tick().await.unwrap();
	let key = TotpKey::from_pepper(&Secret::new(PEPPER.into()));
	auth::add_user(&store, &key, "ola", Role::Operator, PASSWORD, start_time()).unwrap();
	auth::add_user(&store, &key, "vic", Role::Viewer, PASSWORD, start_time()).unwrap();
	let now = Arc::new(Mutex::new(start_time()));
	let clock_now = now.clone();
	let console = Arc::new(Console {
		store: store.clone(),
		ops: agent.clone(),
		sessions: Sessions::default(),
		key,
		settings: vec![],
		limits: None,
		two_person: merchant_agent_core::config::TwoPerson {
			enabled: false,
			approve_over: None,
		},
		clock: Arc::new(move || *clock_now.lock().unwrap()),
	});
	let h = Harness {
		world,
		agent: Arc::new(
			World::new()
				.agent(tempfile::tempdir().unwrap().path())
				.unwrap(),
		),
		console,
		now,
		_dir: dir,
	};

	let (vic, vic_csrf) = h.login("vic").await;
	let path = format!("/trades/{TRADE_ID}/reveal");
	assert_eq!(
		h.post(&path, Some(&vic), &[("csrf", &vic_csrf)])
			.await
			.status(),
		StatusCode::FORBIDDEN,
		"viewers can't reveal"
	);

	let (ola, csrf) = h.login("ola").await;
	let r = h.post(&path, Some(&ola), &[("csrf", &csrf)]).await;
	assert_eq!(r.headers()[header::CACHE_CONTROL], "no-store");
	let page = body(r).await;
	assert!(
		page.contains(ACCOUNT) && page.contains(NAME),
		"the person paying sees the details"
	);
	assert!(
		page.contains("data-hide-after=\"60\""),
		"and the page hides them after a minute"
	);
	let task = store.open_task_for(TRADE_ID).unwrap().unwrap();
	assert_eq!(
		(task.reveals, task.last_revealed_by.as_deref()),
		(1, Some("ola")),
		"recorded"
	);

	let r = h
		.post(
			&format!("/trades/{TRADE_ID}/manual-paid"),
			Some(&ola),
			&[("csrf", &csrf), ("reference", "BANKAPP-1")],
		)
		.await;
	assert!(decoded(&location(&r)).contains("Recorded"));
	agent.tick().await.unwrap();
	assert_eq!(
		agent.store().get(TRADE_ID).unwrap().unwrap().state,
		AgentState::Reported {
			bank_confirmed: true
		}
	);
	let r = h.post(&path, Some(&ola), &[("csrf", &csrf)]).await;
	assert!(
		decoded(&location(&r)).contains("Not shown"),
		"no task, no reveal"
	);
}
