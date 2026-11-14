//! Opening the bank details a customer sealed to this merchant.
//!
//! The envelope format is the client's (`payee.ts`, `sealPayee`), matched
//! byte for byte and pinned by `vectors/fiat-payee.json` (design §5):
//!
//! - `{v: 1, alg: "ECIES-secp256k1-AES256GCM", epk, iv, ct, tag}`, every hex
//!   field `0x`-prefixed.
//! - shared = the X coordinate of ECDH(payee key, epk).
//! - key = HKDF-SHA256(ikm = shared, salt = epk (65 bytes),
//!   info = "recoilpay/fiat-payee/v1", 32 bytes).
//! - AES-256-GCM over `ct ‖ tag`, no associated data.
//! - plaintext = JSON `{v: 1, rail, bank_code, account, name, salt}`.
//!
//! **Opening recomputes the commitment and refuses a mismatch**
//! (invariant 4). The agent never pays details that don't match what the
//! trade committed to. Otherwise a customer could seal one account, commit
//! to another, be paid, and then dispute the payout as going to the wrong
//! account.
//!
//! Checks run in the same order as `openPayee`: envelope, decrypt, payload
//! version, identity, salt, commitment, name. So a broken envelope gets the
//! same error code in both languages.
//!
//! Where Rust is stricter than the TypeScript, it refuses rather than
//! guesses: hex fields must be `0x`-prefixed, `epk` must be the 65-byte
//! uncompressed form, and the payload must hold exactly its six fields as
//! strings. The client only ever produces that, so nothing real is refused.

mod identity;
mod key;
mod name;
mod tag;

pub use identity::{IdentityError, PayeeIdentity, SALT_BYTES};
pub use key::{KeyError, PayeePrivateKey, KEY_DOMAIN};
pub use name::{is_js_whitespace, normalise_name, NameError, MAX_NAME_UTF16_UNITS};
pub use tag::payee_tag;

use aes_gcm::aead::{Aead, KeyInit};
use aes_gcm::{Aes256Gcm, Nonce};
use hkdf::Hkdf;
use k256::PublicKey;
use serde::Deserialize;
use sha2::Sha256;
use zeroize::{Zeroize, ZeroizeOnDrop, Zeroizing};

use crate::pii::HolderName;

pub const ENVELOPE_VERSION: u64 = 1;
pub const PAYLOAD_VERSION: u64 = 1;
pub const ALG: &str = "ECIES-secp256k1-AES256GCM";
pub const HKDF_INFO: &[u8] = b"recoilpay/fiat-payee/v1";
/// The aggregator refuses a larger `ct` (`validate_sealed_payee`).
pub const MAX_CT_BYTES: usize = 1024;

/// A sealed payee, as the trade carries it in `payee_sealed`. Ciphertext
/// only, so it may be printed.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Envelope {
	pub v: u64,
	pub alg: String,
	pub epk: String,
	pub iv: String,
	pub ct: String,
	pub tag: String,
}

/// Why an envelope did not open. Messages and codes never carry a value
/// from inside it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum OpenError {
	#[error("unsupported envelope version or algorithm")]
	UnsupportedEnvelope,
	#[error("the envelope's {0} field is malformed")]
	MalformedEnvelope(&'static str),
	#[error("the envelope does not decrypt with this payee key")]
	DecryptFailed,
	#[error("unsupported payee payload")]
	UnsupportedPayload,
	#[error("the sealed {0}")]
	InvalidIdentity(IdentityError),
	#[error("the sealed salt is not 32 bytes")]
	InvalidSalt,
	#[error("the trade's commitment is malformed")]
	MalformedCommitment,
	#[error("the sealed details do not match the trade's commitment")]
	CommitmentMismatch,
	#[error("the sealed name is invalid: {0}")]
	InvalidName(NameError),
}

impl OpenError {
	/// The code shared with the TypeScript in the test vectors.
	pub fn code(&self) -> &'static str {
		match self {
			Self::UnsupportedEnvelope => "unsupported_envelope",
			Self::MalformedEnvelope(_) => "malformed_envelope",
			Self::DecryptFailed => "decrypt_failed",
			Self::UnsupportedPayload => "unsupported_payload",
			Self::InvalidIdentity(_) => "invalid_identity",
			Self::InvalidSalt => "invalid_salt",
			Self::MalformedCommitment => "malformed_commitment",
			Self::CommitmentMismatch => "commitment_mismatch",
			Self::InvalidName(_) => "invalid_name",
		}
	}
}

/// What opening yields. Personal data: no `Debug`, `Display`, `Clone` or
/// `Serialize`, and zeroed on drop.
pub struct OpenedPayee {
	pub identity: PayeeIdentity,
	pub name: HolderName,
	salt: Zeroizing<[u8; SALT_BYTES]>,
}

