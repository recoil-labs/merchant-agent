//! The engine tests' world: fake RecoilPay, fake lock check, the simulated
//! bank, a manual clock. Shared by `engine.rs` and `rails.rs`.

#![allow(dead_code, unused_imports)]

pub use std::collections::HashMap;
pub use std::path::Path;
pub use std::sync::atomic::{AtomicUsize, Ordering};
pub use std::sync::{Arc, Mutex};
pub use std::time::Duration;

pub use chrono::{DateTime, Utc};
pub use merchant_agent_core::aggregator::types::{Trade, TradeState};
pub use merchant_agent_core::aggregator::{AggregatorError, ErrorBody, ReportOutcome};
pub use merchant_agent_core::engine::{
	Clock, CrashHook, CrashPoint, Engine, EngineConfig, EngineError, LockCheck, Locks, PayeeChecks,
	Platform,
};
pub use merchant_agent_core::machine::{
	AgentState, HoldReason, LockOutcome, PayoutStatus, PersonReason, SendOutcome,
};
pub use merchant_agent_core::payee::PayeePrivateKey;
pub use merchant_agent_core::rail::simulated::SimulatedRail;
pub use merchant_agent_core::rail::{PayoutRail, PayoutRequest, RailError};
pub use merchant_agent_core::secrets::Secret;
pub use merchant_agent_core::store::{Store, StoreError};
pub use serde_json::{json, Value};

pub const API: &str = include_str!("../../../../vectors/merchant-agent-api.json");
pub const PAYEE: &str = include_str!("../../../../vectors/fiat-payee.json");
pub const SOLVER: &str = "solver-1111111111111111111111111111111111111111";
pub const TRADE_ID: &str = "ft-7c1e9a2b4d6f48e0a1b2c3d4e5f60718";
/// The sealed vector's account and name: they must never be stored.
pub const ACCOUNT: &str = "0000000000";
pub const NAME: &str = "Test Payee";

pub fn start_time() -> DateTime<Utc> {
	"2026-11-13T12:00:00Z".parse().unwrap()
}

// ── the world ────────────────────────────────────────────────────────────

#[derive(Clone)]
pub struct ManualClock(pub Arc<Mutex<DateTime<Utc>>>);

impl ManualClock {
	pub fn advance(&self, d: Duration) {
		let mut t = self.0.lock().unwrap();
		*t += chrono::Duration::from_std(d).unwrap();
	}
}

impl Clock for ManualClock {
	fn now(&self) -> DateTime<Utc> {
		*self.0.lock().unwrap()
	}
}

#[derive(Clone, Default)]
pub struct FakePlatform {
	pub trades: Arc<Mutex<HashMap<String, Trade>>>,
	pub reports: Arc<Mutex<Vec<(String, String)>>>,
	pub statements: Arc<Mutex<Vec<String>>>,
	pub offers_paused: Arc<Mutex<bool>>,
	/// The operator's quote book, as RecoilPay serves it.
	pub quotes: Arc<Mutex<Vec<merchant_agent_core::aggregator::Quote>>>,
	pub quotes_down: Arc<Mutex<bool>>,
	/// RecoilPay unreachable: pings and heartbeats fail.
	pub down: Arc<Mutex<bool>>,
	pub heartbeats: Arc<AtomicUsize>,
}

impl FakePlatform {
	pub fn edit(&self, f: impl FnOnce(&mut Trade)) {
		f(self.trades.lock().unwrap().get_mut(TRADE_ID).unwrap());
	}
	pub fn get(&self) -> Trade {
		self.trades.lock().unwrap()[TRADE_ID].clone()
	}
}

