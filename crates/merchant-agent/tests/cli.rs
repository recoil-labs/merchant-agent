//! The binary end to end: `init` writes a sandbox config that `doctor`
//! accepts, and `doctor` is honest about what it has not checked yet.
//!
//! `doctor` talks to the aggregator, so these tests run a small fake one
//! serving the shared fixture (`vectors/merchant-agent-api.json`).

use std::path::Path;
use std::process::{Command, Output};

use axum::http::StatusCode;
use axum::response::IntoResponse;
use axum::routing::get;
use serde_json::{json, Value};

const FIXTURE: &str = include_str!("../../../vectors/merchant-agent-api.json");

/// How the fake aggregator should answer.
struct FakeAggregator {
	/// The fiat escrow the registry lists for Base Sepolia.
	escrow: &'static str,
	/// The payee key RecoilPay holds; `None` answers `NO_KEY`.
	payee_key: Option<String>,
	/// Whether the operator's API key is accepted.
	key_accepted: bool,
	/// How long listing trades takes: a slow first tick.
	trades_delay: std::time::Duration,
}

impl Default for FakeAggregator {
	fn default() -> Self {
		Self {
			escrow: "0x112a534104ac10E73D57f7C5a4C6469632392173",
			payee_key: Some(test_payee_public_key()),
			key_accepted: true,
			trades_delay: std::time::Duration::ZERO,
		}
	}
}

impl FakeAggregator {
	/// Serve on a free local port, on its own thread, for the rest of the
	/// test process. Returns the URL.
	fn start(self) -> String {
		let fixture: Value = serde_json::from_str(FIXTURE).unwrap();
		let mut chains = fixture["chains"].clone();
		chains["data"][0]["fiat_escrow"] = json!(self.escrow);
		let mut quotes = fixture["fiatQuotes"].clone();
		// The shared fixture's offer expires on a fixed date, and `resume`
		// republishes only offers still live. Keep it live, whatever today is.
		quotes["quotes"][0]["expiry"] = json!((chrono::Utc::now() + chrono::Duration::days(1))
			.to_rfc3339_opts(chrono::SecondsFormat::Secs, true));
		let put_quotes = quotes.clone();
		let (payee, accepted) = (self.payee_key, self.key_accepted);
		let delay = self.trades_delay;

		let app = axum::Router::new()
			.route("/health", get(|| async { StatusCode::OK }))
			// A minimal Base Sepolia RPC: the right chain id, code at the
			// escrow, and an empty lock for any id.
			.route("/rpc", axum::routing::post(fake_rpc))
			.route(
				"/solver-api/operators/{id}/heartbeat",
				axum::routing::post(|| async { StatusCode::NO_CONTENT }),
			)
			// No cash-outs waiting.
			.route(
				"/solver-api/operators/{id}/fiat-trades",
				get(move || async move {
					tokio::time::sleep(delay).await;
					axum::Json(json!([]))
				}),
			)
			.route(
				"/api/v1/chains",
				get(move || async move { axum::Json(chains) }),
			)
			.route(
				"/solver-api/operators/{id}/fiat-quotes",
				get(move || async move {
					if accepted {
						axum::Json(quotes).into_response()
					} else {
						StatusCode::UNAUTHORIZED.into_response()
					}
				})
				// Replacing the book: an empty book withdraws every offer;
				// republishing gives back the stored quote.
				.put(move |axum::Json(body): axum::Json<Value>| async move {
					let empty = body["quotes"].as_array().is_some_and(|q| q.is_empty());
					if empty {
						axum::Json(json!({"quotes": []}))
					} else {
						axum::Json(put_quotes)
					}
				}),
			)
			.route(
				"/solver-api/operators/{id}/fiat-payee-key",
				get(move || async move {
					match payee {
						Some(k) => axum::Json(json!({ "publicKey": k })).into_response(),
						None => (
							StatusCode::NOT_FOUND,
							axum::Json(
								json!({"error": "NO_KEY", "message": "no payee key is set", "timestamp": 0}),
							),
						)
							.into_response(),
					}
				}),
			);
		let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
		listener.set_nonblocking(true).unwrap();
		let url = format!("http://{}", listener.local_addr().unwrap());
		std::thread::spawn(move || {
			let rt = tokio::runtime::Builder::new_current_thread()
				.enable_all()
				.build()
				.unwrap();
			rt.block_on(async move {
				let listener = tokio::net::TcpListener::from_std(listener).unwrap();
				axum::serve(listener, app).await.unwrap();
			});
		});
		url
	}
}

