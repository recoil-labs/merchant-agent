//! Money is whole minor units (kobo for naira), held as `u128`.
//!
//! Never a float, anywhere: a float cannot hold most decimal amounts exactly,
//! and a rounding error in a payout is a payout to the wrong amount. The
//! workspace denies `clippy::float_arithmetic` so a float cannot creep in.

use serde::de::{self, Deserializer, Visitor};
use serde::Deserialize;
use std::fmt;

/// An amount in minor units of a currency.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Default)]
pub struct Minor(pub u128);

impl Minor {
	/// Parse a whole number of minor units: ASCII digits only, no sign, no
	/// separators, no decimal point.
	pub fn parse(s: &str) -> Result<Self, String> {
		if s.is_empty() || !s.bytes().all(|b| b.is_ascii_digit()) {
			return Err("must be a whole number of minor units, digits only".into());
		}
		s.parse::<u128>()
			.map(Minor)
			.map_err(|_| "is too large".into())
	}

	pub fn get(self) -> u128 {
		self.0
	}
}

impl fmt::Display for Minor {
	fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
		write!(f, "{}", self.0)
	}
}

/// Accepts a YAML string of digits (`"5000000"`) or a plain integer
/// (`5000000`). Refuses anything with a decimal point, because YAML would
/// read `5000000.0` as a float.
impl<'de> Deserialize<'de> for Minor {
	fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
		struct V;
		impl Visitor<'_> for V {
			type Value = Minor;
			fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
				f.write_str("a whole number of minor units, such as \"5000000\"")
			}
			fn visit_str<E: de::Error>(self, s: &str) -> Result<Minor, E> {
				Minor::parse(s).map_err(E::custom)
			}
			fn visit_u64<E: de::Error>(self, n: u64) -> Result<Minor, E> {
				Ok(Minor(n.into()))
			}
			fn visit_i64<E: de::Error>(self, n: i64) -> Result<Minor, E> {
				u128::try_from(n)
					.map(Minor)
					.map_err(|_| E::custom("an amount cannot be negative"))
			}
		}
		d.deserialize_any(V)
	}
}

/// `minor` naira as a person reads it: `₦50,000.00`.
pub fn naira(minor: Minor) -> String {
	let whole = (minor.0 / 100).to_string();
	let mut grouped = String::new();
	for (i, c) in whole.chars().enumerate() {
		if i > 0 && (whole.len() - i).is_multiple_of(3) {
			grouped.push(',');
		}
		grouped.push(c);
	}
	format!("₦{grouped}.{:02}", minor.0 % 100)
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn parses_digits_only() {
		assert_eq!(Minor::parse("5000000"), Ok(Minor(5_000_000)));
		for bad in ["", "-1", "1.5", "1,000", "+5", " 5", "1e6"] {
			assert!(Minor::parse(bad).is_err(), "{bad:?}");
		}
	}

	#[test]
	fn yaml_floats_are_refused() {
		#[derive(Deserialize)]
		struct W {
			#[allow(dead_code)]
			a: Minor,
		}
		assert!(serde_yaml_ng::from_str::<W>("a: 5000000.0").is_err());
		assert!(serde_yaml_ng::from_str::<W>("a: -5").is_err());
		assert!(serde_yaml_ng::from_str::<W>("a: \"5000000\"").is_ok());
		assert!(serde_yaml_ng::from_str::<W>("a: 5000000").is_ok());
	}

	#[test]
	fn formats_naira() {
		assert_eq!(naira(Minor(5_000_000)), "₦50,000.00");
		assert_eq!(naira(Minor(15_081_152)), "₦150,811.52");
		assert_eq!(naira(Minor(5)), "₦0.05");
	}
}
