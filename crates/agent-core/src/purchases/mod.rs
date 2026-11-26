//! Merchant-signed purchases (v0.3): the agent locks each of the operator's
//! purchases from its own float wallet, by a Permit2 signature for that one
//! trade. RecoilPay's fill wallet only relays it and pays gas, so RecoilPay
//! never holds the merchant's sale USDC.
//!
//! **Invariant 4 (purchases): the agent signs only a purchase it can
//! justify itself.** RecoilPay's request is a claim. The agent signs a lock
//! only when:
//!
//! - the asset is exactly what the merchant's **own first-seen on-ramp
//!   quote terms** give for the buyer's naira (`price_onramp`), and the
//!   rate is inside the reference band;
//! - it is within the purchase limits (per trade, per Lagos day) and leaves
//!   the float wallet above its floor, with Permit2 allowed to move it;
//! - the buyer's wallet is not on the deny list;
//! - the escrow, token and chain are the ones pinned in config, the nonce is
//!   the trade's own and the deadline is near.
//!
//! The lock is rebuilt from those values and signed; RecoilPay's typed data
//! must say exactly the same, field by field. A refusal is recorded with its
//! reason, audited and alerted, and nothing is signed, so nothing moves.
//!
//! **Crash safety:** a signature is stored before it is posted, and posted
//! again after a crash or a failed post. A trade is never signed twice, and
//! its nonce is its own, so a repeated post can lock nothing more.

pub mod permit;

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use alloy::primitives::{Address, U256};
use chrono::{DateTime, Utc};
use k256::ecdsa::SigningKey;

use crate::aggregator::types::{Direction, PurchaseLock};
use crate::aggregator::{AggregatorError, Client};
use crate::engine::Alert;
use crate::machine::{AgentState, HoldReason};
use crate::money::Minor;
use crate::policy::pricing::{check_rate, price_onramp, QuoteTerms};
use crate::policy::screening::DenyList;
use crate::store::{NewPurchase, PurchaseStatus, Store, StoreError};
use permit::Permit;

/// Why a purchase was not signed. Each is recorded, audited and alerted.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Refusal {
	NotAPurchase,
	WrongChain,
	WrongToken,
	WrongEscrow,
	UnknownQuote,
	NotAPurchaseQuote,
	AmountMismatch,
	RateOutOfBand,
	BadBuyer,
	DeniedBuyer,
	Expired,
	DeadlineTooFar,
	OverTradeLimit,
	OverDailyLimit,
	BelowFloatFloor,
	NoPermit2Allowance,
	TypedDataDiffers(String),
}

impl Refusal {
	pub fn message(&self) -> String {
		match self {
			Self::NotAPurchase => "It is not a purchase of this operator.".into(),
			Self::WrongChain => "It is on a chain that isn't in your config.".into(),
			Self::WrongToken => "Its token is not the USDC pinned for the chain.".into(),
			Self::WrongEscrow => "Its escrow is not the one pinned for the chain.".into(),
			Self::UnknownQuote => "It names a quote this agent never saw in your book.".into(),
			Self::NotAPurchaseQuote => {
				"Its quote is not one of your purchase (on-ramp) offers.".into()
			}
			Self::AmountMismatch => {
				"Its USDC amount is not what your own offer gives for the buyer's naira.".into()
			}
			Self::RateOutOfBand => "Its quote's rate is too far from your reference rate.".into(),
			Self::BadBuyer => "Its buyer is not a wallet address.".into(),
			Self::DeniedBuyer => "The buyer's wallet is on your deny list.".into(),
			Self::Expired => "Its signing window has already closed.".into(),
			Self::DeadlineTooFar => {
				"Its deadline is further away than purchases.max_deadline_secs allows.".into()
			}
			Self::OverTradeLimit => "It is over purchases.max_per_trade_minor.".into(),
			Self::OverDailyLimit => {
				"It would take today's purchases over purchases.max_per_day_minor.".into()
			}
			Self::BelowFloatFloor => "It would leave the float wallet below its floor.".into(),
			Self::NoPermit2Allowance => {
				"The float wallet has not approved Permit2 for this much USDC.".into()
			}
			Self::TypedDataDiffers(why) => format!("RecoilPay asked for something else: {why}."),
		}
	}

	/// The closest cash-out hold reason, for metrics.
	fn hold_reason(&self) -> HoldReason {
		match self {
			Self::UnknownQuote | Self::NotAPurchaseQuote => HoldReason::UnknownQuote,
			Self::RateOutOfBand => HoldReason::RateOutOfBand,
			Self::OverTradeLimit => HoldReason::OverTradeLimit,
			Self::OverDailyLimit => HoldReason::OverDailyLimit,
			Self::DeniedBuyer => HoldReason::Screening,
			Self::BelowFloatFloor | Self::NoPermit2Allowance => HoldReason::InsufficientFloat,
			Self::Expired | Self::DeadlineTooFar => HoldReason::DeadlineTooClose,
			_ => HoldReason::LockMismatch,
		}
	}
}

