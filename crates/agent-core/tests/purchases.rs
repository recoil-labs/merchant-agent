//! Merchant-signed purchases (v0.3): the agent signs a purchase's Permit2
//! lock only when it can justify it from its own quote history, limits,
//! float and pinned config (invariant 4), signs each trade at most once, and
//! survives a crash between signing and posting.
//!
//! RecoilPay and the chain are fakes here. The signature itself is checked
//! against the shared Permit2 vector in `purchases::permit`'s unit tests.

use std::collections::VecDeque;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use alloy::primitives::{Address, U256};
use chrono::{DateTime, TimeZone, Utc};
use k256::ecdsa::{RecoveryId, Signature, VerifyingKey};
use merchant_agent_core::aggregator::types::{Direction, PurchaseLock};
use merchant_agent_core::aggregator::{AggregatorError, ErrorBody, Quote};
use merchant_agent_core::money::Minor;
use merchant_agent_core::policy::pricing::price_onramp;
use merchant_agent_core::policy::screening::DenyList;
use merchant_agent_core::purchases::permit::{self, Permit};
use merchant_agent_core::purchases::{
	check_float, plan, ChainPin, Facts, FloatReader, PurchasePlatform, Purchases, Refusal, Rules,
};
use merchant_agent_core::store::{PurchaseStatus, Store};
use serde_json::{json, Value};

const FIXTURE: &str = include_str!("../../../vectors/merchant-agent-api.json");
const SOLVER: &str = "solver-1111111111111111111111111111111111111111";
const CHAIN: &str = "eip155:84532";
const ESCROW: &str = "0x112a534104ac10e73d57f7c5a4c6469632392173";
const USDC: &str = "0x73c83dacc74bb8a704717ac09703b959e74b9705";
const BUYER: &str = "0x2222222222222222222222222222222222222222";
/// A throwaway test key, never a real one.
const FLOAT_KEY: &str = "0x00000000000000000000000000000000000000000000000000000000000000f1";
const QUOTE: &str = "fq-onramp-1";

fn now() -> DateTime<Utc> {
	Utc.with_ymd_and_hms(2026, 11, 13, 12, 0, 0).unwrap()
}

fn pin() -> ChainPin {
	ChainPin {
		caip2: CHAIN.into(),
		chain_id: 84532,
		escrow: ESCROW.parse().unwrap(),
		usdc: USDC.parse().unwrap(),
		usdc_decimals: 6,
		float_floor: 50_000_000,
	}
}

fn rules() -> Rules {
	Rules {
		solver_id: SOLVER.into(),
		max_per_trade: Minor(45_000_000),
		max_per_day: Minor(100_000_000),
		max_deadline_secs: 300,
		max_rate_deviation_bps: 200,
	}
}

/// The merchant's purchase offer, as its book returns it: ₦1,520.50 per
/// USDC, ₦100 + 0.25%.
fn onramp_quote() -> Quote {
	let all: Value = serde_json::from_str(FIXTURE).unwrap();
	let mut q = all["fiatQuotes"]["quotes"][0].clone();
	q["id"] = json!(QUOTE);
	q["solver_id"] = json!(SOLVER);
	q["direction"] = json!("onramp");
	q["chain"] = json!(CHAIN);
	serde_json::from_value(q).unwrap()
}

