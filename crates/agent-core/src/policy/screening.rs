//! Screening: payees to hold for review before paying.
//!
//! The hook is [`Screening`]; the agent ships two: [`NoScreening`] and a
//! [`DenyList`] of payee tags. A tag is `HMAC-SHA256(pepper, canonical
//! account)` (design §3.7), so the list itself reveals no account numbers
//! to anyone without the pepper, and matches however the customer typed
//! the account. `merchant-agent screening hash` turns a plain list into
//! tags with this agent's pepper.
//!
//! A merchant's own sanctions or fraud screening plugs in as another
//! `Screening`.

use std::collections::HashSet;
use std::path::Path;

use crate::payee::{payee_tag, PayeeIdentity};
use crate::pii::HolderName;
use crate::secrets::Secret;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ScreenResult {
	Clear,
	/// Hold for review. The reason is shown to people, so it must not
	/// carry personal data.
	Hold(String),
}

pub trait Screening: Send + Sync {
	fn screen(&self, payee_tag: &str, identity: &PayeeIdentity, name: &HolderName) -> ScreenResult;
}

pub struct NoScreening;

impl Screening for NoScreening {
	fn screen(&self, _: &str, _: &PayeeIdentity, _: &HolderName) -> ScreenResult {
		ScreenResult::Clear
	}
}

pub struct DenyList {
	tags: HashSet<String>,
}

/// Shows the size only: the tags are a list of payees.
impl std::fmt::Debug for DenyList {
	fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
		write!(f, "DenyList({} tags)", self.tags.len())
	}
}

fn is_tag(s: &str) -> bool {
	s.len() == 64 && s.bytes().all(|b| b.is_ascii_hexdigit())
}

impl DenyList {
	/// One tag per line; blank lines and `#` comments are ignored. A line
	/// that is not a tag is an error naming its line number (never its
	/// content: it might be a raw account number pasted by mistake).
	pub fn parse(text: &str) -> Result<Self, String> {
		let mut tags = HashSet::new();
		for (i, line) in text.lines().enumerate() {
			let line = line.split('#').next().unwrap_or("").trim();
			if line.is_empty() {
				continue;
			}
			if !is_tag(line) {
				return Err(format!(
					"line {}: not a payee tag. Make the list with `merchant-agent screening hash`",
					i + 1
				));
			}
			tags.insert(line.to_ascii_lowercase());
		}
		Ok(Self { tags })
	}

	pub fn load(path: &Path) -> Result<Self, String> {
		let text = std::fs::read_to_string(path)
			.map_err(|e| format!("{}: {}", path.display(), e.kind()))?;
		Self::parse(&text).map_err(|e| format!("{}: {e}", path.display()))
	}

	pub fn len(&self) -> usize {
		self.tags.len()
	}

	pub fn is_empty(&self) -> bool {
		self.tags.is_empty()
	}
}

impl Screening for DenyList {
	fn screen(&self, payee_tag: &str, _: &PayeeIdentity, _: &HolderName) -> ScreenResult {
		if self.tags.contains(payee_tag) {
			ScreenResult::Hold("the payee is on your deny-list".into())
		} else {
			ScreenResult::Clear
		}
	}
}

/// Turn a plain list into tags. Each line is `bank_code,account` (for
/// `ng.nip`) or `rail,bank_code,account`; blank lines and `#` comments are
/// skipped. Errors name the line, never its content.
pub fn hash_list(pepper: &Secret, text: &str) -> Result<Vec<String>, String> {
	let mut out = Vec::new();
	for (i, line) in text.lines().enumerate() {
		let line = line.split('#').next().unwrap_or("").trim();
		if line.is_empty() {
			continue;
		}
		let parts: Vec<&str> = line.split(',').map(str::trim).collect();
		let (rail, bank, account) = match parts.as_slice() {
			[bank, account] => ("ng.nip", *bank, *account),
			[rail, bank, account] => (*rail, *bank, *account),
			_ => {
				return Err(format!(
					"line {}: expected bank_code,account or rail,bank_code,account",
					i + 1
				))
			}
		};
		let identity =
			PayeeIdentity::new(rail, bank, account).map_err(|e| format!("line {}: {e}", i + 1))?;
		out.push(payee_tag(pepper, &identity));
	}
	Ok(out)
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn a_listed_account_is_held_however_it_was_typed() {
		let pepper = Secret::new("test-pepper".into());
		let tags = hash_list(&pepper, "# known fraud\n058,0000000000\n").unwrap();
		let list = DenyList::parse(&tags.join("\n")).unwrap();
		let typed = PayeeIdentity::new("ng.nip", " 058 ", "000 000-0000").unwrap();
		let tag = payee_tag(&pepper, &typed);
		let name = HolderName::new("Test Payee".into());
		assert!(matches!(
			list.screen(&tag, &typed, &name),
			ScreenResult::Hold(_)
		));
		let other = PayeeIdentity::new("ng.nip", "058", "0000000001").unwrap();
		assert_eq!(
			list.screen(&payee_tag(&pepper, &other), &other, &name),
			ScreenResult::Clear
		);
	}

	#[test]
	fn errors_name_the_line_not_the_account() {
		let e = DenyList::parse("0123456789\n").unwrap_err();
		assert!(e.contains("line 1") && !e.contains("0123456789"), "{e}");
		let e = hash_list(&Secret::new("p".into()), "058;0123456789\n").unwrap_err();
		assert!(e.contains("line 1") && !e.contains("0123456789"), "{e}");
	}
}
