//! Payout rails against the full engine and state machine.
//!
//! Every fault the simulated bank can produce is run end to end. In every
//! run the bank's books must balance and no trade may be paid twice.
//! Then: the name check (invariant 4), the float check and the automatic
//! pause, and the manual rail.

mod common;
use common::*;

use merchant_agent_core::engine::Revealed;
use merchant_agent_core::machine::PersonAction;
use merchant_agent_core::money::Minor;
use merchant_agent_core::rail::manual::ManualRail;
use merchant_agent_core::rail::simulated::Faults;

const AMOUNT: u128 = 15_081_152;

fn faulty(faults: Faults) -> World {
	let world = World::new();
	world.bank.inner.set_faults(faults);
	world
}

/// Run until the trade stops moving (or reaches `done`), then check the
/// books.
async fn run(world: &World, done: impl Fn(&AgentState) -> bool) -> AgentState {
	let dir = tempfile::tempdir().unwrap();
	let agent = world.agent(dir.path()).unwrap();
	let end = tick_until(&agent, done).await;
	assert_books(world);
	end
}

fn assert_books(world: &World) {
	assert!(
		world.bank.inner.ledger_balances(),
		"the bank's books must balance"
	);
	let mut keys = std::collections::HashSet::new();
	for p in world.bank.inner.payments() {
		assert!(
			keys.insert(p.idempotency_key.clone()),
			"one payment per key"
		);
	}
	assert!(
		world.bank.inner.settled_payments().len() <= 1,
		"never paid twice"
	);
}

// ── every fault, end to end ──────────────────────────────────────────────

#[tokio::test]
async fn a_lost_send_is_found_missing_and_sent_again_under_the_same_key() {
	let world = faulty(Faults {
		lost_sends: 1,
		..Default::default()
	});
	assert!(reported(&run(&world, reported).await));
	assert_eq!(world.bank.inner.settled_payments().len(), 1);
	assert_eq!(
		world.bank.inner.sends_seen(),
		2,
		"lost once, then delivered"
	);
}

#[tokio::test]
async fn invariant_2_a_5xx_after_paying_is_asked_about_not_resent() {
	let world = faulty(Faults {
		errors_after_paying: 1,
		..Default::default()
	});
	assert!(reported(&run(&world, reported).await));
	assert_eq!(
		world.bank.inner.sends_seen(),
		1,
		"asked by key, never sent again"
	);
	assert_eq!(world.bank.inner.settled_payments().len(), 1);
}

#[tokio::test]
async fn a_duplicated_delivery_pays_once() {
	let world = faulty(Faults {
		duplicate_delivery: true,
		..Default::default()
	});
	assert!(reported(&run(&world, reported).await));
	assert_eq!(world.bank.inner.sends_seen(), 2);
	assert_eq!(world.bank.inner.payments().len(), 1);
}

#[tokio::test]
async fn a_slow_pending_transfer_is_waited_for() {
	let world = faulty(Faults {
		pending_polls: 4,
		..Default::default()
	});
	assert!(reported(&run(&world, reported).await));
	assert_eq!(world.bank.inner.settled_payments().len(), 1);
}

#[tokio::test]
async fn a_brief_status_outage_is_ridden_out() {
	let world = faulty(Faults {
		status_outages: 2,
		..Default::default()
	});
	assert!(reported(&run(&world, reported).await));
}

#[tokio::test]
async fn invariant_2_an_unknown_status_goes_to_a_person_and_nothing_is_resent() {
	let world = faulty(Faults {
		status_unknown: true,
		..Default::default()
	});
	let end = run(&world, |s| matches!(s, AgentState::NeedsPerson(_))).await;
	assert_eq!(end, AgentState::NeedsPerson(PersonReason::BankUnknown));
	assert_eq!(world.bank.inner.sends_seen(), 1, "nothing is sent again");
}

#[tokio::test]
async fn a_reversed_transfer_goes_to_a_person_and_the_money_is_back() {
	let world = faulty(Faults {
		reverse_next: true,
		..Default::default()
	});
	let end = run(&world, |s| matches!(s, AgentState::NeedsPerson(_))).await;
	assert_eq!(end, AgentState::NeedsPerson(PersonReason::BankFailed));
	assert!(world.bank.inner.settled_payments().is_empty());
}

