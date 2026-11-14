//! The engine: the only place the agent does I/O for a trade.
//!
//! The main loop ([`Engine::tick`]):
//!
//! 1. **Poll:** the operator's trades awaiting payment.
//! 2. **Reconcile:** record new trades as `Detected`, and feed every known
//!    trade's platform state to the machine.
//! 3. **Advance:** for each trade, ask [`machine::step`] what single piece
//!    of I/O is next, do it, apply the result as an event, and write the
//!    new state durably **before** anything else. One transition, one
//!    write.
//!
//! Alerts and dispute evidence go out only after the write that caused
//! them. A crash between the two loses at most a notice, never money.
//!
//! The engine talks to the world through traits ([`Platform`], [`Locks`],
//! [`Checks`], [`PayoutRail`], [`Clock`]), so every path, crashes
//! included, can be tested without a network. [`CrashHook`] lets the crash
//! tests stop the engine dead at any step boundary.

use std::collections::{BTreeMap, HashMap};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use chrono::{DateTime, Utc};

use crate::aggregator::types::{Direction, Trade, TradeState};
use crate::aggregator::{AggregatorError, Client, Quote, ReportOutcome};
use crate::chain::{expected_lock, ChainError, ChainReader, LockVerdict};
use crate::config;
use crate::machine::{
	self, AgentState, Ctx, Effect, Event, HoldReason, Io, LockOutcome, PayoutStatus, PersonAction,
	ReportResult, SendOutcome,
};
use crate::money::Minor;
use crate::payee::{self, payee_tag, OpenedPayee, PayeePrivateKey};
use crate::policy::limits;
use crate::rail::{names, AccountLookup, PayoutRail, PayoutRequest};
use crate::secrets::Secret;
use crate::store::{
	reserved_unsent, ApprovalStep, Intent, NewTrade, Patch, Store, StoreError, TradeRecord,
};

// ── the world, as traits ─────────────────────────────────────────────────

pub trait Platform: Send + Sync {
	fn awaiting_fiat(
		&self,
	) -> impl std::future::Future<Output = Result<Vec<Trade>, AggregatorError>> + Send;
	fn trade(
		&self,
		id: &str,
	) -> impl std::future::Future<Output = Result<Trade, AggregatorError>> + Send;
	fn report(
		&self,
		id: &str,
		reference: &str,
	) -> impl std::future::Future<Output = Result<ReportOutcome, AggregatorError>> + Send;
	fn add_statement(
		&self,
		id: &str,
		text: &str,
		reference: Option<&str>,
	) -> impl std::future::Future<Output = Result<(), AggregatorError>> + Send;
	/// The operator's quote book, for the agent's own quote history.
	fn quotes(
		&self,
	) -> impl std::future::Future<Output = Result<Vec<Quote>, AggregatorError>> + Send;
	/// Withdraw every offer so no new trades arrive. Returns the withdrawn
	/// book (as JSON quote inputs) so a resume can republish it.
	fn pause_offers(
		&self,
	) -> impl std::future::Future<Output = Result<String, AggregatorError>> + Send;
	/// Republish a book withdrawn by a pause, dropping offers that have
	/// expired since. Returns how many were republished.
	fn resume_offers(
		&self,
		book: &str,
	) -> impl std::future::Future<Output = Result<usize, AggregatorError>> + Send;
	/// Is RecoilPay reachable at all? For `/readyz`.
	fn ping(&self) -> impl std::future::Future<Output = Result<(), AggregatorError>> + Send;
	/// Tell RecoilPay this agent is alive (for its dead-man's switch).
	fn heartbeat(&self) -> impl std::future::Future<Output = Result<(), AggregatorError>> + Send;
}

impl Platform for Client {
	async fn awaiting_fiat(&self) -> Result<Vec<Trade>, AggregatorError> {
		self.list_own_trades(TradeState::AwaitingFiat).await
	}
	async fn trade(&self, id: &str) -> Result<Trade, AggregatorError> {
		self.get_trade(id).await
	}
	async fn report(&self, id: &str, reference: &str) -> Result<ReportOutcome, AggregatorError> {
		self.report_payment(id, reference).await
	}
	async fn add_statement(
		&self,
		id: &str,
		text: &str,
		reference: Option<&str>,
	) -> Result<(), AggregatorError> {
		Client::add_statement(self, id, text, reference).await
	}
	async fn quotes(&self) -> Result<Vec<Quote>, AggregatorError> {
		self.get_quotes().await
	}
	async fn pause_offers(&self) -> Result<String, AggregatorError> {
		let book: Vec<crate::aggregator::QuoteInput> =
			self.get_quotes().await?.iter().map(Into::into).collect();
		let saved =
			serde_json::to_string(&book).map_err(|e| AggregatorError::Decode(e.to_string()))?;
		self.put_quotes(&[]).await?;
		Ok(saved)
	}
	async fn ping(&self) -> Result<(), AggregatorError> {
		self.clock_skew().await.map(|_| ())
	}
	async fn heartbeat(&self) -> Result<(), AggregatorError> {
		Client::heartbeat(self).await
	}
	async fn resume_offers(&self, book: &str) -> Result<usize, AggregatorError> {
		let saved: Vec<crate::aggregator::QuoteInput> =
			serde_json::from_str(book).map_err(|e| AggregatorError::Decode(e.to_string()))?;
		// An expired offer would make RecoilPay refuse the whole book.
		let cutoff = Utc::now().timestamp() + 60;
		let live: Vec<_> = saved.into_iter().filter(|q| q.expiry > cutoff).collect();
		if live.is_empty() {
			return Ok(0);
		}
		Ok(self.put_quotes(&live).await?.len())
	}
}

/// What the escrow check found, and the lock's real protocol fee.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LockCheck {
	pub outcome: LockOutcome,
	pub protocol_fee: Option<String>,
}

pub trait Locks: Send + Sync {
	/// `Err` means the chain could not be read this time; try again.
	fn check(
		&self,
		trade: &Trade,
	) -> impl std::future::Future<Output = Result<LockCheck, String>> + Send;
	/// Can every chain be read right now? For `/readyz`.
	fn probe(&self) -> impl std::future::Future<Output = Result<(), String>> + Send;
}

/// The real lock check: one [`ChainReader`] per pinned chain.
pub struct ChainLocks {
	pub chains: HashMap<String, (config::Chain, ChainReader)>,
	pub wallet: String,
}

impl Locks for ChainLocks {
	async fn probe(&self) -> Result<(), String> {
		for (chain, reader) in self.chains.values() {
			reader
				.ping()
				.await
				.map_err(|e| format!("{}: {e}", chain.name))?;
		}
		Ok(())
	}

