//! The settlement state machine, exhaustively.
//!
//! 1. Every agent-driven transition is listed in one table. Every state ×
//!    event pair not in the table must be refused as `Invalid`.
//! 2. Platform events (what RecoilPay reports) are checked against rules
//!    for every state: they never advance a payout, never fail, and stop
//!    the agent when someone else has acted.
//! 3. Property tests run random event sequences and check the money rules.

use merchant_agent_core::aggregator::types::TradeState;
use merchant_agent_core::machine::{
	apply, step, AgentState as S, Ctx, Effect, Event as Ev, HoldReason as H, Io, LockOutcome,
	PayoutStatus as St, PersonAction as Pa, PersonReason as P, ReportResult as Rr,
	SendOutcome as So,
};
use proptest::prelude::*;

const IDEMPOTENT: Ctx = Ctx {
	rail_idempotent: true,
};
const NOT_IDEMPOTENT: Ctx = Ctx {
	rail_idempotent: false,
};

const PLATFORM_STATES: [TradeState; 7] = [
	TradeState::Quoted,
	TradeState::AwaitingFiat,
	TradeState::AwaitingConfirmation,
	TradeState::Disputed,
	TradeState::SettledToFiatPayer,
	TradeState::RefundedToFunder,
	TradeState::Failed,
];

fn all_states() -> Vec<S> {
	let mut v = vec![
		S::Detected,
		S::LockVerified,
		S::ChecksPassed,
		S::PayoutIntentRecorded,
		S::Paying,
		S::Paid,
		S::Reported {
			bank_confirmed: true,
		},
		S::Reported {
			bank_confirmed: false,
		},
		S::Settled,
		S::Disputed,
		S::Refunded,
		S::Expired,
	];
	v.extend(H::ALL.map(S::Held));
	v.extend(P::ALL.map(S::NeedsPerson));
	v
}

/// Every event the agent itself produces (not platform events).
fn agent_events() -> Vec<Ev> {
	let mut v = vec![
		Ev::Lock(LockOutcome::Verified),
		Ev::Lock(LockOutcome::Wait),
		Ev::Lock(LockOutcome::Problem(H::LockMismatch)),
		Ev::Checks(Ok(())),
		Ev::Checks(Err(H::AmountMismatch)),
		Ev::IntentRecorded,
		Ev::IntentRefused(H::OverDailyLimit),
		Ev::IntentRefused(H::InsufficientFloat),
		Ev::SendStarting,
		Ev::Sent(So::Accepted {
			reference: "BANK-1".into(),
		}),
		Ev::Sent(So::Rejected {
			reason: "insufficient funds".into(),
		}),
		Ev::Sent(So::Unknown),
		Ev::Sent(So::Queued),
		Ev::Status(St::Succeeded {
			reference: "BANK-1".into(),
		}),
		Ev::Status(St::Pending),
		Ev::Status(St::Failed {
			reason: "reversed".into(),
		}),
		Ev::Status(St::NotFound),
		Ev::Status(St::Unknown),
		Ev::Report(Rr::Reported),
		Ev::Report(Rr::Retry),
		Ev::Report(Rr::Refused),
		Ev::DeadlineTooClose,
		Ev::DeadlinePassed,
		Ev::Person(Pa::Approve),
		Ev::Person(Pa::ApproveRepay),
		Ev::Person(Pa::MarkedPaid {
			reference: "MANUAL-1".into(),
		}),
		Ev::Person(Pa::NothingSent),
	];
	v.extend(PLATFORM_STATES.map(|p| Ev::Report(Rr::NotReportable(p))));
	v
}

/// The table: every valid agent-driven transition, as (from, event, ctx
/// sensitivity, to, effects). `None` for ctx means either ctx.
type Row = (S, Ev, Option<bool>, S, Vec<Effect>);

