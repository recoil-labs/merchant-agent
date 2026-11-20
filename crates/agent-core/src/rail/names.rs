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

/// How a bank's name for an account compares with a seller's verified legal
/// name: RecoilPay's shared rule (`oif_types::seller_kyc::names_match`,
/// `namesMatch` in the intent kit), pinned by `vectors/seller-names.json`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SellerNameMatch {
	/// The same words, in any order.
	Match,
	/// Plausibly the same person (a missing or extra middle name, initials):
	/// a person checks.
	Unsure,
	Mismatch,
}

/// Dropped before comparing, when a name has other words left.
/// Mirrors `titles` in `vectors/seller-names.json`.
pub const NAME_TITLES: &[&str] = &[
	"MR", "MRS", "MS", "MISS", "MASTER", "DR", "PROF", "ENGR", "ENG", "CHIEF", "ALHAJI", "ALHAJA",
	"HAJIA", "PASTOR", "REV", "BARR", "SIR", "MADAM", "HON",
];

fn significant_words(name: &str) -> Vec<String> {
	let all = words(name);
	let kept: Vec<String> = all
		.iter()
		.filter(|w| !NAME_TITLES.contains(&w.as_str()))
		.cloned()
		.collect();
	if kept.is_empty() {
		all
	} else {
		kept
	}
}

fn is_initial(w: &str) -> bool {
	w.chars().count() == 1
}

/// Pair each initial left in `from` with the first word left in `to` that
/// it begins (and that is not itself an initial).
fn pair_initials(from: &mut Vec<String>, to: &mut Vec<String>) -> usize {
	let mut paired = 0;
	let mut i = 0;
	while i < from.len() {
		let found = if is_initial(&from[i]) {
			to.iter()
				.position(|w| !is_initial(w) && w.starts_with(from[i].as_str()))
		} else {
			None
		};
		match found {
			Some(j) => {
				from.remove(i);
				to.remove(j);
				paired += 1;
			}
			None => i += 1,
		}
	}
	paired
}

/// The shared rule: the same words in any order is a match; when every word
/// of the shorter name pairs up (exactly, or an initial with a word it
/// begins), with at least two pairs of which one is exact, it is unsure;
/// anything else is a mismatch. A match only clears this check.
pub fn seller_names_match(verified: &str, other: &str) -> SellerNameMatch {
	let (v, t) = (significant_words(verified), significant_words(other));
	if v.is_empty() || t.is_empty() {
		return SellerNameMatch::Mismatch;
	}
	let (mut sv, mut st) = (v.clone(), t.clone());
	sv.sort();
	st.sort();
	if sv == st {
		return SellerNameMatch::Match;
	}
	let (mut a, mut b) = (v, t);
	let mut exact = 0;
	let mut i = 0;
	while i < a.len() {
		if let Some(j) = b.iter().position(|w| *w == a[i]) {
			a.remove(i);
			b.remove(j);
			exact += 1;
		} else {
			i += 1;
		}
	}
	let pairs = exact + pair_initials(&mut a, &mut b) + pair_initials(&mut b, &mut a);
	if (a.is_empty() || b.is_empty()) && pairs >= 2 && exact >= 1 {
		SellerNameMatch::Unsure
	} else {
		SellerNameMatch::Mismatch
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	fn seller_vectors() -> serde_json::Value {
		serde_json::from_str(include_str!("../../../../vectors/seller-names.json")).unwrap()
	}

	#[test]
	fn seller_names_match_the_shared_vectors() {
		let v = seller_vectors();
		let cases = v["cases"].as_array().unwrap();
		assert!(cases.len() >= 20);
		for c in cases {
			let (verified, other) = (
				c["verified"].as_str().unwrap(),
				c["typed"].as_str().unwrap(),
			);
			let want = match c["expect"].as_str().unwrap() {
				"match" => SellerNameMatch::Match,
				"unsure" => SellerNameMatch::Unsure,
				_ => SellerNameMatch::Mismatch,
			};
			assert_eq!(seller_names_match(verified, other), want, "{}", c["why"]);
		}
		let titles: Vec<&str> = v["titles"]
			.as_array()
			.unwrap()
			.iter()
			.map(|t| t.as_str().unwrap())
			.collect();
		assert_eq!(titles, NAME_TITLES);
	}

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