	async fn check(&self, trade: &Trade) -> Result<LockCheck, String> {
		let problem = |r| {
			Ok(LockCheck {
				outcome: LockOutcome::Problem(r),
				protocol_fee: None,
			})
		};
		let Some((chain, reader)) = self.chains.get(&trade.chain) else {
			return problem(HoldReason::LockMismatch);
		};
		let Ok(expected) = expected_lock(trade, chain, &self.wallet) else {
			return problem(HoldReason::LockMismatch);
		};
		match reader.verify(&expected).await {
			// The escrow lets anyone refund a lock to its funder once it is
			// REFUND_DELAY old, without the attestor. A lock that close to
			// that point can't be relied on to pay the merchant back.
			Ok(LockVerdict::Verified { locked_at, .. })
				if lock_too_old(locked_at, Utc::now().timestamp()) =>
			{
				problem(HoldReason::LockClosed)
			}
			Ok(LockVerdict::Verified { protocol_fee, .. }) => Ok(LockCheck {
				outcome: LockOutcome::Verified,
				protocol_fee: Some(protocol_fee.to_string()),
			}),
			Ok(LockVerdict::Missing | LockVerdict::NotYetAtDepth) => Ok(LockCheck {
				outcome: LockOutcome::Wait,
				protocol_fee: None,
			}),
			Ok(LockVerdict::Closed) => problem(HoldReason::LockClosed),
			Ok(LockVerdict::Mismatch(_)) => problem(HoldReason::LockMismatch),
			// RPCs telling different stories: hold, loudly, never guess.
			Err(ChainError::Disagree(_)) => problem(HoldReason::LockMismatch),
			Err(e) => Err(e.to_string()),
		}
	}
}

/// `AttestedEscrow.REFUND_DELAY`: after this long, anyone can refund a lock
/// to its funder without the attestor.
pub const ESCROW_REFUND_DELAY_SECS: i64 = 7 * 24 * 60 * 60;
/// Don't start paying against a lock with less than this left before it
/// becomes refundable: the merchant still needs the attestor to release it.
pub const LOCK_REFUND_SAFETY_SECS: i64 = 24 * 60 * 60;

/// Whether a lock made at `locked_at` (Unix seconds, from the chain) is too
/// close to, or past, the point where its funder can take it back.
pub fn lock_too_old(locked_at: u64, now: i64) -> bool {
	let Ok(locked_at) = i64::try_from(locked_at) else {
		return true;
	};
	now >= locked_at
		.saturating_add(ESCROW_REFUND_DELAY_SECS)
		.saturating_sub(LOCK_REFUND_SAFETY_SECS)
}

/// What the checks found: enough to reserve the payout. No personal data.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Checked {
	pub payee_tag: String,
	pub account_masked: String,
}

pub trait Checks: Send + Sync {
	/// Everything that must hold before a payout is reserved.
	fn check(
		&self,
		trade: &Trade,
		record: &TradeRecord,
	) -> impl std::future::Future<Output = Result<Checked, HoldReason>> + Send;

	/// Re-open the bank details right before sending, from the trade as it
	/// is now, against the commitment stored when the trade was first seen.
	fn open(&self, trade: &Trade, record: &TradeRecord) -> Result<OpenedPayee, HoldReason>;
}

/// The checks M5 makes: the trade still says what it said when first seen,
/// and its bank details open and match the trade's commitment
/// (invariant 4). M7 adds the amount, limits, name and screening.
pub struct PayeeChecks {
	pub key: PayeePrivateKey,
	pub pepper: Secret,
}

/// The trade's money fields haven't moved since it was first seen.
pub fn unchanged(trade: &Trade, record: &TradeRecord) -> bool {
	trade.id == record.trade_id
		&& trade.chain == record.chain
		&& trade.quote_id == record.quote_id
		&& trade.asset_minor_units == record.asset_minor
		&& trade.fiat_minor_units == record.fiat_minor
		&& trade.fee_minor_units == record.fee_minor
		&& trade.rate.as_str() == record.rate
		&& trade
			.user_address
			.eq_ignore_ascii_case(&record.user_address)
		&& trade
			.payee_commitment
			.as_deref()
			.is_some_and(|c| c.eq_ignore_ascii_case(&record.payee_commitment))
}

impl Checks for PayeeChecks {
	async fn check(&self, trade: &Trade, record: &TradeRecord) -> Result<Checked, HoldReason> {
		let opened = self.open(trade, record)?;
		Ok(Checked {
			payee_tag: payee_tag(&self.pepper, &opened.identity),
			account_masked: opened.identity.account.masked(),
		})
	}

	fn open(&self, trade: &Trade, record: &TradeRecord) -> Result<OpenedPayee, HoldReason> {
		if !unchanged(trade, record) {
			return Err(HoldReason::TradeChanged);
		}
		let envelope = trade
			.payee_sealed
			.as_ref()
			.ok_or(HoldReason::PayeeUnreadable)?;
		let opened = payee::open(envelope, &self.key, &record.payee_commitment)
			.map_err(|_| HoldReason::PayeeUnreadable)?;
		// The sealed rail is part of the payee tag that keys the per-payee
		// limit and the deny-list, but the payout ignores it. It must be the
		// trade's rail, or one account could appear as many payees.
		if !opened.identity.rail.eq_ignore_ascii_case(&trade.rail) {
			return Err(HoldReason::PayeeUnreadable);
		}
		Ok(opened)
	}
}

pub trait Clock: Send + Sync {
	fn now(&self) -> DateTime<Utc>;
}

pub struct SystemClock;

impl Clock for SystemClock {
	fn now(&self) -> DateTime<Utc> {
		Utc::now()
	}
}

// ── crash points (tests) ─────────────────────────────────────────────────

/// A step boundary where the crash tests can stop the engine dead.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CrashPoint {
	/// Right after a state was written.
	AfterWrite(&'static str),
	/// The bank has answered the send; nothing about it is written yet.
	AfterBankSend,
	/// RecoilPay has answered the report; nothing about it is written yet.
	AfterReport,
}

/// Return true to crash at this point. Never set outside tests.
pub type CrashHook = Arc<dyn Fn(&CrashPoint) -> bool + Send + Sync>;

// ── the engine ───────────────────────────────────────────────────────────

