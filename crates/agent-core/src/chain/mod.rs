//! The independent lock check (invariant 3).
//!
//! Before paying, the agent reads the escrow lock itself, through the
//! merchant's own RPC, and compares every field with what it expects. The
//! aggregator's word is never enough:
//!
//! - **Where:** the escrow address is the one **pinned** in config.
//! - **What:** funder = the trade's user; counterparty = the merchant's
//!   **pinned** wallet (never the address the aggregator names); token = the
//!   **pinned** USDC; amount = the trade's `asset_minor_units`.
//! - **Open:** created, not released, not refunded.
//! - **Buried:** present `confirmations` blocks deep, so a reorg can't take
//!   it back after the money has gone.
//! - **Agreed:** optionally, every configured RPC must give the same answer.
//!
//! It also reads the lock's `protocolFee`, the fee the escrow will really
//! take, which the amount check (M7) uses instead of the aggregator's
//! figure (design §2).
//!
//! Anything odd is an error or a non-`Verified` verdict, and the caller
//! holds the trade. RPC errors name the endpoint by position (`rpc[0]`),
//! never by URL, because a URL may carry an API key.

mod lock_id;

pub use lock_id::{lock_id, trade_id_hash};

use std::time::Duration;

use alloy::primitives::{Address, Bytes, TxKind, B256, U256};
use alloy::providers::{Provider, RootProvider};
use alloy::rpc::client::RpcClient;
use alloy::rpc::types::{BlockId, TransactionInput, TransactionRequest};
use alloy::sol;
use alloy::sol_types::SolCall;
use alloy::transports::http::Http;

use crate::aggregator::types::{Direction, Trade};
use crate::config;

sol! {
	function locks(bytes32 lockId) external view;
	function balanceOf(address owner) external view returns (uint256);
	function allowance(address owner, address spender) external view returns (uint256);
}

/// What the lock for a trade must look like.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExpectedLock {
	pub lock_id: B256,
	pub funder: Address,
	pub counterparty: Address,
	pub token: Address,
	pub amount: U256,
}

/// Why no lock can even be expected for a trade. Each one holds the trade.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ExpectError {
	#[error("only cash-outs (off-ramp) are paid by this agent")]
	NotOfframp,
	#[error("the trade is on {trade}, but this chain is pinned as {pinned}")]
	WrongChain { trade: String, pinned: String },
	#[error("the trade is in {0}, not this chain's pinned USDC")]
	WrongAsset(String),
	#[error("the trade's {0} is not an address")]
	BadAddress(&'static str),
	#[error("the aggregator says the lock pays {aggregator}, but your wallet is {pinned}")]
	CounterpartyDiffers { aggregator: String, pinned: String },
}

/// The lock `trade` must have on `chain`, paying `wallet`. Every address
/// that decides where money goes comes from config; the trade supplies
/// only the funder, the id and the amount, which the chain then confirms.
pub fn expected_lock(
	trade: &Trade,
	chain: &config::Chain,
	wallet: &str,
) -> Result<ExpectedLock, ExpectError> {
	if trade.direction != Direction::Offramp {
		return Err(ExpectError::NotOfframp);
	}
	if trade.chain != chain.caip2 {
		return Err(ExpectError::WrongChain {
			trade: trade.chain.clone(),
			pinned: chain.caip2.clone(),
		});
	}
	if !(trade.asset.eq_ignore_ascii_case("USDC") || trade.asset.eq_ignore_ascii_case(&chain.usdc))
	{
		return Err(ExpectError::WrongAsset(trade.asset.clone()));
	}
	let pinned: Address = wallet
		.parse()
		.map_err(|_| ExpectError::BadAddress("pinned wallet"))?;
	let named: Address = trade
		.solver_address
		.parse()
		.map_err(|_| ExpectError::BadAddress("solver address"))?;
	if named != pinned {
		return Err(ExpectError::CounterpartyDiffers {
			aggregator: named.to_string(),
			pinned: pinned.to_string(),
		});
	}
	let funder: Address = trade
		.user_address
		.parse()
		.map_err(|_| ExpectError::BadAddress("user address"))?;
	let token: Address = chain
		.usdc
		.parse()
		.map_err(|_| ExpectError::BadAddress("pinned USDC"))?;
	Ok(ExpectedLock {
		lock_id: lock_id(funder, trade_id_hash(&trade.id)),
		funder,
		counterparty: pinned,
		token,
		amount: U256::from(trade.asset_minor_units.get()),
	})
}