impl Platform for FakePlatform {
	async fn awaiting_fiat(&self) -> Result<Vec<Trade>, AggregatorError> {
		Ok(self
			.trades
			.lock()
			.unwrap()
			.values()
			.filter(|t| t.state == TradeState::AwaitingFiat)
			.cloned()
			.collect())
	}
	async fn trade(&self, id: &str) -> Result<Trade, AggregatorError> {
		self.trades
			.lock()
			.unwrap()
			.get(id)
			.cloned()
			.ok_or(AggregatorError::NotFound(ErrorBody::default()))
	}
	async fn report(&self, id: &str, reference: &str) -> Result<ReportOutcome, AggregatorError> {
		self.reports
			.lock()
			.unwrap()
			.push((id.into(), reference.into()));
		let mut trades = self.trades.lock().unwrap();
		let t = trades.get_mut(id).unwrap();
		match t.state {
			TradeState::AwaitingFiat => {
				t.state = TradeState::AwaitingConfirmation;
				t.payment_reference = Some(reference.into());
				Ok(ReportOutcome::Reported(Box::new(t.clone())))
			}
			TradeState::AwaitingConfirmation
				if t.payment_reference.as_deref() == Some(reference) =>
			{
				Ok(ReportOutcome::Reported(Box::new(t.clone())))
			}
			_ => Ok(ReportOutcome::NotReportable(Box::new(t.clone()))),
		}
	}
	async fn add_statement(
		&self,
		_id: &str,
		text: &str,
		_reference: Option<&str>,
	) -> Result<(), AggregatorError> {
		self.statements.lock().unwrap().push(text.into());
		Ok(())
	}
	async fn quotes(&self) -> Result<Vec<merchant_agent_core::aggregator::Quote>, AggregatorError> {
		if *self.quotes_down.lock().unwrap() {
			return Err(AggregatorError::Timeout);
		}
		Ok(self.quotes.lock().unwrap().clone())
	}
	async fn resume_offers(&self, _book: &str) -> Result<usize, AggregatorError> {
		*self.offers_paused.lock().unwrap() = false;
		Ok(0)
	}
	async fn pause_offers(&self) -> Result<String, AggregatorError> {
		*self.offers_paused.lock().unwrap() = true;
		Ok("[]".into())
	}
	async fn ping(&self) -> Result<(), AggregatorError> {
		if *self.down.lock().unwrap() {
			return Err(AggregatorError::Timeout);
		}
		Ok(())
	}
	async fn heartbeat(&self) -> Result<(), AggregatorError> {
		if *self.down.lock().unwrap() {
			return Err(AggregatorError::Timeout);
		}
		self.heartbeats.fetch_add(1, Ordering::SeqCst);
		Ok(())
	}
}

#[derive(Clone)]
/// The lock check's outcome, and the protocol fee the lock holds (0.5% of
/// the fixture's 100 USDC by default), and whether the RPCs are down.
pub struct FakeLocks(
	pub Arc<Mutex<LockOutcome>>,
	pub Arc<Mutex<String>>,
	pub Arc<Mutex<bool>>,
);

impl Default for FakeLocks {
	fn default() -> Self {
		Self(
			Arc::new(Mutex::new(LockOutcome::Verified)),
			Arc::new(Mutex::new("500000".into())),
			Arc::new(Mutex::new(false)),
		)
	}
}

impl Locks for FakeLocks {
	async fn check(&self, _trade: &Trade) -> Result<LockCheck, String> {
		Ok(LockCheck {
			outcome: self.0.lock().unwrap().clone(),
			protocol_fee: Some(self.1.lock().unwrap().clone()),
		})
	}
	async fn probe(&self) -> Result<(), String> {
		if *self.2.lock().unwrap() {
			return Err("every RPC is down".into());
		}
		Ok(())
	}
}

/// A bank whose status queries can be made to fail.
#[derive(Clone, Default)]
pub struct FlakyBank {
	pub inner: SimulatedRail,
	pub status_down: Arc<Mutex<bool>>,
	/// Make the next send hang forever after paying (a runtime dropped mid-call).
	pub hang_after_paying: Arc<Mutex<bool>>,
}

impl PayoutRail for FlakyBank {
	fn idempotent_send(&self) -> bool {
		true
	}
	async fn verify_account(
		&self,
		bank_code: &str,
		account: &merchant_agent_core::pii::AccountNumber,
	) -> Result<merchant_agent_core::rail::AccountLookup, RailError> {
		self.inner.verify_account(bank_code, account).await
	}
	async fn balance(&self) -> Result<Option<merchant_agent_core::money::Minor>, RailError> {
		self.inner.balance().await
	}
	async fn send(&self, req: PayoutRequest<'_>) -> Result<SendOutcome, RailError> {
		let out = self.inner.send(req).await;
		if std::mem::take(&mut *self.hang_after_paying.lock().unwrap()) {
			std::future::pending::<()>().await;
		}
		out
	}
	async fn status(&self, key: &str) -> Result<PayoutStatus, RailError> {
		if *self.status_down.lock().unwrap() {
			return Err(RailError("status is down".into()));
		}
		self.inner.status(key).await
	}
}