#[derive(Debug, Clone)]
pub struct EngineConfig {
	pub solver_id: String,
	/// Don't start a payout with less than this left (invariant 6).
	pub start_margin: Duration,
	/// Report a still-pending payout with this much left (design §9.4).
	pub report_margin: Duration,
	pub lease_ttl: Duration,
	/// Consecutive failed status queries before the bank is "unknown" and
	/// the trade goes to a person (invariant 2).
	pub status_errors_before_unknown: u32,
	/// Most transitions one trade may make in one tick.
	pub max_steps: u32,
	/// How the customer's name must match the bank's (invariant 4).
	pub name_match: config::NameMatch,
	/// Never pay below this balance in the payout account (invariant 5).
	pub min_float: Minor,
	/// Local limits (invariant 5). `None` only in tests of other things.
	pub limits: Option<config::Limits>,
	/// Approvals over a threshold need a second person.
	pub two_person: config::TwoPerson,
	/// The merchant's reference rate per rail, recorded with each new quote.
	pub reference_rates: BTreeMap<String, String>,
	/// The decimals of the pinned USDC on each configured chain (CAIP-2).
	/// A quote on any other chain, or claiming other decimals, is never
	/// recorded: its terms could not have come from this merchant.
	pub chain_usdc_decimals: BTreeMap<String, u8>,
}

impl EngineConfig {
	pub fn new(solver_id: &str, policy: &config::Policy) -> Self {
		Self {
			solver_id: solver_id.to_string(),
			start_margin: Duration::from_secs(policy.start_margin_secs),
			report_margin: Duration::from_secs(policy.report_margin_secs),
			lease_ttl: Duration::from_secs(30),
			status_errors_before_unknown: 10,
			max_steps: 16,
			name_match: policy.name_match,
			min_float: Minor::default(),
			limits: None,
			two_person: policy.two_person.clone(),
			reference_rates: BTreeMap::new(),
			chain_usdc_decimals: BTreeMap::new(),
		}
	}

	/// Everything from the config.
	pub fn from_config(config: &config::Config) -> Self {
		let mut cfg = Self::new(&config.operator.solver_id, &config.policy);
		cfg.min_float = config.limits.min_float;
		cfg.limits = Some(config.limits.clone());
		cfg.reference_rates = config
			.pricing
			.reference_rates
			.iter()
			.map(|(rail, rate)| (rail.clone(), rate.as_str().to_string()))
			.collect();
		cfg.chain_usdc_decimals = config
			.chains
			.iter()
			.map(|c| (c.caip2.clone(), c.usdc_decimals))
			.collect();
		cfg
	}

	pub fn with_min_float(mut self, min_float: Minor) -> Self {
		self.min_float = min_float;
		self
	}
}

#[derive(Debug, thiserror::Error)]
pub enum EngineError {
	#[error(transparent)]
	Store(#[from] StoreError),
	#[error("stopped by a crash hook")]
	Crashed,
	#[error("{0}")]
	Refused(String),
}

/// An alert the engine raised. Delivered by the notifier (M8).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Alert {
	pub trade_id: String,
	pub state: AgentState,
	pub urgent: bool,
	pub account_masked: Option<String>,
	pub amount: Minor,
	/// The bank's or the agent's own words about what happened. No
	/// personal data.
	pub detail: Option<String>,
}

/// How often the agent tells RecoilPay it is alive.
pub const HEARTBEAT_EVERY_SECS: i64 = 30;

/// Each condition `/readyz` checks.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct Readiness {
	/// This agent holds the single-instance lease.
	pub lease: bool,
	/// The database takes writes.
	pub database: bool,
	/// RecoilPay answers.
	pub aggregator: bool,
	/// Every chain's RPC answers.
	pub rpc: bool,
	/// Payouts are not paused (no kill switch).
	pub running: bool,
}

impl Readiness {
	pub fn ready(&self) -> bool {
		self.lease && self.database && self.aggregator && self.rpc && self.running
	}
}

/// What a person's decision came to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Decision {
	Applied(AgentState),
	/// The two-person rule: recorded, waiting for a different person.
	NeedsSecondApprover,
}

/// The bank details of a manual task, revealed to a person on purpose.
/// Personal data: not printable, not kept.
pub struct Revealed {
	pub trade_id: String,
	pub amount: Minor,
	pub opened: OpenedPayee,
}

pub struct Engine<Pl, Lo, Ch, Ra, Cl> {
	store: Arc<Store>,
	platform: Pl,
	locks: Lo,
	checks: Ch,
	rail: Ra,
	clock: Cl,
	cfg: EngineConfig,
	crash: Option<CrashHook>,
	status_errors: Mutex<HashMap<String, u32>>,
	alerts: Mutex<Vec<Alert>>,
	/// Whether this tick's snapshot of the quote book succeeded. Checks
	/// that price a trade wait for a fresh one.
	quotes_fresh: AtomicBool,
	/// When the last tick finished (Unix seconds; 0 before the first).
	last_tick: std::sync::atomic::AtomicI64,
	metrics: crate::metrics::Metrics,
	last_heartbeat: std::sync::atomic::AtomicI64,
	heartbeat_unsupported: std::sync::atomic::AtomicBool,
	readiness: Mutex<Option<(std::time::Instant, Readiness)>>,
	/// Quote ids already refused (and warned about) this run.
	refused_quotes: Mutex<std::collections::HashSet<String>>,
}

impl<Pl: Platform, Lo: Locks, Ch: Checks, Ra: PayoutRail, Cl: Clock> Engine<Pl, Lo, Ch, Ra, Cl> {
	/// Take the lease and build the engine. Fails if another agent holds it.
	#[allow(clippy::too_many_arguments)]
	pub fn start(
		store: Arc<Store>,
		platform: Pl,
		locks: Lo,
		checks: Ch,
		rail: Ra,
		clock: Cl,
		cfg: EngineConfig,
	) -> Result<Self, EngineError> {
		store.acquire_lease(cfg.lease_ttl, clock.now())?;
		Ok(Self {
			store,
			platform,
			locks,
			checks,
			rail,
			clock,
			cfg,
			crash: None,
			status_errors: Mutex::new(HashMap::new()),
			alerts: Mutex::new(Vec::new()),
			quotes_fresh: AtomicBool::new(false),
			last_tick: std::sync::atomic::AtomicI64::new(0),
			metrics: crate::metrics::Metrics::new(),
			last_heartbeat: std::sync::atomic::AtomicI64::new(0),
			heartbeat_unsupported: std::sync::atomic::AtomicBool::new(false),
			readiness: Mutex::new(None),
			refused_quotes: Mutex::new(std::collections::HashSet::new()),
		})
	}

