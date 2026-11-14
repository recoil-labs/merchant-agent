//! The independent lock check against the real `AttestedEscrow`, on anvil.
//!
//! Each test starts its own anvil, deploys the escrow and a mock USDC from
//! the bytecode in `tests/fixtures/` (compiled from RecoilPay's
//! `AttestedEscrow`; refreshed by `scripts/sync-from-monorepo.sh`), and makes real locks with the escrow's own `lock()`.
//! Anvil's accounts are unlocked, so transactions need no signing code.
//!
//! Needs Foundry's `anvil`. Without it each test prints SKIPPED and passes, unless
//! `MERCHANT_AGENT_REQUIRE_ANVIL=1` is set, as CI sets it.

use std::net::TcpListener;
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use alloy::primitives::{Address, B256, U256};
use alloy::sol;
use alloy::sol_types::{SolCall, SolValue};
use merchant_agent_core::aggregator::types::Trade;
use merchant_agent_core::chain::{
	expected_lock, lock_id, trade_id_hash, ChainError, ChainReader, ExpectedLock, LockVerdict,
};
use merchant_agent_core::config::{Chain, Rpc};
use serde_json::{json, Value};

sol! {
	function mint(address to, uint256 amount);
	function approve(address spender, uint256 amount);
	function lock(bytes32 tradeId, address token, uint256 amount, address counterparty);
	function release(bytes32 lockId, address to);
	function lockIdOf(address funder, bytes32 tradeId);
}

const FIXTURE: &str = include_str!("../../../vectors/merchant-agent-api.json");
const TRADE_ID: &str = "ft-7c1e9a2b4d6f48e0a1b2c3d4e5f60718";
const AMOUNT: u64 = 100_000_000; // 100 USDC
const FEE_BPS: u16 = 50;
const CONFIRMATIONS: u64 = 3;

// ── Foundry ──────────────────────────────────────────────────────────────

fn fixture(name: &str) -> PathBuf {
	PathBuf::from(env!("CARGO_MANIFEST_DIR"))
		.join("tests/fixtures")
		.join(name)
}

// Printing is the point: say clearly that these tests did not run.
#[allow(clippy::print_stderr)]
fn skip_or_fail<T>(why: &str) -> Option<T> {
	#[allow(clippy::disallowed_methods)]
	let required = std::env::var("MERCHANT_AGENT_REQUIRE_ANVIL").is_ok_and(|v| v == "1");
	if required {
		panic!("{why}, and MERCHANT_AGENT_REQUIRE_ANVIL=1 requires these tests to run");
	}
	eprintln!(
		"SKIPPED: {why}. Install Foundry (https://getfoundry.sh) to run the on-chain lock tests."
	);
	None
}

fn has(bin: &str) -> bool {
	Command::new(bin)
		.arg("--version")
		.stdout(Stdio::null())
		.stderr(Stdio::null())
		.status()
		.is_ok()
}

/// Creation bytecode for the escrow and the mock token.
fn artifacts() -> Option<(Vec<u8>, Vec<u8>)> {
	if !has("anvil") {
		return skip_or_fail("anvil is not installed");
	}
	let escrow = fixture("AttestedEscrow.json");
	let token = fixture("MockToken.json");
	let code = |p: PathBuf| {
		let v: Value = serde_json::from_str(&std::fs::read_to_string(&p).unwrap()).unwrap();
		hex::decode(
			v["bytecode"]["object"]
				.as_str()
				.unwrap()
				.trim_start_matches("0x"),
		)
		.unwrap()
	};
	Some((code(escrow), code(token)))
}

struct Anvil {
	child: Child,
	url: String,
}

impl Drop for Anvil {
	fn drop(&mut self) {
		let _ = self.child.kill();
		let _ = self.child.wait();
	}
}

fn start_anvil() -> Anvil {
	let port = TcpListener::bind("127.0.0.1:0")
		.unwrap()
		.local_addr()
		.unwrap()
		.port();
	let child = Command::new("anvil")
		.args(["--port", &port.to_string(), "--silent"])
		.stdout(Stdio::null())
		.stderr(Stdio::null())
		.spawn()
		.expect("anvil starts");
	Anvil {
		child,
		url: format!("http://127.0.0.1:{port}"),
	}
}

