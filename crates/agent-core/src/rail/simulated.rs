//! An in-memory bank, for tests and sandbox mode.
//!
//! It behaves like a real payout API: it deduplicates by idempotency key,
//! debits a balance, and answers status queries by key. Clones share
//! everything, so the bank outlives an agent that crashes and restarts, as
//! a real bank would.
//!
//! **Faults** ([`Faults`]) make it misbehave the way real banks do:
//! - **timeouts:** the request is lost before the bank sees it;
//! - **5xx after success:** it pays, then answers with an error;
//! - **duplicate delivery:** the same request arrives twice;
//! - **slow pending:** it answers "pending" for a while before settling;
//! - **wrong account name:** a lookup returns someone else's name;
//! - **insufficient balance:** it refuses a transfer it can't fund;
//! - plus status outages, an "unknown" status, and a reversal after
//!   acceptance.
//!
//! The ledger keeps masked accounts only, and must always balance: the
//! opening balance equals the balance plus everything paid out
//! ([`SimulatedRail::ledger_balances`]).
//!
//! **In sandbox mode** ([`SimulatedRail::sandbox_in`]) the bank keeps its
//! ledger in `sandbox-bank.json` in the data directory, so it outlives the
//! agent's process the way a real bank does: a payout in flight when the
//! agent dies is still found by key after a restart, never paid twice. It
//! also takes **drills**: a `sandbox-faults.json` there (a [`Faults`] in
//! JSON) is read on the next call, applied, and deleted. That is how the
//! end-to-end tests make the bank misbehave from outside.

use std::path::{Path, PathBuf};

use std::collections::HashMap;

use serde::{Deserialize, Serialize};
use std::sync::{Arc, Mutex};

use super::{AccountLookup, PayoutRail, PayoutRequest, RailError};
use crate::machine::{PayoutStatus, SendOutcome};
use crate::money::Minor;
use crate::pii::{AccountNumber, HolderName};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Payment {
	pub idempotency_key: String,
	pub trade_id: String,
	#[serde(with = "minor")]
	pub amount: Minor,
	pub bank_code: String,
	pub account_masked: String,
	pub reference: String,
	/// Reversed by the bank after it was accepted; the money came back.
	pub reversed: bool,
}

/// How the bank should misbehave. Counters count down as they fire.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Faults {
	/// Lose this many sends before the bank sees them.
	pub lost_sends: u32,
	/// Pay, then answer this many sends with an error.
	pub errors_after_paying: u32,
	/// Every send is delivered twice.
	pub duplicate_delivery: bool,
	/// Answer "pending" to this many status queries per payment first.
	pub pending_polls: u32,
	/// Fail this many status queries outright.
	pub status_outages: u32,
	/// Answer "unknown" to every status query.
	pub status_unknown: bool,
	/// Reverse the next accepted payment: status then says it failed.
	pub reverse_next: bool,
	/// Fail this many account lookups outright.
	pub lookup_outages: u32,
	/// Pay, then never answer: the call hangs as if the process died
	/// mid-request. Fires once. For crash drills.
	pub hang_after_paying: bool,
}

/// Minor units as a decimal string, so any reader keeps every digit.
mod minor {
	use super::Minor;
	use serde::{Deserialize, Deserializer, Serializer};

	pub fn serialize<S: Serializer>(m: &Minor, s: S) -> Result<S::Ok, S::Error> {
		s.serialize_str(&m.get().to_string())
	}

	pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<Minor, D::Error> {
		let s = String::deserialize(d)?;
		s.parse().map(Minor).map_err(serde::de::Error::custom)
	}
}

/// What a sandbox bank keeps on disk.
#[derive(Serialize, Deserialize)]
struct Saved {
	#[serde(with = "minor")]
	opening: Minor,
	#[serde(with = "minor")]
	balance: Minor,
	payments: Vec<Payment>,
	pending_left: HashMap<String, u32>,
	faults: Faults,
}