fn table() -> Vec<Row> {
	use Effect::*;
	let urgent = || vec![Alert { urgent: true }];
	let notice = || vec![Alert { urgent: false }];
	let mut rows: Vec<Row> = vec![
		// The happy path.
		(
			S::Detected,
			Ev::Lock(LockOutcome::Verified),
			None,
			S::LockVerified,
			vec![],
		),
		(
			S::Detected,
			Ev::Lock(LockOutcome::Wait),
			None,
			S::Detected,
			vec![],
		),
		(
			S::Detected,
			Ev::Lock(LockOutcome::Problem(H::LockMismatch)),
			None,
			S::Held(H::LockMismatch),
			notice(),
		),
		(
			S::LockVerified,
			Ev::Checks(Ok(())),
			None,
			S::ChecksPassed,
			vec![],
		),
		(
			S::LockVerified,
			Ev::Checks(Err(H::AmountMismatch)),
			None,
			S::Held(H::AmountMismatch),
			notice(),
		),
		(
			S::PayoutIntentRecorded,
			Ev::Checks(Err(H::AmountMismatch)),
			None,
			S::Held(H::AmountMismatch),
			vec![ReleaseReservation, Alert { urgent: false }],
		),
		(
			S::ChecksPassed,
			Ev::IntentRecorded,
			None,
			S::PayoutIntentRecorded,
			vec![],
		),
		(
			S::ChecksPassed,
			Ev::IntentRefused(H::OverDailyLimit),
			None,
			S::Held(H::OverDailyLimit),
			notice(),
		),
		(
			S::PayoutIntentRecorded,
			Ev::SendStarting,
			None,
			S::Paying,
			vec![],
		),
		// The bank.
		(
			S::Paying,
			Ev::Sent(So::Accepted {
				reference: "BANK-1".into(),
			}),
			None,
			S::Paying,
			vec![SetBankReference("BANK-1".into())],
		),
		(
			S::Paying,
			Ev::Sent(So::Rejected {
				reason: "insufficient funds".into(),
			}),
			None,
			S::Held(H::BankRejected),
			vec![ReleaseReservation, Alert { urgent: false }],
		),
		(S::Paying, Ev::Sent(So::Unknown), None, S::Paying, vec![]),
		(S::Paying, Ev::Sent(So::Queued), None, S::Paying, notice()),
		(
			S::ChecksPassed,
			Ev::IntentRefused(H::InsufficientFloat),
			None,
			S::Held(H::InsufficientFloat),
			vec![PauseOffers, Alert { urgent: true }],
		),
		(
			S::Paying,
			Ev::Status(St::Succeeded {
				reference: "BANK-1".into(),
			}),
			None,
			S::Paid,
			vec![SetBankReference("BANK-1".into())],
		),
		(S::Paying, Ev::Status(St::Pending), None, S::Paying, vec![]),
		(
			S::Paying,
			Ev::Status(St::Failed {
				reason: "reversed".into(),
			}),
			None,
			S::NeedsPerson(P::BankFailed),
			urgent(),
		),
		(
			S::Paying,
			Ev::Status(St::NotFound),
			Some(true),
			S::PayoutIntentRecorded,
			vec![],
		),
		(
			S::Paying,
			Ev::Status(St::NotFound),
			Some(false),
			S::NeedsPerson(P::BankUnknown),
			urgent(),
		),
		(
			S::Paying,
			Ev::Status(St::Unknown),
			None,
			S::NeedsPerson(P::BankUnknown),
			urgent(),
		),
		(
			S::Reported {
				bank_confirmed: false,
			},
			Ev::Status(St::Succeeded {
				reference: "BANK-1".into(),
			}),
			None,
			S::Reported {
				bank_confirmed: true,
			},
			vec![SetBankReference("BANK-1".into())],
		),
		(
			S::Reported {
				bank_confirmed: false,
			},
			Ev::Status(St::Pending),
			None,
			S::Reported {
				bank_confirmed: false,
			},
			vec![],
		),
		(
			S::Reported {
				bank_confirmed: false,
			},
			Ev::Status(St::Failed {
				reason: "reversed".into(),
			}),
			None,
			S::NeedsPerson(P::BankFailedAfterReport),
			urgent(),
		),
		(
			S::Reported {
				bank_confirmed: false,
			},
			Ev::Status(St::NotFound),
			None,
			S::NeedsPerson(P::BankFailedAfterReport),
			urgent(),
		),
		(
			S::Reported {
				bank_confirmed: false,
			},
			Ev::Status(St::Unknown),
			None,
			S::NeedsPerson(P::BankFailedAfterReport),
			urgent(),
		),
		// Reporting.
		(
			S::Paid,
			Ev::Report(Rr::Reported),
			None,
			S::Reported {
				bank_confirmed: true,
			},
			vec![],
		),
		(
			S::Paying,
			Ev::Report(Rr::Reported),
			None,
			S::Reported {
				bank_confirmed: false,
			},
			vec![],
		),
		(S::Paid, Ev::Report(Rr::Retry), None, S::Paid, vec![]),
		(S::Paying, Ev::Report(Rr::Retry), None, S::Paying, vec![]),
		(
			S::Paid,
			Ev::Report(Rr::NotReportable(TradeState::SettledToFiatPayer)),
			None,
			S::Settled,
			vec![],
		),
		(
			S::Paid,
			Ev::Report(Rr::Refused),
			None,
			S::NeedsPerson(P::ReportRefused),
			urgent(),
		),
		(
			S::Paying,
			Ev::Report(Rr::Refused),
			None,
			S::NeedsPerson(P::ReportRefused),
			urgent(),
		),
		// Deadlines.
		(
			S::PayoutIntentRecorded,
			Ev::DeadlineTooClose,
			None,
			S::Held(H::DeadlineTooClose),
			vec![ReleaseReservation, Alert { urgent: false }],
		),
		(
			S::PayoutIntentRecorded,
			Ev::DeadlinePassed,
			None,
			S::Expired,
			vec![ReleaseReservation, Alert { urgent: false }],
		),
	];
	for s in [S::Detected, S::LockVerified, S::ChecksPassed] {
		rows.push((
			s.clone(),
			Ev::DeadlineTooClose,
			None,
			S::Held(H::DeadlineTooClose),
			notice(),
		));
		rows.push((s, Ev::DeadlinePassed, None, S::Expired, notice()));
	}
	// Reports the platform would not take.
	for from in [S::Paid, S::Paying] {
		for p in PLATFORM_STATES {
			let next = match (&from, p) {
				(S::Paid, TradeState::SettledToFiatPayer) => continue,
				(_, TradeState::RefundedToFunder | TradeState::Failed) => {
					S::NeedsPerson(P::PaidButRefunded)
				}
				_ => S::NeedsPerson(P::ReportRefused),
			};
			rows.push((
				from.clone(),
				Ev::Report(Rr::NotReportable(p)),
				None,
				next,
				urgent(),
			));
		}
	}
	// Deadline events elsewhere change nothing, except that a held trade
	// whose deadline passes is missed.
	for s in all_states() {
		let pre = matches!(
			s,
			S::Detected | S::LockVerified | S::ChecksPassed | S::PayoutIntentRecorded
		);
		if pre {
			continue;
		}
		rows.push((s.clone(), Ev::DeadlineTooClose, None, s.clone(), vec![]));
		match s {
			S::Held(_) => rows.push((s.clone(), Ev::DeadlinePassed, None, S::Expired, notice())),
			_ => rows.push((s.clone(), Ev::DeadlinePassed, None, s.clone(), vec![])),
		}
	}
	// People.
	for r in H::ALL {
		if r.approvable() {
			let mut effects = vec![Override(r)];
			if r.needs_new_attempt() {
				effects.push(NewAttempt);
			}
			rows.push((
				S::Held(r),
				Ev::Person(Pa::Approve),
				None,
				S::Detected,
				effects,
			));
		}
		rows.push((
			S::Held(r),
			Ev::Person(Pa::MarkedPaid {
				reference: "MANUAL-1".into(),
			}),
			None,
			S::Paid,
			vec![SetBankReference("MANUAL-1".into())],
		));
	}
	for r in P::ALL {
		rows.push((
			S::NeedsPerson(r),
			Ev::Person(Pa::MarkedPaid {
				reference: "MANUAL-1".into(),
			}),
			None,
			S::Paid,
			vec![SetBankReference("MANUAL-1".into())],
		));
	}
	rows.push((
		S::Expired,
		Ev::Person(Pa::MarkedPaid {
			reference: "MANUAL-1".into(),
		}),
		None,
		S::Paid,
		vec![SetBankReference("MANUAL-1".into())],
	));
	for r in [P::BankFailed, P::BankUnknown] {
		rows.push((
			S::NeedsPerson(r),
			Ev::Person(Pa::ApproveRepay),
			None,
			S::Detected,
			vec![NewAttempt],
		));
		rows.push((
			S::NeedsPerson(r),
			Ev::Person(Pa::NothingSent),
			None,
			S::Expired,
			vec![ReleaseReservation],
		));
	}
	rows
}

