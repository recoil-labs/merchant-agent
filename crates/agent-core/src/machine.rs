//! The settlement state machine: a pure function, `(state, event) → (state,
//! effects)`, plus [`step`], which says what single piece of I/O a state
//! needs next.
//!
//! All I/O lives in [`crate::engine`]. The engine asks [`step`] what to do,
//! does it, turns the result into an [`Event`], applies it here, and writes
//! the new state durably **before** doing anything else. One transition per
//! durable write. So after a crash at any point, the stored state says
//! exactly what may already have happened, and the next step is chosen
//! from that.
//!
//! The money rules, all of which the tests check:
//!
//! - **One payment per trade (invariant 1).** A payout is sent only from
//!   [`AgentState::PayoutIntentRecorded`], after the intent and its
//!   idempotency key are on disk. Going to [`AgentState::Paying`] is written
//!   *before* the bank is called. A restart in `Paying` asks the bank for
//!   the status by key; it never sends.
//! - **No blind retries (invariant 2).** The one way back to sending is the
//!   bank saying it has **never heard of the key**, on a rail whose sends
//!   are idempotent by key. Then the same key is sent again, which the bank
//!   deduplicates. An answer of "unknown" goes to a person.
//! - **No payment without checks.** `PayoutIntentRecorded` is reached only
//!   through `LockVerified` and then `ChecksPassed`, and a person's approval
//!   sends a trade back to `Detected`, so even an approved payout re-checks
//!   the lock and everything not explicitly waived.
//! - **Someone else paid.** If the platform shows the trade reported by
//!   anyone else, the agent stops and asks a person, whatever stage it was
//!   at.

use std::fmt;

use crate::aggregator::types::TradeState;

/// Why a trade is held. Each one has a plain-English message for people,
/// and a stable code for storage and metrics.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum HoldReason {
	LockMismatch,
	LockClosed,
	PayeeUnreadable,
	AmountMismatch,
	TradeChanged,
	/// The trade names a quote this agent never saw in your book.
	UnknownQuote,
	/// The quote's rate is too far from your reference rate.
	RateOutOfBand,
	OverTradeLimit,
	OverDailyLimit,
	OverPayeeLimit,
	OverVelocity,
	DeadlineTooClose,
	BankRejected,
	NameMismatch,
	Screening,
	Paused,
	AccountNotFound,
	InsufficientFloat,
	/// The bank's name for the account isn't the seller's verified name, on
	/// an offer that pays only verified sellers.
	VerifiedNameMismatch,
	/// The trade required a verified seller, but the aggregator couldn't show
	/// the name to check against.
	VerifiedNameUnavailable,
}

impl HoldReason {
	pub const ALL: [Self; 20] = [
		Self::LockMismatch,
		Self::LockClosed,
		Self::PayeeUnreadable,
		Self::AmountMismatch,
		Self::TradeChanged,
		Self::UnknownQuote,
		Self::RateOutOfBand,
		Self::OverTradeLimit,
		Self::OverDailyLimit,
		Self::OverPayeeLimit,
		Self::OverVelocity,
		Self::DeadlineTooClose,
		Self::BankRejected,
		Self::NameMismatch,
		Self::Screening,
		Self::Paused,
		Self::AccountNotFound,
		Self::InsufficientFloat,
		Self::VerifiedNameMismatch,
		Self::VerifiedNameUnavailable,
	];

