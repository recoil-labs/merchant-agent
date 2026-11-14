//! The engine end to end, with the money rules under crashes.
//!
//! A fake platform (RecoilPay), a fake lock check, the real payee checks
//! (opening the shared sealed vector), a manual clock, and the simulated
//! bank. The bank and the platform outlive the agent, like the real ones,
//! so a test can kill the agent and start another on the same database.
//!
//! The crash tests stop the agent at **every** step boundary in turn, start
//! a new one, and require exactly one payment and a completed trade.

mod common;
use common::*;

// ── the happy path ───────────────────────────────────────────────────────

#[tokio::test]
async fn a_cash_out_is_paid_once_reported_and_settled() {
	let dir = tempfile::tempdir().unwrap();
	let world = World::new();
	let agent = world.agent(dir.path()).unwrap();

	assert!(reported(&tick_until(&agent, reported).await));
	let payments = world.bank.inner.payments();
	assert_eq!(payments.len(), 1);
	assert_eq!(payments[0].amount.get(), 15_081_152, "the trade's naira");
	assert_eq!(payments[0].bank_code, "058");
	assert_eq!(payments[0].account_masked, "******0000");
	assert_eq!(payments[0].idempotency_key, format!("rpma-{TRADE_ID}"));
	assert_eq!(
		world.platform.get().payment_reference.as_deref(),
		Some(payments[0].reference.as_str())
	);

	// The customer confirms; the trade settles.
	world
		.platform
		.edit(|t| t.state = TradeState::SettledToFiatPayer);
	assert_eq!(
		tick_until(&agent, |s| *s == AgentState::Settled).await,
		AgentState::Settled
	);
	assert_eq!(world.payments(), 1);
}

// ── crashes at every step boundary ───────────────────────────────────────

/// Every crash point a full payout passes, in order.
async fn crash_points() -> Vec<CrashPoint> {
	let dir = tempfile::tempdir().unwrap();
	let world = World::new();
	let seen = Arc::new(Mutex::new(Vec::new()));
	let log = seen.clone();
	let hook: CrashHook = Arc::new(move |p: &CrashPoint| {
		log.lock().unwrap().push(p.clone());
		false
	});
	let agent = world.agent(dir.path()).unwrap().with_crash_hook(hook);
	tick_until(&agent, reported).await;
	let points = seen.lock().unwrap().clone();
	points
}

#[tokio::test]
async fn invariant_1_a_crash_at_any_step_never_pays_twice() {
	let points = crash_points().await;
	assert!(points.len() >= 9, "{points:?}");
	assert!(points.contains(&CrashPoint::AfterBankSend));
	assert!(points.contains(&CrashPoint::AfterWrite("paying")));

	for (i, point) in points.iter().enumerate() {
		let dir = tempfile::tempdir().unwrap();
		let world = World::new();
		{
			let n = Arc::new(AtomicUsize::new(0));
			let hook: CrashHook =
				Arc::new(move |_p: &CrashPoint| n.fetch_add(1, Ordering::SeqCst) == i);
			let agent = world.agent(dir.path()).unwrap().with_crash_hook(hook);
			let mut crashed = false;
			for _ in 0..20 {
				match agent.tick().await {
					Err(EngineError::Crashed) => {
						crashed = true;
						break;
					}
					other => other.unwrap(),
				}
			}
			assert!(crashed, "point {i} ({point:?}) was never reached");
			// The agent dies here: dropped mid-flight, lease still held.
		}
		world.wait_out_the_lease();
		let agent = world.agent(dir.path()).unwrap();
		let end = tick_until(&agent, reported).await;
		assert!(reported(&end), "crash at {point:?}: ended {end:?}");
		assert_eq!(
			world.payments(),
			1,
			"crash at {point:?}: paid {} times",
			world.payments()
		);
		assert_eq!(
			world.platform.get().payment_reference.as_deref(),
			Some(world.bank.inner.payments()[0].reference.as_str()),
			"crash at {point:?}: RecoilPay holds the bank's reference"
		);
	}
}

#[test]
fn invariant_1_dropping_the_runtime_mid_payout_never_pays_twice() {
	let dir = tempfile::tempdir().unwrap();
	let world = World::new();
	*world.bank.hang_after_paying.lock().unwrap() = true;

	// The agent pays, then its process dies while waiting for the bank.
	let rt = tokio::runtime::Runtime::new().unwrap();
	let agent = world.agent(dir.path()).unwrap();
	rt.spawn(async move {
		let _ = agent.tick().await;
	});
	for _ in 0..200 {
		if world.payments() == 1 {
			break;
		}
		std::thread::sleep(Duration::from_millis(10));
	}
	assert_eq!(world.payments(), 1, "the bank made the payment");
	drop(rt);

	world.wait_out_the_lease();
	let rt = tokio::runtime::Runtime::new().unwrap();
	rt.block_on(async {
		let agent = world.agent(dir.path()).unwrap();
		assert!(reported(&tick_until(&agent, reported).await));
	});
	assert_eq!(world.payments(), 1);
}

