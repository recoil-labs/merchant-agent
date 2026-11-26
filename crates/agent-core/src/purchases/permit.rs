//! The Permit2 lock a merchant-signed purchase is signed with: the digest
//! `AttestedEscrow.lockFor` verifies, built here from the agent's own values.
//!
//! `PermitWitnessTransferFrom` with witness `EscrowLock(tradeId,
//! counterparty)`, in Permit2's EIP-712 domain. The digest is pinned to
//! `vectors/fiat-lock.json` (`permit`), the same vector the escrow's
//! Solidity test, the TypeScript signer and the aggregator's relay check;
//! the nonce to `nonces` there.

use alloy::primitives::{address, keccak256, Address, B256, U256};
use alloy::sol;
use alloy::sol_types::{eip712_domain, SolStruct};
use k256::ecdsa::SigningKey;
use serde_json::{json, Value};

/// Permit2's canonical address, the same on every chain.
pub const PERMIT2: Address = address!("000000000022D473030F116dDEE9F6B43aC78BA3");

sol! {
	struct TokenPermissions {
		address token;
		uint256 amount;
	}

	struct EscrowLock {
		bytes32 tradeId;
		address counterparty;
	}

	struct PermitWitnessTransferFrom {
		TokenPermissions permitted;
		address spender;
		uint256 nonce;
		uint256 deadline;
		EscrowLock witness;
	}
}

/// Everything a purchase's lock commits to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Permit {
	pub chain_id: u64,
	/// The escrow: Permit2's spender.
	pub escrow: Address,
	pub token: Address,
	pub amount: U256,
	/// `keccak256(trade id)`.
	pub trade_id_hash: B256,
	/// The buyer.
	pub counterparty: Address,
	pub nonce: U256,
	/// Unix seconds.
	pub deadline: U256,
}

/// The Permit2 nonce for a trade: `keccak256("recoilpay/fiat-lock-nonce/v1:"
/// + trade id)`. Per trade, so one signature can fund only its own trade.
pub fn lock_nonce(trade_id: &str) -> U256 {
	U256::from_be_bytes(keccak256(format!("recoilpay/fiat-lock-nonce/v1:{trade_id}")).0)
}

pub fn trade_id_hash(trade_id: &str) -> B256 {
	keccak256(trade_id.as_bytes())
}

impl Permit {
	/// The EIP-712 digest Permit2 checks.
	pub fn digest(&self) -> B256 {
		let domain = eip712_domain! {
			name: "Permit2",
			chain_id: self.chain_id,
			verifying_contract: PERMIT2,
		};
		PermitWitnessTransferFrom {
			permitted: TokenPermissions {
				token: self.token,
				amount: self.amount,
			},
			spender: self.escrow,
			nonce: self.nonce,
			deadline: self.deadline,
			witness: EscrowLock {
				tradeId: self.trade_id_hash,
				counterparty: self.counterparty,
			},
		}
		.eip712_signing_hash(&domain)
	}

	/// Sign the digest: 65 bytes `r ‖ s ‖ v` (v = 27 or 28), `0x`-hex.
	pub fn sign(&self, key: &SigningKey) -> Result<String, String> {
		let (sig, recid): (k256::ecdsa::Signature, k256::ecdsa::RecoveryId) =
			key.sign_prehash_recoverable(self.digest().as_slice());
		let mut out = sig.to_bytes().to_vec();
		out.push(27 + recid.to_byte());
		Ok(format!("0x{}", hex::encode(out)))
	}