	pub fn code(&self) -> &'static str {
		match self {
			Self::LockMismatch => "lock_mismatch",
			Self::LockClosed => "lock_closed",
			Self::PayeeUnreadable => "payee_unreadable",
			Self::AmountMismatch => "amount_mismatch",
			Self::TradeChanged => "trade_changed",
			Self::UnknownQuote => "unknown_quote",
			Self::RateOutOfBand => "rate_out_of_band",
			Self::OverTradeLimit => "over_trade_limit",
			Self::OverDailyLimit => "over_daily_limit",
			Self::OverPayeeLimit => "over_payee_limit",
			Self::OverVelocity => "over_velocity",
			Self::DeadlineTooClose => "deadline_too_close",
			Self::BankRejected => "bank_rejected",
			Self::NameMismatch => "name_mismatch",
			Self::Screening => "screening",
			Self::Paused => "paused",
			Self::AccountNotFound => "account_not_found",
			Self::InsufficientFloat => "insufficient_float",
			Self::VerifiedNameMismatch => "verified_name_mismatch",
			Self::VerifiedNameUnavailable => "verified_name_unavailable",
		}
	}

	pub fn from_code(s: &str) -> Option<Self> {
		Self::ALL.into_iter().find(|r| r.code() == s)
	}

	pub fn message(&self) -> &'static str {
		match self {
			Self::LockMismatch => "The escrow lock on-chain does not match the trade, so the customer's money may not be there for you.",
			Self::LockClosed => "The escrow lock was already paid out or refunded, so paying now would pay for nothing.",
			Self::PayeeUnreadable => "The customer's bank details could not be opened, or don't match what the trade committed to.",
			Self::AmountMismatch => "The naira amount doesn't match the crypto locked at the rate you published.",
			Self::TradeChanged => "The trade's details changed after the agent first saw it.",
			Self::UnknownQuote => "The trade names an offer this agent never saw in your quote book, so it can't check the price.",
			Self::RateOutOfBand => "The offer's rate is further from your reference rate than you allow.",
			Self::OverTradeLimit => "This payout is larger than your per-trade limit.",
			Self::OverDailyLimit => "Paying this would take today's payouts over your daily limit.",
			Self::OverPayeeLimit => "Paying this would take today's payouts to this one account over your per-payee limit.",
			Self::OverVelocity => "Too many payouts in too short a time, by your velocity limit.",
			Self::DeadlineTooClose => "Too little time was left to pay safely before the deadline.",
			Self::BankRejected => "Your bank refused the transfer. No money moved.",
			Self::NameMismatch => "The account name at the bank doesn't match the name the customer gave.",
			Self::Screening => "The payee matched your screening list.",
			Self::Paused => "Payouts are paused. Resume them from the console or with `merchant-agent resume`.",
			Self::AccountNotFound => "Your bank says the customer's account does not exist.",
			Self::InsufficientFloat => "Your payout account would drop below its minimum balance. New offers are paused until you top it up.",
			Self::VerifiedNameMismatch => "The account isn't in the seller's verified name, or only partly matches it. This offer pays only an account in the seller's own name: check before you approve.",
			Self::VerifiedNameUnavailable => "This trade needs a verified seller, but RecoilPay couldn't show the seller's verified name. Don't pay; contact RecoilPay support.",
		}
	}

	/// Whether a person may approve paying anyway. A lock or payee problem
	/// can't be approved away: the money or the destination is wrong.
	pub fn approvable(&self) -> bool {
		!matches!(
			self,
			Self::LockMismatch
				| Self::LockClosed
				| Self::PayeeUnreadable
				| Self::TradeChanged
				| Self::AccountNotFound
				| Self::VerifiedNameUnavailable
		)
	}

	/// Whether paying after this needs a new idempotency key: the bank
	/// already saw (and refused) the old one.
	pub fn needs_new_attempt(&self) -> bool {
		matches!(self, Self::BankRejected)
	}
}

/// Why a trade needs a person. The agent has stopped and will not move
/// money for it on its own.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum PersonReason {
	/// The bank can't say whether the transfer happened.
	BankUnknown,
	/// The bank says the transfer failed after accepting it.
	BankFailed,
	/// The transfer failed or vanished after the agent had reported it.
	BankFailedAfterReport,
	/// The agent paid, but the platform refunded the customer.
	PaidButRefunded,
	/// The platform would not take the agent's report.
	ReportRefused,
	/// Someone else reported this trade paid.
	ReportedElsewhere,
	/// The customer confirmed receipt before the agent paid.
	SettledWithoutPayout,
}

impl PersonReason {
	pub const ALL: [Self; 7] = [
		Self::BankUnknown,
		Self::BankFailed,
		Self::BankFailedAfterReport,
		Self::PaidButRefunded,
		Self::ReportRefused,
		Self::ReportedElsewhere,
		Self::SettledWithoutPayout,
	];

