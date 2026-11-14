//! Recomputing a trade's naira (invariant 3).
//!
//! The aggregator says how much naira to pay. The agent works it out again
//! from three things it trusts:
//!
//! - the quote's terms, from **its own history** of the merchant's book;
//! - the **lock's protocol fee**, read on-chain (M4);
//! - the trade's crypto amount, which the on-chain lock confirmed.
//!
//! The formula is the aggregator's (design §2, `FiatQuote::price_offramp`),
//! pinned to it by `vectors/fiat-pricing.json`:
//!
//! ```text
//! convertible = asset − protocol fee
//! gross = floor(convertible × R × 10^2 / 10^(decimals + s))   (rate = R with s decimals)
//! fee   = fee_fixed + ceil(gross × fee_bps / 10 000)
//! net   = gross − fee       (min_fiat ≤ gross ≤ max_fiat, fee < gross)
//! ```
//!
//! All integer arithmetic, all checked. An overflow refuses to price.

use crate::machine::HoldReason;
use crate::money::Minor;

/// A quote's terms, as the agent recorded them when it first saw the quote.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct QuoteTerms {
	pub quote_id: String,
	pub chain: String,
	pub asset: String,
	pub asset_decimals: u8,
	pub fiat_currency: String,
	pub rail: String,
	pub rate: String,
	pub fee_fixed: Minor,
	pub fee_bps: u16,
	pub min_fiat: Minor,
	pub max_fiat: Minor,
	/// The merchant's reference rate for the rail when the quote was seen.
	pub reference_rate: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Price {
	pub gross: u128,
	pub fee: u128,
	pub net: u128,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PriceError {
	OutOfBand,
	NothingLeft,
	Unpriceable,
}

/// Digits with at most one decimal point: `(digits as one integer, number
/// of decimal places)`.
pub fn parse_rate(rate: &str) -> Option<(u128, u32)> {
	let (int, frac) = rate.split_once('.').unwrap_or((rate, ""));
	let digits: String = [int, frac].concat();
	if int.is_empty() || digits.is_empty() || !digits.chars().all(|c| c.is_ascii_digit()) {
		return None;
	}
	Some((digits.parse().ok()?, frac.len() as u32))
}

/// Minor units per major unit, as a power of ten. Naira only in v1.
pub fn fiat_minor_exponent(currency: &str) -> Option<u32> {
	match currency {
		"NGN" => Some(2),
		_ => None,
	}
}

pub fn price_offramp(terms: &QuoteTerms, convertible: u128) -> Result<Price, PriceError> {
	use PriceError::*;
	let e = fiat_minor_exponent(&terms.fiat_currency).ok_or(Unpriceable)?;
	let (r, s) = parse_rate(&terms.rate).ok_or(Unpriceable)?;
	let ten = |p: u32| 10u128.checked_pow(p).ok_or(Unpriceable);
	let product = convertible.checked_mul(r).ok_or(Unpriceable)?;
	let down = terms.asset_decimals as u32 + s;
	let gross = if down >= e {
		product / ten(down - e)?
	} else {
		product.checked_mul(ten(e - down)?).ok_or(Unpriceable)?
	};
	if gross < terms.min_fiat.get() || gross > terms.max_fiat.get() {
		return Err(OutOfBand);
	}
	let proportional = gross
		.checked_mul(terms.fee_bps as u128)
		.ok_or(Unpriceable)?
		.div_ceil(10_000);
	let fee = terms
		.fee_fixed
		.get()
		.checked_add(proportional)
		.ok_or(Unpriceable)?;
	if fee >= gross {
		return Err(NothingLeft);
	}
	Ok(Price {
		gross,
		fee,
		net: gross - fee,
	})
}

/// What the trade claims, for the check.
#[derive(Debug, Clone, Copy)]
pub struct Claimed<'a> {
	pub chain: &'a str,
	pub rail: &'a str,
	pub fiat_currency: &'a str,
	pub rate: &'a str,
	pub asset: Minor,
	pub fiat: Minor,
	pub fee: Minor,
	/// The lock's protocol fee, read on-chain.
	pub lock_protocol_fee: Minor,
}