	/// Whether `shown` (the typed data RecoilPay sent) says exactly this,
	/// field by field. `Err` names the first difference. The agent signs its
	/// own digest either way; a difference means RecoilPay asked for
	/// something else, so the purchase is refused.
	pub fn matches(&self, shown: &Value) -> Result<(), String> {
		let differ = |what: &str| {
			Err(format!(
				"the typed data's {what} differs from the agent's own"
			))
		};
		let addr = |v: &Value| v.as_str().and_then(|s| s.parse::<Address>().ok());
		let uint = |v: &Value| v.as_str().and_then(|s| s.parse::<U256>().ok());
		let d = &shown["domain"];
		if d["name"] != "Permit2" || d.as_object().map(|o| o.len()) != Some(3) {
			return differ("domain");
		}
		if d["chainId"].as_u64() != Some(self.chain_id) {
			return differ("chain id");
		}
		if addr(&d["verifyingContract"]) != Some(PERMIT2) {
			return differ("verifying contract");
		}
		if shown["primaryType"] != "PermitWitnessTransferFrom" || shown["types"] != types() {
			return differ("types");
		}
		let m = &shown["message"];
		if addr(&m["permitted"]["token"]) != Some(self.token) {
			return differ("token");
		}
		if uint(&m["permitted"]["amount"]) != Some(self.amount) {
			return differ("amount");
		}
		if addr(&m["spender"]) != Some(self.escrow) {
			return differ("spender");
		}
		if uint(&m["nonce"]) != Some(self.nonce) {
			return differ("nonce");
		}
		if uint(&m["deadline"]) != Some(self.deadline) {
			return differ("deadline");
		}
		if m["witness"]["tradeId"]
			.as_str()
			.and_then(|s| s.parse::<B256>().ok())
			!= Some(self.trade_id_hash)
		{
			return differ("trade id");
		}
		if addr(&m["witness"]["counterparty"]) != Some(self.counterparty) {
			return differ("buyer");
		}
		Ok(())
	}
}

/// The EIP-712 types, as viem and RecoilPay write them.
fn types() -> Value {
	json!({
		"PermitWitnessTransferFrom": [
			{ "name": "permitted", "type": "TokenPermissions" },
			{ "name": "spender", "type": "address" },
			{ "name": "nonce", "type": "uint256" },
			{ "name": "deadline", "type": "uint256" },
			{ "name": "witness", "type": "EscrowLock" }
		],
		"EscrowLock": [
			{ "name": "tradeId", "type": "bytes32" },
			{ "name": "counterparty", "type": "address" }
		],
		"TokenPermissions": [
			{ "name": "token", "type": "address" },
			{ "name": "amount", "type": "uint256" }
		]
	})
}

/// The address a secp256k1 key signs as.
pub fn address_of(key: &SigningKey) -> Address {
	let point = key.verifying_key().to_sec1_point(false);
	Address::from_slice(&keccak256(&point.as_bytes()[1..])[12..])
}

/// A float wallet key from its hex form (with or without `0x`).
pub fn signing_key(hex_key: &str) -> Result<SigningKey, String> {
	let bytes = hex::decode(hex_key.trim().trim_start_matches("0x"))
		.map_err(|_| "the float wallet key is not hex".to_string())?;
	SigningKey::from_slice(&bytes)
		.map_err(|_| "the float wallet key is not a secp256k1 private key".to_string())
}

#[cfg(test)]
mod tests {
	use super::*;
	use k256::ecdsa::{RecoveryId, Signature, VerifyingKey};

	const VECTOR: &str = include_str!("../../../../vectors/fiat-lock.json");

	fn vector() -> (Permit, String) {
		let v: Value = serde_json::from_str(VECTOR).unwrap();
		let p = &v["permit"];
		let s = |k: &str| p[k].as_str().unwrap();
		(
			Permit {
				chain_id: p["chainId"].as_u64().unwrap(),
				escrow: s("spender").parse().unwrap(),
				token: s("token").parse().unwrap(),
				amount: s("amount").parse().unwrap(),
				trade_id_hash: s("tradeIdHash").parse().unwrap(),
				counterparty: s("counterparty").parse().unwrap(),
				nonce: s("nonce").parse().unwrap(),
				deadline: s("deadline").parse().unwrap(),
			},
			s("digest").to_string(),
		)
	}

	#[test]
	fn the_digest_matches_the_shared_vector() {
		let (p, digest) = vector();
		assert_eq!(format!("{:#x}", p.digest()), digest);
	}

	#[test]
	fn nonces_match_the_shared_vector() {
		let v: Value = serde_json::from_str(VECTOR).unwrap();
		for c in v["nonces"].as_array().unwrap() {
			let id = c["tradeId"].as_str().unwrap();
			assert_eq!(
				lock_nonce(id).to_string(),
				c["nonceDecimal"].as_str().unwrap(),
				"{id}"
			);
		}
	}