#[tokio::test]
async fn invariant_2_a_send_that_fails_after_paying_is_asked_about_not_resent() {
	let dir = tempfile::tempdir().unwrap();
	let world = World::new();
	world
		.bank
		.inner
		.set_faults(merchant_agent_core::rail::simulated::Faults {
			errors_after_paying: 1,
			..Default::default()
		});
	let agent = world.agent(dir.path()).unwrap();
	assert!(reported(&tick_until(&agent, reported).await));
	assert_eq!(world.payments(), 1);
}

#[tokio::test]
async fn invariant_2_a_bank_that_cant_say_goes_to_a_person_and_nothing_is_resent() {
	let dir = tempfile::tempdir().unwrap();
	let world = World::new();
	world
		.bank
		.inner
		.set_faults(merchant_agent_core::rail::simulated::Faults {
			errors_after_paying: 1,
			..Default::default()
		});
	*world.bank.status_down.lock().unwrap() = true;
	let agent = world.agent(dir.path()).unwrap();
	let end = tick_until(&agent, |s| matches!(s, AgentState::NeedsPerson(_))).await;
	assert_eq!(end, AgentState::NeedsPerson(PersonReason::BankUnknown));
	assert_eq!(world.payments(), 1);

	// The bank comes back; the agent still leaves it to a person.
	*world.bank.status_down.lock().unwrap() = false;
	for _ in 0..5 {
		agent.tick().await.unwrap();
	}
	assert_eq!(
		state(&agent),
		AgentState::NeedsPerson(PersonReason::BankUnknown)
	);
	assert_eq!(world.payments(), 1);
	assert!(agent.alerts().iter().any(|a| a.urgent));
}

// ── one agent at a time ──────────────────────────────────────────────────

#[tokio::test]
async fn invariant_10_a_second_process_on_the_same_directory_is_refused() {
	let dir = tempfile::tempdir().unwrap();
	let world = World::new();
	let _first = world.agent(dir.path()).unwrap();
	let second = world.agent(dir.path());
	assert!(matches!(
		second,
		Err(EngineError::Store(StoreError::AnotherInstance))
	));
}

#[tokio::test]
async fn invariant_10_two_hosts_on_one_database_only_one_acts() {
	let dir = tempfile::tempdir().unwrap();
	let world = World::new();
	// File locks can't be trusted on a shared volume: only the lease.
	let a = world
		.agent_on(Store::open_shared(dir.path(), "host-a").unwrap())
		.unwrap();
	let b = world.agent_on(Store::open_shared(dir.path(), "host-b").unwrap());
	assert!(
		matches!(b, Err(EngineError::Store(StoreError::LeaseHeld { .. }))),
		"B must not start"
	);

	// A stalls past its lease; B takes over and pays.
	world.wait_out_the_lease();
	let b = world
		.agent_on(Store::open_shared(dir.path(), "host-b").unwrap())
		.unwrap();
	assert!(reported(&tick_until(&b, reported).await));

	// A wakes up: every write it tries is refused, so it can't act.
	let e = a.tick().await.unwrap_err();
	assert!(
		matches!(e, EngineError::Store(StoreError::LeaseLost)),
		"{e}"
	);
	assert_eq!(world.payments(), 1);
}

// ── stopping instead of guessing ─────────────────────────────────────────

#[tokio::test]
async fn someone_else_reporting_first_stops_the_agent() {
	let dir = tempfile::tempdir().unwrap();
	let world = World::new();
	*world.locks.0.lock().unwrap() = LockOutcome::Wait;
	let agent = world.agent(dir.path()).unwrap();
	agent.tick().await.unwrap();
	assert_eq!(state(&agent), AgentState::Detected);

	// The merchant's staff mark it paid in the portal.
	world.platform.edit(|t| {
		t.state = TradeState::AwaitingConfirmation;
		t.payment_reference = Some("PAID-BY-HAND".into());
	});
	*world.locks.0.lock().unwrap() = LockOutcome::Verified;
	agent.tick().await.unwrap();
	assert_eq!(
		state(&agent),
		AgentState::NeedsPerson(PersonReason::ReportedElsewhere)
	);
	assert_eq!(world.payments(), 0);
}

#[tokio::test]
async fn invariant_6_a_trade_too_close_to_its_deadline_is_held_not_paid() {
	let dir = tempfile::tempdir().unwrap();
	let world = World::new();
	world.clock.advance(Duration::from_secs(15 * 60 - 120)); // 2 minutes left, margin is 3
	let agent = world.agent(dir.path()).unwrap();
	agent.tick().await.unwrap();
	assert_eq!(
		state(&agent),
		AgentState::Held(HoldReason::DeadlineTooClose)
	);
	assert_eq!(world.payments(), 0);
	assert!(!agent.alerts().is_empty());

	world.clock.advance(Duration::from_secs(121));
	agent.tick().await.unwrap();
	assert_eq!(state(&agent), AgentState::Expired);
	assert_eq!(world.payments(), 0);
}

