//! The payee tag: a stable, unguessable name for one bank account.
//!
//! Per-payee limits and the deny-list need to recognise the same account
//! across trades, but each trade's commitment is salted differently, and the
//! agent must not store account numbers (invariant 7). A plain hash would not
//! do: a NUBAN is ten digits, so every hash could be tried in minutes. The
//! tag is keyed with a pepper that lives in the secret backend:
//!
//! ```text
//! tag = hex(HMAC-SHA256(pepper, canonical identity))
//! ```
//!
//! Without the pepper a stolen database gives tags that cannot be reversed.

use hmac::{Hmac, Mac};
use sha2::Sha256;

use super::PayeeIdentity;
use crate::secrets::Secret;

pub fn payee_tag(pepper: &Secret, identity: &PayeeIdentity) -> String {
	let mut mac = Hmac::<Sha256>::new_from_slice(pepper.expose().as_bytes())
		.expect("HMAC accepts a key of any length");
	mac.update(identity.canonical().as_bytes());
	hex::encode(mac.finalize().into_bytes())
}

#[cfg(test)]
mod tests {
	use super::*;

	fn id(account: &str) -> PayeeIdentity {
		PayeeIdentity::new("ng.nip", "058", account).unwrap()
	}

	#[test]
	fn the_same_account_gets_the_same_tag_however_it_was_typed() {
		let p = Secret::new("test-pepper".into());
		assert_eq!(
			payee_tag(&p, &id("0000000000")),
			payee_tag(&p, &id("000 000-0000"))
		);
		assert_ne!(
			payee_tag(&p, &id("0000000000")),
			payee_tag(&p, &id("0000000001"))
		);
	}

	#[test]
	fn the_tag_depends_on_the_pepper_and_hides_the_account() {
		let a = payee_tag(&Secret::new("pepper-a".into()), &id("0000000000"));
		let b = payee_tag(&Secret::new("pepper-b".into()), &id("0000000000"));
		assert_ne!(a, b);
		assert!(!a.contains("0000000000"));
		assert_eq!(a.len(), 64);
	}
}