	#[test]
	fn the_signature_recovers_to_the_float_wallet() {
		let key = signing_key("0x00000000000000000000000000000000000000000000000000000000000000f1")
			.unwrap();
		let (p, _) = vector();
		let sig = hex::decode(p.sign(&key).unwrap().trim_start_matches("0x")).unwrap();
		assert_eq!(sig.len(), 65);
		let recid = RecoveryId::from_byte(sig[64] - 27).unwrap();
		let vk = VerifyingKey::recover_from_prehash(
			p.digest().as_slice(),
			&Signature::from_slice(&sig[..64]).unwrap(),
			recid,
		)
		.unwrap();
		assert_eq!(vk, *key.verifying_key());
		// Private key 0xf1's address, from `cast wallet address`.
		assert_eq!(
			format!("{:#x}", address_of(&key)),
			"0x7492ebbc1e7f2838fc7191edc031581d5712978a"
		);
	}

	/// The fixture's purchase lock, signed by the agent and by Foundry's
	/// `cast wallet sign --data` (an independent EIP-712 implementation).
	/// Both sign deterministically (RFC 6979), so the bytes must be equal.
	#[test]
	fn the_signature_is_the_one_cast_makes_for_the_same_typed_data() {
		let v: Value =
			serde_json::from_str(include_str!("../../../../vectors/merchant-agent-api.json"))
				.unwrap();
		let td = &v["purchaseLocks"]["locks"][0]["typedData"];
		let m = &td["message"];
		let s = |x: &Value| x.as_str().unwrap().to_string();
		let p = Permit {
			chain_id: td["domain"]["chainId"].as_u64().unwrap(),
			escrow: s(&m["spender"]).parse().unwrap(),
			token: s(&m["permitted"]["token"]).parse().unwrap(),
			amount: s(&m["permitted"]["amount"]).parse().unwrap(),
			trade_id_hash: s(&m["witness"]["tradeId"]).parse().unwrap(),
			counterparty: s(&m["witness"]["counterparty"]).parse().unwrap(),
			nonce: s(&m["nonce"]).parse().unwrap(),
			deadline: s(&m["deadline"]).parse().unwrap(),
		};
		assert_eq!(p.matches(td), Ok(()));
		let key = signing_key("0x00000000000000000000000000000000000000000000000000000000000000f1")
			.unwrap();
		assert_eq!(
			p.sign(&key).unwrap(),
			"0x16b86989eb22403f0fc065960a37051b7a244af39907c36577ef51c8cc809a367994021f8d42c68a1f7ea6765c05f8b44d217a844276128ff1f92c5a13618fdd1c"
		);
	}

	fn shown(p: &Permit) -> Value {
		let a = |x: Address| format!("{x:#x}");
		json!({
			"domain": { "name": "Permit2", "chainId": p.chain_id, "verifyingContract": "0x000000000022D473030F116dDEE9F6B43aC78BA3" },
			"types": types(),
			"primaryType": "PermitWitnessTransferFrom",
			"message": {
				"permitted": { "token": a(p.token), "amount": p.amount.to_string() },
				"spender": a(p.escrow),
				"nonce": p.nonce.to_string(),
				"deadline": p.deadline.to_string(),
				"witness": { "tradeId": format!("{:#x}", p.trade_id_hash), "counterparty": a(p.counterparty) }
			}
		})
	}

	#[test]
	fn typed_data_that_says_anything_else_is_named() {
		let (p, _) = vector();
		assert_eq!(p.matches(&shown(&p)), Ok(()));
		for (path, value, what) in [
			("/message/permitted/amount", json!("100000001"), "amount"),
			(
				"/message/spender",
				json!("0x0000000000000000000000000000000000000001"),
				"spender",
			),
			(
				"/message/witness/counterparty",
				json!("0x0000000000000000000000000000000000000002"),
				"buyer",
			),
			("/message/nonce", json!("43"), "nonce"),
			("/message/deadline", json!("1800000001"), "deadline"),
			("/domain/chainId", json!(1), "chain id"),
			(
				"/domain/verifyingContract",
				json!("0x0000000000000000000000000000000000000003"),
				"verifying contract",
			),
		] {
			let mut t = shown(&p);
			*t.pointer_mut(path).unwrap() = value;
			assert_eq!(
				p.matches(&t),
				Err(format!(
					"the typed data's {what} differs from the agent's own"
				)),
				"{path}"
			);
		}
	}
}