impl OpenedPayee {
	pub fn salt(&self) -> &[u8; SALT_BYTES] {
		&self.salt
	}

	/// `0x`-hex, as a dispute or a provider proof sends it.
	pub fn salt_hex(&self) -> String {
		format!("0x{}", hex::encode(*self.salt))
	}
}

/// The decrypted payload. Its strings are zeroed when it is dropped.
#[derive(Deserialize, Zeroize, ZeroizeOnDrop)]
#[serde(deny_unknown_fields)]
struct Payload {
	#[zeroize(skip)]
	v: serde_json::Value,
	rail: String,
	bank_code: String,
	account: String,
	name: String,
	salt: String,
}

fn hex_field(value: &str, field: &'static str, len: Option<usize>) -> Result<Vec<u8>, OpenError> {
	let malformed = OpenError::MalformedEnvelope(field);
	let body = value.strip_prefix("0x").ok_or(malformed)?;
	let bytes = hex::decode(body).map_err(|_| malformed)?;
	match len {
		Some(n) if bytes.len() != n => Err(malformed),
		None if bytes.is_empty() || bytes.len() > MAX_CT_BYTES => Err(malformed),
		_ => Ok(bytes),
	}
}

fn bytes32(value: &str) -> Option<[u8; 32]> {
	let body = value.trim().strip_prefix("0x")?;
	let bytes = hex::decode(body).ok()?;
	bytes.try_into().ok()
}

/// Open `envelope` with `key`, and check it against the trade's
/// `expected_commitment` (`0x`-hex).
pub fn open(
	envelope: &Envelope,
	key: &PayeePrivateKey,
	expected_commitment: &str,
) -> Result<OpenedPayee, OpenError> {
	if envelope.v != ENVELOPE_VERSION || envelope.alg != ALG {
		return Err(OpenError::UnsupportedEnvelope);
	}
	let epk = hex_field(&envelope.epk, "epk", Some(65))?;
	if epk[0] != 0x04 {
		return Err(OpenError::MalformedEnvelope("epk"));
	}
	let iv = hex_field(&envelope.iv, "iv", Some(12))?;
	let tag = hex_field(&envelope.tag, "tag", Some(16))?;
	let mut sealed = hex_field(&envelope.ct, "ct", None)?;
	sealed.extend_from_slice(&tag);
	let epk_point =
		PublicKey::from_sec1_bytes(&epk).map_err(|_| OpenError::MalformedEnvelope("epk"))?;

	// The X coordinate: the compressed point without its parity byte.
	let shared =
		k256::ecdh::diffie_hellman(key.secret().to_nonzero_scalar(), epk_point.as_affine());
	let mut okm = Zeroizing::new([0u8; 32]);
	Hkdf::<Sha256>::new(Some(&epk), shared.raw_secret_bytes())
		.expand(HKDF_INFO, &mut okm[..])
		.expect("32 bytes is a valid HKDF-SHA256 output length");
	let cipher = Aes256Gcm::new_from_slice(&okm[..]).expect("the key is 32 bytes");
	let plaintext = Zeroizing::new(
		cipher
			.decrypt(Nonce::from_slice(&iv), sealed.as_slice())
			.map_err(|_| OpenError::DecryptFailed)?,
	);

	let payload: Payload =
		serde_json::from_slice(&plaintext).map_err(|_| OpenError::UnsupportedPayload)?;
	if payload.v.as_u64() != Some(PAYLOAD_VERSION) {
		return Err(OpenError::UnsupportedPayload);
	}
	let identity = PayeeIdentity::new(&payload.rail, &payload.bank_code, &payload.account)
		.map_err(OpenError::InvalidIdentity)?;
	let salt = Zeroizing::new(bytes32(&payload.salt).ok_or(OpenError::InvalidSalt)?);
	let expected = bytes32(expected_commitment).ok_or(OpenError::MalformedCommitment)?;
	if identity.commitment(&salt) != expected {
		return Err(OpenError::CommitmentMismatch);
	}
	let name = normalise_name(&payload.name).map_err(OpenError::InvalidName)?;
	Ok(OpenedPayee {
		identity,
		name,
		salt,
	})
}

#[cfg(test)]
mod tests {
	use super::*;
	use static_assertions::assert_not_impl_any;
	use std::fmt::{Debug, Display};

	assert_not_impl_any!(OpenedPayee: Debug, Display, Clone, serde::Serialize);
	assert_not_impl_any!(PayeeIdentity: Debug, Display, Clone, serde::Serialize);
	assert_not_impl_any!(PayeePrivateKey: Debug, Display, Clone, serde::Serialize);
}
