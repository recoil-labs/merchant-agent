//! The conformance suite: what any payout rail must do for the agent to be
//! safe on it. `merchant-agent adapter-test` runs it against a merchant's
//! adapter; the tests run it against the simulated bank, the template rail
//! (over the protocol, in front of the simulated bank) and a deliberately
//! careless rail that it must catch.
//!
//! It moves money: run it against a test environment only. It sends a few
//! payouts of `amount` (₦1 by default) to the test account, one more to a
//! missing account, and one larger than the balance.
//!
//! What it holds an adapter to, in the agent's own terms:
//! - a payout is found again **by its key**, with the same reference;
//! - the same key sent again, even twice at once, or after the agent gave
//!   up waiting, **never pays twice** (if the adapter says it is idempotent);
//! - a refusal is an explicit `rejected`, and moves no money;
//! - the balance moves by exactly what was paid.

use std::time::Duration;

use super::{AccountLookup, PayoutRail, PayoutRequest};
use crate::doctor::{Outcome, Report};
use crate::machine::{PayoutStatus, SendOutcome};
use crate::money::Minor;
use crate::pii::{AccountNumber, HolderName};

/// The accounts the adapter's test environment must provide.
#[derive(Debug, Clone)]
pub struct Accounts {
	pub bank_code: String,
	/// Resolves to a name, and takes payouts.
	pub account: String,
	/// Doesn't exist: lookups say not found, payouts are refused.
	pub missing_account: String,
	/// The name to pay if the lookup gives none.
	pub name: String,
	/// Each test payout, in kobo.
	pub amount: Minor,
	/// How long a payout may stay pending before the suite gives up.
	pub settle_within: Duration,
}

impl Default for Accounts {
	fn default() -> Self {
		Self {
			bank_code: "058".into(),
			account: "0000000001".into(),
			missing_account: "0000000009".into(),
			name: "Test Payee One".into(),
			amount: Minor(100),
			settle_within: Duration::from_secs(60),
		}
	}
}

const BALANCE: &str = "Balance is readable";
const LOOKUP: &str = "Lookup finds the test account";
const LOOKUP_MISSING: &str = "Lookup says a missing account is not found";
const SEND: &str = "A payout is accepted, with a reference";
const SETTLE: &str = "Its status settles by key, with the same reference";
const RESEND: &str = "The same key again pays nothing more";
const RACE: &str = "Two sends at once under one key pay once";
const ABANDONED: &str = "A send the agent gave up on is resolved by key";
const UNKNOWN_KEY: &str = "An unknown key is not found";
const MISSING: &str = "A payout to a missing account is refused";
const OVERDRAWN: &str = "A payout larger than the balance is refused";
const LEDGER: &str = "The balance moved by exactly what was paid";

fn key(tag: &str) -> String {
	use rand::Rng;
	let n: u64 = rand::rng().random();
	format!("rpma-conformance-{tag}-{n:016x}")
}

struct Suite<'a, R> {
	rail: &'a R,
	a: &'a Accounts,
	name: HolderName,
}

impl<R: PayoutRail> Suite<'_, R> {
	async fn send(&self, key: &str, account: &str, amount: Minor) -> Result<SendOutcome, String> {
		let account = AccountNumber::new(account.to_string());
		self.rail
			.send(PayoutRequest {
				idempotency_key: key,
				trade_id: "ft-conformance",
				amount,
				bank_code: &self.a.bank_code,
				account: &account,
				name: &self.name,
				narration: "RecoilPay adapter conformance test",
			})
			.await
			.map_err(|e| e.to_string())
	}

	/// The status once it stops being pending (or the wait runs out).
	async fn settle(&self, key: &str) -> Result<PayoutStatus, String> {
		let deadline = tokio::time::Instant::now() + self.a.settle_within;
		loop {
			let s = self.rail.status(key).await.map_err(|e| e.to_string())?;
			if s != PayoutStatus::Pending || tokio::time::Instant::now() >= deadline {
				return Ok(s);
			}
			tokio::time::sleep(Duration::from_millis(500)).await;
		}
	}

	async fn balance(&self) -> Option<Minor> {
		self.rail.balance().await.ok().flatten()
	}
}

fn fail(why: impl Into<String>) -> Outcome {
	Outcome::Fail {
		why: why.into(),
		fix: "see docs/adapters.md, \"What the conformance suite checks\"".into(),
	}
}