/// A lock as the escrow stores it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OnchainLock {
	pub token: Address,
	pub amount: U256,
	pub funder: Address,
	pub counterparty: Address,
	pub locked_at: u64,
	pub released: bool,
	/// The 7th word; 0 on an escrow deployed before the fee existed.
	pub protocol_fee: U256,
}

/// Decode `locks(bytes32)`. Six static words, plus a seventh
/// (`protocolFee`) on escrows that charge one, exactly as the aggregator's
/// `decode_lock` reads them. Anything shorter is not this contract.
pub fn decode_lock(out: &[u8]) -> Result<OnchainLock, ChainError> {
	let bad = |m: &str| ChainError::Malformed(m.to_string());
	if out.len() < 6 * 32 || !out.len().is_multiple_of(32) {
		return Err(bad("the escrow answered locks() with the wrong length"));
	}
	let word = |i: usize| &out[i * 32..(i + 1) * 32];
	let address = |i: usize| {
		let w = word(i);
		if w[..12].iter().any(|b| *b != 0) {
			return Err(bad("an address word has high bits set"));
		}
		Ok(Address::from_slice(&w[12..]))
	};
	let locked_at_word = U256::from_be_slice(word(4));
	let released = U256::from_be_slice(word(5));
	if locked_at_word > U256::from(u64::MAX) || released > U256::from(1) {
		return Err(bad("locked_at_word or released is out of range"));
	}
	Ok(OnchainLock {
		token: address(0)?,
		amount: U256::from_be_slice(word(1)),
		funder: address(2)?,
		counterparty: address(3)?,
		locked_at: locked_at_word.to::<u64>(),
		released: released == U256::from(1),
		protocol_fee: if out.len() >= 7 * 32 {
			U256::from_be_slice(word(6))
		} else {
			U256::ZERO
		},
	})
}

/// What the chain says about a trade's lock.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LockVerdict {
	/// Open, buried deep enough, and every field matches.
	Verified { protocol_fee: U256, locked_at: u64 },
	/// No lock (yet). Wait, unless the deadline is near.
	Missing,
	/// The lock is there but not yet `confirmations` deep. Wait.
	NotYetAtDepth,
	/// Released or refunded: paying now would be paying for nothing.
	Closed,
	/// A lock exists under this id but differs; each entry names a field.
	Mismatch(Vec<String>),
}

#[derive(Debug, Clone, thiserror::Error)]
pub enum ChainError {
	#[error("{rpc}: {why}")]
	Rpc { rpc: String, why: String },
	#[error("{rpc} is on chain {got}, but this chain is pinned as {want}")]
	WrongChain { rpc: String, got: u64, want: u64 },
	#[error("there is no contract at the pinned escrow {0}")]
	NoEscrowCode(Address),
	#[error("{0}")]
	Malformed(String),
	#[error("the RPCs disagree about the lock: {0}")]
	Disagree(String),
	#[error("{0}")]
	Config(String),
}

struct Endpoint {
	label: String,
	url: String,
	/// The URL as the HTTP client normalised it: what its errors print.
	parsed: String,
	provider: RootProvider,
}

impl Endpoint {
	/// An RPC error, with the URL (which may carry an API key) replaced by
	/// the endpoint's position.
	fn err(&self, e: impl std::fmt::Display) -> ChainError {
		ChainError::Rpc {
			rpc: self.label.clone(),
			why: scrub_url(&e.to_string(), &[&self.url, &self.parsed], &self.label),
		}
	}
}

/// `why` with every spelling of the endpoint's URL replaced by `label`: the
/// configured text, the client's normalised form (lower-case host, `/`
/// before a query, ...), each with or without a trailing slash, and
/// whatever an HTTP error puts in its "for url (...)" suffix.
fn scrub_url(why: &str, urls: &[&str], label: &str) -> String {
	let mut why = why.to_string();
	for url in urls {
		let url = url.trim();
		if url.is_empty() {
			continue;
		}
		for form in [
			url.to_string(),
			format!("{}/", url.trim_end_matches('/')),
			url.trim_end_matches('/').to_string(),
		] {
			if !form.is_empty() {
				why = why.replace(&form, label);
			}
		}
	}
	const MARK: &str = "for url (";
	let mut out = String::new();
	let mut rest = why.as_str();
	while let Some(i) = rest.find(MARK) {
		out.push_str(&rest[..i + MARK.len()]);
		rest = &rest[i + MARK.len()..];
		match rest.find(')') {
			Some(j) => {
				out.push_str(label);
				rest = &rest[j..];
			}
			None => {
				out.push_str(label);
				out.push(')');
				rest = "";
			}
		}
	}
	out.push_str(rest);
	out
}