// ── raw JSON-RPC, independent of the code under test ─────────────────────

async fn rpc(url: &str, method: &str, params: Value) -> Value {
	let body = json!({"jsonrpc": "2.0", "id": 1, "method": method, "params": params});
	let v: Value = reqwest::Client::new()
		.post(url)
		.json(&body)
		.send()
		.await
		.unwrap()
		.json()
		.await
		.unwrap();
	assert!(v.get("error").is_none(), "{method}: {v}");
	v["result"].clone()
}

async fn wait_up(url: &str) {
	let started = Instant::now();
	while started.elapsed() < Duration::from_secs(15) {
		let body = json!({"jsonrpc": "2.0", "id": 1, "method": "eth_chainId", "params": []});
		if reqwest::Client::new()
			.post(url)
			.json(&body)
			.send()
			.await
			.is_ok()
		{
			return;
		}
		tokio::time::sleep(Duration::from_millis(100)).await;
	}
	panic!("anvil did not start");
}

/// Send a transaction from an unlocked anvil account; return the receipt.
async fn send(url: &str, from: Address, to: Option<Address>, data: Vec<u8>) -> Value {
	let mut tx =
		json!({"from": from, "data": format!("0x{}", hex::encode(data)), "gas": "0x989680"});
	if let Some(to) = to {
		tx["to"] = json!(to);
	}
	let hash = rpc(url, "eth_sendTransaction", json!([tx])).await;
	// Automining is quick but not instant under parallel tests.
	let started = Instant::now();
	let receipt = loop {
		let r = rpc(url, "eth_getTransactionReceipt", json!([hash])).await;
		if !r.is_null() || started.elapsed() > Duration::from_secs(10) {
			break r;
		}
		tokio::time::sleep(Duration::from_millis(50)).await;
	};
	assert_eq!(receipt["status"], "0x1", "transaction reverted: {receipt}");
	receipt
}

async fn mine(url: &str, blocks: u64) {
	rpc(url, "anvil_mine", json!([format!("0x{blocks:x}")])).await;
}

// ── a deployed escrow with a lock ────────────────────────────────────────

struct World {
	anvil: Anvil,
	escrow: Address,
	token: Address,
	attestor: Address,
	funder: Address,
	merchant: Address,
	stranger: Address,
}

/// Deploy the escrow (50 bps fee) and a mock USDC. Deterministic: the same
/// calls on a fresh anvil give the same addresses.
async fn deploy() -> Option<World> {
	let (escrow_code, token_code) = artifacts()?;
	let anvil = start_anvil();
	wait_up(&anvil.url).await;
	let accounts: Vec<Address> =
		serde_json::from_value(rpc(&anvil.url, "eth_accounts", json!([])).await).unwrap();
	let [deployer, attestor, treasury, fee_admin, funder, merchant, stranger] =
		accounts[..7].try_into().unwrap();

	let mut code = escrow_code;
	code.extend((attestor, treasury, fee_admin, FEE_BPS).abi_encode_params());
	let escrow = contract_address(send(&anvil.url, deployer, None, code).await);
	let token = contract_address(send(&anvil.url, deployer, None, token_code).await);
	Some(World {
		anvil,
		escrow,
		token,
		attestor,
		funder,
		merchant,
		stranger,
	})
}

fn contract_address(receipt: Value) -> Address {
	receipt["contractAddress"]
		.as_str()
		.unwrap()
		.parse()
		.unwrap()
}

impl World {
	fn url(&self) -> &str {
		&self.anvil.url
	}

	/// The funder locks `AMOUNT` for `TRADE_ID`, paying `counterparty`.
	async fn lock(&self, counterparty: Address) {
		let url = self.url();
		send(
			url,
			self.funder,
			Some(self.token),
			mintCall {
				to: self.funder,
				amount: U256::from(AMOUNT),
			}
			.abi_encode(),
		)
		.await;
		send(
			url,
			self.funder,
			Some(self.token),
			approveCall {
				spender: self.escrow,
				amount: U256::from(AMOUNT),
			}
			.abi_encode(),
		)
		.await;
		let call = lockCall {
			tradeId: trade_id_hash(TRADE_ID),
			token: self.token,
			amount: U256::from(AMOUNT),
			counterparty,
		};
		send(url, self.funder, Some(self.escrow), call.abi_encode()).await;
	}