/// The typed data as RecoilPay writes it.
fn typed_data(p: &Permit) -> Value {
	let a = |x: Address| format!("{x:#x}");
	json!({
		"domain": { "name": "Permit2", "chainId": p.chain_id, "verifyingContract": "0x000000000022D473030F116dDEE9F6B43aC78BA3" },
		"types": {
			"PermitWitnessTransferFrom": [
				{ "name": "permitted", "type": "TokenPermissions" },
				{ "name": "spender", "type": "address" },
				{ "name": "nonce", "type": "uint256" },
				{ "name": "deadline", "type": "uint256" },
				{ "name": "witness", "type": "EscrowLock" }
			],
			"EscrowLock": [
				{ "name": "tradeId", "type": "bytes32" },
				{ "name": "counterparty", "type": "address" }
			],
			"TokenPermissions": [
				{ "name": "token", "type": "address" },
				{ "name": "amount", "type": "uint256" }
			]
		},
		"primaryType": "PermitWitnessTransferFrom",
		"message": {
			"permitted": { "token": a(p.token), "amount": p.amount.to_string() },
			"spender": a(p.escrow),
			"nonce": p.nonce.to_string(),
			"deadline": p.deadline.to_string(),
			"witness": { "tradeId": format!("{:#x}", p.trade_id_hash), "counterparty": a(p.counterparty) }
		}
	})
}

/// A purchase RecoilPay asks the agent to sign: `gross` kobo on the
/// merchant's offer, priced as the aggregator prices it.
fn lock(trade_id: &str, gross: u128) -> PurchaseLock {
	let terms = terms_of(&onramp_quote());
	let asset = price_onramp(&terms, gross).unwrap();
	let sign_by = now() + chrono::Duration::seconds(180);
	let p = Permit {
		chain_id: 84532,
		escrow: ESCROW.parse().unwrap(),
		token: USDC.parse().unwrap(),
		amount: U256::from(asset),
		trade_id_hash: permit::trade_id_hash(trade_id),
		counterparty: BUYER.parse().unwrap(),
		nonce: permit::lock_nonce(trade_id),
		deadline: U256::from(sign_by.timestamp() as u64),
	};
	PurchaseLock {
		trade_id: trade_id.into(),
		quote_id: QUOTE.into(),
		chain: CHAIN.into(),
		buyer: BUYER.into(),
		fiat_currency: "NGN".into(),
		fiat_gross: Minor(gross),
		asset_amount: Minor(asset),
		token: USDC.into(),
		escrow: ESCROW.into(),
		sign_by,
		typed_data: typed_data(&p),
	}
}

fn terms_of(q: &Quote) -> merchant_agent_core::policy::pricing::QuoteTerms {
	merchant_agent_core::policy::pricing::QuoteTerms {
		quote_id: q.id.clone(),
		direction: q.direction,
		chain: q.chain.clone(),
		asset: q.asset.clone(),
		asset_decimals: q.asset_decimals,
		fiat_currency: q.fiat_currency.clone(),
		rail: q.rail.clone(),
		rate: q.rate.as_str().to_string(),
		fee_fixed: q.fee_fixed,
		fee_bps: q.fee_bps,
		min_fiat: q.min_fiat,
		max_fiat: q.max_fiat,
		reference_rate: None,
	}
}

// ── the checks (invariant 4) ────────────────────────────────────────────

fn facts<'a>(
	terms: Option<&'a merchant_agent_core::policy::pricing::QuoteTerms>,
	deny: &'a DenyList,
	signed_today: u128,
) -> Facts<'a> {
	Facts {
		terms,
		signed_today: Minor(signed_today),
		deny,
		now: now(),
	}
}

#[test]
fn invariant_4_a_purchase_its_own_terms_justify_is_signed_for_exactly_that() {
	let terms = terms_of(&onramp_quote());
	let deny = DenyList::parse("").unwrap();
	let l = lock("ft-on-aaaaaaaaaaaaaaaa", 15_128_975);
	let p = plan(&l, &pin(), &rules(), &facts(Some(&terms), &deny, 0)).unwrap();
	assert_eq!(p.amount, U256::from(l.asset_amount.get()));
	assert_eq!(
		p.nonce,
		permit::lock_nonce(&l.trade_id),
		"invariant 5: the trade's own nonce"
	);
	assert_eq!(p.deadline, U256::from(l.sign_by.timestamp() as u64));
	assert_eq!(p.counterparty, BUYER.parse::<Address>().unwrap());
}