/// What `check` found at each endpoint.
#[derive(Debug, Clone)]
pub struct EndpointStatus {
	pub rpc: String,
	pub latest_block: u64,
}

/// Reads one chain's escrow through the merchant's own RPC endpoints.
pub struct ChainReader {
	chain_id: u64,
	escrow: Address,
	confirmations: u64,
	require_agreement: bool,
	endpoints: Vec<Endpoint>,
}

impl ChainReader {
	/// `rpc_urls` are the chain's endpoints with any secret ones already
	/// resolved. `https_only` must be true in production (invariant 9).
	pub fn new(
		chain: &config::Chain,
		rpc_urls: Vec<String>,
		timeout: Duration,
		https_only: bool,
	) -> Result<Self, ChainError> {
		let chain_id: u64 = chain
			.caip2
			.strip_prefix("eip155:")
			.and_then(|n| n.parse().ok())
			.ok_or_else(|| ChainError::Config(format!("{} is not an EVM chain", chain.caip2)))?;
		let escrow: Address = chain
			.escrow
			.parse()
			.map_err(|_| ChainError::Config("the pinned escrow is not an address".into()))?;
		if rpc_urls.is_empty() {
			return Err(ChainError::Config("no RPC endpoints".into()));
		}
		let mut endpoints = Vec::new();
		for (i, url) in rpc_urls.into_iter().enumerate() {
			let label = format!("{} rpc[{i}]", chain.name);
			if https_only && !url.starts_with("https://") {
				return Err(ChainError::Config(format!(
					"{label} must use https:// in production"
				)));
			}
			// alloy's own reqwest (a newer major than the aggregator client's).
			use alloy::transports::http::reqwest;
			let parsed: reqwest::Url = url
				.trim()
				.parse()
				.map_err(|_| ChainError::Config(format!("{label} is not a URL")))?;
			let parsed_text = parsed.to_string();
			let http = reqwest::Client::builder()
				.timeout(timeout)
				.connect_timeout(timeout)
				.redirect(reqwest::redirect::Policy::none())
				.https_only(https_only)
				.build()
				.map_err(|e| ChainError::Config(e.to_string()))?;
			let provider =
				RootProvider::new(RpcClient::new(Http::with_client(http, parsed), false));
			endpoints.push(Endpoint {
				label,
				url,
				parsed: parsed_text,
				provider,
			});
		}
		Ok(Self {
			chain_id,
			escrow,
			confirmations: chain.confirmations.max(1),
			require_agreement: chain.require_rpc_agreement,
			endpoints,
		})
	}

	/// For `doctor`: every endpoint answers, is on the pinned chain, and
	/// has an escrow at the pinned address that answers `locks()` as
	/// `AttestedEscrow` does.
	pub async fn check(&self) -> Result<Vec<EndpointStatus>, ChainError> {
		let mut out = Vec::new();
		for ep in &self.endpoints {
			self.require_chain(ep).await?;
			let code = ep
				.provider
				.get_code_at(self.escrow)
				.await
				.map_err(|e| ep.err(e))?;
			if code.is_empty() {
				return Err(ChainError::NoEscrowCode(self.escrow));
			}
			let latest = ep
				.provider
				.get_block_number()
				.await
				.map_err(|e| ep.err(e))?;
			let probe = self.read_lock(ep, B256::ZERO, latest).await?;
			if probe.locked_at != 0 {
				return Err(ChainError::Malformed(
					"the escrow reports a lock under the zero id".into(),
				));
			}
			out.push(EndpointStatus {
				rpc: ep.label.clone(),
				latest_block: latest,
			});
		}
		Ok(out)
	}

	/// Is the chain readable now? Every endpoint when they must agree,
	/// otherwise any one of them. For `/readyz`.
	pub async fn ping(&self) -> Result<(), ChainError> {
		let mut last = None;
		for ep in &self.endpoints {
			match self.require_chain(ep).await {
				Ok(()) if !self.require_agreement => return Ok(()),
				Ok(()) => {}
				Err(e) if self.require_agreement => return Err(e),
				Err(e) => last = Some(e),
			}
		}
		match last {
			Some(e) => Err(e),
			None => Ok(()),
		}
	}