#[tokio::test]
async fn invariant_3_a_lock_problem_holds_the_trade() {
	let dir = tempfile::tempdir().unwrap();
	let world = World::new();
	*world.locks.0.lock().unwrap() = LockOutcome::Problem(HoldReason::LockMismatch);
	let agent = world.agent(dir.path()).unwrap();
	agent.tick().await.unwrap();
	assert_eq!(state(&agent), AgentState::Held(HoldReason::LockMismatch));
	assert_eq!(world.payments(), 0);
}

#[tokio::test]
async fn a_trade_whose_amount_changed_after_detection_is_held() {
	let dir = tempfile::tempdir().unwrap();
	let world = World::new();
	*world.locks.0.lock().unwrap() = LockOutcome::Wait;
	let agent = world.agent(dir.path()).unwrap();
	agent.tick().await.unwrap();
	world
		.platform
		.edit(|t| t.fiat_minor_units = merchant_agent_core::money::Minor(99_000_000));
	*world.locks.0.lock().unwrap() = LockOutcome::Verified;
	agent.tick().await.unwrap();
	assert_eq!(state(&agent), AgentState::Held(HoldReason::TradeChanged));
	assert_eq!(world.payments(), 0);
}

#[tokio::test]
async fn invariant_4_a_payee_that_does_not_match_the_commitment_is_held() {
	let dir = tempfile::tempdir().unwrap();
	let world = World::new();
	world
		.platform
		.edit(|t| t.payee_commitment = Some(format!("0x{}", "ab".repeat(32))));
	let agent = world.agent(dir.path()).unwrap();
	agent.tick().await.unwrap();
	assert_eq!(state(&agent), AgentState::Held(HoldReason::PayeeUnreadable));
	assert_eq!(world.payments(), 0);
}

#[tokio::test]
async fn a_refund_after_paying_goes_to_a_person() {
	let dir = tempfile::tempdir().unwrap();
	let world = World::new();
	let agent = world.agent(dir.path()).unwrap();
	tick_until(&agent, reported).await;
	world
		.platform
		.edit(|t| t.state = TradeState::RefundedToFunder);
	agent.tick().await.unwrap();
	assert_eq!(state(&agent), AgentState::Refunded);
	assert!(agent
		.alerts()
		.iter()
		.any(|a| a.urgent && a.state == AgentState::Refunded));
}

#[tokio::test]
async fn a_dispute_attaches_plain_ascii_evidence() {
	let dir = tempfile::tempdir().unwrap();
	let world = World::new();
	let agent = world.agent(dir.path()).unwrap();
	tick_until(&agent, reported).await;
	world.platform.edit(|t| t.state = TradeState::Disputed);
	agent.tick().await.unwrap();
	assert_eq!(state(&agent), AgentState::Disputed);
	let statements = world.platform.statements.lock().unwrap().clone();
	assert_eq!(statements.len(), 1);
	assert!(statements[0].is_ascii(), "{}", statements[0]);
	assert!(
		statements[0].contains("NGN 150,811.52") && statements[0].contains("******0000"),
		"{}",
		statements[0]
	);
}

#[tokio::test]
async fn a_pending_payout_is_reported_at_the_deadline_margin() {
	let dir = tempfile::tempdir().unwrap();
	let world = World::new();
	*world.bank.status_down.lock().unwrap() = true;
	let agent = world.agent(dir.path()).unwrap();
	agent.tick().await.unwrap();
	assert_eq!(state(&agent), AgentState::Paying);

	world.clock.advance(Duration::from_secs(15 * 60 - 60)); // 1 minute left, report margin is 2
	agent.tick().await.unwrap();
	assert_eq!(
		state(&agent),
		AgentState::Reported {
			bank_confirmed: false
		}
	);
	assert!(world.platform.get().payment_reference.is_some());

	*world.bank.status_down.lock().unwrap() = false;
	agent.tick().await.unwrap();
	assert_eq!(
		state(&agent),
		AgentState::Reported {
			bank_confirmed: true
		}
	);
	assert_eq!(world.payments(), 1);
}

// ── personal data ────────────────────────────────────────────────────────