fn test_payee_public_key() -> String {
	merchant_agent_core::payee::PayeePrivateKey::from_hex(TEST_PAYEE_KEY)
		.unwrap()
		.public_key_hex()
}

/// Nothing listens here, so the aggregator is unreachable at once.
const UNREACHABLE: &str = "http://127.0.0.1:9";

const BIN: &str = env!("CARGO_BIN_EXE_merchant-agent");

fn agent(config: &Path, args: &[&str]) -> Command {
	let mut c = Command::new(BIN);
	c.arg("--config")
		.arg(config)
		.args(args)
		.env_remove("MERCHANT_AGENT_CONFIG");
	for (name, _) in merchant_agent_core::init::SANDBOX_SECRETS {
		c.env_remove(name);
	}
	c
}

fn text(o: &Output) -> String {
	format!(
		"{}{}",
		String::from_utf8_lossy(&o.stdout),
		String::from_utf8_lossy(&o.stderr)
	)
}

fn init(dir: &Path) -> std::path::PathBuf {
	init_at(dir, UNREACHABLE)
}

fn init_at(dir: &Path, aggregator_url: &str) -> std::path::PathBuf {
	let config = dir.join("merchant-agent.yaml");
	let data = dir.join("data");
	let out = agent(
		&config,
		&["init", "--yes", "--solver-id", "solver-1a2b3c4d5e6f"],
	)
	.args(["--wallet", "0x1111111111111111111111111111111111111111"])
	.args(["--aggregator-url", aggregator_url])
	.arg("--data-dir")
	.arg(&data)
	.output()
	.unwrap();
	assert!(out.status.success(), "{}", text(&out));
	assert!(
		text(&out).contains("export MERCHANT_AGENT_API_KEY"),
		"{}",
		text(&out)
	);
	// Never the public RPC from a test: the fake's, or nothing.
	let written = std::fs::read_to_string(&config).unwrap();
	let local = written.replace("https://sepolia.base.org", &format!("{aggregator_url}/rpc"));
	std::fs::write(&config, local).unwrap();
	config
}

async fn fake_rpc(axum::Json(req): axum::Json<Value>) -> axum::Json<Value> {
	let result = match req["method"].as_str().unwrap_or("") {
		"eth_chainId" => json!("0x14a34"),
		"eth_blockNumber" => json!("0x100"),
		"eth_getCode" => json!("0x6080604052"),
		"eth_call" => json!(format!("0x{}", "00".repeat(7 * 32))),
		other => panic!("the fake RPC does not answer {other}"),
	};
	axum::Json(json!({"jsonrpc": "2.0", "id": req["id"], "result": result}))
}

#[test]
fn init_writes_a_sandbox_config() {
	let dir = tempfile::tempdir().unwrap();
	let config = init(dir.path());
	let written = std::fs::read_to_string(&config).unwrap();
	assert!(written.contains("mode: sandbox"));
	assert!(written.contains("kind: simulated"));
	merchant_agent_core::config::Config::parse(&written).unwrap();
}

#[test]
fn init_refuses_to_overwrite_without_force() {
	let dir = tempfile::tempdir().unwrap();
	let config = init(dir.path());
	let out = agent(
		&config,
		&["init", "--yes", "--solver-id", "solver-1a2b3c4d5e6f"],
	)
	.output()
	.unwrap();
	assert!(!out.status.success());
	assert!(text(&out).contains("--force"));
}

#[test]
fn init_refuses_bad_answers() {
	let dir = tempfile::tempdir().unwrap();
	let config = dir.path().join("c.yaml");
	let out = agent(
		&config,
		&["init", "--yes", "--solver-id", "bob", "--wallet", "0x12"],
	)
	.output()
	.unwrap();
	assert!(!out.status.success());
	assert!(!config.exists());
}

