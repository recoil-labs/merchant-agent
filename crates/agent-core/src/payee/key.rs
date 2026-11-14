//! The payee key: the private half that opens the bank details customers
//! seal to this merchant.
//!
//! Derived exactly as the dashboard derives it
//! (`payeeKeyFromSignature` in the RecoilPay operator portal):
//!
//! ```text
//! key = keccak256(utf8("recoilpay/fiat-payee-key/v1") ‖ utf8(solver_id) ‖ signature)
//! ```
//!
//! where `signature` is the 65 bytes (`r ‖ s ‖ v`) the operator wallet's
//! `personal_sign` returned over `payeeKeyMessage(solver_id)`. The agent
//! takes either that signature or the derived key itself (design §9.2).
//! Pinned to the dashboard by the `payeeKey` vectors.
//!
//! The key is a `k256::SecretKey`, which zeroes itself on drop. This type
//! has no `Debug`, `Display` or `Clone`.

use k256::elliptic_curve::sec1::ToSec1Point;
use k256::SecretKey;
use sha3::{Digest, Keccak256};
use zeroize::Zeroizing;

use crate::config::PayeeKeyForm;
use crate::secrets::Secret;

pub const KEY_DOMAIN: &str = "recoilpay/fiat-payee-key/v1";

#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum KeyError {
	#[error("the payee key must be 0x followed by 64 hex digits")]
	MalformedKey,
	#[error("the signature must be 0x followed by 130 hex digits (65 bytes)")]
	MalformedSignature,
	#[error("the value is not a valid secp256k1 private key")]
	NotAScalar,
}

pub struct PayeePrivateKey(SecretKey);

fn hex_bytes(s: &str) -> Option<Zeroizing<Vec<u8>>> {
	let body = s.trim().strip_prefix("0x")?;
	hex::decode(body).ok().map(Zeroizing::new)
}

impl PayeePrivateKey {
	/// The derived key, as exported from the dashboard.
	pub fn from_hex(s: &str) -> Result<Self, KeyError> {
		let bytes = hex_bytes(s).ok_or(KeyError::MalformedKey)?;
		if bytes.len() != 32 {
			return Err(KeyError::MalformedKey);
		}
		SecretKey::from_slice(&bytes)
			.map(Self)
			.map_err(|_| KeyError::NotAScalar)
	}

	/// Derive the key from the operator wallet's signature over the payee-key
	/// message, as the dashboard does.
	pub fn from_signature(solver_id: &str, signature: &str) -> Result<Self, KeyError> {
		let sig = hex_bytes(signature).ok_or(KeyError::MalformedSignature)?;
		if sig.len() != 65 {
			return Err(KeyError::MalformedSignature);
		}
		let mut h = Keccak256::new();
		h.update(KEY_DOMAIN.as_bytes());
		h.update(solver_id.as_bytes());
		h.update(&sig[..]);
		let digest = Zeroizing::new(<[u8; 32]>::from(h.finalize()));
		SecretKey::from_slice(&digest[..])
			.map(Self)
			.map_err(|_| KeyError::NotAScalar)
	}

	/// The key from its secret, in whichever form the config says it holds.
	pub fn load(form: PayeeKeyForm, solver_id: &str, secret: &Secret) -> Result<Self, KeyError> {
		match form {
			PayeeKeyForm::PrivateKey => Self::from_hex(secret.expose()),
			PayeeKeyForm::Signature => Self::from_signature(solver_id, secret.expose()),
		}
	}

	/// The uncompressed public key, `0x04…`, lower case: the form the
	/// aggregator stores and customers seal to.
	pub fn public_key_hex(&self) -> String {
		let point = self.0.public_key().to_sec1_point(false);
		format!("0x{}", hex::encode(point.as_bytes()))
	}

	pub(crate) fn secret(&self) -> &SecretKey {
		&self.0
	}

	/// The private key as `0x`-hex. Only for tests and export tooling.
	pub fn expose_hex(&self) -> Zeroizing<String> {
		Zeroizing::new(format!("0x{}", hex::encode(self.0.to_bytes())))
	}
}