	fn lock_id(&self) -> B256 {
		lock_id(self.funder, trade_id_hash(TRADE_ID))
	}

	fn chain(&self) -> Chain {
		Chain {
			caip2: "eip155:31337".into(),
			name: "anvil".into(),
			escrow: self.escrow.to_string(),
			usdc: self.token.to_string(),
			usdc_decimals: 6,
			rpcs: vec![Rpc {
				url: Some(self.url().into()),
				secret: None,
			}],
			confirmations: CONFIRMATIONS,
			require_rpc_agreement: false,
		}
	}

	fn reader(&self) -> ChainReader {
		ChainReader::new(
			&self.chain(),
			vec![self.url().into()],
			Duration::from_secs(5),
			false,
		)
		.unwrap()
	}

	/// The trade the aggregator would send for this lock: the shared
	/// fixture, pointed at this anvil's addresses.
	fn trade(&self) -> Trade {
		let mut t = serde_json::from_str::<Value>(FIXTURE).unwrap()["ownFiatTrade"].clone();
		t["chain"] = json!("eip155:31337");
		t["user_address"] = json!(self.funder);
		t["solver_address"] = json!(self.merchant);
		t["asset_minor_units"] = json!(AMOUNT.to_string());
		serde_json::from_value(t).unwrap()
	}

	fn expected(&self) -> ExpectedLock {
		expected_lock(&self.trade(), &self.chain(), &self.merchant.to_string()).unwrap()
	}
}

fn fee() -> U256 {
	U256::from(AMOUNT) * U256::from(FEE_BPS) / U256::from(10_000)
}

// ── tests ────────────────────────────────────────────────────────────────

#[tokio::test]
async fn invariant_3_a_matching_lock_at_depth_is_verified() {
	let Some(w) = deploy().await else { return };
	w.lock(w.merchant).await;
	mine(w.url(), CONFIRMATIONS - 1).await;
	match w.reader().verify(&w.expected()).await.unwrap() {
		LockVerdict::Verified {
			protocol_fee,
			locked_at,
		} => {
			assert_eq!(protocol_fee, fee(), "the fee the escrow will really take");
			assert!(locked_at > 0);
		}
		other => panic!("expected Verified, got {other:?}"),
	}
}

#[tokio::test]
async fn invariant_3_a_lock_not_yet_at_depth_waits() {
	let Some(w) = deploy().await else { return };
	w.lock(w.merchant).await;
	let reader = w.reader();
	let expected = w.expected();
	assert_eq!(
		reader.verify(&expected).await.unwrap(),
		LockVerdict::NotYetAtDepth
	);
	mine(w.url(), CONFIRMATIONS - 2).await;
	assert_eq!(
		reader.verify(&expected).await.unwrap(),
		LockVerdict::NotYetAtDepth,
		"one block short"
	);
	mine(w.url(), 1).await;
	assert!(matches!(
		reader.verify(&expected).await.unwrap(),
		LockVerdict::Verified { .. }
	));
}

#[tokio::test]
async fn invariant_3_each_single_field_mismatch_is_caught() {
	let Some(w) = deploy().await else { return };
	w.lock(w.merchant).await;
	mine(w.url(), CONFIRMATIONS).await;
	let reader = w.reader();
	let right = w.expected();

	let cases: [(&str, ExpectedLock); 4] = [
		(
			"funder",
			ExpectedLock {
				funder: w.stranger,
				..right.clone()
			},
		),
		(
			"counterparty",
			ExpectedLock {
				counterparty: w.stranger,
				..right.clone()
			},
		),
		(
			"token",
			ExpectedLock {
				token: w.stranger,
				..right.clone()
			},
		),
		(
			"amount",
			ExpectedLock {
				amount: right.amount + U256::from(1),
				..right.clone()
			},
		),
	];
	for (field, expected) in cases {
		match reader.verify(&expected).await.unwrap() {
			LockVerdict::Mismatch(wrong) => {
				assert_eq!(
					wrong.len(),
					1,
					"{field}: exactly one field differs: {wrong:?}"
				);
				assert!(wrong[0].starts_with(field), "{field}: {wrong:?}");
			}
			other => panic!("{field}: expected Mismatch, got {other:?}"),
		}
	}
}

