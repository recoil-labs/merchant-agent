//! Local limits (invariant 5), enforced inside the agent whatever the
//! aggregator says.
//!
//! [`check`] runs **inside the write-ahead intent's SQLite transaction**
//! ([`crate::store::Store::record_intent`]), which is an immediate write
//! transaction: only one runs at a time. So two trades processed together
//! can't both see room under a limit that only fits one. What counts is
//! every payout written ahead and not cancelled, including those still
//! with the bank.
//!
//! The limits day runs midnight to midnight Lagos time (WAT, UTC+1, no
//! daylight saving), the day a Nigerian merchant's bank uses.

use chrono::{DateTime, Duration, FixedOffset, Utc};
use rusqlite::{params, Transaction};

use crate::config::Limits;
use crate::machine::HoldReason;
use crate::store::{Intent, StoreResult};

/// The limits day a moment falls in: `YYYY-MM-DD` in Lagos.
pub fn day_of(now: DateTime<Utc>) -> String {
	let lagos = FixedOffset::east_opt(3600).expect("UTC+1 is a valid offset");
	now.with_timezone(&lagos).date_naive().to_string()
}

/// Would this payout go over a limit? Checked in this order, so the reason
/// given is the most specific: the trade itself, then the payee, then the
/// day, then the pace. A limit a person approved away is skipped.
pub fn check(
	tx: &Transaction<'_>,
	limits: &Limits,
	waived: &[HoldReason],
	intent: &Intent,
	now: DateTime<Utc>,
) -> StoreResult<Result<(), HoldReason>> {
	let amount = intent.amount.get();
	let on = |r: HoldReason| !waived.contains(&r);

	if on(HoldReason::OverTradeLimit) && amount > limits.per_trade.get() {
		return Ok(Err(HoldReason::OverTradeLimit));
	}

	let sum = |sql: &str, args: &[&dyn rusqlite::ToSql]| -> StoreResult<u128> {
		let mut stmt = tx.prepare(sql)?;
		let amounts = stmt
			.query_map(args, |r| r.get::<_, String>(0))?
			.collect::<Result<Vec<_>, _>>()?;
		Ok(amounts
			.iter()
			.map(|a| a.parse::<u128>().unwrap_or(u128::MAX))
			.fold(0u128, u128::saturating_add))
	};

	if on(HoldReason::OverPayeeLimit) {
		let payee_today = sum(
			"SELECT amount_minor FROM payouts WHERE day = ?1 AND payee_tag = ?2 AND status != 'cancelled'",
			&[&intent.day, &intent.payee_tag],
		)?;
		if payee_today.saturating_add(amount) > limits.per_payee_per_day.get() {
			return Ok(Err(HoldReason::OverPayeeLimit));
		}
	}

	if on(HoldReason::OverDailyLimit) {
		let today = sum(
			"SELECT amount_minor FROM payouts WHERE day = ?1 AND status != 'cancelled'",
			&[&intent.day],
		)?;
		if today.saturating_add(amount) > limits.per_day.get() {
			return Ok(Err(HoldReason::OverDailyLimit));
		}
	}

	if on(HoldReason::OverVelocity) {
		let since = now - Duration::seconds(limits.velocity_window_secs as i64);
		let since = since.to_rfc3339_opts(chrono::SecondsFormat::Micros, true);
		let recent: u32 = tx.query_row(
			"SELECT COUNT(*) FROM payouts WHERE created_at > ?1 AND status != 'cancelled'",
			params![since],
			|r| r.get(0),
		)?;
		if recent + 1 > limits.velocity_max_payouts {
			return Ok(Err(HoldReason::OverVelocity));
		}
	}

	Ok(Ok(()))
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn the_limits_day_is_lagos_time() {
		let late_utc: DateTime<Utc> = "2026-11-13T23:30:00Z".parse().unwrap();
		assert_eq!(
			day_of(late_utc),
			"2026-11-14",
			"00:30 in Lagos is the next day"
		);
		let early_utc: DateTime<Utc> = "2026-11-13T22:59:59Z".parse().unwrap();
		assert_eq!(day_of(early_utc), "2026-11-13");
	}
}
