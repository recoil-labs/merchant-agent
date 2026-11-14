//! The template rail and the adapter conformance suite.
//!
//! The suite passes on the simulated bank, and on the template rail talking
//! the adapter protocol to that same bank; it catches a careless adapter;
//! and the engine's fault drills run through the template rail end to end,
//! never paying twice.

mod common;
use common::*;

use merchant_agent_core::doctor::Outcome;
use merchant_agent_core::money::Minor;
use merchant_agent_core::rail::conformance::{self, Accounts};
use merchant_agent_core::rail::protocol_server;
use merchant_agent_core::rail::simulated::Faults;
use merchant_agent_core::rail::template::TemplateRail;
use merchant_agent_core::rail::AccountLookup;

const TOKEN: &str = "test-adapter-token-0000";

fn accounts() -> Accounts {
	Accounts {
		settle_within: Duration::from_secs(10),
		..Accounts::default()
	}
}

/// A bank that knows the suite's test account, and the engine's payee.
fn test_bank() -> SimulatedRail {
	let bank = SimulatedRail::with_balance(Minor(100_000_000));
	bank.add_account("058", "0000000001", "TEST PAYEE ONE");
	bank.add_account("058", ACCOUNT, "TEST PAYEE");
	bank
}

/// Serve `rail` over the protocol on a free port.
async fn serve<R: PayoutRail + 'static>(rail: R, name: &str) -> String {
	let app = protocol_server::router(rail, Secret::new(TOKEN.into()), name);
	let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
	let url = format!("http://{}", listener.local_addr().unwrap());
	tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
	url
}

async fn connect(url: &str) -> TemplateRail {
	TemplateRail::connect(url, Secret::new(TOKEN.into()), Duration::from_secs(5))
		.await
		.unwrap()
}

fn outcome<'a>(r: &'a merchant_agent_core::doctor::Report, name: &str) -> &'a Outcome {
	&r.checks
		.iter()
		.find(|c| c.name.starts_with(name))
		.unwrap_or_else(|| panic!("no check {name}"))
		.outcome
}

fn failed(r: &merchant_agent_core::doctor::Report) -> Vec<String> {
	r.checks
		.iter()
		.filter(|c| matches!(c.outcome, Outcome::Fail { .. }))
		.map(|c| c.name.clone())
		.collect()
}

// ── the suite ────────────────────────────────────────────────────────────

#[tokio::test]
async fn the_simulated_bank_passes_the_conformance_suite() {
	let report = conformance::run(&test_bank(), &accounts()).await;
	assert!(report.passed(), "{report}");
	assert!(matches!(
		outcome(&report, "The balance moved"),
		Outcome::Pass(_)
	));
}

#[tokio::test]
async fn the_template_rail_passes_over_the_protocol() {
	let bank = test_bank();
	let url = serve(bank.clone(), "simulated bank").await;
	let rail = connect(&url).await;
	assert!(rail.idempotent_send());
	assert_eq!(rail.info().name, "simulated bank");
	let report = conformance::run(&rail, &accounts()).await;
	assert!(report.passed(), "{report}");
	assert!(bank.ledger_balances());
	// Every key paid at most once at the bank itself.
	let mut keys: Vec<_> = bank
		.payments()
		.iter()
		.map(|p| p.idempotency_key.clone())
		.collect();
	let n = keys.len();
	keys.dedup();
	assert_eq!(keys.len(), n);
}

/// An adapter that ignores the key: every send is a new transfer.
struct Careless {
	bank: SimulatedRail,
	sends: AtomicUsize,
	latest: Mutex<HashMap<String, String>>,
}

impl PayoutRail for Careless {
	fn idempotent_send(&self) -> bool {
		true // it says so; the suite checks
	}
	async fn verify_account(
		&self,
		b: &str,
		a: &merchant_agent_core::pii::AccountNumber,
	) -> Result<AccountLookup, RailError> {
		self.bank.verify_account(b, a).await
	}
	async fn send(&self, req: PayoutRequest<'_>) -> Result<SendOutcome, RailError> {
		let fresh = format!(
			"{}-{}",
			req.idempotency_key,
			self.sends.fetch_add(1, Ordering::SeqCst)
		);
		self.latest
			.lock()
			.unwrap()
			.insert(req.idempotency_key.into(), fresh.clone());
		self.bank
			.send(PayoutRequest {
				idempotency_key: &fresh,
				..req
			})
			.await
	}
	async fn status(&self, key: &str) -> Result<PayoutStatus, RailError> {
		let inner = self.latest.lock().unwrap().get(key).cloned();
		match inner {
			Some(k) => self.bank.status(&k).await,
			None => Ok(PayoutStatus::NotFound),
		}
	}
	async fn balance(&self) -> Result<Option<Minor>, RailError> {
		self.bank.balance().await
	}
}