#[test]
fn doctor_fails_on_unset_secrets_and_skips_what_needs_them() {
	let dir = tempfile::tempdir().unwrap();
	let config = init(dir.path());
	let out = agent(&config, &["doctor"]).output().unwrap();
	let t = text(&out);
	assert!(!out.status.success(), "{t}");
	assert!(t.contains("PASS  Config is valid"), "{t}");
	assert!(
		t.contains("FAIL  Secret readable: aggregator API key"),
		"{t}"
	);
	assert!(t.contains("fix: export the variable"), "{t}");
	assert!(
		t.contains(
			"SKIP  Aggregator reachable and API key accepted  the API key secret could not be read"
		),
		"{t}"
	);
	assert!(
		t.contains("FAIL  RPC reachable, right chain id, escrow deployed  base-sepolia rpc[0]"),
		"{t}"
	);
	assert!(
		t.contains("PASS  Alert channels configured  none; alerts are only logged"),
		"{t}"
	);
}

#[test]
fn doctor_passes_a_sandbox_with_its_secrets_set() {
	let dir = tempfile::tempdir().unwrap();
	let config = init_at(dir.path(), &FakeAggregator::default().start());
	let out = with_sandbox_secrets(agent(&config, &["doctor"]), TEST_PAYEE_KEY)
		.output()
		.unwrap();
	let t = text(&out);
	assert!(out.status.success(), "{t}");
	assert!(t.contains("PASS  Data directory writable"), "{t}");
	assert!(
		t.contains("PASS  Payee key is a valid key  public key 0x04"),
		"{t}"
	);
	for check in [
		"PASS  Aggregator reachable and API key accepted  1 offer(s)",
		"PASS  Clock agrees with the aggregator",
		"PASS  Pinned escrow and USDC match RecoilPay's registry  1 chain(s) match",
		"PASS  Payee key matches the key registered with RecoilPay  they match",
		"PASS  RPC reachable, right chain id, escrow deployed  base-sepolia: 1 endpoint(s), latest block 256",
		"PASS  Database opens and migrations are current  none yet",
		"PASS  No other agent holds the lease  free",
	] {
		assert!(t.contains(check), "missing {check:?}: {t}");
	}
	assert!(
		!t.contains("fake-sandbox-value"),
		"doctor must never print a secret: {t}"
	);
	assert!(
		!t.contains(&TEST_PAYEE_KEY[2..]),
		"doctor must never print the payee key: {t}"
	);
}

/// A fixed test key, obviously not a real one.
const TEST_PAYEE_KEY: &str = "0x2222222222222222222222222222222222222222222222222222222222222222";

fn with_sandbox_secrets(mut cmd: Command, payee_key: &str) -> Command {
	for (name, _) in merchant_agent_core::init::SANDBOX_SECRETS {
		cmd.env(name, "fake-sandbox-value");
	}
	cmd.env("MERCHANT_AGENT_PAYEE_KEY", payee_key);
	cmd
}

#[test]
fn doctor_fails_a_payee_key_that_is_not_a_key() {
	let dir = tempfile::tempdir().unwrap();
	let config = init(dir.path());
	let out = with_sandbox_secrets(agent(&config, &["doctor"]), "not-a-key")
		.output()
		.unwrap();
	let t = text(&out);
	assert!(!out.status.success(), "{t}");
	assert!(t.contains("FAIL  Payee key is a valid key"), "{t}");
	assert!(t.contains("payee.key_form (private_key)"), "{t}");
}

#[test]
fn doctor_reports_a_broken_config_with_every_problem() {
	let dir = tempfile::tempdir().unwrap();
	let config = init(dir.path());
	let broken = std::fs::read_to_string(&config)
		.unwrap()
		.replace("mode: sandbox", "mode: production");
	std::fs::write(&config, broken).unwrap();
	let out = agent(&config, &["doctor"]).output().unwrap();
	let t = text(&out);
	assert!(!out.status.success());
	assert!(t.contains("simulated is for sandbox only"), "{t}");
	assert!(
		t.contains("env:// secrets are refused in production"),
		"{t}"
	);
	assert!(t.contains("https:// in production"), "{t}");
}

