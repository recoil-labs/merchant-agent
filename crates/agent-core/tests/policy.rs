//! Policy: amounts (invariant 3), limits (invariant 5), the deadline guard
//! (invariant 6), screening and the two-person rule. Each rule is tested at
//! its boundary: just under, exactly at, just over.
//!
//! The trade is the shared fixture: 100 USDC at 1520.50 on the fixture
//! quote (fee ₦100 + 25 bps), with a 0.5% protocol fee in the lock, which
//! is design §2's worked example: 15,081,152 kobo.

mod common;
use common::*;

use std::sync::Arc;

use merchant_agent_core::aggregator::Quote;
use merchant_agent_core::config::{Limits, TwoPerson};
use merchant_agent_core::engine::{Decision, Engine, EngineConfig};
use merchant_agent_core::machine::PersonAction;
use merchant_agent_core::money::Minor;
use merchant_agent_core::payee::{payee_tag, PayeeIdentity};
use merchant_agent_core::policy::limits::{self, day_of};
use merchant_agent_core::policy::pricing::{check_rate, QuoteTerms};
use merchant_agent_core::policy::screening::DenyList;
use merchant_agent_core::policy::PolicyChecks;
use merchant_agent_core::store::{Intent, NewTrade, Patch};

const NET: u128 = 15_081_152;

type PolicyAgent = Engine<FakePlatform, FakeLocks, PolicyChecks<DenyList>, FlakyBank, ManualClock>;

fn fixture_quote() -> Quote {
	let v: Value = serde_json::from_str(API).unwrap();
	serde_json::from_value(v["fiatQuotes"]["quotes"][0].clone()).unwrap()
}

fn generous_limits() -> Limits {
	Limits {
		currency: "NGN".into(),
		per_trade: Minor(1_000_000_000),
		per_day: Minor(10_000_000_000),
		per_payee_per_day: Minor(1_000_000_000),
		velocity_max_payouts: 100,
		velocity_window_secs: 3600,
		min_float: Minor(0),
	}
}

struct Setup {
	cfg: EngineConfig,
	deny: String,
	tolerance_bps: u32,
	max_rate_deviation_bps: u32,
}

impl Default for Setup {
	fn default() -> Self {
		let mut cfg = World::cfg();
		cfg.limits = Some(generous_limits());
		Self {
			cfg,
			deny: String::new(),
			tolerance_bps: 0,
			max_rate_deviation_bps: 300,
		}
	}
}

fn world() -> World {
	let w = World::new();
	w.platform.quotes.lock().unwrap().push(fixture_quote());
	w
}

fn agent(world: &World, dir: &std::path::Path, setup: Setup) -> PolicyAgent {
	let store = Arc::new(merchant_agent_core::store::Store::open(dir, "policy").unwrap());
	let checks = PolicyChecks {
		payee: checks(),
		store: store.clone(),
		screening: DenyList::parse(&setup.deny).unwrap(),
		amount_tolerance_bps: setup.tolerance_bps,
		max_rate_deviation_bps: setup.max_rate_deviation_bps,
	};
	Engine::start(
		store,
		world.platform.clone(),
		world.locks.clone(),
		checks,
		world.bank.clone(),
		world.clock.clone(),
		setup.cfg,
	)
	.unwrap()
}

fn st(agent: &PolicyAgent) -> AgentState {
	agent.store().get(TRADE_ID).unwrap().unwrap().state
}

async fn settle(agent: &PolicyAgent) -> AgentState {
	for _ in 0..10 {
		agent.tick().await.unwrap();
		let s = st(agent);
		if reported(&s) || matches!(s, AgentState::Held(_) | AgentState::NeedsPerson(_)) {
			return s;
		}
	}
	st(agent)
}

async fn run(world: &World, setup: Setup) -> AgentState {
	let dir = tempfile::tempdir().unwrap();
	settle(&agent(world, dir.path(), setup)).await
}

// ── amounts (invariant 3) ────────────────────────────────────────────────