	pub fn code(&self) -> &'static str {
		match self {
			Self::BankUnknown => "bank_unknown",
			Self::BankFailed => "bank_failed",
			Self::BankFailedAfterReport => "bank_failed_after_report",
			Self::PaidButRefunded => "paid_but_refunded",
			Self::ReportRefused => "report_refused",
			Self::ReportedElsewhere => "reported_elsewhere",
			Self::SettledWithoutPayout => "settled_without_payout",
		}
	}

	pub fn from_code(s: &str) -> Option<Self> {
		Self::ALL.into_iter().find(|r| r.code() == s)
	}

	pub fn message(&self) -> &'static str {
		match self {
			Self::BankUnknown => "The bank can't say whether this transfer happened. Check your bank before doing anything; the agent will not send it again.",
			Self::BankFailed => "The bank accepted this transfer and then said it failed.",
			Self::BankFailedAfterReport => "The transfer failed or vanished after it was reported to RecoilPay as paid.",
			Self::PaidButRefunded => "You paid this customer, but RecoilPay refunded their crypto. Recover the naira directly.",
			Self::ReportRefused => "RecoilPay would not accept the payment report for this trade.",
			Self::ReportedElsewhere => "Someone else reported this trade as paid, so the agent stopped to avoid paying twice.",
			Self::SettledWithoutPayout => "The customer confirmed receipt before the agent paid anything.",
		}
	}
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AgentState {
	Detected,
	LockVerified,
	ChecksPassed,
	PayoutIntentRecorded,
	/// The bank may have the transfer: written before the bank is called.
	Paying,
	/// The bank confirmed the transfer.
	Paid,
	/// RecoilPay holds the bank's reference. `bank_confirmed` is false only
	/// when a still-pending transfer was reported at the deadline margin.
	Reported {
		bank_confirmed: bool,
	},
	Settled,
	Held(HoldReason),
	NeedsPerson(PersonReason),
	Disputed,
	Refunded,
	/// The deadline passed and the agent never sent money.
	Expired,
}

impl AgentState {
	/// Settled and Refunded: nothing more can happen.
	pub fn is_terminal(&self) -> bool {
		matches!(self, Self::Settled | Self::Refunded)
	}

	/// The words people see (design §6.1).
	pub fn label(&self) -> &'static str {
		match self {
			Self::Detected => "Waiting for lock",
			Self::LockVerified | Self::ChecksPassed => "Checking",
			Self::PayoutIntentRecorded | Self::Paying | Self::Paid => "Paying",
			Self::Reported { .. } => "Paid, waiting for the customer",
			Self::Settled => "Settled",
			Self::Held(_) => "Held for review",
			Self::NeedsPerson(_) => "Needs a person",
			Self::Disputed => "Customer disputed",
			Self::Refunded => "Refunded to customer",
			Self::Expired => "Missed, not paid",
		}
	}

	/// `(state code, reason code)` for storage.
	pub fn to_codes(&self) -> (&'static str, Option<&'static str>) {
		match self {
			Self::Detected => ("detected", None),
			Self::LockVerified => ("lock_verified", None),
			Self::ChecksPassed => ("checks_passed", None),
			Self::PayoutIntentRecorded => ("payout_intent_recorded", None),
			Self::Paying => ("paying", None),
			Self::Paid => ("paid", None),
			Self::Reported {
				bank_confirmed: true,
			} => ("reported", None),
			Self::Reported {
				bank_confirmed: false,
			} => ("reported_bank_pending", None),
			Self::Settled => ("settled", None),
			Self::Held(r) => ("held", Some(r.code())),
			Self::NeedsPerson(r) => ("needs_person", Some(r.code())),
			Self::Disputed => ("disputed", None),
			Self::Refunded => ("refunded", None),
			Self::Expired => ("expired", None),
		}
	}

	pub fn from_codes(state: &str, reason: Option<&str>) -> Option<Self> {
		Some(match state {
			"detected" => Self::Detected,
			"lock_verified" => Self::LockVerified,
			"checks_passed" => Self::ChecksPassed,
			"payout_intent_recorded" => Self::PayoutIntentRecorded,
			"paying" => Self::Paying,
			"paid" => Self::Paid,
			"reported" => Self::Reported {
				bank_confirmed: true,
			},
			"reported_bank_pending" => Self::Reported {
				bank_confirmed: false,
			},
			"settled" => Self::Settled,
			"held" => Self::Held(HoldReason::from_code(reason?)?),
			"needs_person" => Self::NeedsPerson(PersonReason::from_code(reason?)?),
			"disputed" => Self::Disputed,
			"refunded" => Self::Refunded,
			"expired" => Self::Expired,
			_ => return None,
		})
	}
}

impl fmt::Display for AgentState {
	fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
		f.write_str(self.label())
	}
}

/// What the escrow check found.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LockOutcome {
	Verified,
	/// Not there yet, or not deep enough: wait.
	Wait,
	Problem(HoldReason),
}

/// The bank's answer to a send.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SendOutcome {
	Accepted {
		reference: String,
	},
	/// Taken, with no reference yet: a person will pay it (the manual rail).
	Queued,
	/// Refused; no money moved. The reason is the bank's, with no personal data.
	Rejected {
		reason: String,
	},
	Unknown,
}