#[test]
fn version_prints_the_schema_versions() {
	let out = Command::new(BIN).arg("version").output().unwrap();
	let t = text(&out);
	assert!(out.status.success());
	assert!(t.contains("config schema   1"), "{t}");
	assert!(t.contains("git commit"), "{t}");
}

#[test]
fn audit_verify_and_export_check_the_chain_from_the_command_line() {
	let dir = tempfile::tempdir().unwrap();
	let config = init_at(dir.path(), &FakeAggregator::default().start());
	// Two audited decisions: paused and resumed. (Saving the withdrawn book
	// after the pause is not a third.)
	assert!(run_cli(&config, &["pause", "--by", "ola"]).0);
	assert!(run_cli(&config, &["resume", "--by", "ola"]).0);

	let (ok, t) = run_cli(&config, &["audit", "verify"]);
	assert!(ok, "{t}");
	assert!(t.contains("OK: 2 entries"), "{t}");

	let export = dir.path().join("export.jsonl");
	let (ok, t) = run_cli(
		&config,
		&[
			"audit",
			"export",
			"--from",
			"2000-01-01T00:00:00Z",
			"--to",
			"2100-01-01T00:00:00Z",
			"--out",
			export.to_str().unwrap(),
		],
	);
	assert!(ok, "{t}");
	assert!(t.contains("Wrote 2 entries"), "{t}");
	let (ok, t) = run_cli(
		&config,
		&["audit", "verify", "--export", export.to_str().unwrap()],
	);
	assert!(ok, "{t}");
	assert!(t.contains("OK: 2 exported entries"), "{t}");

	// Tamper with the export: caught.
	let edited = std::fs::read_to_string(&export)
		.unwrap()
		.replace("\"ola\"", "\"mallory\"");
	std::fs::write(&export, edited).unwrap();
	let (ok, t) = run_cli(
		&config,
		&["audit", "verify", "--export", export.to_str().unwrap()],
	);
	assert!(!ok);
	assert!(t.contains("was changed after it was written"), "{t}");

	// Tamper with the log itself: caught, and called an incident.
	let log = dir.path().join("data").join("audit.jsonl");
	let lines: Vec<String> = std::fs::read_to_string(&log)
		.unwrap()
		.lines()
		.map(String::from)
		.collect();
	std::fs::write(&log, format!("{}\n", lines[1])).unwrap();
	let (ok, t) = run_cli(&config, &["audit", "verify"]);
	assert!(!ok);
	assert!(
		t.contains("PROBLEM") && t.contains("security incident"),
		"{t}"
	);

	let (ok, t) = run_cli(
		&config,
		&["audit", "export", "--from", "yesterday", "--to", "today"],
	);
	assert!(!ok);
	assert!(t.contains("not an RFC 3339 time"), "{t}");
}

fn doctor_against(fake: FakeAggregator) -> (bool, String) {
	let dir = tempfile::tempdir().unwrap();
	let config = init_at(dir.path(), &fake.start());
	let out = with_sandbox_secrets(agent(&config, &["doctor"]), TEST_PAYEE_KEY)
		.output()
		.unwrap();
	(out.status.success(), text(&out))
}

#[test]
fn doctor_fails_an_unreachable_aggregator_and_skips_what_needs_it() {
	let dir = tempfile::tempdir().unwrap();
	let config = init(dir.path());
	let out = with_sandbox_secrets(agent(&config, &["doctor"]), TEST_PAYEE_KEY)
		.output()
		.unwrap();
	let t = text(&out);
	assert!(!out.status.success(), "{t}");
	assert!(
		t.contains("FAIL  Aggregator reachable and API key accepted"),
		"{t}"
	);
	assert!(t.contains("fix: check aggregator.url"), "{t}");
	assert!(
		t.contains("SKIP  Clock agrees with the aggregator  needs the aggregator"),
		"{t}"
	);
}

#[test]
fn doctor_fails_a_refused_api_key() {
	let (ok, t) = doctor_against(FakeAggregator {
		key_accepted: false,
		..Default::default()
	});
	assert!(!ok, "{t}");
	assert!(t.contains("the API key was refused"), "{t}");
}