	/// Check the lock for `expected`. With `require_rpc_agreement`, every
	/// endpoint must agree; otherwise the first endpoint that answers
	/// decides, and the rest are fallbacks.
	pub async fn verify(&self, expected: &ExpectedLock) -> Result<LockVerdict, ChainError> {
		if !self.require_agreement {
			let mut last = None;
			for ep in &self.endpoints {
				match self.verify_at(ep, expected).await {
					Ok(v) => return Ok(v),
					Err(e) => last = Some(e),
				}
			}
			return Err(last.expect("at least one endpoint"));
		}

		let mut verdicts = Vec::new();
		for ep in &self.endpoints {
			verdicts.push((ep.label.clone(), self.verify_at(ep, expected).await?));
		}
		let (_, first) = &verdicts[0];
		if verdicts.iter().all(|(_, v)| v == first) {
			return Ok(first.clone());
		}
		// One endpoint being a block or two behind is not a disagreement:
		// a lock some see at depth and others see shallower just waits.
		let settled_or_waiting = |v: &LockVerdict| {
			matches!(v, LockVerdict::Verified { .. } | LockVerdict::NotYetAtDepth)
		};
		let verified: Vec<_> = verdicts
			.iter()
			.filter_map(|(_, v)| match v {
				LockVerdict::Verified { .. } => Some(v),
				_ => None,
			})
			.collect();
		if verdicts.iter().all(|(_, v)| settled_or_waiting(v))
			&& verified.windows(2).all(|w| w[0] == w[1])
		{
			return Ok(LockVerdict::NotYetAtDepth);
		}
		Err(ChainError::Disagree(
			verdicts
				.iter()
				.map(|(rpc, v)| format!("{rpc}: {v:?}"))
				.collect::<Vec<_>>()
				.join("; "),
		))
	}

	/// A float wallet's balance of `token` and how much of it `spender`
	/// (Permit2) may move, at the latest block, from the first endpoint that
	/// answers on the pinned chain. For merchant-signed purchases (v0.3).
	pub async fn float(
		&self,
		token: Address,
		owner: Address,
		spender: Address,
	) -> Result<(U256, U256), ChainError> {
		let mut last = ChainError::Config("no RPC endpoints".into());
		for ep in &self.endpoints {
			match self.float_at(ep, token, owner, spender).await {
				Ok(v) => return Ok(v),
				Err(e) => last = e,
			}
		}
		Err(last)
	}

	async fn float_at(
		&self,
		ep: &Endpoint,
		token: Address,
		owner: Address,
		spender: Address,
	) -> Result<(U256, U256), ChainError> {
		self.require_chain(ep).await?;
		let read = |input: Vec<u8>| async move {
			let call = TransactionRequest {
				to: Some(TxKind::Call(token)),
				input: TransactionInput::new(Bytes::from(input)),
				..Default::default()
			};
			let out = ep.provider.call(call).await.map_err(|e| ep.err(e))?;
			if out.len() != 32 {
				return Err(ChainError::Malformed(
					"the token's answer is not one uint256".into(),
				));
			}
			Ok(U256::from_be_slice(&out))
		};
		let balance = read(balanceOfCall { owner }.abi_encode()).await?;
		let allowance = read(allowanceCall { owner, spender }.abi_encode()).await?;
		Ok((balance, allowance))
	}

	async fn require_chain(&self, ep: &Endpoint) -> Result<(), ChainError> {
		let got = ep.provider.get_chain_id().await.map_err(|e| ep.err(e))?;
		if got != self.chain_id {
			return Err(ChainError::WrongChain {
				rpc: ep.label.clone(),
				got,
				want: self.chain_id,
			});
		}
		Ok(())
	}

	async fn read_lock(
		&self,
		ep: &Endpoint,
		id: B256,
		block: u64,
	) -> Result<OnchainLock, ChainError> {
		let call = TransactionRequest {
			to: Some(TxKind::Call(self.escrow)),
			input: TransactionInput::new(Bytes::from(locksCall { lockId: id }.abi_encode())),
			..Default::default()
		};
		let out = ep
			.provider
			.call(call)
			.block(BlockId::number(block))
			.await
			.map_err(|e| ep.err(e))?;
		decode_lock(&out)
	}