/// The trade's naira must be what the merchant's own offer gives for the
/// crypto actually locked: equal, or within `tolerance_bps` of the net.
pub fn check_amount(
	claimed: &Claimed<'_>,
	terms: &QuoteTerms,
	tolerance_bps: u32,
) -> Result<Price, HoldReason> {
	let mismatch = Err(HoldReason::AmountMismatch);
	if claimed.chain != terms.chain
		|| claimed.rail != terms.rail
		|| claimed.fiat_currency != terms.fiat_currency
		|| claimed.rate != terms.rate
	{
		return mismatch;
	}
	let Some(convertible) = claimed
		.asset
		.get()
		.checked_sub(claimed.lock_protocol_fee.get())
	else {
		return mismatch;
	};
	let Ok(price) = price_offramp(terms, convertible) else {
		return mismatch;
	};
	let allowed = price.net.saturating_mul(tolerance_bps as u128) / 10_000;
	let off = price.net.abs_diff(claimed.fiat.get());
	if off > allowed || (tolerance_bps == 0 && claimed.fee.get() != price.fee) {
		return mismatch;
	}
	Ok(price)
}

/// The quote's rate must be within `max_deviation_bps` of the merchant's
/// reference rate for its rail. No reference rate: no check.
pub fn check_rate(terms: &QuoteTerms, max_deviation_bps: u32) -> Result<(), HoldReason> {
	let Some(reference) = &terms.reference_rate else {
		return Ok(());
	};
	let out = Err(HoldReason::RateOutOfBand);
	let (Some((a, sa)), Some((b, sb))) = (parse_rate(&terms.rate), parse_rate(reference)) else {
		return out;
	};
	// Both to the same number of decimal places, then compare as integers.
	let s = sa.max(sb);
	let scale = |n: u128, from: u32| 10u128.checked_pow(s - from).and_then(|p| n.checked_mul(p));
	let (Some(a), Some(b)) = (scale(a, sa), scale(b, sb)) else {
		return out;
	};
	if b == 0 {
		return out;
	}
	let (Some(lhs), Some(rhs)) = (
		a.abs_diff(b).checked_mul(10_000),
		b.checked_mul(max_deviation_bps as u128),
	) else {
		return out;
	};
	if lhs <= rhs {
		Ok(())
	} else {
		out
	}
}

#[cfg(test)]
mod tests {
	use super::*;
	use serde_json::Value;

	const VECTOR: &str = include_str!("../../../../vectors/fiat-pricing.json");

	fn terms(q: &Value) -> QuoteTerms {
		let s = |k: &str| q[k].as_str().unwrap().to_string();
		let m = |k: &str| Minor::parse(q[k].as_str().unwrap()).unwrap();
		QuoteTerms {
			quote_id: "fq-vector".into(),
			chain: "eip155:84532".into(),
			asset: "USDC".into(),
			asset_decimals: q["assetDecimals"].as_u64().unwrap() as u8,
			fiat_currency: s("fiatCurrency"),
			rail: "ng.nip".into(),
			rate: s("rate"),
			fee_fixed: m("feeFixed"),
			fee_bps: q["feeBps"].as_u64().unwrap() as u16,
			min_fiat: m("minFiat"),
			max_fiat: m("maxFiat"),
			reference_rate: None,
		}
	}

	#[test]
	fn prices_match_the_shared_vector() {
		let v: Value = serde_json::from_str(VECTOR).unwrap();
		for c in v["cases"].as_array().unwrap() {
			let name = c["name"].as_str().unwrap();
			let convertible: u128 = c["convertible"].as_str().unwrap().parse().unwrap();
			let expect = &c["expect"];
			match (
				price_offramp(&terms(&c["quote"]), convertible),
				expect["error"].as_str(),
			) {
				(Ok(p), None) => {
					assert_eq!(
						p.gross.to_string(),
						expect["gross"].as_str().unwrap(),
						"{name}"
					);
					assert_eq!(p.fee.to_string(), expect["fee"].as_str().unwrap(), "{name}");
					assert_eq!(p.net.to_string(), expect["net"].as_str().unwrap(), "{name}");
				}
				(Err(PriceError::OutOfBand), Some("out_of_band"))
				| (Err(PriceError::NothingLeft), Some("nothing_left")) => {}
				(got, want) => panic!("{name}: got {got:?}, want {want:?}"),
			}
		}
	}
}
