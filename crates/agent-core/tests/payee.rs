//! The payee key and envelope, pinned to the TypeScript by
//! `vectors/fiat-payee.json`. The kit's `payee.test.ts` reads the same
//! file in the RecoilPay repo, so both languages agree on every byte.

use merchant_agent_core::payee::{open, payee_tag, Envelope, PayeeIdentity, PayeePrivateKey};
use merchant_agent_core::secrets::Secret;
use proptest::prelude::*;
use serde::Deserialize;
use std::collections::BTreeSet;
use std::sync::OnceLock;

const VECTOR: &str = include_str!("../../../vectors/fiat-payee.json");

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Vectors {
	commitments: Vec<CommitmentCase>,
	invalid: Vec<InvalidCase>,
	sealed_recipient: Recipient,
	sealed: Vec<SealedCase>,
	roundtrip: Vec<SealedCase>,
	payee_key: Vec<KeyCase>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct IdentityInput {
	rail: String,
	bank_code: String,
	account: String,
}

#[derive(Deserialize)]
struct CommitmentCase {
	name: String,
	input: IdentityInput,
	canonical: String,
	salt: String,
	commitment: String,
}

#[derive(Deserialize)]
struct InvalidCase {
	why: String,
	input: IdentityInput,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Recipient {
	private_key: String,
	public_key: String,
}

#[derive(Deserialize, Clone)]
#[serde(rename_all = "camelCase")]
struct SealedCase {
	name: String,
	envelope: Envelope,
	commitment: String,
	recipient_private_key: Option<String>,
	expect: Option<Expected>,
	error: Option<String>,
}

#[derive(Deserialize, Clone)]
struct Expected {
	rail: String,
	bank_code: String,
	account: String,
	name: String,
	salt: String,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct KeyCase {
	name: String,
	solver_id: String,
	signature: String,
	private_key: String,
	public_key: String,
}

fn vectors() -> &'static Vectors {
	static V: OnceLock<Vectors> = OnceLock::new();
	V.get_or_init(|| serde_json::from_str(VECTOR).expect("the vector file parses"))
}

fn key_for(c: &SealedCase) -> PayeePrivateKey {
	let hex = c
		.recipient_private_key
		.as_deref()
		.unwrap_or(&vectors().sealed_recipient.private_key);
	PayeePrivateKey::from_hex(hex).unwrap()
}

fn salt32(hex: &str) -> [u8; 32] {
	hex::decode(hex.trim_start_matches("0x"))
		.unwrap()
		.try_into()
		.unwrap()
}

#[test]
fn commitments_match_the_shared_vector() {
	for c in &vectors().commitments {
		let id = PayeeIdentity::new(&c.input.rail, &c.input.bank_code, &c.input.account)
			.unwrap_or_else(|e| panic!("{}: {e}", c.name));
		assert_eq!(*id.canonical(), c.canonical, "{}", c.name);
		let got = format!("0x{}", hex::encode(id.commitment(&salt32(&c.salt))));
		assert_eq!(got, c.commitment, "{}", c.name);
	}
}

#[test]
fn every_invalid_identity_in_the_shared_vector_is_refused() {
	for c in &vectors().invalid {
		assert!(
			PayeeIdentity::new(&c.input.rail, &c.input.bank_code, &c.input.account).is_err(),
			"should refuse: {}",
			c.why
		);
	}
}

#[test]
fn the_recipient_key_pair_is_consistent() {
	let r = &vectors().sealed_recipient;
	assert_eq!(
		PayeePrivateKey::from_hex(&r.private_key)
			.unwrap()
			.public_key_hex(),
		r.public_key
	);
}

#[test]
fn payee_keys_derive_exactly_as_the_dashboard_does() {
	for k in &vectors().payee_key {
		let derived = PayeePrivateKey::from_signature(&k.solver_id, &k.signature)
			.unwrap_or_else(|e| panic!("{}: {e}", k.name));
		assert_eq!(*derived.expose_hex(), k.private_key, "{}", k.name);
		assert_eq!(derived.public_key_hex(), k.public_key, "{}", k.name);
		let imported = PayeePrivateKey::from_hex(&k.private_key).unwrap();
		assert_eq!(imported.public_key_hex(), k.public_key, "{}", k.name);
	}
}

#[test]
fn malformed_keys_and_signatures_are_refused() {
	assert!(PayeePrivateKey::from_hex("0x1234").is_err());
	assert!(PayeePrivateKey::from_hex(&format!("0x{}", "00".repeat(32))).is_err());
	assert!(
		PayeePrivateKey::from_hex(&"11".repeat(32)).is_err(),
		"needs 0x"
	);
	assert!(PayeePrivateKey::from_signature("solver-x", "0x1234").is_err());
}

fn check_opens(c: &SealedCase) {
	let want = c.expect.as_ref().unwrap();
	let o =
		open(&c.envelope, &key_for(c), &c.commitment).unwrap_or_else(|e| panic!("{}: {e}", c.name));
	assert_eq!(o.identity.rail, want.rail, "{}", c.name);
	assert_eq!(o.identity.bank_code, want.bank_code, "{}", c.name);
	assert_eq!(o.identity.account.expose(), want.account, "{}", c.name);
	assert_eq!(o.name.expose(), want.name, "{}", c.name);
	assert_eq!(o.salt_hex(), want.salt, "{}", c.name);
}

#[test]
fn every_valid_sealed_vector_opens_to_what_typescript_opens() {
	let valid: Vec<_> = vectors()
		.sealed
		.iter()
		.filter(|c| c.expect.is_some())
		.collect();
	assert!(valid.len() >= 5);
	for c in valid {
		check_opens(c);
	}
}

/// The round trip: payees generated and sealed in TypeScript, opened here.
#[test]
fn identities_sealed_in_typescript_round_trip_in_rust() {
	assert!(vectors().roundtrip.len() >= 32);
	for c in &vectors().roundtrip {
		check_opens(c);
	}
}

#[test]
fn invariant_4_every_tampered_vector_is_refused_with_the_right_error() {
	let mut seen = BTreeSet::new();
	for c in vectors().sealed.iter().filter(|c| c.error.is_some()) {
		let want = c.error.as_deref().unwrap();
		match open(&c.envelope, &key_for(c), &c.commitment) {
			Ok(_) => panic!("{}: opened, but should fail with {want}", c.name),
			Err(e) => assert_eq!(e.code(), want, "{}: {e}", c.name),
		}
		seen.insert(want);
	}
	let all: BTreeSet<_> = [
		"commitment_mismatch",
		"decrypt_failed",
		"invalid_identity",
		"invalid_name",
		"invalid_salt",
		"unsupported_envelope",
		"unsupported_payload",
	]
	.into();
	assert_eq!(seen, all, "the vectors must cover every shared error code");
}

#[test]
fn invariant_4_a_commitment_mismatch_is_never_paid() {
	let c = vectors()
		.sealed
		.iter()
		.find(|c| c.error.as_deref() == Some("commitment_mismatch"))
		.unwrap();
	let err = open(&c.envelope, &key_for(c), &c.commitment).err().unwrap();
	assert_eq!(err.code(), "commitment_mismatch");
}

#[test]
fn invariant_7_open_errors_never_carry_personal_data() {
	for c in &vectors().sealed {
		if let Err(e) = open(&c.envelope, &key_for(c), &c.commitment) {
			let shown = format!("{e} {e:?}");
			for leak in ["0000000000", "Test Payee", "058"] {
				assert!(!shown.contains(leak), "{}: {shown}", c.name);
			}
		}
	}
}

#[test]
fn a_malformed_commitment_from_the_aggregator_is_refused() {
	let c = &vectors().sealed[0];
	assert_eq!(
		open(&c.envelope, &key_for(c), "0x1234")
			.err()
			.unwrap()
			.code(),
		"malformed_commitment"
	);
}

#[test]
fn hex_must_be_0x_prefixed_and_epk_uncompressed() {
	let c = &vectors().sealed[0];
	let mut e = c.envelope.clone();
	e.iv = e.iv.trim_start_matches("0x").to_string();
	assert_eq!(
		open(&e, &key_for(c), &c.commitment).err().unwrap().code(),
		"malformed_envelope"
	);

	let mut e = c.envelope.clone();
	e.epk = format!("0x02{}", &c.envelope.epk[4..68]);
	assert_eq!(
		open(&e, &key_for(c), &c.commitment).err().unwrap().code(),
		"malformed_envelope"
	);
}

#[test]
fn the_payee_tag_is_the_same_for_every_trade_to_one_account() {
	let pepper = Secret::new("test-pepper".into());
	let tags: BTreeSet<String> = vectors()
		.sealed
		.iter()
		.filter(|c| {
			c.expect
				.as_ref()
				.is_some_and(|e| e.rail == "ng.nip" && e.account == "0000000000")
		})
		.map(|c| {
			payee_tag(
				&pepper,
				&open(&c.envelope, &key_for(c), &c.commitment)
					.unwrap()
					.identity,
			)
		})
		.collect();
	assert_eq!(
		tags.len(),
		1,
		"differently salted trades to one account share a tag"
	);
}

proptest! {
	#![proptest_config(ProptestConfig::with_cases(512))]

	/// Flipping any bit of any field of a valid envelope never opens it and
	/// never panics.
	#[test]
	fn invariant_4_any_single_bit_flip_is_refused(field in 0usize..4, byte in 0usize..200, bit in 0u8..8) {
		let c = &vectors().sealed[0];
		let mut e = c.envelope.clone();
		let target = match field { 0 => &mut e.epk, 1 => &mut e.iv, 2 => &mut e.ct, _ => &mut e.tag };
		let mut bytes = hex::decode(target.trim_start_matches("0x")).unwrap();
		let i = byte % bytes.len();
		bytes[i] ^= 1 << bit;
		*target = format!("0x{}", hex::encode(bytes));
		prop_assert!(open(&e, &key_for(c), &c.commitment).is_err());
	}

	/// Arbitrary field values never panic the opener.
	#[test]
	fn arbitrary_envelopes_never_panic(v in 0u64..3, epk in ".{0,140}", iv in ".{0,30}", ct in ".{0,60}", tag in ".{0,40}") {
		let e = Envelope { v, alg: "ECIES-secp256k1-AES256GCM".into(), epk, iv, ct, tag };
		let c = &vectors().sealed[0];
		let _ = open(&e, &key_for(c), &c.commitment);
	}
}