#[test]
fn invariant_3_doctor_fails_when_the_registry_lists_another_escrow() {
	let (ok, t) = doctor_against(FakeAggregator {
		escrow: "0x9999999999999999999999999999999999999999",
		..Default::default()
	});
	assert!(!ok, "{t}");
	assert!(
		t.contains("FAIL  Pinned escrow and USDC match RecoilPay's registry"),
		"{t}"
	);
	assert!(t.contains("escrow is pinned to 0x112a"), "{t}");
	assert!(
		t.contains("the agent will not pay while these differ"),
		"{t}"
	);
}

#[test]
fn doctor_fails_when_recoilpay_holds_another_payee_key() {
	let other =
		merchant_agent_core::payee::PayeePrivateKey::from_hex(&format!("0x{}", "33".repeat(32)))
			.unwrap()
			.public_key_hex();
	let (ok, t) = doctor_against(FakeAggregator {
		payee_key: Some(other),
		..Default::default()
	});
	assert!(!ok, "{t}");
	assert!(
		t.contains("FAIL  Payee key matches the key registered with RecoilPay"),
		"{t}"
	);
	assert!(t.contains("could not open them"), "{t}");
}

#[test]
fn doctor_fails_when_no_payee_key_is_registered() {
	let (ok, t) = doctor_against(FakeAggregator {
		payee_key: None,
		..Default::default()
	});
	assert!(!ok, "{t}");
	assert!(
		t.contains("no payee key is registered with RecoilPay"),
		"{t}"
	);
}

#[test]
fn run_starts_ticks_and_stops_cleanly_on_ctrl_c() {
	let dir = tempfile::tempdir().unwrap();
	let config = init_at(dir.path(), &FakeAggregator::default().start());
	let (console, health) = (free_port(), free_port());
	let yaml = std::fs::read_to_string(&config)
		.unwrap()
		.replace("127.0.0.1:8740", &format!("127.0.0.1:{console}"))
		.replace("127.0.0.1:8741", &format!("127.0.0.1:{health}"));
	std::fs::write(&config, yaml).unwrap();
	let child = with_sandbox_secrets(agent(&config, &["run"]), TEST_PAYEE_KEY)
		.stdout(std::process::Stdio::piped())
		.stderr(std::process::Stdio::piped())
		.spawn()
		.unwrap();
	// Started once the database exists and has been written to.
	let db = dir.path().join("data").join("agent.db");
	for _ in 0..100 {
		if db.exists() {
			break;
		}
		std::thread::sleep(std::time::Duration::from_millis(50));
	}
	std::thread::sleep(std::time::Duration::from_millis(500));
	assert!(db.exists(), "run should have opened its database");
	let (status, body) = http_get(health, "/healthz");
	assert_eq!((status, body.as_str()), (200, "ok\n"));
	let (_, body) = http_get(health, "/readyz");
	assert!(body.contains("\"checks\""), "{body}");
	let (status, body) = http_get(health, "/metrics");
	assert_eq!(status, 200);
	assert!(body.contains("merchant_agent_up 1"), "{body}");
	let pid = rustix::process::Pid::from_child(&child);
	rustix::process::kill_process(pid, rustix::process::Signal::INT).unwrap();
	let out = child.wait_with_output().unwrap();
	let t = text(&out);
	assert!(out.status.success(), "{t}");
	assert!(t.contains("merchant agent started"), "{t}");
	assert!(t.contains("stopping"), "{t}");
	assert!(
		!t.contains(&TEST_PAYEE_KEY[2..]),
		"the payee key must never be logged"
	);
}

#[test]
fn run_refuses_production_without_a_passing_doctor() {
	let dir = tempfile::tempdir().unwrap();
	let config = init(dir.path());
	let prod = std::fs::read_to_string(&config)
		.unwrap()
		.replace("mode: sandbox", "mode: production")
		.replace("http://127.0.0.1:9", "https://127.0.0.1:9")
		.replace("kind: simulated", "kind: manual")
		.replace("name_match: token_set", "name_match: strict")
		.replace(
			"env://MERCHANT_AGENT_API_KEY",
			"file:///nonexistent/api-key",
		)
		.replace(
			"env://MERCHANT_AGENT_PAYEE_KEY",
			"file:///nonexistent/payee-key",
		)
		.replace(
			"env://MERCHANT_AGENT_PAYEE_TAG_PEPPER",
			"file:///nonexistent/pepper",
		);
	let prod = prod.replace(&format!("{UNREACHABLE}/rpc"), "https://127.0.0.1:9/rpc");
	std::fs::write(&config, prod).unwrap();
	let out = agent(&config, &["run"]).output().unwrap();
	let t = text(&out);
	assert!(!out.status.success(), "{t}");
	assert!(t.contains("needs a passing `merchant-agent doctor`"), "{t}");
	assert!(
		!dir.path().join("data").join("agent.db").exists(),
		"nothing ran"
	);
}

