//! The account holder's name, normalised exactly as the client does.
//!
//! `payee.ts` normalises with `name.normalize('NFC').trim().replace(/\s+/g, ' ')`,
//! then requires 1 to 140 by `.length`. Three JavaScript details matter, and
//! Rust's own helpers get each one wrong:
//!
//! - **`trim` and `\s` use ECMAScript's whitespace set**, not Unicode's
//!   `White_Space`. ECMAScript includes U+FEFF (BOM) and excludes U+0085
//!   (NEL); `char::is_whitespace` does the opposite on both.
//! - **`.length` counts UTF-16 code units**, so an emoji counts as two.
//! - **NFC** comes from `unicode-normalization`, as `String.normalize` does.
//!
//! The `sealed` vectors pin each of these.

use unicode_normalization::UnicodeNormalization;
use zeroize::Zeroizing;

use crate::pii::HolderName;

pub const MAX_NAME_UTF16_UNITS: usize = 140;

#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum NameError {
	#[error("the account holder name is required")]
	Empty,
	#[error("the account holder name is too long")]
	TooLong,
}

/// ECMAScript `WhiteSpace` ∪ `LineTerminator`: what `\s` and `trim()` match.
pub fn is_js_whitespace(c: char) -> bool {
	matches!(
		c,
		'\u{0009}'
			| '\u{000A}'
			| '\u{000B}'
			| '\u{000C}'
			| '\u{000D}'
			| '\u{0020}'
			| '\u{00A0}'
			| '\u{1680}'
			| '\u{2000}'
			..='\u{200A}'
				| '\u{2028}' | '\u{2029}'
				| '\u{202F}' | '\u{205F}'
				| '\u{3000}' | '\u{FEFF}'
	)
}

pub fn normalise_name(raw: &str) -> Result<HolderName, NameError> {
	let nfc: Zeroizing<String> = Zeroizing::new(raw.nfc().collect());
	let trimmed = nfc.trim_matches(is_js_whitespace);
	let mut out = Zeroizing::new(String::with_capacity(trimmed.len()));
	let mut in_space = false;
	for c in trimmed.chars() {
		if is_js_whitespace(c) {
			if !in_space {
				out.push(' ');
			}
			in_space = true;
		} else {
			out.push(c);
			in_space = false;
		}
	}
	if out.is_empty() {
		return Err(NameError::Empty);
	}
	if out.encode_utf16().count() > MAX_NAME_UTF16_UNITS {
		return Err(NameError::TooLong);
	}
	Ok(HolderName::new(std::mem::take(&mut *out)))
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn whitespace_set_is_ecmascripts() {
		assert!(is_js_whitespace('\u{FEFF}'));
		assert!(!is_js_whitespace('\u{0085}'));
		assert!(is_js_whitespace('\u{00A0}'));
		assert!(!is_js_whitespace('a'));
	}

	#[test]
	fn collapses_and_trims() {
		let n = normalise_name("  Test\u{00A0}\u{00A0}Payee\u{3000}").unwrap();
		assert_eq!(n.expose(), "Test Payee");
	}

	#[test]
	fn counts_utf16_units() {
		assert!(normalise_name(&"\u{1F600}".repeat(70)).is_ok());
		assert_eq!(
			normalise_name(&"\u{1F600}".repeat(71)).err(),
			Some(NameError::TooLong)
		);
	}
}