#[tokio::test]
async fn invariant_3_the_exact_recomputed_amount_is_paid() {
	let world = world();
	assert!(reported(&run(&world, Setup::default()).await));
	assert_eq!(world.bank.inner.settled_payments()[0].amount, Minor(NET));
}

#[tokio::test]
async fn invariant_3_one_kobo_more_or_less_than_the_recomputation_is_held() {
	for fiat in [NET + 1, NET - 1] {
		let world = world();
		world.platform.edit(|t| t.fiat_minor_units = Minor(fiat));
		assert_eq!(
			run(&world, Setup::default()).await,
			AgentState::Held(HoldReason::AmountMismatch),
			"{fiat}"
		);
		assert!(world.bank.inner.payments().is_empty());
	}
}

#[tokio::test]
async fn invariant_3_the_tolerance_boundary() {
	// 1 bps of 15,081,152 is 1,508 kobo.
	for (fiat, paid) in [
		(NET + 1_507, true),
		(NET + 1_508, true),
		(NET + 1_509, false),
	] {
		let world = world();
		world.platform.edit(|t| t.fiat_minor_units = Minor(fiat));
		let end = run(
			&world,
			Setup {
				tolerance_bps: 1,
				..Default::default()
			},
		)
		.await;
		assert_eq!(reported(&end), paid, "{fiat}: {end:?}");
	}
}

#[tokio::test]
async fn invariant_3_a_lock_with_a_different_protocol_fee_is_held() {
	// The escrow charges no fee, so the merchant would convert 100 USDC,
	// not 99.5: the aggregator's naira no longer matches the lock.
	let world = world();
	*world.locks.1.lock().unwrap() = "0".into();
	assert_eq!(
		run(&world, Setup::default()).await,
		AgentState::Held(HoldReason::AmountMismatch)
	);
}

#[tokio::test]
async fn invariant_3_a_trade_on_an_offer_never_seen_is_held() {
	let world = World::new(); // an empty book
	assert_eq!(
		run(&world, Setup::default()).await,
		AgentState::Held(HoldReason::UnknownQuote)
	);
}

#[tokio::test]
async fn invariant_3_a_trade_whose_rate_differs_from_its_offer_is_held() {
	let world = world();
	world
		.platform
		.edit(|t| t.rate = serde_json::from_value(json!("1600.00")).unwrap());
	assert_eq!(
		run(&world, Setup::default()).await,
		AgentState::Held(HoldReason::AmountMismatch)
	);
}

#[tokio::test]
async fn invariant_3_first_seen_terms_win_over_a_rewritten_quote() {
	let world = world();
	let trade = world
		.platform
		.trades
		.lock()
		.unwrap()
		.remove(TRADE_ID)
		.unwrap();
	let dir = tempfile::tempdir().unwrap();
	let agent = agent(&world, dir.path(), Setup::default());
	agent.tick().await.unwrap(); // records the quote as the merchant published it

	// The same quote id comes back with a richer rate, and a trade on it.
	let richer: merchant_agent_core::aggregator::types::Rate =
		serde_json::from_value(json!("1600.00")).unwrap();
	world.platform.quotes.lock().unwrap()[0].rate = richer.clone();
	let mut trade = trade;
	trade.rate = richer;
	world
		.platform
		.trades
		.lock()
		.unwrap()
		.insert(TRADE_ID.into(), trade);
	assert_eq!(
		settle(&agent).await,
		AgentState::Held(HoldReason::AmountMismatch)
	);
	assert!(world.bank.inner.payments().is_empty());
}

#[tokio::test]
async fn pricing_waits_while_the_quote_book_cannot_be_read() {
	let world = world();
	*world.platform.quotes_down.lock().unwrap() = true;
	let dir = tempfile::tempdir().unwrap();
	let agent = agent(&world, dir.path(), Setup::default());
	agent.tick().await.unwrap();
	assert_eq!(
		st(&agent),
		AgentState::LockVerified,
		"waits, neither holds nor pays"
	);
	*world.platform.quotes_down.lock().unwrap() = false;
	assert!(reported(&settle(&agent).await));
}