	async fn verify_at(
		&self,
		ep: &Endpoint,
		expected: &ExpectedLock,
	) -> Result<LockVerdict, ChainError> {
		self.require_chain(ep).await?;
		let latest = ep
			.provider
			.get_block_number()
			.await
			.map_err(|e| ep.err(e))?;

		// Latest first: a lock released since it was buried is closed now,
		// whatever an older block says.
		let now = self.read_lock(ep, expected.lock_id, latest).await?;
		if now.locked_at == 0 {
			return Ok(LockVerdict::Missing);
		}
		if now.released {
			return Ok(LockVerdict::Closed);
		}
		// Then `confirmations` deep: block `latest - confirmations + 1`
		// holds the lock only if it was mined at least that many blocks ago.
		let Some(depth) = (latest + 1).checked_sub(self.confirmations) else {
			return Ok(LockVerdict::NotYetAtDepth);
		};
		let buried = self.read_lock(ep, expected.lock_id, depth).await?;
		if buried.locked_at == 0 {
			return Ok(LockVerdict::NotYetAtDepth);
		}
		if buried != now {
			return Err(ChainError::Malformed(
				"a lock's fields changed between blocks".into(),
			));
		}

		let mut wrong = Vec::new();
		if buried.funder != expected.funder {
			wrong.push(format!(
				"funder is {}, expected {}",
				buried.funder, expected.funder
			));
		}
		if buried.counterparty != expected.counterparty {
			wrong.push(format!(
				"counterparty is {}, expected your wallet {}",
				buried.counterparty, expected.counterparty
			));
		}
		if buried.token != expected.token {
			wrong.push(format!(
				"token is {}, expected USDC {}",
				buried.token, expected.token
			));
		}
		if buried.amount != expected.amount {
			wrong.push(format!(
				"amount is {}, expected {}",
				buried.amount, expected.amount
			));
		}
		if buried.protocol_fee > buried.amount {
			wrong.push(format!(
				"protocol fee {} exceeds the amount",
				buried.protocol_fee
			));
		}
		if !wrong.is_empty() {
			return Ok(LockVerdict::Mismatch(wrong));
		}
		Ok(LockVerdict::Verified {
			protocol_fee: buried.protocol_fee,
			locked_at: buried.locked_at,
		})
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	fn words(ws: &[[u8; 32]]) -> Vec<u8> {
		ws.iter().flatten().copied().collect()
	}

	fn w_u(n: u64) -> [u8; 32] {
		U256::from(n).to_be_bytes()
	}

	fn w_a(a: u8) -> [u8; 32] {
		let mut w = [0u8; 32];
		w[31] = a;
		w
	}

	#[test]
	fn decodes_six_and_seven_word_locks() {
		let six = words(&[w_a(1), w_u(100), w_a(2), w_a(3), w_u(1_700_000_000), w_u(0)]);
		let l = decode_lock(&six).unwrap();
		assert_eq!(l.amount, U256::from(100));
		assert_eq!(l.protocol_fee, U256::ZERO);
		assert!(!l.released);

		let seven = words(&[w_a(1), w_u(100), w_a(2), w_a(3), w_u(1), w_u(1), w_u(5)]);
		let l = decode_lock(&seven).unwrap();
		assert!(l.released);
		assert_eq!(l.protocol_fee, U256::from(5));
	}

	#[test]
	fn refuses_answers_that_are_not_this_contract() {
		assert!(decode_lock(&[0u8; 64]).is_err());
		assert!(decode_lock(&[0u8; 6 * 32 + 1]).is_err());
		let dirty_address = words(&[[0xff; 32], w_u(1), w_a(2), w_a(3), w_u(1), w_u(0)]);
		assert!(decode_lock(&dirty_address).is_err());
		let bad_bool = words(&[w_a(1), w_u(1), w_a(2), w_a(3), w_u(1), w_u(2)]);
		assert!(decode_lock(&bad_bool).is_err());
	}
}

#[cfg(test)]
mod scrub_tests {
	use super::scrub_url;

	#[test]
	fn every_spelling_of_an_rpc_url_is_scrubbed() {
		let raw = "https://RPC.example.com?apikey=K3Y";
		let parsed = "https://rpc.example.com/?apikey=K3Y";
		for why in [
			format!("error sending request for url ({parsed})"),
			format!("{raw} timed out"),
			"error for url (https://rpc.example.com:443/?apikey=K3Y) after 30s".to_string(),
		] {
			let out = scrub_url(&why, &[raw, parsed], "base rpc[0]");
			assert!(!out.contains("K3Y"), "{out}");
			assert!(out.contains("base rpc[0]"), "{out}");
		}
	}
}