#[test]
fn invariant_4_anything_its_own_terms_cannot_justify_is_refused() {
	let terms = terms_of(&onramp_quote());
	let deny =
		DenyList::parse("0x2222222222222222222222222222222222222222 # a denied buyer").unwrap();
	let clear = DenyList::parse("").unwrap();
	let ok = lock("ft-on-bbbbbbbbbbbbbbbb", 15_128_975);
	let check = |l: &PurchaseLock, f: &Facts<'_>| plan(l, &pin(), &rules(), f).err();

	// More USDC than the buyer's naira buys on the merchant's own terms: a
	// compromised RecoilPay asking for more is refused. Its typed data
	// matching its claim makes no difference.
	let mut more = ok.clone();
	more.asset_amount = Minor(ok.asset_amount.get() + 1);
	assert_eq!(
		check(&more, &facts(Some(&terms), &clear, 0)),
		Some(Refusal::AmountMismatch)
	);

	assert_eq!(
		check(&ok, &facts(None, &clear, 0)),
		Some(Refusal::UnknownQuote)
	);
	let mut cash_out = terms.clone();
	cash_out.direction = Direction::Offramp;
	assert_eq!(
		check(&ok, &facts(Some(&cash_out), &clear, 0)),
		Some(Refusal::NotAPurchaseQuote)
	);
	let mut banded = terms.clone();
	banded.reference_rate = Some("1400".into());
	assert_eq!(
		check(&ok, &facts(Some(&banded), &clear, 0)),
		Some(Refusal::RateOutOfBand)
	);

	assert_eq!(
		check(&ok, &facts(Some(&terms), &deny, 0)),
		Some(Refusal::DeniedBuyer)
	);

	let mut t = ok.clone();
	t.token = "0x0000000000000000000000000000000000000001".into();
	assert_eq!(
		check(&t, &facts(Some(&terms), &clear, 0)),
		Some(Refusal::WrongToken)
	);
	let mut t = ok.clone();
	t.escrow = "0x0000000000000000000000000000000000000002".into();
	assert_eq!(
		check(&t, &facts(Some(&terms), &clear, 0)),
		Some(Refusal::WrongEscrow)
	);
	let mut t = ok.clone();
	t.chain = "eip155:1".into();
	assert_eq!(
		check(&t, &facts(Some(&terms), &clear, 0)),
		Some(Refusal::WrongChain)
	);

	let mut t = ok.clone();
	t.sign_by = now();
	assert_eq!(
		check(&t, &facts(Some(&terms), &clear, 0)),
		Some(Refusal::Expired)
	);
	let mut t = ok.clone();
	t.sign_by = now() + chrono::Duration::seconds(301);
	assert_eq!(
		check(&t, &facts(Some(&terms), &clear, 0)),
		Some(Refusal::DeadlineTooFar)
	);

	// Limits: ₦450,000 a trade, ₦1,000,000 a day.
	let big = lock("ft-on-cccccccccccccccc", 46_000_000);
	assert_eq!(
		check(&big, &facts(Some(&terms), &clear, 0)),
		Some(Refusal::OverTradeLimit)
	);
	assert_eq!(
		check(&ok, &facts(Some(&terms), &clear, 90_000_000)),
		Some(Refusal::OverDailyLimit)
	);

	// Typed data asking for another buyer than the trade's, or another nonce.
	let mut t = ok.clone();
	t.typed_data["message"]["witness"]["counterparty"] =
		json!("0x6666666666666666666666666666666666666666");
	assert!(matches!(
		check(&t, &facts(Some(&terms), &clear, 0)),
		Some(Refusal::TypedDataDiffers(_))
	));
	let mut t = ok.clone();
	t.typed_data["message"]["nonce"] = json!(permit::lock_nonce("ft-on-another").to_string());
	assert!(matches!(
		check(&t, &facts(Some(&terms), &clear, 0)),
		Some(Refusal::TypedDataDiffers(_))
	));
}