#[test]
fn invariant_7_no_account_number_or_name_in_the_database_logs_or_alerts() {
	let dir = tempfile::tempdir().unwrap();
	let world = World::new();
	let logs = Arc::new(Mutex::new(Vec::<u8>::new()));
	let sink = logs.clone();
	let subscriber = tracing_subscriber::fmt()
		.json()
		.with_max_level(tracing::Level::TRACE)
		.with_writer(merchant_agent_core::logging::RedactingMakeWriter::new(
			move || CaptureWriter(sink.clone()),
		))
		.finish();
	let alerts = tracing::subscriber::with_default(subscriber, || {
		tokio::runtime::Builder::new_current_thread()
			.enable_all()
			.build()
			.unwrap()
			.block_on(async {
				let agent = world.agent(dir.path()).unwrap();
				tick_until(&agent, reported).await;
				world.platform.edit(|t| t.state = TradeState::Disputed);
				agent.tick().await.unwrap();
				world
					.platform
					.edit(|t| t.state = TradeState::RefundedToFunder);
				agent.tick().await.unwrap();
				format!("{:?}", agent.alerts())
			})
	});

	let mut haystack = String::new();
	for file in std::fs::read_dir(dir.path()).unwrap() {
		haystack.push_str(&String::from_utf8_lossy(
			&std::fs::read(file.unwrap().path()).unwrap(),
		));
	}
	haystack.push_str(&String::from_utf8_lossy(&logs.lock().unwrap()));
	haystack.push_str(&alerts);
	haystack.push_str(&world.platform.statements.lock().unwrap().join("\n"));
	assert!(
		haystack.contains("******0000"),
		"the masked account is what's kept"
	);
	assert!(!haystack.contains(ACCOUNT), "the account number leaked");
	assert!(!haystack.contains(NAME), "the account holder's name leaked");
}

struct CaptureWriter(Arc<Mutex<Vec<u8>>>);

impl std::io::Write for CaptureWriter {
	fn write(&mut self, b: &[u8]) -> std::io::Result<usize> {
		self.0.lock().unwrap().extend_from_slice(b);
		Ok(b.len())
	}
	fn flush(&mut self) -> std::io::Result<()> {
		Ok(())
	}
}

// ── audit fixes ──────────────────────────────────────────────────────────

#[tokio::test]
async fn a_trade_whose_id_is_not_a_plain_identifier_is_ignored() {
	let world = World::new();
	let mut odd = base_trade();
	odd.id = format!("{TRADE_ID}/approve?");
	world
		.platform
		.trades
		.lock()
		.unwrap()
		.insert(odd.id.clone(), odd.clone());
	let dir = tempfile::tempdir().unwrap();
	let agent = world.agent(dir.path()).unwrap();
	agent.tick().await.unwrap();
	assert!(agent.store().get(&odd.id).unwrap().is_none());
}

#[tokio::test]
async fn sealed_bank_details_for_another_rail_are_held() {
	// The sealed rail keys the per-payee limit and the deny-list; it must be
	// the trade's rail, or one account could pass as many payees.
	let world = World::new();
	world.platform.edit(|t| t.rail = "ng.other".into());
	let dir = tempfile::tempdir().unwrap();
	let agent = world.agent(dir.path()).unwrap();
	assert_eq!(
		tick_until(&agent, |s| matches!(s, AgentState::Held(_))).await,
		AgentState::Held(HoldReason::PayeeUnreadable)
	);
	assert!(world.bank.inner.payments().is_empty());
}

#[tokio::test]
async fn a_lock_that_closed_after_it_was_verified_is_caught_before_paying() {
	// Verified once, then refunded before the checks finished: the lock is
	// read again right before the payout is reserved.
	let world = World::new();
	*world.platform.quotes_down.lock().unwrap() = true;
	let dir = tempfile::tempdir().unwrap();
	let agent = world.agent(dir.path()).unwrap();
	agent.tick().await.unwrap();
	assert_eq!(state(&agent), AgentState::LockVerified);
	*world.locks.0.lock().unwrap() = LockOutcome::Problem(HoldReason::LockClosed);
	*world.platform.quotes_down.lock().unwrap() = false;
	assert_eq!(
		tick_until(&agent, |s| matches!(s, AgentState::Held(_))).await,
		AgentState::Held(HoldReason::LockClosed)
	);
	assert!(world.bank.inner.payments().is_empty());
}

#[test]
fn a_lock_near_its_refund_time_is_too_old_to_pay_against() {
	use merchant_agent_core::engine::{
		lock_too_old, ESCROW_REFUND_DELAY_SECS, LOCK_REFUND_SAFETY_SECS,
	};
	let locked = 1_700_000_000u64;
	let edge = locked as i64 + ESCROW_REFUND_DELAY_SECS - LOCK_REFUND_SAFETY_SECS;
	assert!(!lock_too_old(locked, locked as i64));
	assert!(!lock_too_old(locked, edge - 1));
	assert!(lock_too_old(locked, edge));
	assert!(lock_too_old(
		locked,
		locked as i64 + ESCROW_REFUND_DELAY_SECS + 1
	));
	assert!(lock_too_old(u64::MAX, 0));
}
