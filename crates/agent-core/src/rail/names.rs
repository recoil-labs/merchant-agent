//! Does the name the customer gave match the name the bank holds?
//!
//! Banks report names in their own format: upper case, no accents,
//! surname first ("OSAHON GINO" for "Gino Osahon"). So both names are
//! reduced to words before comparing: accents are stripped (NFD, combining
//! marks dropped), case is folded, and anything that isn't a letter or digit
//! separates words.
//!
//! - `strict`: the same words in the same order.
//! - `token_set`: the same words, in any order.
//! - `off`: no check (sandbox only; config refuses it in production).
//!
//! A mismatch holds the trade (invariant 4); a person can look and approve.

use unicode_normalization::UnicodeNormalization;

use crate::config::NameMatch;

/// The words of a name, normalised.
pub fn words(name: &str) -> Vec<String> {
	let folded: String = name
		.nfd()
		.filter(|c| !unicode_normalization::char::is_combining_mark(*c))
		.flat_map(char::to_uppercase)
		.collect();
	folded
		.split(|c: char| !c.is_alphanumeric())
		.filter(|w| !w.is_empty())
		.map(str::to_string)
		.collect()
}

pub fn matches(policy: NameMatch, customer: &str, bank: &str) -> bool {
	let (a, b) = (words(customer), words(bank));
	match policy {
		NameMatch::Off => true,
		_ if a.is_empty() || b.is_empty() => false,
		NameMatch::Strict => a == b,
		NameMatch::TokenSet => {
			let (mut a, mut b) = (a, b);
			a.sort();
			b.sort();
			a == b
		}
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn banks_format_names_their_own_way() {
		assert!(matches(NameMatch::TokenSet, "Gino Osahon", "OSAHON GINO"));
		assert!(matches(NameMatch::TokenSet, "Adébáyọ̀  Ọlá", "ADEBAYO OLA"));
		assert!(matches(
			NameMatch::TokenSet,
			"O'Brien-Smith, Ann",
			"ANN O BRIEN SMITH"
		));
		assert!(matches(NameMatch::Strict, "Gino Osahon", "GINO OSAHON"));
		assert!(!matches(NameMatch::Strict, "Gino Osahon", "OSAHON GINO"));
	}

	#[test]
	fn different_people_do_not_match() {
		assert!(!matches(NameMatch::TokenSet, "Gino Osahon", "OSAHON GINA"));
		assert!(!matches(
			NameMatch::TokenSet,
			"Gino Osahon",
			"GINO OSAHON PETER"
		));
		assert!(!matches(NameMatch::TokenSet, "Gino", ""));
		assert!(!matches(NameMatch::Strict, "", ""));
		assert!(matches(NameMatch::Off, "anyone", "else"));
	}
}