/// The bank's answer to a status query by idempotency key.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PayoutStatus {
	Succeeded {
		reference: String,
	},
	Pending,
	/// Failed after being accepted. The reason is the bank's, with no personal data.
	Failed {
		reason: String,
	},
	/// The bank has no transfer under this key.
	NotFound,
	Unknown,
}

/// What reporting the payment to RecoilPay came to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReportResult {
	Reported,
	NotReportable(TradeState),
	/// RecoilPay refused the report outright (the key, the trade, or the
	/// reference), so trying again won't help.
	Refused,
	/// Couldn't tell yet; try again.
	Retry,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PersonAction {
	/// Pay a held trade anyway.
	Approve,
	/// Pay again, under a new key, after the bank failed or couldn't say.
	ApproveRepay,
	/// A person paid it themselves.
	MarkedPaid { reference: String },
	/// A person confirmed with the bank that nothing was sent.
	NothingSent,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Event {
	/// The platform's view of the trade. `reported_by_us` is true when the
	/// trade holds this agent's own bank reference.
	Platform {
		state: TradeState,
		reported_by_us: bool,
	},
	Lock(LockOutcome),
	Checks(Result<(), HoldReason>),
	IntentRecorded,
	IntentRefused(HoldReason),
	/// About to call the bank. Written before the call.
	SendStarting,
	Sent(SendOutcome),
	Status(PayoutStatus),
	Report(ReportResult),
	DeadlineTooClose,
	DeadlinePassed,
	Person(PersonAction),
}

/// Something to do once the new state is written.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Effect {
	/// Give back the limit reservation of an intent that was never sent.
	ReleaseReservation,
	/// The next payout uses a new attempt number, so a new key.
	NewAttempt,
	SetBankReference(String),
	/// Tell people. Urgent means money may already have moved wrongly.
	Alert {
		urgent: bool,
	},
	/// Attach the payment evidence to the dispute.
	AddDisputeEvidence,
	/// A person approved paying despite this hold; the check that raised it
	/// is waived for this trade (and only it).
	Override(HoldReason),
	/// Withdraw the merchant's offers so no new trades arrive (the kill
	/// switch, pulled automatically).
	PauseOffers,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Transition {
	pub next: AgentState,
	pub effects: Vec<Effect>,
}

/// An event that makes no sense in a state. The engine changes nothing and
/// logs it; it never guesses.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("{event} does not apply to a trade that is {state:?}")]
pub struct Invalid {
	pub state: AgentState,
	pub event: String,
}

/// What the machine needs to know about the world.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Ctx {
	/// The rail deduplicates sends by idempotency key, so re-sending a key
	/// the bank has never seen is safe.
	pub rail_idempotent: bool,
}

/// The one piece of I/O a state needs next.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Io {
	CheckLock,
	RunChecks,
	RecordIntent,
	Send,
	QueryStatus,
	Report,
	/// Nothing to do but follow the platform (and people).
	Watch,
	Done,
}

pub fn step(state: &AgentState) -> Io {
	use AgentState as S;
	match state {
		S::Detected => Io::CheckLock,
		S::LockVerified => Io::RunChecks,
		S::ChecksPassed => Io::RecordIntent,
		S::PayoutIntentRecorded => Io::Send,
		S::Paying
		| S::Reported {
			bank_confirmed: false,
		} => Io::QueryStatus,
		S::Paid => Io::Report,
		S::Reported {
			bank_confirmed: true,
		}
		| S::Disputed
		| S::Held(_)
		| S::NeedsPerson(_)
		| S::Expired => Io::Watch,
		S::Settled | S::Refunded => Io::Done,
	}
}

fn to(next: AgentState) -> Result<Transition, Invalid> {
	Ok(Transition {
		next,
		effects: vec![],
	})
}

fn with(next: AgentState, effects: Vec<Effect>) -> Result<Transition, Invalid> {
	Ok(Transition { next, effects })
}

fn alert(next: AgentState, urgent: bool) -> Result<Transition, Invalid> {
	with(next, vec![Effect::Alert { urgent }])
}