#[test]
fn every_agent_transition_is_in_the_table_and_nothing_else_is_allowed() {
	let rows = table();
	let mut checked = 0;
	for ctx in [IDEMPOTENT, NOT_IDEMPOTENT] {
		for state in all_states() {
			for event in agent_events() {
				let row = rows.iter().find(|(s, e, c, _, _)| {
					*s == state && *e == event && c.is_none_or(|c| c == ctx.rail_idempotent)
				});
				let got = apply(&state, &event, ctx);
				match row {
					Some((_, _, _, next, effects)) => {
						let t = got.unwrap_or_else(|e| {
							panic!("{state:?} + {event:?}: refused, but the table allows it: {e}")
						});
						assert_eq!(&t.next, next, "{state:?} + {event:?}");
						assert_eq!(&t.effects, effects, "{state:?} + {event:?}");
					}
					None => assert!(
						got.is_err(),
						"{state:?} + {event:?}: allowed, but not in the table: {got:?}"
					),
				}
				checked += 1;
			}
		}
	}
	assert!(checked > 1500, "{checked}");
}

#[test]
fn platform_events_never_advance_a_payout_and_never_fail() {
	for ctx in [IDEMPOTENT, NOT_IDEMPOTENT] {
		for state in all_states() {
			for p in PLATFORM_STATES {
				for ours in [true, false] {
					let t = apply(
						&state,
						&Ev::Platform {
							state: p,
							reported_by_us: ours,
						},
						ctx,
					)
					.unwrap_or_else(|e| panic!("{state:?} + {p:?}: {e}"));
					let advanced = matches!(
						t.next,
						S::LockVerified
							| S::ChecksPassed | S::PayoutIntentRecorded
							| S::Paying | S::Paid
					) && t.next != state;
					assert!(
						!advanced,
						"{state:?} + {p:?} advanced the payout to {:?}",
						t.next
					);
					if state.is_terminal() {
						assert_eq!(t.next, state, "terminal states never move");
					}
				}
			}
		}
	}
}