pub const LEDGER_FILE: &str = "sandbox-bank.json";
pub const DRILL_FILE: &str = "sandbox-faults.json";

struct Bank {
	opening: u128,
	balance: u128,
	payments: Vec<Payment>,
	by_key: HashMap<String, usize>,
	pending_left: HashMap<String, u32>,
	/// (bank code, account) → name. Simulated accounts only.
	accounts: HashMap<(String, String), String>,
	/// Answer lookups of unknown accounts with this name (sandbox mode).
	/// Sandbox mode: unknown accounts can't be looked up (rather than not
	/// existing), so the name check is left to the sandbox's own setting.
	sandbox: bool,
	faults: Faults,
	sends_seen: u32,
	lookups: u32,
	/// The data directory, for a sandbox bank kept on disk.
	dir: Option<PathBuf>,
}

impl Bank {
	/// Apply a waiting drill, if any, and save it with the ledger at once:
	/// a drill applied only in memory would come back from the old ledger
	/// after a restart.
	fn drill(&mut self) -> Result<(), RailError> {
		let Some(dir) = &self.dir else { return Ok(()) };
		let path = dir.join(DRILL_FILE);
		let Ok(text) = std::fs::read_to_string(&path) else {
			return Ok(());
		};
		let _ = std::fs::remove_file(&path);
		match serde_json::from_str::<Faults>(&text) {
			Ok(f) => {
				tracing::warn!(faults = ?f, "sandbox bank drill applied");
				self.faults = f;
				self.save()
			}
			Err(e) => {
				tracing::error!(error = %e, "ignored a sandbox bank drill that is not valid");
				Ok(())
			}
		}
	}

	/// Write the ledger, atomically. A bank that can't keep its books
	/// stops: better than paying from a ledger it will forget.
	fn save(&self) -> Result<(), RailError> {
		let Some(dir) = &self.dir else { return Ok(()) };
		let saved = Saved {
			opening: Minor(self.opening),
			balance: Minor(self.balance),
			payments: self.payments.clone(),
			pending_left: self.pending_left.clone(),
			faults: self.faults.clone(),
		};
		let write = || -> std::io::Result<()> {
			let tmp = dir.join(format!("{LEDGER_FILE}.tmp"));
			let mut f = std::fs::File::create(&tmp)?;
			std::io::Write::write_all(
				&mut f,
				&serde_json::to_vec_pretty(&saved).map_err(std::io::Error::other)?,
			)?;
			f.sync_all()?;
			std::fs::rename(&tmp, dir.join(LEDGER_FILE))
		};
		write().map_err(|e| RailError(format!("the sandbox bank could not save its ledger: {e}")))
	}
}

#[derive(Clone)]
pub struct SimulatedRail {
	bank: Arc<Mutex<Bank>>,
}

impl SimulatedRail {
	/// A bank with `opening_balance` kobo and no accounts.
	pub fn with_balance(opening_balance: Minor) -> Self {
		Self {
			bank: Arc::new(Mutex::new(Bank {
				opening: opening_balance.get(),
				balance: opening_balance.get(),
				payments: Vec::new(),
				by_key: HashMap::new(),
				pending_left: HashMap::new(),
				accounts: HashMap::new(),
				sandbox: false,
				faults: Faults::default(),
				sends_seen: 0,
				lookups: 0,
				dir: None,
			})),
		}
	}

	/// For sandbox mode: pretend money, and accounts the bank doesn't know
	/// are "can't look up" rather than "doesn't exist", since nobody in a
	/// sandbox knows the name a customer sealed.
	pub fn sandbox(opening_balance: Minor) -> Self {
		let rail = Self::with_balance(opening_balance);
		rail.bank().sandbox = true;
		rail
	}