#[test]
fn invariant_3_the_rate_band_boundary() {
	let terms = |rate: &str| QuoteTerms {
		quote_id: "q".into(),
		chain: "eip155:84532".into(),
		asset: "USDC".into(),
		asset_decimals: 6,
		fiat_currency: "NGN".into(),
		rail: "ng.nip".into(),
		rate: rate.into(),
		fee_fixed: Minor(0),
		fee_bps: 0,
		min_fiat: Minor(0),
		max_fiat: Minor(u128::MAX),
		reference_rate: Some("1500.00".into()),
	};
	// 100 bps of 1500 is 15.00.
	assert!(check_rate(&terms("1514.99"), 100).is_ok(), "just under");
	assert!(check_rate(&terms("1515"), 100).is_ok(), "exactly at");
	assert!(check_rate(&terms("1515.01"), 100).is_err(), "just over");
	assert!(
		check_rate(&terms("1484.99"), 100).is_err(),
		"below the band too"
	);
	let no_reference = QuoteTerms {
		reference_rate: None,
		..terms("9999")
	};
	assert!(
		check_rate(&no_reference, 100).is_ok(),
		"no reference: no check"
	);
}

#[tokio::test]
async fn invariant_3_an_offer_far_from_the_reference_rate_is_held() {
	// 1520.50 is 136.7 bps above 1500.00.
	for (max_bps, paid) in [(137, true), (136, false)] {
		let world = world();
		let mut setup = Setup {
			max_rate_deviation_bps: max_bps,
			..Default::default()
		};
		setup
			.cfg
			.reference_rates
			.insert("ng.nip".into(), "1500.00".into());
		let end = run(&world, setup).await;
		assert_eq!(reported(&end), paid, "{max_bps}: {end:?}");
		if !paid {
			assert_eq!(end, AgentState::Held(HoldReason::RateOutOfBand));
		}
	}
}

// ── limits (invariant 5) ─────────────────────────────────────────────────

#[tokio::test]
async fn invariant_5_the_per_trade_limit_boundary() {
	for (limit, paid) in [(NET - 1, false), (NET, true), (NET + 1, true)] {
		let world = world();
		let mut setup = Setup::default();
		setup.cfg.limits.as_mut().unwrap().per_trade = Minor(limit);
		let end = run(&world, setup).await;
		assert_eq!(reported(&end), paid, "{limit}: {end:?}");
		if !paid {
			assert_eq!(end, AgentState::Held(HoldReason::OverTradeLimit));
		}
	}
}

/// A store with `n` trades at ChecksPassed, ready to record intents.
fn store_with_trades(
	dir: &std::path::Path,
	n: usize,
) -> (Arc<merchant_agent_core::store::Store>, Vec<String>) {
	let store = Arc::new(merchant_agent_core::store::Store::open(dir, "limits").unwrap());
	// A day-long lease: these tests move the clock by hours.
	store
		.acquire_lease(std::time::Duration::from_secs(86_400), start_time())
		.unwrap();
	let mut ids = Vec::new();
	for i in 0..n {
		let id = format!("ft-limit-{i}");
		store
			.insert_detected(
				&NewTrade {
					trade_id: id.clone(),
					chain: "eip155:84532".into(),
					quote_id: "fq".into(),
					asset_minor: Minor(1),
					fiat_minor: Minor(NET),
					fee_minor: Minor(0),
					rate: "1".into(),
					user_address: "0x2222222222222222222222222222222222222222".into(),
					payee_commitment: "0x00".into(),
					deadline_at: start_time() + chrono::Duration::minutes(15),
					platform_state: TradeState::AwaitingFiat,
				},
				start_time(),
			)
			.unwrap();
		let r = store.get(&id).unwrap().unwrap();
		let r = store
			.transition(
				&r,
				&AgentState::LockVerified,
				&[],
				&Patch::default(),
				start_time(),
			)
			.unwrap();
		store
			.transition(
				&r,
				&AgentState::ChecksPassed,
				&[],
				&Patch::default(),
				start_time(),
			)
			.unwrap();
		ids.push(id);
	}
	(store, ids)
}