/// One chain as pinned in config.
#[derive(Debug, Clone)]
pub struct ChainPin {
	pub caip2: String,
	pub chain_id: u64,
	pub escrow: Address,
	pub usdc: Address,
	pub usdc_decimals: u8,
	/// Never leave the float wallet with less than this (USDC base units).
	pub float_floor: u128,
}

/// The purchase rules from config.
#[derive(Debug, Clone)]
pub struct Rules {
	pub solver_id: String,
	pub max_per_trade: Minor,
	pub max_per_day: Minor,
	pub max_deadline_secs: i64,
	pub max_rate_deviation_bps: u32,
}

/// What the agent knows besides the lock itself.
pub struct Facts<'a> {
	/// The quote's first-seen terms, if the agent saw it.
	pub terms: Option<&'a QuoteTerms>,
	/// Naira of purchases already signed today.
	pub signed_today: Minor,
	pub deny: &'a DenyList,
	pub now: DateTime<Utc>,
}

/// The lock this agent would sign for `lock`, rebuilt from its own terms and
/// config, or why it won't. Everything but the float wallet's balance, which
/// needs the chain ([`check_float`]).
pub fn plan(
	lock: &PurchaseLock,
	pin: &ChainPin,
	rules: &Rules,
	facts: &Facts<'_>,
) -> Result<Permit, Refusal> {
	use Refusal::*;
	if !lock.trade_id.starts_with("ft-on-") || !crate::aggregator::valid_id(&lock.trade_id) {
		return Err(NotAPurchase);
	}
	if lock.chain != pin.caip2 {
		return Err(WrongChain);
	}
	if lock.token.parse::<Address>().ok() != Some(pin.usdc) {
		return Err(WrongToken);
	}
	if lock.escrow.parse::<Address>().ok() != Some(pin.escrow) {
		return Err(WrongEscrow);
	}

	// Invariant 4: the asset from the merchant's own first-seen terms.
	let terms = facts.terms.ok_or(UnknownQuote)?;
	if terms.direction != Direction::Onramp
		|| terms.chain != lock.chain
		|| terms.fiat_currency != lock.fiat_currency
		|| terms.asset != "USDC"
		|| terms.asset_decimals != pin.usdc_decimals
	{
		return Err(NotAPurchaseQuote);
	}
	let asset = price_onramp(terms, lock.fiat_gross.get()).map_err(|_| AmountMismatch)?;
	if asset != lock.asset_amount.get() {
		return Err(AmountMismatch);
	}
	check_rate(terms, rules.max_rate_deviation_bps).map_err(|_| RateOutOfBand)?;

	let buyer: Address = lock.buyer.parse().map_err(|_| BadBuyer)?;
	if buyer == Address::ZERO {
		return Err(BadBuyer);
	}
	if facts.deny.denies_wallet(&lock.buyer) {
		return Err(DeniedBuyer);
	}
	if lock.sign_by <= facts.now {
		return Err(Expired);
	}
	if (lock.sign_by - facts.now).num_seconds() > rules.max_deadline_secs {
		return Err(DeadlineTooFar);
	}
	if lock.fiat_gross > rules.max_per_trade {
		return Err(OverTradeLimit);
	}
	let after = facts
		.signed_today
		.get()
		.checked_add(lock.fiat_gross.get())
		.ok_or(OverDailyLimit)?;
	if after > rules.max_per_day.get() {
		return Err(OverDailyLimit);
	}

	let permit = Permit {
		chain_id: pin.chain_id,
		escrow: pin.escrow,
		token: pin.usdc,
		amount: U256::from(asset),
		trade_id_hash: permit::trade_id_hash(&lock.trade_id),
		counterparty: buyer,
		nonce: permit::lock_nonce(&lock.trade_id),
		deadline: U256::from(lock.sign_by.timestamp().max(0) as u64),
	};
	permit.matches(&lock.typed_data).map_err(TypedDataDiffers)?;
	Ok(permit)
}

/// The float wallet can fund `permit` and stay above its floor.
pub fn check_float(
	permit: &Permit,
	pin: &ChainPin,
	balance: U256,
	allowance: U256,
) -> Result<(), Refusal> {
	if allowance < permit.amount {
		return Err(Refusal::NoPermit2Allowance);
	}
	match balance.checked_sub(permit.amount) {
		Some(left) if left >= U256::from(pin.float_floor) => Ok(()),
		_ => Err(Refusal::BelowFloatFloor),
	}
}