#[tokio::test]
async fn the_suite_catches_an_adapter_that_pays_a_repeated_key_twice() {
	let careless = Careless {
		bank: test_bank(),
		sends: AtomicUsize::new(0),
		latest: Mutex::new(HashMap::new()),
	};
	let url = serve(careless, "careless").await;
	let report = conformance::run(&connect(&url).await, &accounts()).await;
	let failed = failed(&report);
	assert!(!report.passed());
	for check in [
		"The same key again pays nothing more",
		"Two sends at once under one key pay once",
		"The balance moved by exactly what was paid",
	] {
		assert!(
			failed.iter().any(|f| f == check),
			"{check} should fail: {report}"
		);
	}
}

/// An adapter that answers a refusal with an HTTP error instead of an
/// explicit `rejected`.
struct ErrorsInsteadOfRefusing(SimulatedRail);

impl PayoutRail for ErrorsInsteadOfRefusing {
	fn idempotent_send(&self) -> bool {
		true
	}
	async fn verify_account(
		&self,
		b: &str,
		a: &merchant_agent_core::pii::AccountNumber,
	) -> Result<AccountLookup, RailError> {
		self.0.verify_account(b, a).await
	}
	async fn send(&self, req: PayoutRequest<'_>) -> Result<SendOutcome, RailError> {
		match self.0.send(req).await? {
			SendOutcome::Rejected { .. } => Err(RailError("refused".into())),
			other => Ok(other),
		}
	}
	async fn status(&self, key: &str) -> Result<PayoutStatus, RailError> {
		self.0.status(key).await
	}
	async fn balance(&self) -> Result<Option<Minor>, RailError> {
		self.0.balance().await
	}
}

#[tokio::test]
async fn the_suite_requires_an_explicit_rejection() {
	// The simulated bank refuses unknown accounts only by lookup; make the
	// over-balance payout the refusal that comes back as an error.
	let url = serve(ErrorsInsteadOfRefusing(test_bank()), "errors").await;
	let report = conformance::run(&connect(&url).await, &accounts()).await;
	assert!(
		failed(&report)
			.iter()
			.any(|f| f == "A payout larger than the balance is refused"),
		"{report}"
	);
}

// ── the protocol, edge by edge ───────────────────────────────────────────

#[tokio::test]
async fn a_wrong_token_or_protocol_is_refused_at_connect() {
	let url = serve(test_bank(), "bank").await;
	let e = TemplateRail::connect(&url, Secret::new("wrong".into()), Duration::from_secs(5))
		.await
		.err()
		.unwrap();
	assert!(e.contains("refused the token"), "{e}");

	let app = axum::Router::new().route(
		"/v1/info",
		axum::routing::get(|| async {
			axum::Json(json!({"protocol": 2, "name": "future", "idempotent_send": true}))
		}),
	);
	let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
	let url = format!("http://{}", listener.local_addr().unwrap());
	tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
	let e = TemplateRail::connect(&url, Secret::new(TOKEN.into()), Duration::from_secs(5))
		.await
		.err()
		.unwrap();
	assert!(e.contains("protocol 2"), "{e}");
}

/// A scripted adapter: `/v1/info` is fine; the payout answers are given.
async fn scripted(payout: (u16, &'static str), status: (u16, &'static str)) -> TemplateRail {
	use axum::http::StatusCode;
	use axum::routing::{get, post};
	let app = axum::Router::new()
		.route(
			"/v1/info",
			get(|| async {
				axum::Json(json!({"protocol": 1, "name": "scripted", "idempotent_send": false}))
			}),
		)
		.route(
			"/v1/payouts",
			post(move || async move { (StatusCode::from_u16(payout.0).unwrap(), payout.1) }),
		)
		.route(
			"/v1/payouts/{key}",
			get(move || async move { (StatusCode::from_u16(status.0).unwrap(), status.1) }),
		);
	let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
	let url = format!("http://{}", listener.local_addr().unwrap());
	tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
	connect(&url).await
}