/// Record an intent for `id` under `limits`, as the engine does.
fn try_intent(
	store: &merchant_agent_core::store::Store,
	id: &str,
	tag: &str,
	l: &Limits,
	now: chrono::DateTime<chrono::Utc>,
) -> Result<(), HoldReason> {
	let rec = store.get(id).unwrap().unwrap();
	let intent = Intent {
		amount: rec.fiat_minor,
		payee_tag: tag.into(),
		day: day_of(now),
	};
	store
		.record_intent(&rec, &intent, now, |tx| {
			limits::check(tx, l, &[], &intent, now)
		})
		.unwrap()
		.map(|_| ())
}

#[test]
fn invariant_5_the_daily_limit_boundary() {
	for (per_day, second_paid) in [(2 * NET - 1, false), (2 * NET, true), (2 * NET + 1, true)] {
		let dir = tempfile::tempdir().unwrap();
		let (store, ids) = store_with_trades(dir.path(), 2);
		let l = Limits {
			per_day: Minor(per_day),
			..generous_limits()
		};
		assert_eq!(try_intent(&store, &ids[0], "a", &l, start_time()), Ok(()));
		let second = try_intent(&store, &ids[1], "b", &l, start_time());
		assert_eq!(second.is_ok(), second_paid, "{per_day}: {second:?}");
		if !second_paid {
			assert_eq!(second, Err(HoldReason::OverDailyLimit));
		}
	}
}

#[test]
fn invariant_5_the_per_payee_limit_boundary_and_other_payees_are_unaffected() {
	for (per_payee, second_paid) in [(2 * NET - 1, false), (2 * NET, true), (2 * NET + 1, true)] {
		let dir = tempfile::tempdir().unwrap();
		let (store, ids) = store_with_trades(dir.path(), 3);
		let l = Limits {
			per_payee_per_day: Minor(per_payee),
			..generous_limits()
		};
		assert_eq!(
			try_intent(&store, &ids[0], "same", &l, start_time()),
			Ok(())
		);
		let second = try_intent(&store, &ids[1], "same", &l, start_time());
		assert_eq!(second.is_ok(), second_paid, "{per_payee}");
		if !second_paid {
			assert_eq!(second, Err(HoldReason::OverPayeeLimit));
			assert_eq!(
				try_intent(&store, &ids[2], "someone-else", &l, start_time()),
				Ok(())
			);
		}
	}
}

#[test]
fn invariant_5_the_velocity_limit_boundary() {
	let dir = tempfile::tempdir().unwrap();
	let (store, ids) = store_with_trades(dir.path(), 4);
	let l = Limits {
		velocity_max_payouts: 2,
		velocity_window_secs: 3600,
		..generous_limits()
	};
	assert_eq!(try_intent(&store, &ids[0], "a", &l, start_time()), Ok(()));
	assert_eq!(
		try_intent(&store, &ids[1], "b", &l, start_time()),
		Ok(()),
		"exactly at"
	);
	assert_eq!(
		try_intent(&store, &ids[2], "c", &l, start_time()),
		Err(HoldReason::OverVelocity),
		"just over"
	);
	let later = start_time() + chrono::Duration::seconds(3601);
	assert_eq!(
		try_intent(&store, &ids[2], "c", &l, later),
		Ok(()),
		"the window has moved on"
	);
}