#[tokio::test]
async fn invariant_3_a_lock_paying_someone_else_is_caught() {
	let Some(w) = deploy().await else { return };
	// The aggregator says the merchant is paid, but the lock pays a stranger.
	w.lock(w.stranger).await;
	mine(w.url(), CONFIRMATIONS).await;
	match w.reader().verify(&w.expected()).await.unwrap() {
		LockVerdict::Mismatch(wrong) => assert!(wrong[0].contains("counterparty"), "{wrong:?}"),
		other => panic!("expected Mismatch, got {other:?}"),
	}
}

#[tokio::test]
async fn invariant_3_a_released_lock_is_closed() {
	let Some(w) = deploy().await else { return };
	w.lock(w.merchant).await;
	mine(w.url(), CONFIRMATIONS).await;
	let call = releaseCall {
		lockId: w.lock_id(),
		to: w.funder,
	};
	send(w.url(), w.attestor, Some(w.escrow), call.abi_encode()).await;
	assert_eq!(
		w.reader().verify(&w.expected()).await.unwrap(),
		LockVerdict::Closed
	);
}

#[tokio::test]
async fn a_missing_lock_is_missing() {
	let Some(w) = deploy().await else { return };
	mine(w.url(), CONFIRMATIONS).await;
	assert_eq!(
		w.reader().verify(&w.expected()).await.unwrap(),
		LockVerdict::Missing
	);
}

#[tokio::test]
async fn the_contract_and_the_agent_derive_the_same_lock_id() {
	let Some(w) = deploy().await else { return };
	let call = lockIdOfCall {
		funder: w.funder,
		tradeId: trade_id_hash(TRADE_ID),
	};
	let out = rpc(
		w.url(),
		"eth_call",
		json!([{"to": w.escrow, "data": format!("0x{}", hex::encode(call.abi_encode()))}, "latest"]),
	)
	.await;
	assert_eq!(out.as_str().unwrap(), w.lock_id().to_string());
}

#[tokio::test]
async fn invariant_3_an_rpc_on_another_chain_is_refused() {
	let Some(w) = deploy().await else { return };
	let chain = Chain {
		caip2: "eip155:84532".into(),
		..w.chain()
	};
	let reader =
		ChainReader::new(&chain, vec![w.url().into()], Duration::from_secs(5), false).unwrap();
	let e = reader.verify(&w.expected()).await.unwrap_err();
	assert!(
		matches!(
			e,
			ChainError::WrongChain {
				got: 31337,
				want: 84532,
				..
			}
		),
		"{e}"
	);
}

#[tokio::test]
async fn two_rpcs_must_agree_when_required() {
	let Some(w) = deploy().await else { return };
	w.lock(w.merchant).await;
	mine(w.url(), CONFIRMATIONS).await;

	let both_same = Chain {
		require_rpc_agreement: true,
		..w.chain()
	};
	let reader = ChainReader::new(
		&both_same,
		vec![w.url().into(), w.url().into()],
		Duration::from_secs(5),
		false,
	)
	.unwrap();
	assert!(matches!(
		reader.verify(&w.expected()).await.unwrap(),
		LockVerdict::Verified { .. }
	));

	// A second chain with the same contracts at the same addresses, but no
	// lock: an RPC telling a different story.
	let Some(other) = deploy().await else { return };
	assert_eq!(other.escrow, w.escrow, "deterministic deployment");
	mine(other.url(), CONFIRMATIONS).await;
	let reader = ChainReader::new(
		&both_same,
		vec![w.url().into(), other.url().into()],
		Duration::from_secs(5),
		false,
	)
	.unwrap();
	let e = reader.verify(&w.expected()).await.unwrap_err();
	assert!(matches!(e, ChainError::Disagree(_)), "{e}");
}

#[tokio::test]
async fn the_doctor_check_finds_the_escrow_or_its_absence() {
	let Some(w) = deploy().await else { return };
	let status = w.reader().check().await.unwrap();
	assert_eq!(status.len(), 1);

	let no_code = Chain {
		escrow: w.stranger.to_string(),
		..w.chain()
	};
	let reader = ChainReader::new(
		&no_code,
		vec![w.url().into()],
		Duration::from_secs(5),
		false,
	)
	.unwrap();
	assert!(matches!(
		reader.check().await,
		Err(ChainError::NoEscrowCode(_))
	));
}