#[test]
fn the_float_must_stay_above_its_floor_with_permit2_allowed() {
	let terms = terms_of(&onramp_quote());
	let deny = DenyList::parse("").unwrap();
	let l = lock("ft-on-dddddddddddddddd", 15_128_975);
	let p = plan(&l, &pin(), &rules(), &facts(Some(&terms), &deny, 0)).unwrap();
	let floor = U256::from(pin().float_floor);
	assert_eq!(
		check_float(&p, &pin(), floor + p.amount, p.amount),
		Ok(()),
		"exactly at the floor after"
	);
	assert_eq!(
		check_float(&p, &pin(), floor + p.amount - U256::from(1u8), p.amount),
		Err(Refusal::BelowFloatFloor)
	);
	assert_eq!(
		check_float(&p, &pin(), U256::MAX, p.amount - U256::from(1u8)),
		Err(Refusal::NoPermit2Allowance)
	);
}

// ── the watcher ─────────────────────────────────────────────────────────

#[derive(Default)]
struct Inner {
	locks: Mutex<Vec<PurchaseLock>>,
	posts: Mutex<Vec<(String, String)>>,
	fail_posts: Mutex<VecDeque<AggregatorError>>,
}

#[derive(Clone, Default)]
struct Fake(Arc<Inner>);

impl PurchasePlatform for Fake {
	async fn purchase_locks(&self) -> Result<Vec<PurchaseLock>, AggregatorError> {
		Ok(self.0.locks.lock().unwrap().clone())
	}
	async fn post_purchase_lock(
		&self,
		trade_id: &str,
		signature: &str,
	) -> Result<String, AggregatorError> {
		if let Some(e) = self.0.fail_posts.lock().unwrap().pop_front() {
			return Err(e);
		}
		self.0
			.posts
			.lock()
			.unwrap()
			.push((trade_id.into(), signature.into()));
		Ok("awaiting_fiat".into())
	}
}

struct Floats(U256);

impl FloatReader for Floats {
	async fn float(&self, _: &str, _: Address, _: Address) -> Result<(U256, U256), String> {
		Ok((self.0, U256::MAX))
	}
}

static HOLDER: AtomicUsize = AtomicUsize::new(0);

fn store(dir: &std::path::Path) -> Arc<Store> {
	let s = Store::open(
		dir,
		&format!("purchases-{}", HOLDER.fetch_add(1, Ordering::SeqCst)),
	)
	.unwrap();
	s.acquire_lease(Duration::from_secs(3600), now()).unwrap();
	s.record_quotes(&[onramp_quote()], |_| None, now()).unwrap();
	Arc::new(s)
}

fn watcher(store: Arc<Store>, fake: Fake, float: u128) -> Purchases<Fake, Floats> {
	Purchases::new(
		store,
		fake,
		Floats(U256::from(float)),
		permit::signing_key(FLOAT_KEY).unwrap(),
		vec![pin()],
		rules(),
		Arc::new(DenyList::parse("").unwrap()),
	)
}

fn recovers_to_float_wallet(l: &PurchaseLock, signature: &str) -> bool {
	let p = plan(
		l,
		&pin(),
		&rules(),
		&facts(
			Some(&terms_of(&onramp_quote())),
			&DenyList::parse("").unwrap(),
			0,
		),
	)
	.unwrap();
	let sig = alloy::primitives::hex::decode(signature).unwrap();
	let vk = VerifyingKey::recover_from_prehash(
		p.digest().as_slice(),
		&Signature::from_slice(&sig[..64]).unwrap(),
		RecoveryId::from_byte(sig[64] - 27).unwrap(),
	)
	.unwrap();
	vk == *permit::signing_key(FLOAT_KEY).unwrap().verifying_key()
}