pub fn base_trade() -> Trade {
	let mut t: Value = serde_json::from_str::<Value>(API).unwrap()["ownFiatTrade"].clone();
	let m = t.as_object_mut().unwrap();
	m.remove("payer_name");
	for k in [
		"payment_reference",
		"proof_kind",
		"proof",
		"proof_payment_id",
		"release_tx_hash",
		"settled_at",
	] {
		m.insert(k.into(), Value::Null);
	}
	m.insert(
		"deadline_at".into(),
		json!((start_time() + chrono::Duration::minutes(15)).to_rfc3339()),
	);
	serde_json::from_value(t).unwrap()
}

pub fn checks() -> PayeeChecks {
	let v: Value = serde_json::from_str(PAYEE).unwrap();
	let key =
		PayeePrivateKey::from_hex(v["sealedRecipient"]["privateKey"].as_str().unwrap()).unwrap();
	PayeeChecks {
		key,
		pepper: Secret::new("test-pepper".into()),
	}
}

/// Everything outside the agent. Survives the agent's crashes.
#[derive(Clone)]
pub struct World {
	pub platform: FakePlatform,
	pub locks: FakeLocks,
	pub bank: FlakyBank,
	pub clock: ManualClock,
}

impl World {
	pub fn new() -> Self {
		let platform = FakePlatform::default();
		platform
			.trades
			.lock()
			.unwrap()
			.insert(TRADE_ID.into(), base_trade());
		let bank = FlakyBank::default();
		// The sealed vector's payee, as the simulated bank knows them.
		bank.inner.add_account("058", ACCOUNT, "TEST PAYEE");
		Self {
			platform,
			locks: FakeLocks::default(),
			bank,
			clock: ManualClock(Arc::new(Mutex::new(start_time()))),
		}
	}

	pub fn cfg() -> EngineConfig {
		EngineConfig {
			solver_id: SOLVER.into(),
			start_margin: Duration::from_secs(180),
			report_margin: Duration::from_secs(120),
			lease_ttl: Duration::from_secs(30),
			status_errors_before_unknown: 3,
			max_steps: 16,
			name_match: merchant_agent_core::config::NameMatch::TokenSet,
			require_verified_name_match: true,
			min_float: merchant_agent_core::money::Minor(0),
			limits: None,
			two_person: merchant_agent_core::config::TwoPerson {
				enabled: false,
				approve_over: None,
			},
			reference_rates: Default::default(),
			chain_usdc_decimals: [("eip155:84532".to_string(), 6u8)].into_iter().collect(),
		}
	}

	/// Start an agent on `dir`, as a fresh process would (a new lease holder).
	pub fn agent(&self, dir: &Path) -> Result<Agent, EngineError> {
		let holder = format!("test-{}", NEXT_HOLDER.fetch_add(1, Ordering::SeqCst));
		let store = Store::open(dir, &holder)?;
		self.agent_on(store)
	}

	/// An agent with its own engine settings.
	pub fn agent_with(&self, dir: &Path, cfg: EngineConfig) -> Result<Agent, EngineError> {
		let holder = format!("test-{}", NEXT_HOLDER.fetch_add(1, Ordering::SeqCst));
		Engine::start(
			Arc::new(Store::open(dir, &holder)?),
			self.platform.clone(),
			self.locks.clone(),
			checks(),
			self.bank.clone(),
			self.clock.clone(),
			cfg,
		)
	}

	pub fn agent_on(&self, store: Store) -> Result<Agent, EngineError> {
		Engine::start(
			Arc::new(store),
			self.platform.clone(),
			self.locks.clone(),
			checks(),
			self.bank.clone(),
			self.clock.clone(),
			Self::cfg(),
		)
	}

	pub fn payments(&self) -> usize {
		self.bank.inner.payments().len()
	}

	/// A crashed agent's lease runs out before a new one can act.
	pub fn wait_out_the_lease(&self) {
		self.clock.advance(Duration::from_secs(31));
	}
}

pub static NEXT_HOLDER: AtomicUsize = AtomicUsize::new(0);

pub type Agent = Engine<FakePlatform, FakeLocks, PayeeChecks, FlakyBank, ManualClock>;

pub fn state(agent: &Agent) -> AgentState {
	agent.store().get(TRADE_ID).unwrap().unwrap().state
}

pub async fn tick_until(agent: &Agent, done: impl Fn(&AgentState) -> bool) -> AgentState {
	for _ in 0..20 {
		agent.tick().await.unwrap();
		let s = state(agent);
		if done(&s) {
			return s;
		}
	}
	panic!("never finished; stuck at {:?}", state(agent));
}

pub fn reported(s: &AgentState) -> bool {
	*s == AgentState::Reported {
		bank_confirmed: true,
	}
}