	/// A sandbox bank kept in `dir`: picks up its ledger from an earlier
	/// run, or opens with `opening_balance`.
	pub fn sandbox_in(dir: &Path, opening_balance: Minor) -> Result<Self, String> {
		let rail = Self::sandbox(opening_balance);
		{
			let mut b = rail.bank();
			b.dir = Some(dir.to_path_buf());
			match std::fs::read_to_string(dir.join(LEDGER_FILE)) {
				Ok(text) => {
					let saved: Saved = serde_json::from_str(&text)
						.map_err(|e| format!("{LEDGER_FILE} is not a sandbox ledger: {e}"))?;
					b.opening = saved.opening.get();
					b.balance = saved.balance.get();
					b.by_key = saved
						.payments
						.iter()
						.enumerate()
						.map(|(i, p)| (p.idempotency_key.clone(), i))
						.collect();
					b.payments = saved.payments;
					b.pending_left = saved.pending_left;
					b.faults = saved.faults;
				}
				Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
				Err(e) => return Err(format!("cannot read {LEDGER_FILE}: {e}")),
			}
		}
		Ok(rail)
	}

	fn bank(&self) -> std::sync::MutexGuard<'_, Bank> {
		self.bank.lock().unwrap_or_else(|e| e.into_inner())
	}

	/// Register a simulated account.
	pub fn add_account(&self, bank_code: &str, account: &str, name: &str) {
		self.bank()
			.accounts
			.insert((bank_code.into(), account.into()), name.into());
	}

	pub fn set_faults(&self, faults: Faults) {
		self.bank().faults = faults;
	}

	pub fn faults(&self) -> Faults {
		self.bank().faults.clone()
	}

	pub fn set_balance(&self, kobo: Minor) {
		let mut b = self.bank();
		let paid: u128 = b
			.payments
			.iter()
			.filter(|p| !p.reversed)
			.map(|p| p.amount.get())
			.sum();
		b.balance = kobo.get();
		b.opening = kobo.get() + paid;
	}

	/// Every payment the bank made, in order.
	pub fn payments(&self) -> Vec<Payment> {
		self.bank().payments.clone()
	}

	/// Payments that moved money and stayed moved.
	pub fn settled_payments(&self) -> Vec<Payment> {
		self.payments()
			.into_iter()
			.filter(|p| !p.reversed)
			.collect()
	}

	/// Sends the bank received (including duplicates and lost ones).
	pub fn sends_seen(&self) -> u32 {
		self.bank().sends_seen
	}

	pub fn lookups(&self) -> u32 {
		self.bank().lookups
	}

	/// The books balance: opening = balance + everything paid out.
	pub fn ledger_balances(&self) -> bool {
		let b = self.bank();
		let paid: u128 = b
			.payments
			.iter()
			.filter(|p| !p.reversed)
			.map(|p| p.amount.get())
			.sum();
		b.opening == b.balance + paid
	}

	pub fn current_balance(&self) -> Minor {
		Minor(self.bank().balance)
	}

	/// One delivery of a send to the bank.
	fn deliver(b: &mut Bank, req: &PayoutRequest<'_>) -> SendOutcome {
		if let Some(&i) = b.by_key.get(req.idempotency_key) {
			return SendOutcome::Accepted {
				reference: b.payments[i].reference.clone(),
			};
		}
		// Outside sandbox the bank knows its accounts: one it doesn't know
		// does not exist, and a transfer to it is refused.
		if !b.sandbox
			&& !b
				.accounts
				.contains_key(&(req.bank_code.to_string(), req.account.expose().to_string()))
		{
			return SendOutcome::Rejected {
				reason: "the account does not exist".into(),
			};
		}
		let amount = req.amount.get();
		if amount > b.balance {
			return SendOutcome::Rejected {
				reason: "insufficient funds in the payout account".into(),
			};
		}
		b.balance -= amount;
		let reference = format!("SIM-{:06}", b.payments.len() + 1);
		b.payments.push(Payment {
			idempotency_key: req.idempotency_key.to_string(),
			trade_id: req.trade_id.to_string(),
			amount: req.amount,
			bank_code: req.bank_code.to_string(),
			account_masked: req.account.masked(),
			reference: reference.clone(),
			reversed: false,
		});
		let i = b.payments.len() - 1;
		b.by_key.insert(req.idempotency_key.to_string(), i);
		let polls = b.faults.pending_polls;
		b.pending_left
			.insert(req.idempotency_key.to_string(), polls);
		if std::mem::take(&mut b.faults.reverse_next) {
			b.payments[i].reversed = true;
			b.balance += amount;
		}
		SendOutcome::Accepted { reference }
	}
}