#[test]
fn screening_hash_turns_accounts_into_tags_without_echoing_them() {
	let dir = tempfile::tempdir().unwrap();
	let config = init(dir.path());
	let mut cmd = with_sandbox_secrets(agent(&config, &["screening", "hash"]), TEST_PAYEE_KEY);
	let mut child = cmd
		.stdin(std::process::Stdio::piped())
		.stdout(std::process::Stdio::piped())
		.stderr(std::process::Stdio::piped())
		.spawn()
		.unwrap();
	use std::io::Write;
	child
		.stdin
		.take()
		.unwrap()
		.write_all(b"# fraud cases\n058,0123456789\nke.mpesa,,+254700000000\n")
		.unwrap();
	let out = child.wait_with_output().unwrap();
	let t = text(&out);
	assert!(out.status.success(), "{t}");
	let tags: Vec<&str> = t.lines().filter(|l| !l.starts_with('#')).collect();
	assert_eq!(tags.len(), 2, "{t}");
	assert!(
		tags.iter()
			.all(|l| l.len() == 64 && l.bytes().all(|b| b.is_ascii_hexdigit())),
		"{t}"
	);
	assert!(
		!t.contains("0123456789") && !t.contains("254700000000"),
		"{t}"
	);
}

fn run_cli(config: &Path, args: &[&str]) -> (bool, String) {
	let out = with_sandbox_secrets(agent(config, args), TEST_PAYEE_KEY)
		.output()
		.unwrap();
	(out.status.success(), text(&out))
}

#[test]
fn invariant_11_pause_status_and_resume_from_the_command_line() {
	let dir = tempfile::tempdir().unwrap();
	let config = init_at(dir.path(), &FakeAggregator::default().start());

	let (ok, t) = run_cli(&config, &["status"]);
	assert!(ok, "{t}");
	assert!(
		t.contains("Agent: not running")
			&& t.contains("Payouts: running")
			&& t.contains("Exception queue: empty"),
		"{t}"
	);

	let (ok, t) = run_cli(&config, &["pause", "--by", "ola"]);
	assert!(ok, "{t}");
	assert!(
		t.contains("no new payouts will start") && t.contains("offers on RecoilPay are withdrawn"),
		"{t}"
	);
	let (_, t) = run_cli(&config, &["status"]);
	assert!(t.contains("Payouts: PAUSED by ola"), "{t}");

	let (ok, t) = run_cli(&config, &["resume", "--by", "ola"]);
	assert!(ok, "{t}");
	assert!(t.contains("Resumed. 1 offer(s) republished."), "{t}");
	let (_, t) = run_cli(&config, &["status"]);
	assert!(t.contains("Payouts: running"), "{t}");
}

#[test]
fn resuming_after_an_automatic_pause_is_refused_on_the_command_line() {
	let dir = tempfile::tempdir().unwrap();
	let config = init_at(dir.path(), &FakeAggregator::default().start());
	let store = merchant_agent_core::store::Store::open_control(&dir.path().join("data")).unwrap();
	store.set_paused_control(true, "agent", true, None).unwrap();
	drop(store);
	let (ok, t) = run_cli(&config, &["resume"]);
	assert!(!ok, "{t}");
	assert!(
		t.contains("needs an approver") && t.contains("console"),
		"{t}"
	);
	let (_, t) = run_cli(&config, &["status"]);
	assert!(t.contains("PAUSED") && t.contains("automatically"), "{t}");
}