#[tokio::test]
async fn rpc_errors_never_show_the_url() {
	// No anvil needed: nothing listens on port 1.
	let chain = Chain {
		caip2: "eip155:31337".into(),
		name: "test".into(),
		escrow: format!("0x{}", "11".repeat(20)),
		usdc: format!("0x{}", "22".repeat(20)),
		usdc_decimals: 6,
		rpcs: vec![],
		confirmations: 1,
		require_rpc_agreement: false,
	};
	let url = "http://127.0.0.1:1/v2/secret-rpc-key-abc123".to_string();
	let reader = ChainReader::new(&chain, vec![url], Duration::from_secs(2), false).unwrap();
	let e = reader.check().await.unwrap_err();
	let shown = format!("{e} {e:?}");
	assert!(!shown.contains("secret-rpc-key-abc123"), "{shown}");
	assert!(shown.contains("rpc[0]"), "{shown}");
}

#[tokio::test]
async fn invariant_9_production_refuses_a_plain_http_rpc() {
	let chain = Chain {
		caip2: "eip155:31337".into(),
		name: "test".into(),
		escrow: format!("0x{}", "11".repeat(20)),
		usdc: format!("0x{}", "22".repeat(20)),
		usdc_decimals: 6,
		rpcs: vec![],
		confirmations: 1,
		require_rpc_agreement: false,
	};
	let e = ChainReader::new(
		&chain,
		vec!["http://rpc.example".into()],
		Duration::from_secs(2),
		true,
	)
	.err()
	.unwrap();
	assert!(e.to_string().contains("https://"), "{e}");
}

// ── what lock to expect (no chain needed) ────────────────────────────────

fn plain_chain() -> Chain {
	Chain {
		caip2: "eip155:84532".into(),
		name: "base-sepolia".into(),
		escrow: "0x112a534104ac10E73D57f7C5a4C6469632392173".into(),
		usdc: "0x73c83DAcc74bB8a704717AC09703b959E74b9705".into(),
		usdc_decimals: 6,
		rpcs: vec![],
		confirmations: 3,
		require_rpc_agreement: false,
	}
}

fn fixture_trade(edit: impl FnOnce(&mut Value)) -> Trade {
	let mut t = serde_json::from_str::<Value>(FIXTURE).unwrap()["ownFiatTrade"].clone();
	edit(&mut t);
	serde_json::from_value(t).unwrap()
}

const WALLET: &str = "0x1111111111111111111111111111111111111111";

#[test]
fn the_expected_lock_takes_where_money_goes_from_config() {
	let e = expected_lock(&fixture_trade(|_| {}), &plain_chain(), WALLET).unwrap();
	assert_eq!(e.counterparty, WALLET.parse::<Address>().unwrap());
	assert_eq!(e.token, plain_chain().usdc.parse::<Address>().unwrap());
	assert_eq!(e.amount, U256::from(100_000_000u64));
	// The fixture trade's lock id is the shared vector's first case.
	assert_eq!(
		e.lock_id.to_string(),
		"0x20a1970aecf3aec04414773d5a08dd822ad3225a78a7cda00f490dbc7297d568"
	);
}

#[test]
fn invariant_3_a_trade_naming_another_payee_is_refused() {
	let t = fixture_trade(|t| {
		t["solver_address"] = json!("0x9999999999999999999999999999999999999999")
	});
	let e = expected_lock(&t, &plain_chain(), WALLET).unwrap_err();
	assert!(e.to_string().contains("your wallet is"), "{e}");
}

#[test]
fn a_trade_on_another_chain_or_asset_or_direction_is_refused() {
	let t = fixture_trade(|t| t["chain"] = json!("eip155:1"));
	assert!(expected_lock(&t, &plain_chain(), WALLET).is_err());
	let t = fixture_trade(|t| t["asset"] = json!("USDT"));
	assert!(expected_lock(&t, &plain_chain(), WALLET).is_err());
	let t = fixture_trade(|t| t["direction"] = json!("onramp"));
	assert!(expected_lock(&t, &plain_chain(), WALLET).is_err());
}