pub fn apply(state: &AgentState, event: &Event, ctx: Ctx) -> Result<Transition, Invalid> {
	use AgentState as S;
	use Effect as E;
	let same = || to(state.clone());
	let invalid = || {
		Err(Invalid {
			state: state.clone(),
			event: format!("{event:?}"),
		})
	};

	match event {
		Event::Platform {
			state: platform,
			reported_by_us,
		} => on_platform(state, *platform, *reported_by_us),

		Event::Lock(outcome) => match (state, outcome) {
			(S::Detected, LockOutcome::Verified) => to(S::LockVerified),
			(S::Detected, LockOutcome::Wait) => same(),
			(S::Detected, LockOutcome::Problem(r)) => alert(S::Held(*r), false),
			_ => invalid(),
		},

		Event::Checks(result) => match (state, result) {
			(S::LockVerified, Ok(())) => to(S::ChecksPassed),
			(S::LockVerified, Err(r)) => alert(S::Held(*r), false),
			// The last look before sending (the bank details are re-opened
			// from the trade) found a problem: nothing was sent.
			(S::PayoutIntentRecorded, Err(r)) => with(
				S::Held(*r),
				vec![E::ReleaseReservation, E::Alert { urgent: false }],
			),
			_ => invalid(),
		},

		Event::IntentRecorded => match state {
			S::ChecksPassed => to(S::PayoutIntentRecorded),
			_ => invalid(),
		},
		Event::IntentRefused(r) => match state {
			S::ChecksPassed if *r == HoldReason::InsufficientFloat => {
				with(S::Held(*r), vec![E::PauseOffers, E::Alert { urgent: true }])
			}
			S::ChecksPassed => alert(S::Held(*r), false),
			_ => invalid(),
		},

		Event::SendStarting => match state {
			S::PayoutIntentRecorded => to(S::Paying),
			_ => invalid(),
		},

		Event::Sent(outcome) => match (state, outcome) {
			(S::Paying, SendOutcome::Accepted { reference }) => {
				with(S::Paying, vec![E::SetBankReference(reference.clone())])
			}
			(S::Paying, SendOutcome::Queued) => alert(S::Paying, false),
			(S::Paying, SendOutcome::Rejected { .. }) => with(
				S::Held(HoldReason::BankRejected),
				vec![E::ReleaseReservation, E::Alert { urgent: false }],
			),
			// Invariant 2: don't guess; the next step asks by key.
			(S::Paying, SendOutcome::Unknown) => same(),
			_ => invalid(),
		},

		Event::Status(status) => match (state, status) {
			(S::Paying, PayoutStatus::Succeeded { reference }) => {
				with(S::Paid, vec![E::SetBankReference(reference.clone())])
			}
			(S::Paying, PayoutStatus::Pending) => same(),
			// Never reached the bank: re-send the *same* key, which the bank
			// deduplicates. Only on a rail that guarantees that.
			(S::Paying, PayoutStatus::NotFound) if ctx.rail_idempotent => {
				to(S::PayoutIntentRecorded)
			}
			(S::Paying, PayoutStatus::NotFound | PayoutStatus::Unknown) => {
				alert(S::NeedsPerson(PersonReason::BankUnknown), true)
			}
			(S::Paying, PayoutStatus::Failed { .. }) => {
				alert(S::NeedsPerson(PersonReason::BankFailed), true)
			}

			(
				S::Reported {
					bank_confirmed: false,
				},
				PayoutStatus::Succeeded { reference },
			) => with(
				S::Reported {
					bank_confirmed: true,
				},
				vec![E::SetBankReference(reference.clone())],
			),
			(
				S::Reported {
					bank_confirmed: false,
				},
				PayoutStatus::Pending,
			) => same(),
			(
				S::Reported {
					bank_confirmed: false,
				},
				_,
			) => alert(S::NeedsPerson(PersonReason::BankFailedAfterReport), true),
			_ => invalid(),
		},

		Event::Report(result) => match (state, result) {
			(S::Paid, ReportResult::Reported) => to(S::Reported {
				bank_confirmed: true,
			}),
			// Reported at the deadline margin while the bank still says pending.
			(S::Paying, ReportResult::Reported) => to(S::Reported {
				bank_confirmed: false,
			}),
			(S::Paid | S::Paying, ReportResult::Retry) => same(),
			(S::Paid, ReportResult::NotReportable(TradeState::SettledToFiatPayer)) => {
				to(S::Settled)
			}
			(
				S::Paid | S::Paying,
				ReportResult::NotReportable(TradeState::RefundedToFunder | TradeState::Failed),
			) => alert(S::NeedsPerson(PersonReason::PaidButRefunded), true),
			(S::Paid | S::Paying, ReportResult::NotReportable(_) | ReportResult::Refused) => {
				alert(S::NeedsPerson(PersonReason::ReportRefused), true)
			}
			_ => invalid(),
		},

		Event::DeadlineTooClose => match state {
			S::Detected | S::LockVerified | S::ChecksPassed => {
				alert(S::Held(HoldReason::DeadlineTooClose), false)
			}
			S::PayoutIntentRecorded => with(
				S::Held(HoldReason::DeadlineTooClose),
				vec![E::ReleaseReservation, E::Alert { urgent: false }],
			),
			_ => same(),
		},
		Event::DeadlinePassed => match state {
			S::Detected | S::LockVerified | S::ChecksPassed | S::Held(_) => {
				alert(S::Expired, false)
			}
			S::PayoutIntentRecorded => with(
				S::Expired,
				vec![E::ReleaseReservation, E::Alert { urgent: false }],
			),
			_ => same(),
		},

		Event::Person(action) => match (state, action) {
			// Approval starts again from the lock: whatever was held, the
			// money must still be there and every other check must still
			// pass. Only the approved rule is waived.
			(S::Held(r), PersonAction::Approve) if r.approvable() => {
				let mut effects = vec![E::Override(*r)];
				if r.needs_new_attempt() {
					effects.push(E::NewAttempt);
				}
				with(S::Detected, effects)
			}
			(
				S::NeedsPerson(PersonReason::BankFailed | PersonReason::BankUnknown),
				PersonAction::ApproveRepay,
			) => with(S::Detected, vec![E::NewAttempt]),
			(
				S::Held(_) | S::NeedsPerson(_) | S::Expired,
				PersonAction::MarkedPaid { reference },
			) => with(S::Paid, vec![E::SetBankReference(reference.clone())]),
			(
				S::NeedsPerson(PersonReason::BankFailed | PersonReason::BankUnknown),
				PersonAction::NothingSent,
			) => with(S::Expired, vec![E::ReleaseReservation]),
			_ => invalid(),
		},
	}
}

