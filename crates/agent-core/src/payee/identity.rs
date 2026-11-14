//! The payee's account identity and its commitment.
//!
//! The same rules as the client (`@recoilpay/intent-core`,
//! `src/fiat/payee.ts`) and the aggregator (`oif_types::fiat_trades::payee`),
//! copied rather than depended on (design §2) and pinned to both by the
//! `commitments` and `invalid` cases in `vectors/fiat-payee.json`:
//!
//! - ASCII whitespace (space, tab, CR, LF) is removed from every field, and
//!   `-` from the account.
//! - The character set is checked before case is changed, and only ASCII
//!   passes.
//! - The canonical form is compact JSON with keys sorted; the character sets
//!   mean no value ever needs escaping.
//!
//! The difference from the aggregator's copy: the account number is held as
//! [`AccountNumber`], so an identity cannot be printed or copied.

use sha3::{Digest, Keccak256};
use zeroize::Zeroizing;

use crate::pii::AccountNumber;

pub const SALT_BYTES: usize = 32;

/// Which identity field was invalid. Never carries the value.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum IdentityError {
	#[error("invalid rail")]
	Rail,
	#[error("invalid bank code")]
	BankCode,
	#[error("invalid account number")]
	Account,
}

/// Where the money goes, normalised.
pub struct PayeeIdentity {
	pub rail: String,
	pub bank_code: String,
	pub account: AccountNumber,
}

fn strip_ascii_space(s: &str) -> Zeroizing<String> {
	Zeroizing::new(
		s.chars()
			.filter(|c| !matches!(c, ' ' | '\t' | '\r' | '\n'))
			.collect(),
	)
}

fn ascii_alnum(s: &str) -> bool {
	s.chars().all(|c| c.is_ascii_alphanumeric())
}

/// `[A-Za-z]{2}\.[A-Za-z0-9_]{1,30}`
fn valid_rail(s: &str) -> bool {
	let Some((country, rail)) = s.split_once('.') else {
		return false;
	};
	country.len() == 2
		&& country.chars().all(|c| c.is_ascii_alphabetic())
		&& (1..=30).contains(&rail.len())
		&& rail.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
}

/// `\+?[A-Za-z0-9]{1,34}`
fn valid_account(s: &str) -> bool {
	let digits = s.strip_prefix('+').unwrap_or(s);
	(1..=34).contains(&digits.len()) && ascii_alnum(digits)
}

impl PayeeIdentity {
	pub fn new(rail: &str, bank_code: &str, account: &str) -> Result<Self, IdentityError> {
		let rail = strip_ascii_space(rail);
		let bank_code = strip_ascii_space(bank_code);
		let account: Zeroizing<String> = Zeroizing::new(
			strip_ascii_space(account)
				.chars()
				.filter(|c| *c != '-')
				.collect(),
		);

		if !valid_rail(&rail) {
			return Err(IdentityError::Rail);
		}
		if bank_code.len() > 16 || !ascii_alnum(&bank_code) {
			return Err(IdentityError::BankCode);
		}
		if !valid_account(&account) {
			return Err(IdentityError::Account);
		}
		Ok(Self {
			rail: rail.to_ascii_lowercase(),
			bank_code: bank_code.to_ascii_uppercase(),
			account: AccountNumber::new(account.to_ascii_uppercase()),
		})
	}

	/// `{"account":…,"bank_code":…,"rail":…}`. Holds the account number, so
	/// it is zeroed on drop.
	pub fn canonical(&self) -> Zeroizing<String> {
		Zeroizing::new(format!(
			r#"{{"account":"{}","bank_code":"{}","rail":"{}"}}"#,
			self.account.expose(),
			self.bank_code,
			self.rail
		))
	}

	/// `keccak256(canonical ‖ salt)`: the trade's `payee_commitment`.
	pub fn commitment(&self, salt: &[u8; SALT_BYTES]) -> [u8; 32] {
		let mut h = Keccak256::new();
		h.update(self.canonical().as_bytes());
		h.update(salt);
		h.finalize().into()
	}
}