#[test]
fn someone_else_reporting_stops_the_agent_at_every_stage_before_paying() {
	for state in [
		S::Detected,
		S::LockVerified,
		S::ChecksPassed,
		S::PayoutIntentRecorded,
		S::Paying,
		S::Paid,
		S::Held(H::OverDailyLimit),
		S::Expired,
	] {
		let t = apply(
			&state,
			&Ev::Platform {
				state: TradeState::AwaitingConfirmation,
				reported_by_us: false,
			},
			IDEMPOTENT,
		)
		.unwrap();
		assert_eq!(t.next, S::NeedsPerson(P::ReportedElsewhere), "{state:?}");
		assert!(
			t.effects.contains(&Effect::Alert { urgent: true }),
			"{state:?}"
		);
		assert_eq!(
			t.effects.contains(&Effect::ReleaseReservation),
			state == S::PayoutIntentRecorded,
			"{state:?}"
		);
	}
}

#[test]
fn our_own_report_seen_after_a_crash_completes_the_report() {
	let seen = |s: S| {
		apply(
			&s,
			&Ev::Platform {
				state: TradeState::AwaitingConfirmation,
				reported_by_us: true,
			},
			IDEMPOTENT,
		)
		.unwrap()
		.next
	};
	assert_eq!(
		seen(S::Paid),
		S::Reported {
			bank_confirmed: true
		}
	);
	assert_eq!(
		seen(S::Paying),
		S::Reported {
			bank_confirmed: false
		}
	);
}

#[test]
fn a_refund_after_paying_is_urgent_and_a_refund_before_is_quiet() {
	let refund = |s: S| {
		apply(
			&s,
			&Ev::Platform {
				state: TradeState::RefundedToFunder,
				reported_by_us: false,
			},
			IDEMPOTENT,
		)
		.unwrap()
	};
	assert_eq!(refund(S::Paying).next, S::NeedsPerson(P::PaidButRefunded));
	assert_eq!(refund(S::Paid).next, S::NeedsPerson(P::PaidButRefunded));
	let t = refund(S::Reported {
		bank_confirmed: true,
	});
	assert_eq!(t.next, S::Refunded);
	assert!(t.effects.contains(&Effect::Alert { urgent: true }));
	assert_eq!(refund(S::Detected).next, S::Refunded);
	assert_eq!(
		refund(S::PayoutIntentRecorded).effects,
		vec![Effect::ReleaseReservation]
	);
}

#[test]
fn a_dispute_attaches_evidence() {
	let t = apply(
		&S::Reported {
			bank_confirmed: true,
		},
		&Ev::Platform {
			state: TradeState::Disputed,
			reported_by_us: true,
		},
		IDEMPOTENT,
	)
	.unwrap();
	assert_eq!(t.next, S::Disputed);
	assert!(t.effects.contains(&Effect::AddDisputeEvidence));
}

#[test]
fn only_one_state_sends_money() {
	for s in all_states() {
		assert_eq!(step(&s) == Io::Send, s == S::PayoutIntentRecorded, "{s:?}");
	}
	assert_eq!(
		step(&S::Paying),
		Io::QueryStatus,
		"a restart while paying asks, never sends"
	);
}