fn on_platform(
	state: &AgentState,
	platform: TradeState,
	ours: bool,
) -> Result<Transition, Invalid> {
	use AgentState as S;
	use Effect as E;
	use TradeState as P;
	let same = || to(state.clone());
	let release_if_intent = |next: AgentState, urgent: bool| {
		let mut effects = vec![];
		if *state == S::PayoutIntentRecorded {
			effects.push(E::ReleaseReservation);
		}
		effects.push(E::Alert { urgent });
		with(next, effects)
	};
	let before_paying = matches!(
		state,
		S::Detected
			| S::LockVerified
			| S::ChecksPassed
			| S::PayoutIntentRecorded
			| S::Held(_)
			| S::Expired
	);

	if state.is_terminal() {
		return same();
	}
	match platform {
		P::Quoted | P::AwaitingFiat => same(),

		P::AwaitingConfirmation | P::Disputed => match state {
			_ if before_paying => {
				release_if_intent(S::NeedsPerson(PersonReason::ReportedElsewhere), true)
			}
			S::Paying if ours => to(S::Reported {
				bank_confirmed: false,
			}),
			S::Paid if ours => to(S::Reported {
				bank_confirmed: true,
			}),
			S::Paying | S::Paid => alert(S::NeedsPerson(PersonReason::ReportedElsewhere), true),
			// Reported while the bank still said pending: keep asking the
			// bank. Only once it confirms (Reported { true }, next event)
			// does the trade move to Disputed and file "Paid" evidence; if
			// the bank fails it instead, a person is told urgently.
			S::Reported {
				bank_confirmed: false,
			} if platform == P::Disputed => same(),
			S::Reported {
				bank_confirmed: true,
			} if platform == P::Disputed => with(
				S::Disputed,
				vec![E::AddDisputeEvidence, E::Alert { urgent: false }],
			),
			_ => same(),
		},

		P::SettledToFiatPayer => match state {
			_ if before_paying => {
				release_if_intent(S::NeedsPerson(PersonReason::SettledWithoutPayout), true)
			}
			S::Reported { .. } | S::Disputed | S::Paid => to(S::Settled),
			_ => same(),
		},

		P::RefundedToFunder | P::Failed => match state {
			S::PayoutIntentRecorded => with(S::Refunded, vec![E::ReleaseReservation]),
			_ if before_paying => to(S::Refunded),
			S::Paying | S::Paid => alert(S::NeedsPerson(PersonReason::PaidButRefunded), true),
			S::Reported { .. } | S::Disputed => alert(S::Refunded, true),
			_ => same(),
		},
	}
}