#[tokio::test]
async fn an_account_lookup_outage_is_retried() {
	let world = faulty(Faults {
		lookup_outages: 2,
		..Default::default()
	});
	assert!(reported(&run(&world, reported).await));
}

#[tokio::test]
async fn invariant_4_a_wrong_account_name_holds_the_trade() {
	let world = World::new();
	world
		.bank
		.inner
		.add_account("058", ACCOUNT, "SOMEONE ELSE ENTIRELY");
	let end = run(&world, |s| matches!(s, AgentState::Held(_))).await;
	assert_eq!(end, AgentState::Held(HoldReason::NameMismatch));
	assert!(world.bank.inner.payments().is_empty());
}

#[tokio::test]
async fn invariant_4_a_name_mismatch_approved_by_a_person_is_paid() {
	let world = World::new();
	world.bank.inner.add_account("058", ACCOUNT, "PAYEE T");
	let dir = tempfile::tempdir().unwrap();
	let agent = world.agent(dir.path()).unwrap();
	tick_until(&agent, |s| matches!(s, AgentState::Held(_))).await;
	agent
		.person(TRADE_ID, PersonAction::Approve, "ops")
		.await
		.unwrap();
	assert!(reported(&tick_until(&agent, reported).await));
	assert_eq!(world.bank.inner.settled_payments().len(), 1);
	assert_books(&world);
}

#[tokio::test]
async fn an_account_the_bank_does_not_know_is_held_and_cannot_be_approved() {
	// A bank with no record of the account.
	let world = World {
		bank: FlakyBank::default(),
		..World::new()
	};
	let end = run(&world, |s| matches!(s, AgentState::Held(_))).await;
	assert_eq!(end, AgentState::Held(HoldReason::AccountNotFound));
	assert!(!HoldReason::AccountNotFound.approvable());
}

// ── float and the automatic pause ────────────────────────────────────────

#[tokio::test]
async fn insufficient_float_holds_the_trade_and_pauses_new_offers() {
	let world = World::new();
	world.bank.inner.set_balance(Minor(AMOUNT - 1));
	let dir = tempfile::tempdir().unwrap();
	let agent = world.agent(dir.path()).unwrap();
	let end = tick_until(&agent, |s| matches!(s, AgentState::Held(_))).await;
	assert_eq!(end, AgentState::Held(HoldReason::InsufficientFloat));
	assert!(world.bank.inner.payments().is_empty());
	assert!(
		*world.platform.offers_paused.lock().unwrap(),
		"the offers are withdrawn"
	);
	assert!(agent.store().is_paused().unwrap(), "new payouts are paused");
	let (_, by, auto, book) = agent.store().control().unwrap();
	assert_eq!(
		(by.as_deref(), auto, book.as_deref()),
		(Some("agent"), true, Some("[]"))
	);
	assert!(agent
		.alerts()
		.iter()
		.any(|a| a.urgent && a.state == AgentState::Held(HoldReason::InsufficientFloat)));
	assert_books(&world);
}

#[tokio::test]
async fn the_minimum_float_is_kept_back() {
	let world = World::new();
	world.bank.inner.set_balance(Minor(AMOUNT + 999));
	let dir = tempfile::tempdir().unwrap();
	let mut cfg = World::cfg();
	cfg.min_float = Minor(1_000);
	let store =
		std::sync::Arc::new(merchant_agent_core::store::Store::open(dir.path(), "float").unwrap());
	let agent = Engine::start(
		store,
		world.platform.clone(),
		world.locks.clone(),
		checks(),
		world.bank.clone(),
		world.clock.clone(),
		cfg,
	)
	.unwrap();
	let end = tick_until(&agent, |s| matches!(s, AgentState::Held(_))).await;
	assert_eq!(end, AgentState::Held(HoldReason::InsufficientFloat));

	// With the minimum covered, it pays.
	world.bank.inner.set_balance(Minor(AMOUNT + 1_000));
	*world.platform.offers_paused.lock().unwrap() = false;
	agent
		.store()
		.set_paused(false, "ops", false, None, world.clock.now())
		.unwrap();
	agent
		.person(TRADE_ID, PersonAction::Approve, "ops")
		.await
		.unwrap();
	assert!(reported(&tick_until(&agent, reported).await));
	assert_eq!(world.bank.inner.current_balance(), Minor(1_000));
	assert_books(&world);
}