/// RecoilPay, as the purchase watcher uses it.
pub trait PurchasePlatform: Send + Sync {
	fn purchase_locks(
		&self,
	) -> impl std::future::Future<Output = Result<Vec<PurchaseLock>, AggregatorError>> + Send;
	/// The trade's state after the post.
	fn post_purchase_lock(
		&self,
		trade_id: &str,
		signature: &str,
	) -> impl std::future::Future<Output = Result<String, AggregatorError>> + Send;
}

impl PurchasePlatform for Client {
	async fn purchase_locks(&self) -> Result<Vec<PurchaseLock>, AggregatorError> {
		self.get_purchase_locks().await
	}
	async fn post_purchase_lock(
		&self,
		trade_id: &str,
		signature: &str,
	) -> Result<String, AggregatorError> {
		Client::post_purchase_lock(self, trade_id, signature).await
	}
}

/// The float wallet's balance and Permit2 allowance on a chain.
pub trait FloatReader: Send + Sync {
	fn float(
		&self,
		chain: &str,
		token: Address,
		owner: Address,
	) -> impl std::future::Future<Output = Result<(U256, U256), String>> + Send;
}

/// The merchant's own RPC endpoints, per chain.
pub struct ChainFloats(pub HashMap<String, Arc<crate::chain::ChainReader>>);

impl FloatReader for ChainFloats {
	async fn float(
		&self,
		chain: &str,
		token: Address,
		owner: Address,
	) -> Result<(U256, U256), String> {
		let reader = self
			.0
			.get(chain)
			.ok_or_else(|| format!("{chain} is not configured"))?;
		reader
			.float(token, owner, permit::PERMIT2)
			.await
			.map_err(|e| e.to_string())
	}
}

/// Called between storing a signature and posting it, in tests, to crash
/// there.
pub type CrashHook = Arc<dyn Fn(&str) + Send + Sync>;

/// Watches for purchases to sign, and signs the ones it can justify.
pub struct Purchases<P, F> {
	store: Arc<Store>,
	platform: P,
	floats: F,
	key: SigningKey,
	float_wallet: Address,
	pins: HashMap<String, ChainPin>,
	rules: Rules,
	deny: Arc<DenyList>,
	alerts: Mutex<Vec<Alert>>,
	crash: Option<CrashHook>,
}

#[derive(Debug, thiserror::Error)]
pub enum PurchaseError {
	#[error(transparent)]
	Store(#[from] StoreError),
}

impl<P: PurchasePlatform, F: FloatReader> Purchases<P, F> {
	#[allow(clippy::too_many_arguments)]
	pub fn new(
		store: Arc<Store>,
		platform: P,
		floats: F,
		key: SigningKey,
		pins: Vec<ChainPin>,
		rules: Rules,
		deny: Arc<DenyList>,
	) -> Self {
		let float_wallet = permit::address_of(&key);
		Self {
			store,
			platform,
			floats,
			key,
			float_wallet,
			pins: pins.into_iter().map(|p| (p.caip2.clone(), p)).collect(),
			rules,
			deny,
			alerts: Mutex::new(Vec::new()),
			crash: None,
		}
	}

	pub fn with_crash_hook(mut self, hook: CrashHook) -> Self {
		self.crash = Some(hook);
		self
	}

	/// The float wallet this agent signs as.
	pub fn float_wallet(&self) -> Address {
		self.float_wallet
	}

	pub fn take_alerts(&self) -> Vec<Alert> {
		std::mem::take(&mut *self.alerts.lock().unwrap_or_else(|e| e.into_inner()))
	}

	/// One pass: post any signature still waiting, then look at every
	/// purchase RecoilPay has for this operator. RecoilPay and chain
	/// hiccups wait for the next pass; only storage problems (including a
	/// lost lease) are errors.
	pub async fn tick(&self, now: DateTime<Utc>) -> Result<(), PurchaseError> {
		for record in self.store.unposted_purchases()? {
			if let Some(sig) = &record.signature {
				self.post(&record.trade_id, sig, now).await?;
			}
		}
		let locks = match self.platform.purchase_locks().await {
			Ok(l) => l,
			Err(e) => {
				tracing::warn!(error = %e, "could not read the purchases waiting for a signature; trying again next pass");
				return Ok(());
			}
		};
		for lock in locks {
			// A trade is looked at once: signed, posted or refused, its
			// answer stands.
			if self.store.purchase(&lock.trade_id)?.is_some() {
				continue;
			}
			self.consider(&lock, now).await?;
		}
		Ok(())
	}