#[test]
fn users_are_added_with_a_password_and_an_authenticator_secret() {
	let dir = tempfile::tempdir().unwrap();
	let config = init(dir.path());
	let mut child = with_sandbox_secrets(
		agent(
			&config,
			&[
				"users",
				"add",
				"ada",
				"--role",
				"approver",
				"--password-stdin",
				"--no-confirm",
			],
		),
		TEST_PAYEE_KEY,
	)
	.stdin(std::process::Stdio::piped())
	.stdout(std::process::Stdio::piped())
	.stderr(std::process::Stdio::piped())
	.spawn()
	.unwrap();
	use std::io::Write;
	child
		.stdin
		.take()
		.unwrap()
		.write_all(b"correct horse battery staple\n")
		.unwrap();
	let out = child.wait_with_output().unwrap();
	let t = text(&out);
	assert!(out.status.success(), "{t}");
	assert!(
		t.contains("Added ada as approver") && t.contains("otpauth://totp/"),
		"{t}"
	);
	assert!(
		!t.contains("correct horse"),
		"the password is never echoed: {t}"
	);

	let (_, t) = run_cli(&config, &["users", "list"]);
	assert!(
		t.contains("ada") && t.contains("approver") && t.contains("active"),
		"{t}"
	);
	let (ok, _) = run_cli(&config, &["users", "disable", "ada"]);
	assert!(ok);
	let (_, t) = run_cli(&config, &["users", "list"]);
	assert!(t.contains("disabled"), "{t}");

	let weak = with_sandbox_secrets(
		agent(
			&config,
			&[
				"users",
				"add",
				"bo",
				"--role",
				"viewer",
				"--password-stdin",
				"--no-confirm",
			],
		),
		TEST_PAYEE_KEY,
	)
	.stdin(std::process::Stdio::null())
	.output()
	.unwrap();
	assert!(!weak.status.success(), "an empty password is refused");
}

fn free_port() -> u16 {
	std::net::TcpListener::bind("127.0.0.1:0")
		.unwrap()
		.local_addr()
		.unwrap()
		.port()
}

/// A bare HTTP/1.1 GET: status and body.
fn http_get(port: u16, path: &str) -> (u16, String) {
	use std::io::{Read, Write};
	let mut last = None;
	for _ in 0..50 {
		match std::net::TcpStream::connect(("127.0.0.1", port)) {
			Ok(mut s) => {
				write!(
					s,
					"GET {path} HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n"
				)
				.unwrap();
				let mut raw = String::new();
				s.read_to_string(&mut raw).unwrap();
				let status = raw[9..12].parse().unwrap();
				let body = raw
					.split_once("\r\n\r\n")
					.map(|(_, b)| b.to_string())
					.unwrap_or_default();
				return (status, body);
			}
			Err(e) => last = Some(e),
		}
		std::thread::sleep(std::time::Duration::from_millis(100));
	}
	panic!("the health port never opened: {last:?}");
}

#[test]
fn init_prints_a_container_config_that_reads_secrets_from_files() {
	let dir = tempfile::tempdir().unwrap();
	let config = dir.path().join("unused.yaml");
	let out = agent(&config, &["init", "--yes", "--stdout", "--bind-all"])
		.args(["--secrets-dir", "/run/merchant-agent/secrets"])
		.args(["--solver-id", "solver-1a2b3c4d5e6f"])
		.args(["--wallet", "0x1111111111111111111111111111111111111111"])
		.args(["--aggregator-url", "https://api.recoilpay.com"])
		.args(["--data-dir", "/var/lib/merchant-agent"])
		.output()
		.unwrap();
	assert!(out.status.success(), "{}", text(&out));
	let yaml = String::from_utf8(out.stdout).unwrap();
	assert!(!config.exists(), "--stdout writes no file");
	assert!(
		yaml.starts_with("# RecoilPay Merchant Agent config."),
		"only the config on stdout"
	);
	assert!(yaml.contains("console_bind: \"0.0.0.0:8740\"") && yaml.contains("bind_public: true"));
	assert!(yaml.contains("api_key: file:///run/merchant-agent/secrets/api-key"));
	assert!(yaml.contains("tag_pepper: file:///run/merchant-agent/secrets/payee-tag-pepper"));
	assert!(!yaml.contains("env://MERCHANT_AGENT_API_KEY"));
	merchant_agent_core::config::Config::parse(&yaml).expect("the printed config validates");
}