impl Default for SimulatedRail {
	fn default() -> Self {
		Self::with_balance(Minor(1_000_000_000_000))
	}
}

fn countdown(n: &mut u32) -> bool {
	if *n > 0 {
		*n -= 1;
		true
	} else {
		false
	}
}

impl PayoutRail for SimulatedRail {
	fn idempotent_send(&self) -> bool {
		true
	}

	async fn verify_account(
		&self,
		bank_code: &str,
		account: &AccountNumber,
	) -> Result<AccountLookup, RailError> {
		let mut b = self.bank();
		b.drill()?;
		b.lookups += 1;
		if countdown(&mut b.faults.lookup_outages) {
			b.save()?;
			return Err(RailError("simulated lookup outage".into()));
		}
		let found = b
			.accounts
			.get(&(bank_code.to_string(), account.expose().to_string()))
			.cloned();
		Ok(match found {
			Some(name) => AccountLookup::Name(HolderName::new(name)),
			None if b.sandbox => AccountLookup::Unsupported,
			None => AccountLookup::NotFound,
		})
	}

	async fn send(&self, req: PayoutRequest<'_>) -> Result<SendOutcome, RailError> {
		// The guard is dropped before any await, so the future stays `Send`.
		let (outcome, error_after, hang) = {
			let mut b = self.bank();
			b.drill()?;
			b.sends_seen += 1;
			if countdown(&mut b.faults.lost_sends) {
				b.save()?;
				return Err(RailError(
					"simulated timeout: the request never reached the bank".into(),
				));
			}
			let mut outcome = Self::deliver(&mut b, &req);
			if b.faults.duplicate_delivery {
				b.sends_seen += 1;
				outcome = Self::deliver(&mut b, &req);
			}
			let accepted = matches!(outcome, SendOutcome::Accepted { .. });
			let error_after = accepted && countdown(&mut b.faults.errors_after_paying);
			let hang = accepted && std::mem::take(&mut b.faults.hang_after_paying);
			b.save()?;
			(outcome, error_after, hang)
		};
		if hang {
			tracing::warn!("sandbox bank drill: paid, and now never answering");
			std::future::pending::<()>().await;
		}
		if error_after {
			return Err(RailError("simulated 502 after the payment was made".into()));
		}
		Ok(outcome)
	}

	async fn status(&self, key: &str) -> Result<PayoutStatus, RailError> {
		let mut b = self.bank();
		b.drill()?;
		let out = Self::status_of(&mut b, key);
		b.save()?;
		out
	}

	async fn balance(&self) -> Result<Option<Minor>, RailError> {
		let mut b = self.bank();
		b.drill()?;
		Ok(Some(Minor(b.balance)))
	}
}

impl SimulatedRail {
	fn status_of(b: &mut Bank, key: &str) -> Result<PayoutStatus, RailError> {
		if countdown(&mut b.faults.status_outages) {
			return Err(RailError("simulated status outage".into()));
		}
		if b.faults.status_unknown {
			return Ok(PayoutStatus::Unknown);
		}
		let Some(&i) = b.by_key.get(key) else {
			return Ok(PayoutStatus::NotFound);
		};
		if let Some(left) = b.pending_left.get_mut(key) {
			if countdown(left) {
				return Ok(PayoutStatus::Pending);
			}
		}
		let p = &b.payments[i];
		Ok(if p.reversed {
			PayoutStatus::Failed {
				reason: "the bank reversed the transfer".into(),
			}
		} else {
			PayoutStatus::Succeeded {
				reference: p.reference.clone(),
			}
		})
	}
}