#[test]
fn invariant_5_cancelled_payouts_do_not_count_and_the_day_rolls_over_in_lagos() {
	let dir = tempfile::tempdir().unwrap();
	let (store, ids) = store_with_trades(dir.path(), 3);
	let l = Limits {
		per_day: Minor(NET),
		..generous_limits()
	};
	assert_eq!(try_intent(&store, &ids[0], "a", &l, start_time()), Ok(()));
	assert_eq!(
		try_intent(&store, &ids[1], "b", &l, start_time()),
		Err(HoldReason::OverDailyLimit)
	);

	// The first is refused by the bank: its reservation is given back.
	let r = store.get(&ids[0]).unwrap().unwrap();
	let r = store
		.transition(
			&r,
			&AgentState::Paying,
			&[],
			&Patch::default(),
			start_time(),
		)
		.unwrap();
	store
		.transition(
			&r,
			&AgentState::Held(HoldReason::BankRejected),
			&[merchant_agent_core::machine::Effect::ReleaseReservation],
			&Patch::default(),
			start_time(),
		)
		.unwrap();
	assert_eq!(try_intent(&store, &ids[1], "b", &l, start_time()), Ok(()));

	// 23:00 UTC is midnight in Lagos: a new day.
	let tomorrow: chrono::DateTime<chrono::Utc> = "2026-11-13T23:00:00Z".parse().unwrap();
	assert_eq!(try_intent(&store, &ids[2], "c", &l, tomorrow), Ok(()));
}

#[test]
fn invariant_5_concurrent_intents_never_slip_under_the_daily_limit_together() {
	let dir = tempfile::tempdir().unwrap();
	let (store, ids) = store_with_trades(dir.path(), 12);
	let l = Limits {
		per_day: Minor(3 * NET),
		..generous_limits()
	};
	let handles: Vec<_> = ids
		.into_iter()
		.map(|id| {
			let (store, l) = (store.clone(), l.clone());
			std::thread::spawn(move || try_intent(&store, &id, &id, &l, start_time()))
		})
		.collect();
	let results: Vec<_> = handles.into_iter().map(|h| h.join().unwrap()).collect();
	let ok = results.iter().filter(|r| r.is_ok()).count();
	assert_eq!(ok, 3, "exactly what fits: {results:?}");
	assert!(results
		.iter()
		.all(|r| r.is_ok() || *r == Err(HoldReason::OverDailyLimit)));
	let total: u128 = store
		.payouts()
		.unwrap()
		.iter()
		.map(|p| p.amount.get())
		.sum();
	assert_eq!(total, 3 * NET);
}

#[tokio::test]
async fn invariant_5_an_approved_limit_hold_pays_once_and_only_that_limit_is_waived() {
	let world = world();
	let dir = tempfile::tempdir().unwrap();
	let mut setup = Setup::default();
	setup.cfg.limits.as_mut().unwrap().per_trade = Minor(NET - 1);
	let agent = agent(&world, dir.path(), setup);
	assert_eq!(
		settle(&agent).await,
		AgentState::Held(HoldReason::OverTradeLimit)
	);
	agent
		.person(TRADE_ID, PersonAction::Approve, "ops")
		.await
		.unwrap();
	assert!(reported(&settle(&agent).await));
	assert_eq!(
		agent.store().get(TRADE_ID).unwrap().unwrap().overrides,
		vec![HoldReason::OverTradeLimit]
	);
	assert_eq!(world.bank.inner.settled_payments().len(), 1);
}

// ── the deadline (invariant 6) ───────────────────────────────────────────

#[tokio::test]
async fn invariant_6_the_deadline_margin_boundary() {
	// The trade's deadline is 15 minutes out; the margin is 180 s.
	for (left, paid) in [(179, false), (180, true), (181, true)] {
		let world = world();
		world
			.clock
			.advance(std::time::Duration::from_secs(15 * 60 - left));
		let end = run(&world, Setup::default()).await;
		assert_eq!(reported(&end), paid, "{left}s left: {end:?}");
		if !paid {
			assert_eq!(end, AgentState::Held(HoldReason::DeadlineTooClose));
		}
	}
}

// ── screening ────────────────────────────────────────────────────────────

fn fixture_tag() -> String {
	let id = PayeeIdentity::new("ng.nip", "058", ACCOUNT).unwrap();
	payee_tag(&checks().pepper, &id)
}

#[tokio::test]
async fn a_payee_on_the_deny_list_is_held() {
	let world = world();
	let end = run(
		&world,
		Setup {
			deny: format!("# fraud\n{}\n", fixture_tag()),
			..Default::default()
		},
	)
	.await;
	assert_eq!(end, AgentState::Held(HoldReason::Screening));
	assert!(world.bank.inner.payments().is_empty());

	let world = self::world();
	let other = "ab".repeat(32);
	assert!(reported(
		&run(
			&world,
			Setup {
				deny: other,
				..Default::default()
			}
		)
		.await
	));
}

