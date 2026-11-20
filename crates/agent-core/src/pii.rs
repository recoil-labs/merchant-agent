//! Personal data: bank account numbers and account holders' names.
//!
//! Invariant 7: personal data is decrypted in memory, used, and dropped. It
//! never reaches logs, metrics, alerts, crash reports, the audit log or the
//! database. The redaction layer in [`crate::logging`] is a backstop; this
//! module is the rule.
//!
//! **Personal-data types implement neither `Debug`, `Display`, `Serialize`
//! nor `Clone`.** So `tracing::info!(?account)`, `format!("{account}")`,
//! `serde_json::to_string(&account)` and quiet copies do not compile. The
//! only way to the value is a method named `expose`, which is easy to find in
//! review. Values are zeroed when dropped. The tests assert the missing
//! impls at compile time.

use zeroize::Zeroizing;

/// A bank account number (a NUBAN in Nigeria).
pub struct AccountNumber(Zeroizing<String>);

impl AccountNumber {
	pub fn new(value: String) -> Self {
		Self(Zeroizing::new(value))
	}

	/// The value itself. Only for the bank adapter's request body.
	pub fn expose(&self) -> &str {
		&self.0
	}

	/// The last four characters behind stars, `******6789`: the only form
	/// the agent stores, logs or alerts.
	pub fn masked(&self) -> String {
		mask(&self.0)
	}
}

/// An account holder's name.
pub struct HolderName(Zeroizing<String>);

impl HolderName {
	pub fn new(value: String) -> Self {
		Self(Zeroizing::new(value))
	}

	/// The value itself. Only for the name check against the bank's lookup.
	pub fn expose(&self) -> &str {
		&self.0
	}
}

/// A seller's verified legal name, sent by the aggregator on a cash-out that
/// required a verified seller (`payer_name`). Personal data like the rest:
/// compared with the bank's name for the account, then dropped. Never
/// logged, alerted, audited or stored.
pub struct VerifiedName(Zeroizing<String>);

impl VerifiedName {
	pub fn new(value: String) -> Self {
		Self(Zeroizing::new(value))
	}

	/// The value itself. Only for the name check against the bank's lookup.
	pub fn expose(&self) -> &str {
		&self.0
	}

	/// The aggregator sends `""` when the trade required a verified seller
	/// but the name can't be shown: never pay then.
	pub fn is_unavailable(&self) -> bool {
		self.0.trim().is_empty()
	}
}

/// Stars for everything but the last four characters. Short values are
/// starred entirely: four characters of a five-character value would reveal
/// almost all of it.
pub fn mask(value: &str) -> String {
	let n = value.chars().count();
	if n <= 6 {
		return "*".repeat(n.max(4));
	}
	let tail: String = value.chars().skip(n - 4).collect();
	format!("{}{tail}", "*".repeat(n - 4))
}

#[cfg(test)]
mod tests {
	use super::*;
	use static_assertions::assert_not_impl_any;
	use std::fmt::{Debug, Display};

	assert_not_impl_any!(AccountNumber: Debug, Display, Clone, serde::Serialize);
	assert_not_impl_any!(HolderName: Debug, Display, Clone, serde::Serialize);
	assert_not_impl_any!(VerifiedName: Debug, Display, Clone, serde::Serialize);

	#[test]
	fn masks_all_but_the_last_four() {
		assert_eq!(
			AccountNumber::new("0123456789".into()).masked(),
			"******6789"
		);
		assert_eq!(mask("12345"), "*****");
		assert_eq!(mask(""), "****");
	}
}