#[tokio::test]
async fn a_bank_rejection_releases_the_reservation_and_moves_no_money() {
	let world = World::new();
	let bank = world.bank.inner.clone();
	// The balance drains between the float check and the send.
	let hook: CrashHook = std::sync::Arc::new(move |p: &CrashPoint| {
		if *p == CrashPoint::AfterWrite("payout_intent_recorded") {
			bank.set_balance(Minor(0));
		}
		false
	});
	let dir = tempfile::tempdir().unwrap();
	let agent = world.agent(dir.path()).unwrap().with_crash_hook(hook);
	let end = tick_until(&agent, |s| matches!(s, AgentState::Held(_))).await;
	assert_eq!(end, AgentState::Held(HoldReason::BankRejected));
	assert!(world.bank.inner.payments().is_empty());
	let payouts = agent.store().payouts().unwrap();
	assert_eq!(
		payouts[0].status, "cancelled",
		"the reservation is given back"
	);
	let alert = agent
		.alerts()
		.into_iter()
		.find(|a| a.state == AgentState::Held(HoldReason::BankRejected))
		.unwrap();
	assert_eq!(
		alert.detail.as_deref(),
		Some("insufficient funds in the payout account")
	);
	assert_books(&world);
}

// ── every fault mixed, many times ────────────────────────────────────────

#[tokio::test]
async fn invariant_1_any_mix_of_faults_never_pays_twice_and_the_books_balance() {
	// A fixed pseudo-random walk over fault combinations: reproducible.
	let mut seed: u64 = 0x5eed_1234_abcd_0001;
	let mut next = move || {
		seed ^= seed << 13;
		seed ^= seed >> 7;
		seed ^= seed << 17;
		seed
	};
	let (mut paid, mut to_a_person) = (0, 0);
	for round in 0..150 {
		let r = next();
		let faults = Faults {
			lost_sends: (r % 3) as u32,
			errors_after_paying: ((r >> 2) % 3) as u32,
			duplicate_delivery: (r >> 4) % 2 == 0,
			pending_polls: ((r >> 5) % 4) as u32,
			status_outages: ((r >> 7) % 3) as u32,
			status_unknown: (r >> 9) % 7 == 0,
			reverse_next: (r >> 12) % 6 == 0,
			lookup_outages: ((r >> 15) % 2) as u32,
			// Hanging is a crash drill; the crash tests cover it.
			hang_after_paying: false,
		};
		let world = faulty(faults.clone());
		let dir = tempfile::tempdir().unwrap();
		let agent = world.agent(dir.path()).unwrap();
		for _ in 0..25 {
			agent.tick().await.unwrap();
			let s = state(&agent);
			if reported(&s) || matches!(s, AgentState::NeedsPerson(_) | AgentState::Held(_)) {
				break;
			}
		}
		let end = state(&agent);
		assert_books(&world);
		let settled = world.bank.inner.settled_payments().len();
		match &end {
			s if reported(s) => {
				assert_eq!(settled, 1, "round {round} {faults:?}");
				paid += 1;
			}
			AgentState::NeedsPerson(PersonReason::BankUnknown) => {
				assert!(settled <= 1);
				to_a_person += 1;
			}
			AgentState::NeedsPerson(PersonReason::BankFailed) => {
				assert_eq!(settled, 0, "round {round}");
				to_a_person += 1;
			}
			other => panic!("round {round} {faults:?}: ended {other:?}"),
		}
	}
	assert!(
		paid > 50 && to_a_person > 10,
		"the walk covered both outcomes: {paid} paid, {to_a_person} to a person"
	);
}

// ── the manual rail ──────────────────────────────────────────────────────

type ManualAgent =
	Engine<FakePlatform, FakeLocks, PayeeChecks, ManualRail<ManualClock>, ManualClock>;