	pub fn with_crash_hook(mut self, hook: CrashHook) -> Self {
		self.crash = Some(hook);
		self
	}

	pub fn store(&self) -> &Arc<Store> {
		&self.store
	}

	pub fn alerts(&self) -> Vec<Alert> {
		self.alerts
			.lock()
			.unwrap_or_else(|e| e.into_inner())
			.clone()
	}

	fn crash_at(&self, point: CrashPoint) -> Result<(), EngineError> {
		match &self.crash {
			Some(hook) if hook(&point) => Err(EngineError::Crashed),
			_ => Ok(()),
		}
	}

	fn ctx(&self) -> Ctx {
		Ctx {
			rail_idempotent: self.rail.idempotent_send(),
		}
	}

	/// Why a quote from the aggregator's book can't be one this merchant
	/// published, or `None` if it can. The quote book drives pricing, so its
	/// terms must match what the agent pins locally: this operator's
	/// cash-out, USDC on a configured chain with that chain's decimals, and
	/// (when the merchant set any reference rates) a rail with a reference,
	/// so the rate band always applies.
	fn quote_refusal(&self, q: &Quote) -> Option<&'static str> {
		if !crate::aggregator::valid_id(&q.id) {
			return Some("the quote id is not a plain identifier");
		}
		if q.solver_id != self.cfg.solver_id || q.direction != Direction::Offramp {
			return Some("not this operator's cash-out quote");
		}
		if q.asset != "USDC" {
			return Some("not a USDC quote");
		}
		match self.cfg.chain_usdc_decimals.get(&q.chain) {
			None => return Some("not on a chain in your config"),
			Some(d) if *d != q.asset_decimals => {
				return Some("its asset decimals differ from the pinned USDC's")
			}
			Some(_) => {}
		}
		if !self.cfg.reference_rates.is_empty() && !self.cfg.reference_rates.contains_key(&q.rail) {
			return Some("no reference rate is set for its rail, so its rate can't be checked");
		}
		None
	}

	/// One pass of the main loop. Platform and chain hiccups are retried on
	/// the next tick; only storage problems (or a lost lease) are errors.
	pub async fn tick(&self) -> Result<(), EngineError> {
		let now = self.clock.now();
		self.store.renew_lease(self.cfg.lease_ttl, now)?;

		// The quote history first, so a trade on a quote published since the
		// last tick can be priced (design §4.4).
		let fresh = match self.platform.quotes().await {
			Ok(quotes) => {
				let quotes: Vec<Quote> = quotes
					.into_iter()
					.filter(|q| match self.quote_refusal(q) {
						None => true,
						Some(why) => {
							if self.refused_quotes.lock().unwrap().insert(q.id.clone()) {
								tracing::warn!(quote_id = %q.id, reason = why, "not recording a quote that could not be this merchant's; trades on it will be held as an unknown offer");
							}
							false
						}
					})
					.collect();
				let rates = &self.cfg.reference_rates;
				let added =
					self.store
						.record_quotes(&quotes, |q| rates.get(&q.rail).cloned(), now)?;
				if added > 0 {
					tracing::info!(added, "recorded new quotes");
				}
				true
			}
			Err(e) => {
				tracing::warn!(error = %e, "could not read the quote book; pricing waits for the next tick");
				false
			}
		};
		self.quotes_fresh.store(fresh, Ordering::SeqCst);
		self.last_tick.store(now.timestamp(), Ordering::SeqCst);
		self.metrics.tick(now.timestamp());
		// The heartbeat, for RecoilPay's dead-man's switch (P2).
		if now.timestamp() - self.last_heartbeat.load(Ordering::SeqCst) >= HEARTBEAT_EVERY_SECS {
			let ok = match self.platform.heartbeat().await {
				Ok(()) => true,
				// A RecoilPay without the heartbeat route (older than the
				// agent): say so once, then quietly keep trying.
				Err(AggregatorError::NotFound(_)) => {
					if !self.heartbeat_unsupported.swap(true, Ordering::SeqCst) {
						tracing::warn!("RecoilPay has no heartbeat route (an older version?); the agent keeps sending them");
					}
					false
				}
				Err(e) => {
					tracing::warn!(error = %e, "the heartbeat to RecoilPay failed");
					false
				}
			};
			self.metrics.heartbeat(ok);
			if ok {
				self.last_heartbeat.store(now.timestamp(), Ordering::SeqCst);
			}
		}

		let mut seen: HashMap<String, Trade> = HashMap::new();
		match self.platform.awaiting_fiat().await {
			Ok(trades) => {
				for t in trades {
					if !crate::aggregator::valid_id(&t.id) {
						tracing::warn!("ignoring a trade whose id is not a plain identifier");
						continue;
					}
					if t.solver_id != self.cfg.solver_id || t.direction != Direction::Offramp {
						tracing::warn!(trade_id = %t.id, "ignoring a trade that is not this operator's cash-out");
						continue;
					}
					let Some(commitment) = t.payee_commitment.clone() else {
						tracing::warn!(trade_id = %t.id, "ignoring a cash-out with no payee commitment");
						continue;
					};
					let Some(deadline) = t.deadline_at else {
						tracing::warn!(trade_id = %t.id, "ignoring a trade awaiting payment with no deadline");
						continue;
					};
					let new = NewTrade {
						trade_id: t.id.clone(),
						chain: t.chain.clone(),
						quote_id: t.quote_id.clone(),
						asset_minor: t.asset_minor_units,
						fiat_minor: t.fiat_minor_units,
						fee_minor: t.fee_minor_units,
						rate: t.rate.as_str().to_string(),
						user_address: t.user_address.clone(),
						payee_commitment: commitment,
						deadline_at: deadline,
						platform_state: t.state,
					};
					if self.store.insert_detected(&new, now)? {
						tracing::info!(trade_id = %t.id, "new cash-out detected");
						self.crash_at(CrashPoint::AfterWrite("detected"))?;
					}
					seen.insert(t.id.clone(), t);
				}
			}
			Err(e) => tracing::warn!(error = %e, "could not list trades; trying again next tick"),
		}

		for record in self.store.active()? {
			let trade = match seen.remove(&record.trade_id) {
				Some(t) => t,
				None => match self.platform.trade(&record.trade_id).await {
					Ok(t) => t,
					Err(e) => {
						tracing::warn!(trade_id = %record.trade_id, error = %e, "could not read the trade; next tick");
						continue;
					}
				},
			};
			self.advance(record, &trade).await?;
		}
		Ok(())
	}

	/// Move one trade as far as it can go now.
	async fn advance(&self, mut rec: TradeRecord, trade: &Trade) -> Result<(), EngineError> {
		let ours =
			trade.payment_reference.is_some() && trade.payment_reference == rec.bank_reference;
		let patch = Patch {
			platform_state: Some(trade.state),
			deadline_at: trade
				.deadline_at
				.filter(|_| trade.state == TradeState::AwaitingFiat),
			..Default::default()
		};
		rec = self
			.apply(
				rec,
				&Event::Platform {
					state: trade.state,
					reported_by_us: ours,
				},
				patch,
			)
			.await?;

		for _ in 0..self.cfg.max_steps {
			let before = (rec.state.clone(), rec.version);
			rec = self.one_step(rec, trade).await?;
			if (rec.state.clone(), rec.version) == before {
				break;
			}
		}
		Ok(())
	}

	async fn one_step(&self, rec: TradeRecord, trade: &Trade) -> Result<TradeRecord, EngineError> {
		let now = self.clock.now();

		// Invariant 6: don't start a payout that could land after the deadline.
		let pre_send = matches!(
			rec.state,
			AgentState::Detected
				| AgentState::LockVerified
				| AgentState::ChecksPassed
				| AgentState::PayoutIntentRecorded
				| AgentState::Held(_)
		);
		if pre_send {
			let left = rec.deadline_at - now;
			let margin = chrono::Duration::from_std(self.cfg.start_margin).unwrap_or_default();
			if left <= chrono::Duration::zero() {
				return self
					.apply(rec, &Event::DeadlinePassed, Patch::default())
					.await;
			}
			if left < margin && !matches!(rec.state, AgentState::Held(_)) {
				return self
					.apply(rec, &Event::DeadlineTooClose, Patch::default())
					.await;
			}
		}

		match machine::step(&rec.state) {
			Io::CheckLock => match self.locks.check(trade).await {
				Ok(lc) => {
					let patch = Patch {
						lock_protocol_fee: lc.protocol_fee.clone(),
						..Default::default()
					};
					self.apply(rec, &Event::Lock(lc.outcome), patch).await
				}
				Err(e) => {
					tracing::warn!(trade_id = %rec.trade_id, error = %e, "could not read the lock; next tick");
					Ok(rec)
				}
			},

			Io::RunChecks => {
				if !self.quotes_fresh.load(Ordering::SeqCst) {
					return Ok(rec);
				}
				// Invariant 4: the account must belong to the person named.
				let opened = match self.checks.open(trade, &rec) {
					Ok(o) => o,
					Err(r) => {
						return self
							.apply(rec, &Event::Checks(Err(r)), Patch::default())
							.await
					}
				};
				// A trade held from here on still shows which account it is
				// (masked), so a person can act on it.
				let masked = Patch {
					account_masked: Some(opened.identity.account.masked()),
					..Default::default()
				};
				let lookup = self
					.rail
					.verify_account(&opened.identity.bank_code, &opened.identity.account)
					.await;
				let name_ok = match lookup {
					Ok(AccountLookup::Name(bank_name)) => {
						rec.overrides.contains(&HoldReason::NameMismatch)
							|| names::matches(
								self.cfg.name_match,
								opened.name.expose(),
								bank_name.expose(),
							)
					}
					Ok(AccountLookup::NotFound) => {
						drop(opened);
						return self
							.apply(
								rec,
								&Event::Checks(Err(HoldReason::AccountNotFound)),
								masked,
							)
							.await;
					}
					Ok(AccountLookup::Unsupported) => true,
					Err(e) => {
						tracing::warn!(trade_id = %rec.trade_id, error = %e, "could not look the account up; next tick");
						self.metrics.bank_error("lookup");
						return Ok(rec);
					}
				};
				drop(opened);
				if !name_ok {
					return self
						.apply(rec, &Event::Checks(Err(HoldReason::NameMismatch)), masked)
						.await;
				}
				// Read the lock again right before the payout can be reserved:
				// it may have been refunded, or grown too old to rely on, since
				// it was first verified (a trade can wait here for a while).
				match self.locks.check(trade).await {
					Ok(lc) => match lc.outcome {
						LockOutcome::Verified => {}
						LockOutcome::Problem(r) => {
							return self.apply(rec, &Event::Checks(Err(r)), masked).await;
						}
						LockOutcome::Wait => {
							return self
								.apply(rec, &Event::Checks(Err(HoldReason::LockClosed)), masked)
								.await;
						}
					},
					Err(e) => {
						tracing::warn!(trade_id = %rec.trade_id, error = %e, "could not re-read the lock before paying; next tick");
						return Ok(rec);
					}
				}
				self.run_checks(rec, trade, masked).await
			}

			Io::RecordIntent => self.record_intent(rec, now).await,

			Io::Send => {
				// The last look: re-open the details from the trade as it is now.
				let opened = match self.checks.open(trade, &rec) {
					Ok(o) => o,
					Err(r) => {
						return self
							.apply(rec, &Event::Checks(Err(r)), Patch::default())
							.await
					}
				};
				// Written before the bank is called: from here on, a restart
				// asks the bank by key and never sends blind.
				let rec = self
					.apply(rec, &Event::SendStarting, Patch::default())
					.await?;
				let key = rec.idempotency_key();
				let short: String = rec.trade_id.chars().take(11).collect();
				let narration = format!("RecoilPay cash-out {short}");
				let outcome = self
					.rail
					.send(PayoutRequest {
						idempotency_key: &key,
						trade_id: &rec.trade_id,
						amount: rec.fiat_minor,
						bank_code: &opened.identity.bank_code,
						account: &opened.identity.account,
						name: &opened.name,
						narration: &narration,
					})
					.await;
				drop(opened);
				self.crash_at(CrashPoint::AfterBankSend)?;
				let outcome = outcome.unwrap_or_else(|e| {
					tracing::warn!(trade_id = %rec.trade_id, error = %e, "the bank did not answer the send; asking by key");
					self.metrics.bank_error("send");
					SendOutcome::Unknown
				});
				self.apply(rec, &Event::Sent(outcome), Patch::default())
					.await
			}

			Io::QueryStatus => {
				let margin = chrono::Duration::from_std(self.cfg.report_margin).unwrap_or_default();
				let report_now = rec.state == AgentState::Paying
					&& rec.bank_reference.is_some()
					&& rec.deadline_at - now < margin;
				if report_now {
					return self.report(rec).await;
				}
				match self.rail.status(&rec.idempotency_key()).await {
					Ok(status) => {
						self.status_errors
							.lock()
							.unwrap_or_else(|e| e.into_inner())
							.remove(&rec.trade_id);
						self.apply(rec, &Event::Status(status), Patch::default())
							.await
					}
					Err(e) => {
						self.metrics.bank_error("status");
						let n = {
							let mut errors =
								self.status_errors.lock().unwrap_or_else(|e| e.into_inner());
							let n = errors.entry(rec.trade_id.clone()).or_default();
							*n += 1;
							*n
						};
						tracing::warn!(trade_id = %rec.trade_id, error = %e, failures = n, "could not ask the bank");
						if n >= self.cfg.status_errors_before_unknown {
							self.apply(rec, &Event::Status(PayoutStatus::Unknown), Patch::default())
								.await
						} else {
							Ok(rec)
						}
					}
				}
			}

			Io::Report => self.report(rec).await,

			Io::Watch | Io::Done => Ok(rec),
		}
	}

	async fn run_checks(
		&self,
		rec: TradeRecord,
		trade: &Trade,
		on_hold: Patch,
	) -> Result<TradeRecord, EngineError> {
		match self.checks.check(trade, &rec).await {
			Ok(c) => {
				let patch = Patch {
					payee_tag: Some(c.payee_tag),
					account_masked: Some(c.account_masked),
					..Default::default()
				};
				self.apply(rec, &Event::Checks(Ok(())), patch).await
			}
			Err(r) => self.apply(rec, &Event::Checks(Err(r)), on_hold).await,
		}
	}

	/// The write-ahead intent, with the float check (invariant 5) inside
	/// its transaction: the balance read now, less what earlier intents have
	/// reserved but not yet sent, must cover this payout and the minimum.
	async fn record_intent(
		&self,
		rec: TradeRecord,
		now: DateTime<Utc>,
	) -> Result<TradeRecord, EngineError> {
		let Some(tag) = rec.payee_tag.clone() else {
			return self
				.apply(
					rec,
					&Event::IntentRefused(HoldReason::PayeeUnreadable),
					Patch::default(),
				)
				.await;
		};
		let balance = match self.rail.balance().await {
			Ok(b) => {
				if let Some(kobo) = b {
					self.metrics.float(kobo.get());
				}
				b
			}
			Err(e) => {
				self.metrics.bank_error("balance");
				tracing::warn!(trade_id = %rec.trade_id, error = %e, "could not read the balance; next tick");
				return Ok(rec);
			}
		};
		let amount = rec.fiat_minor.get();
		let min_float = self.cfg.min_float.get();
		let float_waived = rec.overrides.contains(&HoldReason::InsufficientFloat);
		let intent = Intent {
			amount: rec.fiat_minor,
			payee_tag: tag,
			day: limits::day_of(now),
		};
		let result = self.store.record_intent(&rec, &intent, now, |tx| {
			if let (Some(balance), false) = (balance, float_waived) {
				let reserved = reserved_unsent(tx)?;
				let needed = reserved.saturating_add(amount).saturating_add(min_float);
				if balance.get() < needed {
					return Ok(Err(HoldReason::InsufficientFloat));
				}
			}
			// Invariant 5, atomically with the intent itself.
			match &self.cfg.limits {
				Some(l) => limits::check(tx, l, &rec.overrides, &intent, now),
				None => Ok(Ok(())),
			}
		})?;
		match result {
			Ok(next) => {
				tracing::info!(trade_id = %rec.trade_id, key = %next.idempotency_key(), "payout intent recorded");
				self.crash_at(CrashPoint::AfterWrite("payout_intent_recorded"))?;
				Ok(next)
			}
			Err(r) => {
				self.apply(rec, &Event::IntentRefused(r), Patch::default())
					.await
			}
		}
	}

	async fn report(&self, rec: TradeRecord) -> Result<TradeRecord, EngineError> {
		let Some(reference) = rec.bank_reference.clone() else {
			// Paid always carries the bank's reference; without one there is
			// nothing true to report.
			return self
				.apply(rec, &Event::Report(ReportResult::Refused), Patch::default())
				.await;
		};
		let outcome = self.platform.report(&rec.trade_id, &reference).await;
		self.crash_at(CrashPoint::AfterReport)?;
		let result = match outcome {
			Ok(ReportOutcome::Reported(_)) => ReportResult::Reported,
			Ok(ReportOutcome::NotReportable(t)) => ReportResult::NotReportable(t.state),
			Err(e) if e.is_transient() => ReportResult::Retry,
			Err(
				AggregatorError::Unauthorized
				| AggregatorError::Forbidden(_)
				| AggregatorError::NotFound(_)
				| AggregatorError::BadRequest(_)
				| AggregatorError::InvalidInput(_),
			) => ReportResult::Refused,
			Err(_) => ReportResult::Retry,
		};
		self.apply(rec, &Event::Report(result), Patch::default())
			.await
	}

	/// Apply one event: the machine decides, the store writes, then the
	/// effects that leave the agent (alerts, evidence) happen.
	async fn apply(
		&self,
		rec: TradeRecord,
		event: &Event,
		patch: Patch,
	) -> Result<TradeRecord, EngineError> {
		let detail = match event {
			Event::Sent(SendOutcome::Rejected { reason })
			| Event::Status(PayoutStatus::Failed { reason }) => Some(reason.clone()),
			Event::Sent(SendOutcome::Queued) => Some(
				"Waiting for a person to pay this from the bank app and enter the reference."
					.into(),
			),
			_ => None,
		};
		let t = match machine::apply(&rec.state, event, self.ctx()) {
			Ok(t) => t,
			Err(invalid) => {
				tracing::warn!(trade_id = %rec.trade_id, error = %invalid, "ignored an event that does not apply");
				return Ok(rec);
			}
		};
		let patch_changes = patch
			.platform_state
			.is_some_and(|p| p != rec.platform_state)
			|| patch.deadline_at.is_some_and(|d| d != rec.deadline_at)
			|| patch.lock_protocol_fee.is_some()
			|| patch.payee_tag.is_some()
			|| patch.account_masked.is_some();
		if t.next == rec.state && t.effects.is_empty() && !patch_changes {
			return Ok(rec);
		}

		let next = self
			.store
			.transition(&rec, &t.next, &t.effects, &patch, self.clock.now())?;
		if next.state != rec.state {
			tracing::info!(trade_id = %rec.trade_id, from = rec.state.label(), to = next.state.label(), "trade moved");
			if let AgentState::Held(r) = &next.state {
				self.metrics.hold(r.code());
			}
			if next.state == AgentState::Paid {
				if let Ok(Some(locked)) = self.store.audit_time(&next.trade_id, "lock_verified") {
					let millis = (self.clock.now() - locked).num_milliseconds().max(0) as u64;
					self.metrics.lock_to_paid(millis);
				}
			}
		}
		self.crash_at(CrashPoint::AfterWrite(next.state.to_codes().0))?;

		for effect in &t.effects {
			match effect {
				Effect::Alert { urgent } => {
					self.metrics.alerts(1);
					self.alerts
						.lock()
						.unwrap_or_else(|e| e.into_inner())
						.push(Alert {
							trade_id: next.trade_id.clone(),
							state: next.state.clone(),
							urgent: *urgent,
							account_masked: next.account_masked.clone(),
							amount: next.fiat_minor,
							detail: detail.clone(),
						})
				}
				Effect::PauseOffers => self.pause("agent", true).await?,
				Effect::AddDisputeEvidence => {
					// Statements must be printable ASCII, so NGN, not ₦.
					let naira = crate::money::naira(next.fiat_minor).replace('₦', "NGN ");
					let text = format!(
						"Paid {naira} to account {} by bank transfer.",
						next.account_masked.as_deref().unwrap_or("(unknown)")
					);
					let reference = next.bank_reference.as_deref().filter(|r| {
						r.len() <= 100
							&& r.chars()
								.all(|c| c.is_ascii_alphanumeric() || "-_./".contains(c))
					});
					if let Err(e) = self
						.platform
						.add_statement(&next.trade_id, &text, reference)
						.await
					{
						tracing::warn!(trade_id = %next.trade_id, error = %e, "could not attach the payment evidence");
					}
				}
				_ => {}
			}
		}
		Ok(next)
	}

	// ── what people (and the kill switch) do ─────────────────────────────

	/// Stop new payouts and withdraw the merchant's offers (invariant 11).
	/// In-flight payouts carry on. If the offers can't be withdrawn, the
	/// agent is still paused locally and says so.
	pub async fn pause(&self, by: &str, auto: bool) -> Result<(), EngineError> {
		let now = self.clock.now();
		let book = match self.platform.pause_offers().await {
			Ok(book) => Some(book),
			Err(e) => {
				tracing::error!(error = %e, "could not withdraw the offers; payouts are paused locally");
				None
			}
		};
		self.store
			.set_paused(true, by, auto, book.as_deref(), now)?;
		tracing::warn!(by, automatic = auto, "payouts paused");
		Ok(())
	}

	/// Apply a person's decision to a trade.
	/// Apply a person's decision to a trade, by `by`.
	///
	/// With the two-person rule on, approving a held payout (or a repay)
	/// above the threshold needs two different people: the first call
	/// records the request, a second person's call carries it out.
	pub async fn person(
		&self,
		trade_id: &str,
		action: PersonAction,
		by: &str,
	) -> Result<Decision, EngineError> {
		let rec = self.store.get(trade_id)?.ok_or(StoreError::NotFound)?;
		let before = rec.state.clone();
		let pays = matches!(action, PersonAction::Approve | PersonAction::ApproveRepay);
		let tp = &self.cfg.two_person;
		let needs_two =
			pays && tp.enabled && tp.approve_over.is_none_or(|over| rec.fiat_minor > over);
		if needs_two {
			let code = match action {
				PersonAction::Approve => "approve",
				_ => "approve_repay",
			};
			match self.store.approve(trade_id, code, by, self.clock.now())? {
				ApprovalStep::Requested => {
					tracing::info!(
						trade_id,
						by,
						"approval requested; a second person must confirm"
					);
					return Ok(Decision::NeedsSecondApprover);
				}
				ApprovalStep::AlreadyRequested => {
					return Err(EngineError::Refused(
						"you already approved this; a different person must confirm it".into(),
					));
				}
				ApprovalStep::Confirmed { first } => {
					tracing::info!(trade_id, first = %first, second = by, "approval confirmed by a second person");
				}
			}
		}
		let next = self
			.apply(
				rec,
				&Event::Person(action),
				Patch {
					actor: Some(by.to_string()),
					..Default::default()
				},
			)
			.await?;
		if next.state == before {
			return Err(EngineError::Refused(format!(
				"that does not apply to a trade that is {}",
				before.label()
			)));
		}
		tracing::info!(trade_id, by, to = next.state.label(), "a person decided");
		Ok(Decision::Applied(next.state))
	}

	/// Alerts raised since the last call, for delivery.
	pub fn take_alerts(&self) -> Vec<Alert> {
		std::mem::take(&mut *self.alerts.lock().unwrap_or_else(|e| e.into_inner()))
	}

	/// Reveal a manual task's bank details to the person paying it. Only
	/// while a task is open; every reveal is counted against the task.
	pub async fn reveal(&self, trade_id: &str, by: &str) -> Result<Revealed, EngineError> {
		let task = self.store.open_task_for(trade_id)?.ok_or_else(|| {
			EngineError::Refused("there is no open payment task for that trade".into())
		})?;
		let rec = self.store.get(trade_id)?.ok_or(StoreError::NotFound)?;
		let trade = self
			.platform
			.trade(trade_id)
			.await
			.map_err(|e| EngineError::Refused(format!("could not read the trade: {e}")))?;
		let opened = self
			.checks
			.open(&trade, &rec)
			.map_err(|r| EngineError::Refused(r.message().to_string()))?;
		self.store
			.note_reveal(&task.idempotency_key, by, self.clock.now())?;
		tracing::warn!(trade_id, by, "bank details revealed for a manual payment");
		Ok(Revealed {
			trade_id: trade_id.to_string(),
			amount: task.amount,
			opened,
		})
	}

	/// A person paid a manual task, with the bank's reference. The agent
	/// reports it on the next tick.
	pub fn manual_paid(
		&self,
		trade_id: &str,
		reference: &str,
		by: &str,
	) -> Result<(), EngineError> {
		let reference = crate::aggregator::check_reference(reference)
			.map_err(|e| EngineError::Refused(e.to_string()))?;
		let task = self.store.open_task_for(trade_id)?.ok_or_else(|| {
			EngineError::Refused("there is no open payment task for that trade".into())
		})?;
		self.store.close_manual_task(
			&task.idempotency_key,
			Some(&reference),
			by,
			self.clock.now(),
		)?;
		Ok(())
	}

	/// A person could not pay a manual task. The trade goes to a person.
	pub fn manual_failed(&self, trade_id: &str, by: &str) -> Result<(), EngineError> {
		let task = self.store.open_task_for(trade_id)?.ok_or_else(|| {
			EngineError::Refused("there is no open payment task for that trade".into())
		})?;
		self.store
			.close_manual_task(&task.idempotency_key, None, by, self.clock.now())?;
		Ok(())
	}

	/// Resume after a pause: republish the offers it withdrew (those not
	/// expired since), then let payouts start again. If the offers can't be
	/// republished, it stays paused.
	pub async fn resume(&self, by: &str) -> Result<String, EngineError> {
		let (paused, _, _, book) = self.store.control()?;
		if !paused {
			return Ok("Payouts were not paused.".into());
		}
		let republished = match book.as_deref() {
			Some(b) if b != "[]" => self.platform.resume_offers(b).await.map_err(|e| {
				EngineError::Refused(format!(
					"could not republish your offers, so payouts stay paused: {e}"
				))
			})?,
			_ => 0,
		};
		self.store
			.set_paused(false, by, false, Some("[]"), self.clock.now())?;
		tracing::warn!(by, republished, "payouts resumed");
		Ok(if republished > 0 {
			format!("Resumed. {republished} offer(s) republished.")
		} else {
			"Resumed. No saved offers were still valid: publish a new quote book so customers can find you.".into()
		})
	}

	/// Is the agent fit to pay right now? Each condition separately, for
	/// `/readyz`. Cached for a few seconds so probes can't hammer RecoilPay
	/// or the RPCs.
	pub async fn readiness(&self) -> Readiness {
		if let Some((at, r)) = self
			.readiness
			.lock()
			.unwrap_or_else(|e| e.into_inner())
			.clone()
		{
			if at.elapsed() < std::time::Duration::from_secs(10) {
				return r;
			}
		}
		let r = self.check_readiness().await;
		*self.readiness.lock().unwrap_or_else(|e| e.into_inner()) =
			Some((std::time::Instant::now(), r.clone()));
		r
	}

	/// Each readiness condition, checked now (no cache).
	pub async fn check_readiness(&self) -> Readiness {
		let now = self.clock.now();
		let lease = matches!(self.store.lease(), Ok(Some((h, _, until))) if h == self.store.holder() && until > now);
		let database = self.store.write_probe().is_ok();
		let aggregator = self.platform.ping().await.is_ok();
		let rpc = self.locks.probe().await.is_ok();
		let running = matches!(self.store.is_paused(), Ok(false));
		Readiness {
			lease,
			database,
			aggregator,
			rpc,
			running,
		}
	}

	pub fn metrics(&self) -> &crate::metrics::Metrics {
		&self.metrics
	}

	/// The Prometheus exposition text.
	pub async fn metrics_text(&self) -> String {
		let trades = self.store.trade_state_counts().unwrap_or_default();
		let paused = self.store.is_paused().unwrap_or(true);
		let ready = self.readiness().await.ready();
		self.metrics.render(&trades, paused, ready)
	}

	/// What the console shows.
	pub async fn snapshot(&self) -> Result<crate::console::Snapshot, EngineError> {
		let (paused, paused_by, paused_auto, _) = self.store.control()?;
		let today = limits::day_of(self.clock.now());
		let today_total = self
			.store
			.payouts()?
			.iter()
			.filter(|p| p.day == today && p.status != "cancelled")
			.fold(0u128, |sum, p| sum.saturating_add(p.amount.get()));
		let last = self.last_tick.load(Ordering::SeqCst);
		Ok(crate::console::Snapshot {
			paused,
			paused_by,
			paused_auto,
			trades: self.store.recent_trades(200)?,
			open_tasks: self.store.open_manual_tasks()?,
			balance: self.rail.balance().await.ok().flatten(),
			today: Minor(today_total),
			last_tick: (last > 0)
				.then(|| DateTime::from_timestamp(last, 0))
				.flatten(),
			lease_holder: self.store.lease()?.map(|(holder, _, _)| holder),
		})
	}
}