#[test]
fn states_round_trip_through_their_storage_codes() {
	for s in all_states() {
		let (state, reason) = s.to_codes();
		assert_eq!(S::from_codes(state, reason), Some(s.clone()), "{s:?}");
	}
	assert_eq!(S::from_codes("held", Some("no_such_reason")), None);
	assert_eq!(S::from_codes("teleported", None), None);
}

#[test]
fn every_reason_has_a_plain_message() {
	for r in H::ALL {
		assert!(
			r.message().ends_with('.') && r.message().len() > 20,
			"{r:?}"
		);
	}
	for r in P::ALL {
		assert!(
			r.message().ends_with('.') && r.message().len() > 20,
			"{r:?}"
		);
	}
}

// ── properties ───────────────────────────────────────────────────────────

fn any_event() -> impl Strategy<Value = Ev> {
	let mut events = agent_events();
	for p in PLATFORM_STATES {
		events.push(Ev::Platform {
			state: p,
			reported_by_us: true,
		});
		events.push(Ev::Platform {
			state: p,
			reported_by_us: false,
		});
	}
	prop::sample::select(events)
}

proptest! {
	#![proptest_config(ProptestConfig::with_cases(2000))]

	/// Invariant 1: under any sequence of events, every payout is sent under
	/// an attempt's key at most once, unless the bank said it had never
	/// heard of that key (and the rail deduplicates by key); and a new key
	/// only ever follows a person's approval.
	#[test]
	fn invariant_1_never_two_payouts_for_one_trade(
		events in prop::collection::vec(any_event(), 0..80),
		idempotent in any::<bool>(),
	) {
		let ctx = Ctx { rail_idempotent: idempotent };
		let mut state = S::Detected;
		let mut attempt = 1u32;
		let mut sends: std::collections::HashMap<u32, u32> = Default::default();
		let mut resends_allowed: std::collections::HashMap<u32, u32> = Default::default();
		let mut approvals = 0u32;
		for e in &events {
			let Ok(t) = apply(&state, e, ctx) else { continue };
			if t.effects.contains(&Effect::NewAttempt) {
				attempt += 1;
				approvals += 1;
			}
			if state == S::Paying && *e == Ev::Status(St::NotFound) && t.next == S::PayoutIntentRecorded {
				prop_assert!(idempotent, "a re-send on a rail that can't deduplicate");
				*resends_allowed.entry(attempt).or_default() += 1;
			}
			if t.next == S::Paying && state == S::PayoutIntentRecorded {
				*sends.entry(attempt).or_default() += 1;
			}
			state = t.next;
		}
		for (a, n) in &sends {
			prop_assert!(*n <= 1 + resends_allowed.get(a).copied().unwrap_or(0), "attempt {a} sent {n} times");
		}
		prop_assert!(sends.len() as u32 <= 1 + approvals, "{} keys for {approvals} approvals", sends.len());
	}

	/// No payout without the checks: every entry into PayoutIntentRecorded
	/// follows LockVerified and then ChecksPassed since the trade last
	/// (re)started at Detected.
	#[test]
	fn no_payout_without_lock_and_checks(
		events in prop::collection::vec(any_event(), 0..80),
		idempotent in any::<bool>(),
	) {
		let ctx = Ctx { rail_idempotent: idempotent };
		let mut state = S::Detected;
		let (mut lock, mut checks) = (false, false);
		for e in &events {
			let Ok(t) = apply(&state, e, ctx) else { continue };
			match &t.next {
				S::Detected => (lock, checks) = (false, false),
				S::LockVerified => lock = true,
				S::ChecksPassed => checks = lock,
				S::PayoutIntentRecorded if state == S::ChecksPassed => {
					prop_assert!(lock && checks, "an intent without the lock and checks");
				}
				S::Paying => prop_assert!(lock && checks, "paying without the lock and checks"),
				_ => {}
			}
			state = t.next;
		}
	}

	/// Nothing panics, and a terminal state never moves.
	#[test]
	fn terminal_states_are_final(events in prop::collection::vec(any_event(), 0..80)) {
		let mut state = S::Detected;
		let mut ended: Option<S> = None;
		for e in &events {
			if let Ok(t) = apply(&state, e, IDEMPOTENT) {
				state = t.next;
			}
			if let Some(end) = &ended {
				prop_assert_eq!(&state, end);
			} else if state.is_terminal() {
				ended = Some(state.clone());
			}
		}
	}
}