// ── the two-person rule ──────────────────────────────────────────────────

fn two_person(over: u128) -> Setup {
	let mut setup = Setup {
		deny: fixture_tag(),
		..Default::default()
	};
	setup.cfg.two_person = TwoPerson {
		enabled: true,
		approve_over: Some(Minor(over)),
	};
	setup
}

#[tokio::test]
async fn approving_a_large_held_payout_needs_two_different_people() {
	let world = world();
	let dir = tempfile::tempdir().unwrap();
	let agent = agent(&world, dir.path(), two_person(NET - 1));
	assert_eq!(
		settle(&agent).await,
		AgentState::Held(HoldReason::Screening)
	);

	assert_eq!(
		agent
			.person(TRADE_ID, PersonAction::Approve, "ada")
			.await
			.unwrap(),
		Decision::NeedsSecondApprover
	);
	assert_eq!(
		settle(&agent).await,
		AgentState::Held(HoldReason::Screening),
		"one approval is not enough"
	);
	assert!(
		agent
			.person(TRADE_ID, PersonAction::Approve, "ada")
			.await
			.is_err(),
		"not the same person twice"
	);

	let d = agent
		.person(TRADE_ID, PersonAction::Approve, "bayo")
		.await
		.unwrap();
	assert_eq!(d, Decision::Applied(AgentState::Detected));
	assert!(reported(&settle(&agent).await));
	assert_eq!(world.bank.inner.settled_payments().len(), 1);
}

#[tokio::test]
async fn the_two_person_threshold_boundary() {
	// Over the threshold needs two; at or under it, one approval acts.
	for (over, needs_two) in [(NET - 1, true), (NET, false), (NET + 1, false)] {
		let world = world();
		let dir = tempfile::tempdir().unwrap();
		let agent = agent(&world, dir.path(), two_person(over));
		settle(&agent).await;
		let d = agent
			.person(TRADE_ID, PersonAction::Approve, "ada")
			.await
			.unwrap();
		assert_eq!(
			d == Decision::NeedsSecondApprover,
			needs_two,
			"{over}: {d:?}"
		);
	}
}

// ── pause ────────────────────────────────────────────────────────────────

#[tokio::test]
async fn invariant_11_a_paused_agent_holds_new_payouts() {
	let world = world();
	let dir = tempfile::tempdir().unwrap();
	let agent = agent(&world, dir.path(), Setup::default());
	agent.pause("ops", false).await.unwrap();
	assert!(*world.platform.offers_paused.lock().unwrap());
	assert_eq!(settle(&agent).await, AgentState::Held(HoldReason::Paused));
	assert!(world.bank.inner.payments().is_empty());
}

#[tokio::test]
async fn a_closed_lock_is_held() {
	let world = world();
	*world.locks.0.lock().unwrap() =
		merchant_agent_core::machine::LockOutcome::Problem(HoldReason::LockClosed);
	assert_eq!(
		run(&world, Setup::default()).await,
		AgentState::Held(HoldReason::LockClosed)
	);
}

