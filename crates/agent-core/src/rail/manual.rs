//! The manual rail: a person pays from the merchant's bank app, and the
//! agent does everything else.
//!
//! "Sending" opens a task in the exception queue. A person reveals the bank
//! details on purpose ([`crate::engine::Engine::reveal`]; every reveal is
//! counted, and the console hides them again after 60 seconds), pays, and
//! enters the bank's reference ([`crate::engine::Engine::manual_paid`]).
//! The agent then reports it and follows the trade to the end, exactly as
//! for an automatic rail.
//!
//! Nothing personal is stored: a task holds the trade, the amount and the
//! key. The details are re-opened from the trade at each reveal.
//!
//! This rail can't look up account names or read the balance, so the name
//! check and the float check are the person's to make in their bank app.

use std::sync::Arc;

use super::{AccountLookup, PayoutRail, PayoutRequest, RailError};
use crate::engine::Clock;
use crate::machine::{PayoutStatus, SendOutcome};
use crate::money::Minor;
use crate::pii::AccountNumber;
use crate::store::Store;

pub struct ManualRail<C> {
	store: Arc<Store>,
	clock: C,
}

impl<C: Clock> ManualRail<C> {
	pub fn new(store: Arc<Store>, clock: C) -> Self {
		Self { store, clock }
	}
}

fn store_err(e: crate::store::StoreError) -> RailError {
	RailError(format!("the task queue is unavailable: {e}"))
}

impl<C: Clock> PayoutRail for ManualRail<C> {
	/// A task is opened at most once per key, so a repeated send is safe.
	fn idempotent_send(&self) -> bool {
		true
	}

	async fn verify_account(
		&self,
		_bank_code: &str,
		_account: &AccountNumber,
	) -> Result<AccountLookup, RailError> {
		Ok(AccountLookup::Unsupported)
	}

	async fn send(&self, req: PayoutRequest<'_>) -> Result<SendOutcome, RailError> {
		self.store
			.open_manual_task(
				req.idempotency_key,
				req.trade_id,
				req.amount,
				self.clock.now(),
			)
			.map_err(store_err)?;
		Ok(SendOutcome::Queued)
	}

	async fn status(&self, key: &str) -> Result<PayoutStatus, RailError> {
		let task = self.store.manual_task(key).map_err(store_err)?;
		Ok(match task {
			None => PayoutStatus::NotFound,
			Some(t) => match (t.status.as_str(), t.reference) {
				("paid", Some(reference)) => PayoutStatus::Succeeded { reference },
				("failed", _) => PayoutStatus::Failed {
					reason: "the person paying could not complete it".into(),
				},
				_ => PayoutStatus::Pending,
			},
		})
	}

	async fn balance(&self) -> Result<Option<Minor>, RailError> {
		Ok(None)
	}
}