#[test]
fn init_refuses_a_relative_secrets_dir_on_stderr() {
	let dir = tempfile::tempdir().unwrap();
	let out = agent(&dir.path().join("c.yaml"), &["init", "--yes", "--stdout"])
		.args(["--secrets-dir", "secrets"])
		.args(["--solver-id", "solver-1a2b3c4d5e6f"])
		.args(["--wallet", "0x1111111111111111111111111111111111111111"])
		.output()
		.unwrap();
	assert!(!out.status.success());
	assert!(
		out.stdout.is_empty(),
		"nothing on stdout, so a redirect writes no half config"
	);
	assert!(String::from_utf8_lossy(&out.stderr).contains("absolute path"));
}

#[test]
fn healthcheck_passes_against_a_live_agent_and_fails_without_one() {
	// Nothing listening: unhealthy.
	let port = std::net::TcpListener::bind("127.0.0.1:0")
		.unwrap()
		.local_addr()
		.unwrap()
		.port();
	let out = Command::new(BIN)
		.args(["healthcheck", "--addr", &format!("127.0.0.1:{port}")])
		.output()
		.unwrap();
	assert!(!out.status.success());
	assert!(text(&out).contains("unhealthy"), "{}", text(&out));

	// A server answering /healthz with 200: healthy; 0.0.0.0 is asked on loopback.
	let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
	let port = listener.local_addr().unwrap().port();
	std::thread::spawn(move || {
		use std::io::{Read, Write};
		if let Ok((mut s, _)) = listener.accept() {
			let mut buf = [0u8; 512];
			let _ = s.read(&mut buf);
			let _ = s.write_all(b"HTTP/1.1 200 OK\r\ncontent-length: 3\r\n\r\nok\n");
		}
	});
	let out = Command::new(BIN)
		.args(["healthcheck", "--addr", &format!("0.0.0.0:{port}")])
		.output()
		.unwrap();
	assert!(out.status.success(), "{}", text(&out));
	assert!(text(&out).contains("healthy (127.0.0.1:"), "{}", text(&out));
}

#[test]
fn adapter_test_is_only_for_a_template_rail() {
	let dir = tempfile::tempdir().unwrap();
	let config = init(dir.path());
	let out = agent(&config, &["adapter-test"]).output().unwrap();
	assert!(!out.status.success());
	assert!(
		text(&out).contains("adapter-test is for rail.kind template"),
		"{}",
		text(&out)
	);
}

#[test]
fn a_ctrl_c_during_start_up_still_stops_cleanly() {
	// The signal handlers are installed before anything slow, so a Ctrl-C
	// that lands during the first pass stops the agent, never kills it.
	let dir = tempfile::tempdir().unwrap();
	// RecoilPay answers slowly, so the first tick is still running when the
	// Ctrl-C lands, as on a slow CI runner.
	let slow = FakeAggregator {
		trades_delay: std::time::Duration::from_secs(3),
		..FakeAggregator::default()
	};
	let config = init_at(dir.path(), &slow.start());
	let (console, health) = (free_port(), free_port());
	let yaml = std::fs::read_to_string(&config)
		.unwrap()
		.replace("127.0.0.1:8740", &format!("127.0.0.1:{console}"))
		.replace("127.0.0.1:8741", &format!("127.0.0.1:{health}"));
	std::fs::write(&config, yaml).unwrap();
	let child = with_sandbox_secrets(agent(&config, &["run"]), TEST_PAYEE_KEY)
		.stdout(std::process::Stdio::piped())
		.stderr(std::process::Stdio::piped())
		.spawn()
		.unwrap();
	// Once it has started (the health port answers), mid first tick.
	let (status, _) = http_get(health, "/healthz");
	assert_eq!(status, 200);
	let pid = rustix::process::Pid::from_child(&child);
	rustix::process::kill_process(pid, rustix::process::Signal::INT).unwrap();
	let out = child.wait_with_output().unwrap();
	let t = text(&out);
	assert!(
		out.status.success(),
		"killed by the signal instead of stopping: {t}"
	);
	assert!(t.contains("stopping"), "{t}");
}