#[tokio::test]
async fn a_justified_purchase_is_signed_once_posted_and_audited() {
	let dir = tempfile::tempdir().unwrap();
	let s = store(dir.path());
	let fake = Fake::default();
	let l = lock("ft-on-eeeeeeeeeeeeeeee", 15_128_975);
	fake.0.locks.lock().unwrap().push(l.clone());
	let w = watcher(s.clone(), fake.clone(), 1_000_000_000);

	w.tick(now()).await.unwrap();
	let posts = fake.0.posts.lock().unwrap().clone();
	assert_eq!(posts.len(), 1);
	assert!(
		recovers_to_float_wallet(&l, &posts[0].1),
		"signed by the float wallet over the agent's own lock"
	);
	let rec = s.purchase(&l.trade_id).unwrap().unwrap();
	assert_eq!(
		(rec.status, rec.outcome.as_deref()),
		(PurchaseStatus::Posted, Some("awaiting_fiat"))
	);

	// RecoilPay lists it again (say, before the lock lands): not signed again.
	w.tick(now()).await.unwrap();
	assert_eq!(fake.0.posts.lock().unwrap().len(), 1);
	assert!(w.take_alerts().is_empty());

	let audit = audit_actions(dir.path());
	assert!(
		audit.contains(&"purchase_signed".to_string())
			&& audit.contains(&"purchase_posted".to_string()),
		"{audit:?}"
	);
}

#[tokio::test]
async fn a_refusal_is_recorded_alerted_and_audited_and_nothing_is_signed() {
	let dir = tempfile::tempdir().unwrap();
	let s = store(dir.path());
	let fake = Fake::default();
	let mut l = lock("ft-on-ffffffffffffffff", 15_128_975);
	l.asset_amount = Minor(l.asset_amount.get() * 2);
	fake.0.locks.lock().unwrap().push(l.clone());
	let w = watcher(s.clone(), fake.clone(), 1_000_000_000);

	w.tick(now()).await.unwrap();
	w.tick(now()).await.unwrap();
	assert!(
		fake.0.posts.lock().unwrap().is_empty(),
		"nothing signed, nothing posted"
	);
	let rec = s.purchase(&l.trade_id).unwrap().unwrap();
	assert_eq!(rec.status, PurchaseStatus::Refused);
	assert!(rec.signature.is_none());
	let alerts = w.take_alerts();
	assert_eq!(alerts.len(), 1, "alerted once, not every pass");
	let text = merchant_agent_core::runner::describe(&alerts[0]);
	assert!(
		text.contains("refused") && text.contains("not what your own offer gives"),
		"{text}"
	);
	assert!(audit_actions(dir.path()).contains(&"purchase_refused".to_string()));
}

#[tokio::test]
async fn the_daily_limit_counts_every_purchase_signed_today() {
	let dir = tempfile::tempdir().unwrap();
	let s = store(dir.path());
	let fake = Fake::default();
	// ₦400,000 each against ₦1,000,000 a day: two fit, the third doesn't.
	for id in [
		"ft-on-day0000000000001",
		"ft-on-day0000000000002",
		"ft-on-day0000000000003",
	] {
		fake.0.locks.lock().unwrap().push(lock(id, 40_000_000));
	}
	let w = watcher(s.clone(), fake.clone(), 10_000_000_000);
	w.tick(now()).await.unwrap();
	assert_eq!(fake.0.posts.lock().unwrap().len(), 2);
	assert_eq!(
		s.purchase("ft-on-day0000000000003")
			.unwrap()
			.unwrap()
			.status,
		PurchaseStatus::Refused
	);
	assert_eq!(
		s.signed_purchase_total(&merchant_agent_core::policy::limits::day_of(now()))
			.unwrap(),
		Minor(80_000_000)
	);
}