async fn send_one(rail: &TemplateRail) -> Result<SendOutcome, RailError> {
	let account = merchant_agent_core::pii::AccountNumber::new(ACCOUNT.into());
	let name = merchant_agent_core::pii::HolderName::new(NAME.into());
	rail.send(PayoutRequest {
		idempotency_key: "rpma-ft-7c1e9a2b4d6f48e0a1b2c3d4e5f60718",
		trade_id: TRADE_ID,
		amount: Minor(100),
		bank_code: "058",
		account: &account,
		name: &name,
		narration: "RecoilPay",
	})
	.await
}

#[tokio::test]
async fn only_a_200_with_a_known_result_is_an_answer() {
	// An error status, even a 4xx, is "could not ask": never "nothing moved".
	for (code, body) in [
		(400, r#"{"result":"rejected","reason":"x"}"#),
		(500, "{}"),
		(200, r#"{"result":"maybe"}"#),
		(200, "not json"),
	] {
		let rail = scripted((code, body), (200, r#"{"result":"pending"}"#)).await;
		let e = send_one(&rail).await;
		assert!(e.is_err(), "{code} {body} gave {e:?}");
		assert!(!rail.idempotent_send());
	}
	let rail = scripted(
		(
			200,
			r#"{"result":"rejected","reason":"account 0000000000 closed"}"#,
		),
		(200, "{}"),
	)
	.await;
	match send_one(&rail).await.unwrap() {
		SendOutcome::Rejected { reason } => assert!(!reason.contains(ACCOUNT), "masked: {reason}"),
		other => panic!("{other:?}"),
	}
	let rail = scripted(
		(200, r#"{"result":"accepted","reference":"  "}"#),
		(200, "{}"),
	)
	.await;
	assert_eq!(
		send_one(&rail).await.unwrap(),
		SendOutcome::Unknown,
		"accepted with no reference"
	);
	let rail = scripted(
		(200, r#"{"result":"unknown"}"#),
		(200, r#"{"result":"failed","reason":"reversed"}"#),
	)
	.await;
	assert_eq!(send_one(&rail).await.unwrap(), SendOutcome::Unknown);
	assert!(matches!(
		rail.status("rpma-ft-7c1e9a2b4d6f48e0a1b2c3d4e5f60718")
			.await
			.unwrap(),
		PayoutStatus::Failed { .. }
	));
}

// ── the engine's fault drills, through the adapter ───────────────────────

type ViaAdapter = Engine<FakePlatform, FakeLocks, PayeeChecks, TemplateRail, ManualClock>;

fn state_of(agent: &ViaAdapter) -> AgentState {
	agent.store().get(TRADE_ID).unwrap().unwrap().state
}

async fn settle(agent: &ViaAdapter) -> AgentState {
	for _ in 0..30 {
		agent.tick().await.unwrap();
		let s = state_of(agent);
		if reported(&s) || matches!(s, AgentState::NeedsPerson(_)) {
			return s;
		}
	}
	panic!("never settled; stuck at {:?}", state_of(agent));
}

#[tokio::test]
async fn fault_drills_through_the_adapter_never_pay_twice() {
	let drills = [
		Faults {
			lost_sends: 1,
			..Default::default()
		},
		Faults {
			errors_after_paying: 1,
			..Default::default()
		},
		Faults {
			duplicate_delivery: true,
			..Default::default()
		},
		Faults {
			pending_polls: 3,
			..Default::default()
		},
		Faults {
			errors_after_paying: 1,
			status_outages: 2,
			..Default::default()
		},
		Faults {
			lost_sends: 1,
			errors_after_paying: 1,
			duplicate_delivery: true,
			..Default::default()
		},
		Faults {
			errors_after_paying: 1,
			status_unknown: true,
			..Default::default()
		},
	];
	for faults in drills {
		let world = World::new();
		let bank = test_bank();
		bank.set_faults(faults.clone());
		let url = serve(bank.clone(), "simulated bank").await;
		let dir = tempfile::tempdir().unwrap();
		let store = Store::open(
			dir.path(),
			&format!("adapter-{}", NEXT_HOLDER.fetch_add(1, Ordering::SeqCst)),
		)
		.unwrap();
		let agent: ViaAdapter = Engine::start(
			Arc::new(store),
			world.platform.clone(),
			world.locks.clone(),
			checks(),
			connect(&url).await,
			world.clock.clone(),
			World::cfg(),
		)
		.unwrap();
		let end = settle(&agent).await;
		let paid = bank.settled_payments().len();
		assert!(paid <= 1, "{faults:?}: paid {paid} times");
		if reported(&end) {
			assert_eq!(paid, 1, "{faults:?}: reported without paying");
		}
		assert!(bank.ledger_balances(), "{faults:?}");
	}
}
