//! Policy: the checks that decide whether a verified trade may be paid.
//!
//! - [`pricing`]: the naira is recomputed from the merchant's own offer and
//!   the on-chain lock, and the offer's rate is near the merchant's
//!   reference rate (invariant 3).
//! - [`limits`]: per trade, per day, per payee per day, and velocity,
//!   checked atomically with the payout intent (invariant 5).
//! - [`screening`]: the payee is not on the merchant's deny-list.
//!
//! [`PolicyChecks`] runs them all, after the payee's details are opened and
//! matched to the trade's commitment (invariant 4). The deadline guard
//! (invariant 6) and the float check live in the engine, next to the
//! transitions they guard.

pub mod limits;
pub mod pricing;
pub mod screening;

use std::sync::Arc;

use crate::aggregator::types::Trade;
use crate::engine::{Checked, Checks, PayeeChecks};
use crate::machine::HoldReason;
use crate::money::Minor;
use crate::payee::{payee_tag, OpenedPayee};
use crate::store::{Store, TradeRecord};
use pricing::{check_amount, check_rate, Claimed};
use screening::{ScreenResult, Screening};

/// Every check the agent makes before it reserves a payout.
pub struct PolicyChecks<S> {
	pub payee: PayeeChecks,
	pub store: Arc<Store>,
	pub screening: S,
	pub amount_tolerance_bps: u32,
	pub max_rate_deviation_bps: u32,
}

impl<S: Screening> Checks for PolicyChecks<S> {
	async fn check(&self, trade: &Trade, record: &TradeRecord) -> Result<Checked, HoldReason> {
		let opened = self.payee.open(trade, record)?;
		let tag = payee_tag(&self.payee.pepper, &opened.identity);
		let waived = |r: HoldReason| record.overrides.contains(&r);

		if !waived(HoldReason::Screening) {
			if let ScreenResult::Hold(why) =
				self.screening.screen(&tag, &opened.identity, &opened.name)
			{
				tracing::warn!(trade_id = %record.trade_id, reason = %why, "payee held by screening");
				return Err(HoldReason::Screening);
			}
		}

		if !waived(HoldReason::UnknownQuote) {
			let terms = self
				.store
				.quote(&record.quote_id)
				.map_err(|_| HoldReason::UnknownQuote)?
				.ok_or(HoldReason::UnknownQuote)?;
			if !waived(HoldReason::RateOutOfBand) {
				check_rate(&terms, self.max_rate_deviation_bps)?;
			}
			if !waived(HoldReason::AmountMismatch) {
				let lock_fee = record
					.lock_protocol_fee
					.as_deref()
					.and_then(|f| Minor::parse(f).ok())
					.ok_or(HoldReason::AmountMismatch)?;
				let claimed = Claimed {
					chain: &record.chain,
					rail: &trade.rail,
					fiat_currency: &trade.fiat_currency,
					rate: &record.rate,
					asset: record.asset_minor,
					fiat: record.fiat_minor,
					fee: record.fee_minor,
					lock_protocol_fee: lock_fee,
				};
				check_amount(&claimed, &terms, self.amount_tolerance_bps)?;
			}
		}

		Ok(Checked {
			payee_tag: tag,
			account_masked: opened.identity.account.masked(),
		})
	}

	fn open(&self, trade: &Trade, record: &TradeRecord) -> Result<OpenedPayee, HoldReason> {
		self.payee.open(trade, record)
	}
}
