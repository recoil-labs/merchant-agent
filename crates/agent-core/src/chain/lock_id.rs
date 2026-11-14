//! A fiat trade's lock id, as `AttestedEscrow.lockIdOf` and the aggregator
//! (`oif_types::fiat_trades::onchain`) derive it:
//!
//! ```text
//! trade_id_hash = keccak256(utf8(trade id))
//! lock_id       = keccak256(abi.encode(funder, trade_id_hash))
//! ```
//!
//! Copied rather than depended on (design §2), and pinned to the aggregator
//! by `vectors/fiat-lock.json`, which both sides check.

use alloy::primitives::{keccak256, Address, B256};

pub fn trade_id_hash(trade_id: &str) -> B256 {
	keccak256(trade_id.as_bytes())
}

pub fn lock_id(funder: Address, trade_id_hash: B256) -> B256 {
	// abi.encode(address, bytes32): the address left-padded to 32 bytes.
	let mut encoded = [0u8; 64];
	encoded[12..32].copy_from_slice(funder.as_slice());
	encoded[32..].copy_from_slice(trade_id_hash.as_slice());
	keccak256(encoded)
}

#[cfg(test)]
mod tests {
	use super::*;

	const VECTOR: &str = include_str!("../../../../vectors/fiat-lock.json");

	#[test]
	fn lock_ids_match_the_shared_vector() {
		let v: serde_json::Value = serde_json::from_str(VECTOR).unwrap();
		let cases = v["cases"].as_array().unwrap();
		assert!(cases.len() >= 3);
		for c in cases {
			let id = c["tradeId"].as_str().unwrap();
			let hash = trade_id_hash(id);
			assert_eq!(hash.to_string(), c["tradeIdHash"].as_str().unwrap(), "{id}");
			let funder: Address = c["funder"].as_str().unwrap().parse().unwrap();
			assert_eq!(
				lock_id(funder, hash).to_string(),
				c["lockId"].as_str().unwrap(),
				"{id}"
			);
		}
	}
}