/// Every hold reason, and the test that produces it.
#[test]
fn every_hold_reason_has_a_test() {
	let covered: &[(HoldReason, &str)] = &[
		(
			HoldReason::LockMismatch,
			"engine::invariant_3_a_lock_problem_holds_the_trade",
		),
		(HoldReason::LockClosed, "policy::a_closed_lock_is_held"),
		(
			HoldReason::PayeeUnreadable,
			"engine::invariant_4_a_payee_that_does_not_match_the_commitment_is_held",
		),
		(
			HoldReason::AmountMismatch,
			"policy::invariant_3_one_kobo_more_or_less_than_the_recomputation_is_held",
		),
		(
			HoldReason::TradeChanged,
			"engine::a_trade_whose_amount_changed_after_detection_is_held",
		),
		(
			HoldReason::UnknownQuote,
			"policy::invariant_3_a_trade_on_an_offer_never_seen_is_held",
		),
		(
			HoldReason::RateOutOfBand,
			"policy::invariant_3_an_offer_far_from_the_reference_rate_is_held",
		),
		(
			HoldReason::OverTradeLimit,
			"policy::invariant_5_the_per_trade_limit_boundary",
		),
		(
			HoldReason::OverDailyLimit,
			"policy::invariant_5_the_daily_limit_boundary",
		),
		(
			HoldReason::OverPayeeLimit,
			"policy::invariant_5_the_per_payee_limit_boundary_and_other_payees_are_unaffected",
		),
		(
			HoldReason::OverVelocity,
			"policy::invariant_5_the_velocity_limit_boundary",
		),
		(
			HoldReason::DeadlineTooClose,
			"policy::invariant_6_the_deadline_margin_boundary",
		),
		(
			HoldReason::BankRejected,
			"rails::a_bank_rejection_releases_the_reservation_and_moves_no_money",
		),
		(
			HoldReason::NameMismatch,
			"rails::invariant_4_a_wrong_account_name_holds_the_trade",
		),
		(
			HoldReason::Screening,
			"policy::a_payee_on_the_deny_list_is_held",
		),
		(
			HoldReason::Paused,
			"policy::invariant_11_a_paused_agent_holds_new_payouts",
		),
		(
			HoldReason::AccountNotFound,
			"rails::an_account_the_bank_does_not_know_is_held_and_cannot_be_approved",
		),
		(
			HoldReason::InsufficientFloat,
			"rails::insufficient_float_holds_the_trade_and_pauses_new_offers",
		),
		(
			HoldReason::VerifiedNameMismatch,
			"rails::an_account_in_another_name_than_the_verified_one_is_held",
		),
		(
			HoldReason::VerifiedNameUnavailable,
			"rails::a_required_but_unavailable_verified_name_is_held_and_cannot_be_approved",
		),
	];
	for r in HoldReason::ALL {
		assert!(covered.iter().any(|(c, _)| *c == r), "{r:?} has no test");
		assert!(
			r.message().ends_with('.') && r.message().len() > 20,
			"{r:?} needs a plain message"
		);
	}
	assert_eq!(covered.len(), HoldReason::ALL.len());
}

// ── quotes that can't be the merchant's are never priced on (audit fixes) ─

#[tokio::test]
async fn a_quote_whose_decimals_differ_from_the_pinned_usdc_is_never_recorded() {
	// A book whose decimals are 0 would make 1 micro-USDC price as 1 USDC.
	let world = world();
	world.platform.quotes.lock().unwrap()[0].asset_decimals = 0;
	assert_eq!(
		run(&world, Setup::default()).await,
		AgentState::Held(HoldReason::UnknownQuote)
	);
	assert!(world.bank.inner.payments().is_empty());
}

#[tokio::test]
async fn a_quote_on_a_rail_without_a_reference_rate_is_never_recorded() {
	// Once any reference rate is set, a rail without one can't skip the band.
	let world = world();
	let mut setup = Setup::default();
	setup
		.cfg
		.reference_rates
		.insert("ng.other".into(), "1520.50".into());
	assert_eq!(
		run(&world, setup).await,
		AgentState::Held(HoldReason::UnknownQuote)
	);
	assert!(world.bank.inner.payments().is_empty());
}

#[tokio::test]
async fn a_quote_for_another_operator_or_chain_is_never_recorded() {
	for edit in [
		(|q: &mut Quote| q.solver_id = "solver-2222222222222222222222222222222222222222".into())
			as fn(&mut Quote),
		|q: &mut Quote| q.chain = "eip155:1".into(),
		|q: &mut Quote| q.asset = "DAI".into(),
	] {
		let world = world();
		edit(&mut world.platform.quotes.lock().unwrap()[0]);
		assert_eq!(
			run(&world, Setup::default()).await,
			AgentState::Held(HoldReason::UnknownQuote)
		);
	}
}
