//! Payout rails: the merchant's own way of sending naira.
//!
//! The agent pays through a [`PayoutRail`] and never sees how the bank does
//! it. Every send carries a deterministic idempotency key, and the rail must
//! be able to answer "what happened to this key?" ([`PayoutRail::status`]):
//! that question is what makes a crash or a timeout safe (invariants 1 and
//! 2).
//!
//! **A rail never receives more personal data than the bank needs:** the
//! account lookup gets the bank code and account number; a transfer adds
//! the account holder's name and the amount. Nothing is kept afterwards.
//!
//! Rails:
//! - [`simulated`]: an in-memory bank with every fault the agent must
//!   survive. Every test and sandbox mode use it.
//! - [`manual`]: a person pays from their bank app; the agent does
//!   everything else.
//! - [`paystack`]: Paystack Transfers.
//! - [`template`]: the merchant's own payout adapter, over the adapter
//!   protocol (docs/adapters.md). [`protocol_server`] serves any rail over
//!   that protocol, and [`conformance`] is the suite every adapter must pass.

pub mod conformance;
pub mod manual;
pub mod names;
pub mod paystack;
pub mod protocol_server;
pub mod simulated;
pub mod template;

use crate::machine::{PayoutStatus, SendOutcome};
use crate::money::Minor;
use crate::pii::{AccountNumber, HolderName};

/// One transfer. Only what the bank needs; dropped after the call.
pub struct PayoutRequest<'a> {
	pub idempotency_key: &'a str,
	/// Not personal data; lets a rail tie the payout to its trade.
	pub trade_id: &'a str,
	pub amount: Minor,
	pub bank_code: &'a str,
	pub account: &'a AccountNumber,
	pub name: &'a HolderName,
	/// Shown on the customer's statement. No personal data.
	pub narration: &'a str,
}

/// What the bank says about an account.
pub enum AccountLookup {
	/// The account holder's name, as the bank has it.
	Name(HolderName),
	/// No such account at that bank.
	NotFound,
	/// This rail can't look accounts up (the manual rail): the person who
	/// pays sees the name in their bank app.
	Unsupported,
}

/// The bank could not be asked: a timeout or a connection failure. Whether
/// a send happened is then unknown, and the engine asks by key.
#[derive(Debug, Clone, thiserror::Error)]
#[error("the bank could not be reached: {0}")]
pub struct RailError(pub String);

pub trait PayoutRail: Send + Sync {
	/// Whether sending the same key twice can never pay twice (the bank
	/// deduplicates by key). Only then may the agent re-send a key the bank
	/// says it has never seen.
	fn idempotent_send(&self) -> bool;

	/// Look up who holds an account, for the name check (invariant 4).
	fn verify_account(
		&self,
		bank_code: &str,
		account: &AccountNumber,
	) -> impl std::future::Future<Output = Result<AccountLookup, RailError>> + Send;

	fn send(
		&self,
		req: PayoutRequest<'_>,
	) -> impl std::future::Future<Output = Result<SendOutcome, RailError>> + Send;

	fn status(
		&self,
		idempotency_key: &str,
	) -> impl std::future::Future<Output = Result<PayoutStatus, RailError>> + Send;

	/// The payout account's available balance, or `None` if this rail
	/// can't read it (the manual rail).
	fn balance(&self)
		-> impl std::future::Future<Output = Result<Option<Minor>, RailError>> + Send;
}

/// A bank's or adapter's message, made safe to keep: one line, bounded, and with any
/// long run of digits (an account number) masked.
pub(crate) fn clean(message: &str) -> String {
	let mut out = String::new();
	let mut digits = String::new();
	let flush = |digits: &mut String, out: &mut String| {
		if digits.len() >= 6 {
			out.push_str(&"*".repeat(digits.len()));
		} else {
			out.push_str(digits);
		}
		digits.clear();
	};
	for c in message.chars().take(200) {
		if c.is_ascii_digit() {
			digits.push(c);
			continue;
		}
		flush(&mut digits, &mut out);
		out.push(if c.is_control() { ' ' } else { c });
	}
	flush(&mut digits, &mut out);
	if out.trim().is_empty() {
		"no message".into()
	} else {
		out.trim().to_string()
	}
}
