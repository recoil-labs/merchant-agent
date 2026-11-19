//! What the aggregator sends, parsed strictly.
//!
//! Invariant 13 (fail closed): a trade or quote with a field this agent does
//! not know, a state it does not know, or a money field that is not a whole
//! number of minor units is a **parse error**, never a default. The agent
//! stops rather than guess what a new field means for money.
//!
//! Shapes are pinned by `vectors/merchant-agent-api.json`, which the
//! aggregator's own `merchant_agent_contract` test checks against the real
//! response types (design §9.1).
//!
//! Chains are the one lenient type: they're only cross-checked against the
//! addresses pinned in config, never trusted, so a new registry field must
//! not stop payouts.

use chrono::{DateTime, Utc};
use serde::de::{self, Deserializer, IgnoredAny};
use serde::{Deserialize, Serialize};
use std::fmt;

use crate::money::Minor;
use crate::payee::Envelope;

/// A money field: a JSON **string** of digits. A number, a decimal or a
/// sign is refused. The aggregator always sends strings, and a JSON number
/// would silently lose precision above 2^53 in other clients.
fn minor_str<'de, D: Deserializer<'de>>(d: D) -> Result<Minor, D::Error> {
	struct V;
	impl de::Visitor<'_> for V {
		type Value = Minor;
		fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
			f.write_str("a string of digits (minor units)")
		}
		fn visit_str<E: de::Error>(self, s: &str) -> Result<Minor, E> {
			Minor::parse(s).map_err(E::custom)
		}
	}
	d.deserialize_str(V)
}

/// A rate: fiat (major units) per whole unit of the asset, as decimal text,
/// e.g. `1520.50`. Digits with at most one decimal point; the pricing code
/// (M7) reads it exactly, without floats.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(transparent)]
pub struct Rate(String);

impl Rate {
	pub fn parse(s: &str) -> Result<Self, String> {
		let (int, frac) = s.split_once('.').unwrap_or((s, ""));
		let ok = !int.is_empty()
			&& int.bytes().all(|b| b.is_ascii_digit())
			&& frac.bytes().all(|b| b.is_ascii_digit())
			&& !(s.contains('.') && frac.is_empty());
		if ok {
			Ok(Self(s.to_string()))
		} else {
			Err("a rate is digits with at most one decimal point".into())
		}
	}

	pub fn as_str(&self) -> &str {
		&self.0
	}
}

impl<'de> Deserialize<'de> for Rate {
	fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
		let s = String::deserialize(d)?;
		Rate::parse(&s).map_err(de::Error::custom)
	}
}

/// A field whose contents the agent never needs, and must not keep: it is
/// consumed and discarded while parsing. Personal data (an on-ramp payer's
/// name) or opaque provider evidence.
#[derive(Clone, Copy, PartialEq, Eq)]
pub struct Discarded;

impl<'de> Deserialize<'de> for Discarded {
	fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
		IgnoredAny::deserialize(d).map(|_| Discarded)
	}
}

impl fmt::Debug for Discarded {
	fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
		f.write_str("[discarded]")
	}
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Direction {
	Offramp,
	Onramp,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum TradeState {
	Quoted,
	AwaitingFiat,
	AwaitingConfirmation,
	Disputed,
	SettledToFiatPayer,
	RefundedToFunder,
	Failed,
}

impl TradeState {
	pub fn as_str(&self) -> &'static str {
		match self {
			Self::Quoted => "quoted",
			Self::AwaitingFiat => "awaiting_fiat",
			Self::AwaitingConfirmation => "awaiting_confirmation",
			Self::Disputed => "disputed",
			Self::SettledToFiatPayer => "settled_to_fiat_payer",
			Self::RefundedToFunder => "refunded_to_funder",
			Self::Failed => "failed",
		}
	}

	pub fn is_terminal(&self) -> bool {
		matches!(
			self,
			Self::SettledToFiatPayer | Self::RefundedToFunder | Self::Failed
		)
	}
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum ProofKind {
	Counterparty,
	Provider,
	Zktls,
}

/// The verified legal name on a trade (`payer_name`), as personal data.
///
/// Shared, not copied, when a trade is cloned, and redacted in `Debug`, so a
/// trade can still be cloned and logged with `?trade` without the name ever
/// being written anywhere. Read it with [`PayerName::name`].
#[derive(Clone)]
pub struct PayerName(std::sync::Arc<crate::pii::VerifiedName>);

impl PayerName {
	pub fn new(value: String) -> Self {
		Self(std::sync::Arc::new(crate::pii::VerifiedName::new(value)))
	}

	pub fn name(&self) -> &crate::pii::VerifiedName {
		&self.0
	}
}

impl fmt::Debug for PayerName {
	fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
		f.write_str("PayerName(<redacted>)")
	}
}

impl<'de> Deserialize<'de> for PayerName {
	fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
		let raw = zeroize::Zeroizing::new(String::deserialize(d)?);
		Ok(Self::new(String::clone(&raw)))
	}
}