#[tokio::test]
async fn a_float_below_its_floor_refuses_and_an_unreadable_chain_waits() {
	let dir = tempfile::tempdir().unwrap();
	let s = store(dir.path());
	let fake = Fake::default();
	let l = lock("ft-on-floor00000000001", 15_128_975);
	fake.0.locks.lock().unwrap().push(l.clone());
	// Just less than floor + this purchase.
	let w = watcher(
		s.clone(),
		fake.clone(),
		50_000_000 + l.asset_amount.get() - 1,
	);
	w.tick(now()).await.unwrap();
	assert_eq!(
		s.purchase(&l.trade_id).unwrap().unwrap().reason.as_deref(),
		Some("It would leave the float wallet below its floor.")
	);

	struct Down;
	impl FloatReader for Down {
		async fn float(&self, _: &str, _: Address, _: Address) -> Result<(U256, U256), String> {
			Err("rpc down".into())
		}
	}
	let fake = Fake::default();
	let l = lock("ft-on-chaindown000001", 15_128_975);
	fake.0.locks.lock().unwrap().push(l.clone());
	let w = Purchases::new(
		s.clone(),
		fake.clone(),
		Down,
		permit::signing_key(FLOAT_KEY).unwrap(),
		vec![pin()],
		rules(),
		Arc::new(DenyList::parse("").unwrap()),
	);
	w.tick(now()).await.unwrap();
	assert!(
		s.purchase(&l.trade_id).unwrap().is_none(),
		"not refused: the chain may answer next pass"
	);
}

#[tokio::test]
async fn crash_safety_a_signature_stored_before_a_crash_is_posted_after_it_never_signed_again() {
	let dir = tempfile::tempdir().unwrap();
	let fake = Fake::default();
	let l = lock("ft-on-crash0000000001", 15_128_975);
	fake.0.locks.lock().unwrap().push(l.clone());

	// Sign, store, then "crash" before the post.
	{
		let s = store(dir.path());
		let w = watcher(s.clone(), fake.clone(), 1_000_000_000)
			.with_crash_hook(Arc::new(|_| panic!("crash")));
		let r = tokio::spawn(async move { w.tick(now()).await }).await;
		assert!(r.is_err(), "the hook crashed the pass");
		assert!(fake.0.posts.lock().unwrap().is_empty());
	}
	// A restart: the same signature goes out, and nothing new is signed.
	let s = Arc::new(Store::open(dir.path(), "after-crash").unwrap());
	s.acquire_lease(
		Duration::from_secs(3600),
		now() + chrono::Duration::hours(2),
	)
	.unwrap();
	let stored = s.purchase(&l.trade_id).unwrap().unwrap();
	assert_eq!(stored.status, PurchaseStatus::Signed);
	let w = watcher(s.clone(), fake.clone(), 1_000_000_000);
	w.tick(now() + chrono::Duration::hours(2)).await.unwrap();
	let posts = fake.0.posts.lock().unwrap().clone();
	assert_eq!(
		posts,
		vec![(l.trade_id.clone(), stored.signature.clone().unwrap())],
		"posted once, with the stored signature"
	);
	assert_eq!(
		s.purchase(&l.trade_id).unwrap().unwrap().status,
		PurchaseStatus::Posted
	);
}

#[tokio::test]
async fn a_post_that_fails_is_retried_and_a_rejected_signature_is_urgent() {
	let dir = tempfile::tempdir().unwrap();
	let s = store(dir.path());
	let fake = Fake::default();
	let l = lock("ft-on-retry0000000001", 15_128_975);
	fake.0.locks.lock().unwrap().push(l.clone());
	fake.0
		.fail_posts
		.lock()
		.unwrap()
		.push_back(AggregatorError::Server(503));
	let w = watcher(s.clone(), fake.clone(), 1_000_000_000);
	w.tick(now()).await.unwrap();
	assert_eq!(
		s.purchase(&l.trade_id).unwrap().unwrap().status,
		PurchaseStatus::Signed,
		"kept for the next pass"
	);
	w.tick(now()).await.unwrap();
	assert_eq!(
		s.purchase(&l.trade_id).unwrap().unwrap().status,
		PurchaseStatus::Posted
	);
	assert_eq!(fake.0.posts.lock().unwrap().len(), 1);

	let l = lock("ft-on-badsig000000001", 15_128_975);
	fake.0.locks.lock().unwrap().push(l.clone());
	fake.0
		.fail_posts
		.lock()
		.unwrap()
		.push_back(AggregatorError::BadRequest(ErrorBody {
			error: "BAD_SIGNATURE".into(),
			message: "m".into(),
		}));
	w.tick(now()).await.unwrap();
	let rec = s.purchase(&l.trade_id).unwrap().unwrap();
	assert_eq!(rec.outcome.as_deref(), Some("refused:BAD_SIGNATURE"));
	let alerts = w.take_alerts();
	assert!(
		alerts.iter().any(|a| a.urgent && a.trade_id == l.trade_id),
		"{alerts:?}"
	);
}