fn manual_agent(world: &World, dir: &std::path::Path) -> ManualAgent {
	let store =
		std::sync::Arc::new(merchant_agent_core::store::Store::open(dir, "manual").unwrap());
	Engine::start(
		store.clone(),
		world.platform.clone(),
		world.locks.clone(),
		checks(),
		ManualRail::new(store, world.clock.clone()),
		world.clock.clone(),
		World::cfg(),
	)
	.unwrap()
}

fn manual_state(agent: &ManualAgent) -> AgentState {
	agent.store().get(TRADE_ID).unwrap().unwrap().state
}

#[tokio::test]
async fn a_manual_payout_waits_for_a_person_then_completes() {
	let world = World::new();
	let dir = tempfile::tempdir().unwrap();
	let agent = manual_agent(&world, dir.path());
	agent.tick().await.unwrap();
	assert_eq!(manual_state(&agent), AgentState::Paying);
	let alert = agent.alerts().into_iter().last().unwrap();
	assert!(alert.detail.unwrap().contains("Waiting for a person"));
	assert_eq!(agent.store().open_manual_tasks().unwrap().len(), 1);

	// Nothing happens until a person acts.
	agent.tick().await.unwrap();
	assert_eq!(manual_state(&agent), AgentState::Paying);

	// They reveal the details (counted), pay, and enter the reference.
	let shown: Revealed = agent.reveal(TRADE_ID, "ops@merchant").await.unwrap();
	assert_eq!(shown.opened.identity.account.expose(), ACCOUNT);
	assert_eq!(shown.opened.name.expose(), NAME);
	assert_eq!(shown.amount, Minor(AMOUNT));
	drop(shown);
	let task = agent.store().open_task_for(TRADE_ID).unwrap().unwrap();
	assert_eq!(
		(task.reveals, task.last_revealed_by.as_deref()),
		(1, Some("ops@merchant"))
	);

	agent
		.manual_paid(TRADE_ID, "BANKAPP-778812", "ops@merchant")
		.unwrap();
	agent.tick().await.unwrap();
	assert_eq!(
		manual_state(&agent),
		AgentState::Reported {
			bank_confirmed: true
		}
	);
	assert_eq!(
		world.platform.get().payment_reference.as_deref(),
		Some("BANKAPP-778812")
	);

	// The task is closed: no more reveals.
	assert!(agent.reveal(TRADE_ID, "ops@merchant").await.is_err());
}

#[tokio::test]
async fn a_manual_payout_the_person_could_not_make_goes_to_a_person() {
	let world = World::new();
	let dir = tempfile::tempdir().unwrap();
	let agent = manual_agent(&world, dir.path());
	agent.tick().await.unwrap();
	agent.manual_failed(TRADE_ID, "ops@merchant").unwrap();
	agent.tick().await.unwrap();
	assert_eq!(
		manual_state(&agent),
		AgentState::NeedsPerson(PersonReason::BankFailed)
	);
}

#[tokio::test]
async fn manual_inputs_are_checked() {
	let world = World::new();
	let dir = tempfile::tempdir().unwrap();
	let agent = manual_agent(&world, dir.path());
	assert!(agent.reveal(TRADE_ID, "ops").await.is_err(), "no task yet");
	agent.tick().await.unwrap();
	assert!(
		agent.manual_paid(TRADE_ID, &"R".repeat(65), "ops").is_err(),
		"the aggregator's reference rule"
	);
	assert!(agent.manual_paid("ft-other", "REF", "ops").is_err());
}

#[test]
fn revealed_details_cannot_be_printed() {
	static_assertions::assert_not_impl_any!(Revealed: std::fmt::Debug, std::fmt::Display, Clone);
}

// ── the sandbox bank on disk ─────────────────────────────────────────────

mod sandbox_bank {
	use super::*;
	use merchant_agent_core::money::Minor;
	use merchant_agent_core::pii::{AccountNumber, HolderName};
	use merchant_agent_core::rail::simulated::{Faults, DRILL_FILE, LEDGER_FILE};

	async fn pay(bank: &SimulatedRail, key: &str) -> Result<SendOutcome, RailError> {
		let account = AccountNumber::new(ACCOUNT.into());
		let name = HolderName::new(NAME.into());
		bank.send(PayoutRequest {
			idempotency_key: key,
			trade_id: TRADE_ID,
			amount: Minor(1_000),
			bank_code: "058",
			account: &account,
			name: &name,
			narration: "RecoilPay test",
		})
		.await
	}