	async fn consider(&self, lock: &PurchaseLock, now: DateTime<Utc>) -> Result<(), PurchaseError> {
		let record = NewPurchase {
			trade_id: &lock.trade_id,
			chain: &lock.chain,
			quote_id: &lock.quote_id,
			buyer: &lock.buyer,
			fiat_gross: lock.fiat_gross,
			asset: lock.asset_amount,
		};
		let Some(pin) = self.pins.get(&lock.chain) else {
			return self.refuse(&record, Refusal::WrongChain, now);
		};
		let terms = self.store.quote(&lock.quote_id)?;
		let signed_today = self
			.store
			.signed_purchase_total(&crate::policy::limits::day_of(now))?;
		let facts = Facts {
			terms: terms.as_ref(),
			signed_today,
			deny: &self.deny,
			now,
		};
		let permit = match plan(lock, pin, &self.rules, &facts) {
			Ok(p) => p,
			Err(r) => return self.refuse(&record, r, now),
		};
		let (balance, allowance) = match self
			.floats
			.float(&lock.chain, pin.usdc, self.float_wallet)
			.await
		{
			Ok(v) => v,
			Err(e) => {
				// Not a refusal: the chain may answer next pass, inside the window.
				tracing::warn!(trade_id = %lock.trade_id, error = %e, "could not read the float wallet; trying again next pass");
				return Ok(());
			}
		};
		if let Err(r) = check_float(&permit, pin, balance, allowance) {
			return self.refuse(&record, r, now);
		}

		let signature = match permit.sign(&self.key) {
			Ok(s) => s,
			Err(e) => {
				tracing::error!(trade_id = %lock.trade_id, error = %e, "could not sign a purchase");
				return Ok(());
			}
		};
		// Stored before it is posted: after a crash it is posted again, never
		// signed again.
		self.store
			.record_purchase_signed(&record, &signature, now)?;
		tracing::info!(trade_id = %lock.trade_id, asset = %lock.asset_amount, "purchase signed");
		if let Some(crash) = &self.crash {
			crash(&lock.trade_id);
		}
		self.post(&lock.trade_id, &signature, now).await
	}

	/// Post a stored signature. Accepted, or refused for good, it is marked
	/// posted with RecoilPay's answer; a hiccup leaves it for the next pass.
	async fn post(
		&self,
		trade_id: &str,
		signature: &str,
		now: DateTime<Utc>,
	) -> Result<(), PurchaseError> {
		let outcome = match self.platform.post_purchase_lock(trade_id, signature).await {
			Ok(state) => state,
			// Maybe accepted: post the same signature again next pass.
			Err(e)
				if e.is_transient()
					|| matches!(e, AggregatorError::Decode(_) | AggregatorError::TooLarge) =>
			{
				tracing::warn!(%trade_id, error = %e, "could not post a purchase signature; posting it again next pass");
				return Ok(());
			}
			Err(e) => {
				let code = e.code().unwrap_or("REFUSED").to_string();
				tracing::error!(%trade_id, error = %e, "RecoilPay refused a purchase signature");
				if code == "BAD_SIGNATURE" {
					// Our signature should always verify: the float wallet in
					// config is not the one registered, or the shapes drifted.
					self.alert(trade_id, Minor(0), "RecoilPay rejected the float wallet's signature. Run `merchant-agent doctor`: the signer may not be the registered float wallet.".into(), true);
				}
				format!("refused:{code}")
			}
		};
		self.store.record_purchase_posted(trade_id, &outcome, now)?;
		Ok(())
	}

	fn refuse(
		&self,
		record: &NewPurchase<'_>,
		why: Refusal,
		now: DateTime<Utc>,
	) -> Result<(), PurchaseError> {
		let message = why.message();
		if self.store.record_purchase_refused(record, &message, now)? {
			tracing::warn!(trade_id = %record.trade_id, reason = ?why.hold_reason(), "purchase refused: {message}");
			self.alert(record.trade_id, record.fiat_gross, message, false);
		}
		Ok(())
	}

	fn alert(&self, trade_id: &str, amount: Minor, why: String, urgent: bool) {
		self.alerts
			.lock()
			.unwrap_or_else(|e| e.into_inner())
			.push(Alert {
				trade_id: trade_id.into(),
				state: AgentState::Held(HoldReason::LockMismatch),
				urgent,
				account_masked: None,
				amount,
				detail: None,
				purchase_refusal: Some(why),
			});
	}
}

/// Status the console shows for a purchase.
pub fn label(status: &PurchaseStatus) -> &'static str {
	match status {
		PurchaseStatus::Signed => "Signed, posting",
		PurchaseStatus::Posted => "Signed",
		PurchaseStatus::Refused => "Refused",
	}
}