// ── the quote history and the database ──────────────────────────────────

#[test]
fn quotes_keep_their_direction_and_older_quotes_read_as_cash_outs() {
	use merchant_agent_core::store::DB_FILE;
	let dir = tempfile::tempdir().unwrap();
	let s = store(dir.path());
	assert_eq!(
		s.quote(QUOTE).unwrap().unwrap().direction,
		Direction::Onramp
	);
	drop(s);

	// A schema-4 database, from before purchases, with a quote in it.
	{
		let c = rusqlite::Connection::open(dir.path().join(DB_FILE)).unwrap();
		c.execute_batch(
			"DROP TABLE purchases; ALTER TABLE quotes_seen DROP COLUMN direction; UPDATE quotes_seen SET quote_id = 'fq-old'; PRAGMA user_version = 4;",
		)
		.unwrap();
	}
	let s = Store::open(dir.path(), "migrated").unwrap();
	assert_eq!(
		s.quote("fq-old").unwrap().unwrap().direction,
		Direction::Offramp,
		"every older quote was a cash-out quote"
	);
	assert!(
		s.purchase("ft-on-anything").unwrap().is_none(),
		"the purchases table exists"
	);
}

fn audit_actions(dir: &std::path::Path) -> Vec<String> {
	let c = rusqlite::Connection::open(dir.join(merchant_agent_core::store::DB_FILE)).unwrap();
	let mut stmt = c.prepare("SELECT action FROM audit ORDER BY seq").unwrap();
	stmt.query_map([], |r| r.get(0))
		.unwrap()
		.map(|r| r.unwrap())
		.collect()
}

// ── RecoilPay's shapes ──────────────────────────────────────────────────

#[test]
fn the_purchase_responses_parse_strictly_from_the_shared_fixture() {
	let all: Value = serde_json::from_str(FIXTURE).unwrap();
	let l: PurchaseLock = serde_json::from_value(all["purchaseLocks"]["locks"][0].clone()).unwrap();
	assert_eq!(l.asset_amount, Minor(100_000_000));
	assert_eq!(
		l.typed_data["message"]["nonce"].as_str().unwrap(),
		permit::lock_nonce(&l.trade_id).to_string(),
		"the fixture's nonce is the trade's"
	);
	let f: merchant_agent_core::aggregator::PurchaseFunding =
		serde_json::from_value(all["purchaseFunding"]["chains"][0].clone()).unwrap();
	assert_eq!(
		f.mode,
		merchant_agent_core::aggregator::PurchaseFundingMode::MerchantSigned
	);

	// Invariant 13: an unknown field stops the agent rather than being guessed at.
	let mut extra = all["purchaseLocks"]["locks"][0].clone();
	extra["newField"] = json!(1);
	assert!(serde_json::from_value::<PurchaseLock>(extra).is_err());
	let mut number = all["purchaseLocks"]["locks"][0].clone();
	number["assetAmount"] = json!(100000000);
	assert!(
		serde_json::from_value::<PurchaseLock>(number).is_err(),
		"money is a string of digits"
	);
}