/// Run every check. Each one says what it found; nothing stops early except
/// where a later check depends on an earlier one.
pub async fn run<R: PayoutRail>(rail: &R, a: &Accounts) -> Report {
	let mut r = Report::default();
	let idempotent = rail.idempotent_send();
	let not_idempotent = "the adapter says a repeated key may pay twice, so the agent never resends; a lost answer always goes to a person";

	let start_balance = match rail.balance().await {
		Ok(Some(b)) => {
			r.add(BALANCE, Outcome::Pass(crate::money::naira(b)));
			Some(b)
		}
		Ok(None) => {
			r.add(
				BALANCE,
				Outcome::Warn(
					"the adapter can't tell; the agent can't check your float before paying".into(),
				),
			);
			None
		}
		Err(e) => {
			r.add(BALANCE, fail(e.to_string()));
			None
		}
	};

	let mut suite = Suite {
		rail,
		a,
		name: HolderName::new(a.name.clone()),
	};
	let lookups = match rail
		.verify_account(&a.bank_code, &AccountNumber::new(a.account.clone()))
		.await
	{
		Ok(AccountLookup::Name(n)) => {
			r.add(
				LOOKUP,
				Outcome::Pass(format!(
					"a name of {} characters",
					n.expose().chars().count()
				)),
			);
			suite.name = n;
			true
		}
		Ok(AccountLookup::Unsupported) => {
			r.add(
				LOOKUP,
				Outcome::Warn(
					"the adapter can't look accounts up, so the agent can't check payee names"
						.into(),
				),
			);
			false
		}
		Ok(AccountLookup::NotFound) => {
			r.add(
				LOOKUP,
				fail(format!(
					"{} at bank {} was not found",
					a.account, a.bank_code
				)),
			);
			false
		}
		Err(e) => {
			r.add(LOOKUP, fail(e.to_string()));
			false
		}
	};
	if lookups {
		let missing = rail
			.verify_account(&a.bank_code, &AccountNumber::new(a.missing_account.clone()))
			.await;
		r.add(
			LOOKUP_MISSING,
			match missing {
				Ok(AccountLookup::NotFound) => Outcome::Pass("not found".into()),
				Ok(AccountLookup::Name(_)) => {
					fail("a name came back for an account that should not exist")
				}
				Ok(AccountLookup::Unsupported) => {
					fail("unsupported, though the test account resolved")
				}
				Err(e) => fail(e.to_string()),
			},
		);
	} else {
		r.add(
			LOOKUP_MISSING,
			Outcome::Skip("needs a working lookup".into()),
		);
	}

	// One payout, found again by key.
	let mut paid: u128 = 0;
	let k1 = key("one");
	let accepted = match suite.send(&k1, &a.account, a.amount).await {
		Ok(SendOutcome::Accepted { reference }) => {
			r.add(SEND, Outcome::Pass(reference.clone()));
			Some(reference)
		}
		Ok(other) => {
			r.add(SEND, fail(format!("expected accepted, got {other:?}")));
			None
		}
		Err(e) => {
			r.add(SEND, fail(e));
			None
		}
	};
	if let Some(reference) = &accepted {
		match suite.settle(&k1).await {
			Ok(PayoutStatus::Succeeded { reference: again }) if &again == reference => {
				r.add(SETTLE, Outcome::Pass("succeeded".into()));
				paid += a.amount.get();
			}
			Ok(PayoutStatus::Succeeded { reference: again }) => {
				r.add(
					SETTLE,
					fail(format!(
						"accepted as {reference}, but the status says {again}"
					)),
				);
				paid += a.amount.get();
			}
			Ok(PayoutStatus::Pending) => r.add(
				SETTLE,
				fail(format!("still pending after {:?}", a.settle_within)),
			),
			Ok(other) => r.add(SETTLE, fail(format!("expected succeeded, got {other:?}"))),
			Err(e) => r.add(SETTLE, fail(e)),
		}

		// Again, the same key.
		if !idempotent {
			r.add(RESEND, Outcome::Skip(not_idempotent.into()));
		} else {
			match suite.send(&k1, &a.account, a.amount).await {
				Ok(SendOutcome::Accepted { reference: again }) if &again == reference => {
					r.add(RESEND, Outcome::Pass("the same payout came back".into()))
				}
				Ok(other) => r.add(
					RESEND,
					fail(format!("expected the first payout back, got {other:?}")),
				),
				Err(e) => r.add(RESEND, fail(e)),
			}
		}
	} else {
		r.add(SETTLE, Outcome::Skip("needs an accepted payout".into()));
		r.add(RESEND, Outcome::Skip("needs an accepted payout".into()));
	}

	// Two at once under one key.
	if !idempotent {
		r.add(RACE, Outcome::Skip(not_idempotent.into()));
	} else if accepted.is_none() {
		r.add(RACE, Outcome::Skip("needs an accepted payout".into()));
	} else {
		let k2 = key("race");
		let (x, y) = tokio::join!(
			suite.send(&k2, &a.account, a.amount),
			suite.send(&k2, &a.account, a.amount)
		);
		match (x, y) {
			(
				Ok(SendOutcome::Accepted { reference: p }),
				Ok(SendOutcome::Accepted { reference: q }),
			) if p == q => match suite.settle(&k2).await {
				Ok(PayoutStatus::Succeeded { .. }) => {
					r.add(RACE, Outcome::Pass("one payout, one reference".into()));
					paid += a.amount.get();
				}
				other => r.add(RACE, fail(format!("the payout did not settle: {other:?}"))),
			},
			(x, y) => r.add(
				RACE,
				fail(format!(
					"two answers that are not the same payout: {x:?} and {y:?}"
				)),
			),
		}
	}

	// A send the agent stopped waiting for: the request may or may not have
	// reached the adapter. The key must settle the question.
	if accepted.is_none() {
		r.add(ABANDONED, Outcome::Skip("needs an accepted payout".into()));
	} else {
		let k3 = key("abandoned");
		let _ = tokio::time::timeout(
			Duration::from_millis(5),
			suite.send(&k3, &a.account, a.amount),
		)
		.await;
		tokio::time::sleep(Duration::from_secs(1)).await;
		match suite.settle(&k3).await {
			Ok(PayoutStatus::Succeeded { .. }) => {
				paid += a.amount.get();
				if idempotent {
					match suite.send(&k3, &a.account, a.amount).await {
						Ok(SendOutcome::Accepted { .. }) => r.add(
							ABANDONED,
							Outcome::Pass(
								"it arrived; sending again returned the same payout".into(),
							),
						),
						other => r.add(
							ABANDONED,
							fail(format!("it arrived, but sending again gave {other:?}")),
						),
					}
				} else {
					r.add(
						ABANDONED,
						Outcome::Pass("it arrived, and the status says so".into()),
					);
				}
			}
			Ok(PayoutStatus::NotFound) if idempotent => match suite
				.send(&k3, &a.account, a.amount)
				.await
			{
				Ok(SendOutcome::Accepted { .. }) => match suite.settle(&k3).await {
					Ok(PayoutStatus::Succeeded { .. }) => {
						paid += a.amount.get();
						r.add(
							ABANDONED,
							Outcome::Pass("it never arrived; sending again paid it once".into()),
						);
					}
					other => r.add(
						ABANDONED,
						fail(format!("sent again, but it did not settle: {other:?}")),
					),
				},
				other => r.add(
					ABANDONED,
					fail(format!(
						"it never arrived, and sending again gave {other:?}"
					)),
				),
			},
			Ok(PayoutStatus::NotFound) => r.add(
				ABANDONED,
				Outcome::Pass("it never arrived, and the status says so".into()),
			),
			other => r.add(
				ABANDONED,
				fail(format!("the status after a lost answer: {other:?}")),
			),
		}
	}

	r.add(
		UNKNOWN_KEY,
		match rail.status(&key("never-sent")).await {
			Ok(PayoutStatus::NotFound) => Outcome::Pass("not found".into()),
			Ok(other) => fail(format!("expected not found, got {other:?}")),
			Err(e) => fail(e.to_string()),
		},
	);

	// Refusals: explicit, and no money moves.
	let k4 = key("missing");
	r.add(
		MISSING,
		match suite.send(&k4, &a.missing_account, a.amount).await {
			Ok(SendOutcome::Rejected { reason }) => Outcome::Pass(format!("rejected: {reason}")),
			Ok(SendOutcome::Accepted { .. }) => match suite.settle(&k4).await {
				Ok(PayoutStatus::Failed { reason }) => Outcome::Warn(format!(
					"accepted, then failed ({reason}); better to refuse it at once"
				)),
				other => fail(format!("accepted, and then {other:?}")),
			},
			Ok(other) => fail(format!("expected rejected, got {other:?}")),
			Err(e) => fail(format!("an error, not an explicit rejection: {e}")),
		},
	);
	match start_balance {
		None => r.add(OVERDRAWN, Outcome::Skip("needs a readable balance".into())),
		Some(b) => {
			let k5 = key("overdrawn");
			let more = Minor(b.get() + 1);
			r.add(
				OVERDRAWN,
				match suite.send(&k5, &a.account, more).await {
					Ok(SendOutcome::Rejected { reason }) => {
						Outcome::Pass(format!("rejected: {reason}"))
					}
					Ok(other) => fail(format!("expected rejected, got {other:?}")),
					Err(e) => fail(format!("an error, not an explicit rejection: {e}")),
				},
			);
		}
	}

	match (start_balance, suite.balance().await) {
		(Some(before), Some(after)) => {
			let moved = before.get().saturating_sub(after.get());
			r.add(
				LEDGER,
				if moved == paid && after.get() <= before.get() {
					Outcome::Pass(format!(
						"{} for {} payout(s)",
						crate::money::naira(Minor(paid)),
						paid / a.amount.get().max(1)
					))
				} else {
					fail(format!(
						"{} left the balance, but {} was paid: something was paid twice, or refused but charged",
						crate::money::naira(Minor(moved)),
						crate::money::naira(Minor(paid))
					))
				},
			);
		}
		_ => r.add(LEDGER, Outcome::Skip("needs a readable balance".into())),
	}
	r
}