impl<Pl, Lo, Ch, Ra, Cl> crate::console::AgentOps for Engine<Pl, Lo, Ch, Ra, Cl>
where
	Pl: Platform + 'static,
	Lo: Locks + 'static,
	Ch: Checks + 'static,
	Ra: PayoutRail + 'static,
	Cl: Clock + 'static,
{
	fn readiness(&self) -> crate::notify::BoxFuture<'_, Readiness> {
		Box::pin(Engine::readiness(self))
	}
	fn metrics_text(&self) -> crate::notify::BoxFuture<'_, String> {
		Box::pin(Engine::metrics_text(self))
	}
	fn snapshot(&self) -> crate::notify::BoxFuture<'_, Result<crate::console::Snapshot, String>> {
		Box::pin(async move { Engine::snapshot(self).await.map_err(|e| e.to_string()) })
	}
	fn decide<'a>(
		&'a self,
		trade_id: &'a str,
		action: PersonAction,
		by: &'a str,
	) -> crate::notify::BoxFuture<'a, Result<Decision, String>> {
		Box::pin(async move {
			self.person(trade_id, action, by)
				.await
				.map_err(|e| e.to_string())
		})
	}
	fn reveal<'a>(
		&'a self,
		trade_id: &'a str,
		by: &'a str,
	) -> crate::notify::BoxFuture<'a, Result<Revealed, String>> {
		Box::pin(async move {
			Engine::reveal(self, trade_id, by)
				.await
				.map_err(|e| e.to_string())
		})
	}
	fn manual_paid<'a>(
		&'a self,
		trade_id: &'a str,
		reference: &'a str,
		by: &'a str,
	) -> crate::notify::BoxFuture<'a, Result<(), String>> {
		Box::pin(async move {
			Engine::manual_paid(self, trade_id, reference, by).map_err(|e| e.to_string())
		})
	}
	fn manual_failed<'a>(
		&'a self,
		trade_id: &'a str,
		by: &'a str,
	) -> crate::notify::BoxFuture<'a, Result<(), String>> {
		Box::pin(
			async move { Engine::manual_failed(self, trade_id, by).map_err(|e| e.to_string()) },
		)
	}
	fn pause<'a>(&'a self, by: &'a str) -> crate::notify::BoxFuture<'a, Result<(), String>> {
		Box::pin(async move {
			Engine::pause(self, by, false)
				.await
				.map_err(|e| e.to_string())
		})
	}
	fn resume<'a>(&'a self, by: &'a str) -> crate::notify::BoxFuture<'a, Result<String, String>> {
		Box::pin(async move { Engine::resume(self, by).await.map_err(|e| e.to_string()) })
	}
}