/// One fiat trade, from either the operator's list (`OwnFiatTrade`, which
/// adds `payer_name`) or the single-trade read (`FiatTradeView`, which adds
/// `pay_to`). `pay_to` is on-ramp only and discarded. `payer_name` is the
/// seller's verified legal name on a cash-out that required a verified
/// seller (`""` when it can't be shown); on-ramp it is the payer's, which
/// this agent never uses.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Trade {
	pub id: String,
	pub quote_id: String,
	pub solver_id: String,
	pub direction: Direction,
	pub chain: String,
	pub asset: String,
	#[serde(deserialize_with = "minor_str")]
	pub asset_minor_units: Minor,
	pub fiat_currency: String,
	#[serde(deserialize_with = "minor_str")]
	pub fiat_minor_units: Minor,
	pub rail: String,
	pub rate: Rate,
	#[serde(deserialize_with = "minor_str")]
	pub fee_minor_units: Minor,
	pub user_address: String,
	pub solver_address: String,
	pub state: TradeState,
	pub deadline_at: Option<DateTime<Utc>>,
	pub resolution_note: Option<String>,
	pub payee_sealed: Option<Envelope>,
	pub payee_commitment: Option<String>,
	pub payment_reference: Option<String>,
	pub proof_kind: Option<ProofKind>,
	pub proof: Option<Discarded>,
	pub proof_payment_id: Option<String>,
	pub escrow_tx_hash: Option<String>,
	pub release_tx_hash: Option<String>,
	#[serde(default)]
	pub settled_at: Option<DateTime<Utc>>,
	pub created_at: DateTime<Utc>,
	pub updated_at: DateTime<Utc>,
	#[serde(default)]
	pub payer_name: Option<PayerName>,
	#[serde(default)]
	pub pay_to: Option<Discarded>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum KycTier {
	None,
	Basic,
	Full,
}

/// One quote in the operator's book, as stored.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Quote {
	pub id: String,
	pub solver_id: String,
	pub direction: Direction,
	pub chain: String,
	pub asset: String,
	pub asset_decimals: u8,
	pub fiat_currency: String,
	pub rail: String,
	pub rate: Rate,
	#[serde(deserialize_with = "minor_str")]
	pub fee_fixed: Minor,
	pub fee_bps: u16,
	#[serde(deserialize_with = "minor_str")]
	pub min_fiat: Minor,
	#[serde(deserialize_with = "minor_str")]
	pub max_fiat: Minor,
	pub payment_window_secs: u32,
	pub kyc_tier_required: KycTier,
	pub expiry: DateTime<Utc>,
	pub paused: bool,
	pub created_at: DateTime<Utc>,
	pub updated_at: DateTime<Utc>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct QuotesResponse {
	pub quotes: Vec<Quote>,
}

/// A quote as the operator submits it (`FiatQuoteInput`, camelCase).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct QuoteInput {
	pub direction: Direction,
	pub chain: String,
	pub asset: String,
	pub asset_decimals: u8,
	pub fiat_currency: String,
	pub rail: String,
	pub rate: Rate,
	pub fee_fixed: String,
	pub fee_bps: u16,
	pub min_fiat: String,
	pub max_fiat: String,
	pub payment_window_secs: u32,
	pub kyc_tier_required: KycTier,
	/// Unix seconds.
	pub expiry: i64,
	pub paused: bool,
}

impl From<&Quote> for QuoteInput {
	fn from(q: &Quote) -> Self {
		Self {
			direction: q.direction,
			chain: q.chain.clone(),
			asset: q.asset.clone(),
			asset_decimals: q.asset_decimals,
			fiat_currency: q.fiat_currency.clone(),
			rail: q.rail.clone(),
			rate: q.rate.clone(),
			fee_fixed: q.fee_fixed.to_string(),
			fee_bps: q.fee_bps,
			min_fiat: q.min_fiat.to_string(),
			max_fiat: q.max_fiat.to_string(),
			payment_window_secs: q.payment_window_secs,
			kyc_tier_required: q.kyc_tier_required,
			expiry: q.expiry.timestamp(),
			paused: q.paused,
		}
	}
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub(crate) struct PayeeKeyResponse {
	pub public_key: String,
}

/// A chain in RecoilPay's registry. Lenient: only these fields are read.
#[derive(Debug, Clone, Deserialize)]
pub struct ChainInfo {
	pub chain_id: u64,
	#[serde(default)]
	pub family: ChainFamily,
	pub name: String,
	#[serde(default)]
	pub fiat_escrow: String,
	pub tokens: Vec<TokenInfo>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ChainFamily {
	#[default]
	Evm,
	Solana,
}

#[derive(Debug, Clone, Deserialize)]
pub struct TokenInfo {
	pub symbol: String,
	pub address: String,
	pub decimals: u8,
}

#[derive(Debug, Deserialize)]
pub(crate) struct ChainsResponse {
	pub data: Vec<ChainInfo>,
}

/// The aggregator's error body. Lenient: it is only shown to people.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct ErrorBody {
	#[serde(default)]
	pub error: String,
	#[serde(default)]
	pub message: String,
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn rates_are_plain_decimals() {
		for ok in ["1520", "1520.50", "0.0001"] {
			assert!(Rate::parse(ok).is_ok(), "{ok}");
		}
		for bad in ["", "1520.", ".5", "-1", "1,520", "1e3", "1.2.3", " 1"] {
			assert!(Rate::parse(bad).is_err(), "{bad}");
		}
	}
}