	#[tokio::test]
	async fn the_ledger_outlives_the_process_so_a_restart_never_pays_twice() {
		let dir = tempfile::tempdir().unwrap();
		let bank = SimulatedRail::sandbox_in(dir.path(), Minor(1_000_000)).unwrap();
		pay(&bank, "rpma-1").await.unwrap();
		drop(bank);

		// A new process: the payment is found by key, not made again.
		let bank = SimulatedRail::sandbox_in(dir.path(), Minor(1_000_000)).unwrap();
		assert!(matches!(
			bank.status("rpma-1").await.unwrap(),
			PayoutStatus::Succeeded { .. }
		));
		pay(&bank, "rpma-1").await.unwrap();
		assert_eq!(bank.payments().len(), 1);
		assert_eq!(bank.current_balance(), Minor(999_000));
		assert!(bank.ledger_balances());

		// Invariant 7: the ledger on disk has masked accounts only.
		let text = std::fs::read_to_string(dir.path().join(LEDGER_FILE)).unwrap();
		assert!(!text.contains(ACCOUNT) && !text.contains(NAME), "{text}");
	}

	#[tokio::test]
	async fn a_drill_file_is_applied_once_and_removed() {
		let dir = tempfile::tempdir().unwrap();
		let bank = SimulatedRail::sandbox_in(dir.path(), Minor(1_000_000)).unwrap();
		std::fs::write(dir.path().join(DRILL_FILE), r#"{"status_unknown": true}"#).unwrap();
		pay(&bank, "rpma-1").await.unwrap();
		assert!(!dir.path().join(DRILL_FILE).exists());
		assert_eq!(bank.status("rpma-1").await.unwrap(), PayoutStatus::Unknown);

		// It survives a restart until another drill clears it.
		let bank = SimulatedRail::sandbox_in(dir.path(), Minor(1_000_000)).unwrap();
		assert_eq!(bank.status("rpma-1").await.unwrap(), PayoutStatus::Unknown);
		std::fs::write(dir.path().join(DRILL_FILE), "{}").unwrap();
		assert!(matches!(
			bank.status("rpma-1").await.unwrap(),
			PayoutStatus::Succeeded { .. }
		));

		// A drill picked up by a call that changes nothing else (a balance
		// read) is still saved: a restart must not bring the old faults back.
		std::fs::write(dir.path().join(DRILL_FILE), r#"{"status_unknown": true}"#).unwrap();
		bank.balance().await.unwrap();
		std::fs::write(dir.path().join(DRILL_FILE), "{}").unwrap();
		bank.balance().await.unwrap();
		let bank = SimulatedRail::sandbox_in(dir.path(), Minor(1_000_000)).unwrap();
		assert!(
			!bank.faults().status_unknown,
			"a cleared drill came back after a restart"
		);

		// A drill that is not valid is ignored, not half-applied.
		std::fs::write(dir.path().join(DRILL_FILE), r#"{"no_such_fault": 1}"#).unwrap();
		assert!(matches!(
			bank.status("rpma-1").await.unwrap(),
			PayoutStatus::Succeeded { .. }
		));
		assert!(bank.faults().lost_sends == 0 && !bank.faults().status_unknown);
	}

	#[tokio::test]
	async fn a_hang_after_paying_has_already_saved_the_payment() {
		let dir = tempfile::tempdir().unwrap();
		let bank = SimulatedRail::sandbox_in(dir.path(), Minor(1_000_000)).unwrap();
		bank.set_faults(Faults {
			hang_after_paying: true,
			..Default::default()
		});
		let hung = tokio::time::timeout(Duration::from_millis(200), pay(&bank, "rpma-1")).await;
		assert!(hung.is_err(), "the send should never answer");

		// The process "dies"; the next one finds the payment and doesn't hang.
		let bank = SimulatedRail::sandbox_in(dir.path(), Minor(1_000_000)).unwrap();
		assert_eq!(bank.payments().len(), 1);
		pay(&bank, "rpma-1").await.unwrap();
		assert_eq!(bank.payments().len(), 1);
	}
}
