//! M9: the audit log, readiness, the metrics and the health endpoints.
//!
//! The audit tests tamper with `audit.jsonl` and with the database the way
//! an insider would (edit a line, delete a line) and require `verify` to
//! notice. The metric tests drive real trades through the engine and read
//! the Prometheus text back.

mod common;
use common::*;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use merchant_agent_core::audit;
use merchant_agent_core::config::Mode;
use merchant_agent_core::console::AgentOps;
use merchant_agent_core::runner;
use tower::ServiceExt;

async fn paid_run(dir: &Path) -> (World, Agent) {
	let world = World::new();
	let agent = world.agent(dir).unwrap();
	assert!(reported(&tick_until(&agent, reported).await));
	(world, agent)
}

fn file_lines(dir: &Path) -> Vec<String> {
	std::fs::read_to_string(dir.join(audit::FILE))
		.unwrap()
		.lines()
		.map(String::from)
		.collect()
}

fn write_lines(dir: &Path, lines: &[String]) {
	let text: String = lines.iter().map(|l| format!("{l}\n")).collect();
	std::fs::write(dir.join(audit::FILE), text).unwrap();
}

fn raw_db(dir: &Path) -> rusqlite::Connection {
	rusqlite::Connection::open(dir.join("agent.db")).unwrap()
}

/// The Prometheus value of `series` (name plus labels), if present.
fn metric(text: &str, series: &str) -> Option<String> {
	text.lines()
		.find_map(|l| l.strip_prefix(series).and_then(|r| r.strip_prefix(' ')))
		.map(String::from)
}

// ── the audit log ────────────────────────────────────────────────────────

#[tokio::test]
async fn a_payout_leaves_a_verified_chain_of_every_decision() {
	let dir = tempfile::tempdir().unwrap();
	let (_world, agent) = paid_run(dir.path()).await;
	let v = agent.store().verify_audit().unwrap();
	assert!(v.ok(), "{:?}", v.problems);
	assert_eq!(v.entries, v.in_file, "the file mirrors the database");

	let actions: Vec<String> = file_lines(dir.path())
		.iter()
		.map(|l| serde_json::from_str::<audit::Entry>(l).unwrap().action)
		.collect();
	for want in [
		"detected",
		"lock_verified",
		"payout_intent_recorded",
		"paid",
		"reported",
	] {
		assert!(
			actions.iter().any(|a| a == want),
			"no {want} in {actions:?}"
		);
	}
	// Invariant 7: no personal data in the audit.
	let text = std::fs::read_to_string(dir.path().join(audit::FILE)).unwrap();
	assert!(!text.contains(ACCOUNT) && !text.contains(NAME));
}

#[tokio::test]
async fn invariant_12_editing_any_line_of_the_file_is_detected() {
	let dir = tempfile::tempdir().unwrap();
	let (_world, agent) = paid_run(dir.path()).await;
	let lines = file_lines(dir.path());
	for i in 0..lines.len() {
		let mut tampered = lines.clone();
		let mut e: audit::Entry = serde_json::from_str(&tampered[i]).unwrap();
		e.detail.push_str(" (edited)");
		tampered[i] = serde_json::to_string(&e).unwrap();
		write_lines(dir.path(), &tampered);
		let v = agent.store().verify_audit().unwrap();
		assert!(
			v.problems
				.iter()
				.any(|p| p.contains(&format!("entry {} was changed", e.seq))),
			"line {i}: {:?}",
			v.problems
		);
	}
	write_lines(dir.path(), &lines);
	assert!(agent.store().verify_audit().unwrap().ok());
}

#[tokio::test]
async fn invariant_12_an_edit_that_recomputes_the_hash_still_breaks_the_next_link() {
	let dir = tempfile::tempdir().unwrap();
	let (_world, agent) = paid_run(dir.path()).await;
	let mut lines = file_lines(dir.path());
	let mut e: audit::Entry = serde_json::from_str(&lines[1]).unwrap();
	e.actor = "someone-else".into();
	e.hash = e.compute_hash();
	lines[1] = serde_json::to_string(&e).unwrap();
	write_lines(dir.path(), &lines);
	let v = agent.store().verify_audit().unwrap();
	assert!(!v.ok());
	assert!(
		v.problems
			.iter()
			.any(|p| p.contains("entry 3 does not follow")),
		"{:?}",
		v.problems
	);
	assert!(v
		.problems
		.iter()
		.any(|p| p.contains("differs from the database")));
}

#[tokio::test]
async fn invariant_12_deleting_a_line_of_the_file_is_detected() {
	let dir = tempfile::tempdir().unwrap();
	let (_world, agent) = paid_run(dir.path()).await;
	let lines = file_lines(dir.path());
	for i in 0..lines.len() {
		let mut cut = lines.clone();
		cut.remove(i);
		write_lines(dir.path(), &cut);
		let v = agent.store().verify_audit().unwrap();
		assert!(!v.ok(), "deleting line {i} went unnoticed");
	}
}

#[tokio::test]
async fn invariant_12_editing_or_deleting_a_database_row_is_detected() {
	let dir = tempfile::tempdir().unwrap();
	let (_world, agent) = paid_run(dir.path()).await;

	raw_db(dir.path())
		.execute("UPDATE audit SET action = 'nothing' WHERE seq = 2", [])
		.unwrap();
	let v = agent.store().verify_audit().unwrap();
	assert!(
		v.problems
			.iter()
			.any(|p| p.starts_with("database: entry 2 was changed")),
		"{:?}",
		v.problems
	);

	let dir = tempfile::tempdir().unwrap();
	let (_world, agent) = paid_run(dir.path()).await;
	raw_db(dir.path())
		.execute("DELETE FROM audit WHERE seq = 3", [])
		.unwrap();
	let v = agent.store().verify_audit().unwrap();
	assert!(
		v.problems
			.iter()
			.any(|p| p.starts_with("database: entry 3 is missing")),
		"{:?}",
		v.problems
	);
}

#[tokio::test]
async fn an_export_verifies_on_its_own_and_tampering_with_it_is_detected() {
	let dir = tempfile::tempdir().unwrap();
	let (_world, agent) = paid_run(dir.path()).await;
	let all = agent
		.store()
		.export_audit(
			start_time() - chrono::Duration::days(1),
			start_time() + chrono::Duration::days(1),
		)
		.unwrap();
	assert_eq!(all.len(), file_lines(dir.path()).len());
	assert!(audit::verify_export(&all).is_empty());

	// A slice from the middle verifies too: it carries its own links.
	let slice = all[2..5].to_vec();
	assert!(audit::verify_export(&slice).is_empty());

	let mut edited = all.clone();
	edited[3] = edited[3].replace("\"agent\"", "\"cli\"");
	assert!(!audit::verify_export(&edited).is_empty());

	let mut cut = all.clone();
	cut.remove(3);
	assert!(
		audit::verify_export(&cut)
			.iter()
			.any(|p| p.contains("entry 4 is missing")),
		"{:?}",
		audit::verify_export(&cut)
	);

	// Out of range: nothing.
	let none = agent
		.store()
		.export_audit(
			start_time() + chrono::Duration::days(1),
			start_time() + chrono::Duration::days(2),
		)
		.unwrap();
	assert!(none.is_empty());
}

#[tokio::test]
async fn the_file_catches_up_after_it_could_not_be_written() {
	let dir = tempfile::tempdir().unwrap();
	let world = World::new();
	let agent = world.agent(dir.path()).unwrap();
	// Block the file: a directory where the file should be.
	std::fs::create_dir(dir.path().join(audit::FILE)).unwrap();
	agent.tick().await.unwrap();
	assert!(agent.store().verify_audit().unwrap().entries > 0);
	std::fs::remove_dir(dir.path().join(audit::FILE)).unwrap();
	let v = agent.store().verify_audit().unwrap();
	assert!(v.ok(), "{:?}", v.problems);
	assert_eq!(v.entries, v.in_file);
}

#[tokio::test]
async fn pausing_and_resuming_are_audited_with_who() {
	let dir = tempfile::tempdir().unwrap();
	let world = World::new();
	let agent = world.agent(dir.path()).unwrap();
	agent.pause("ada@console", false).await.unwrap();
	agent.resume("ada@console").await.unwrap();
	let paused = agent.store().last_audit("paused").unwrap().unwrap();
	assert_eq!(paused.actor, "ada@console");
	assert!(agent.store().last_audit("resumed").unwrap().is_some());

	// Saving the withdrawn book after a pause (as the CLI does) or pausing
	// twice is not a second decision: one entry per change.
	let store = agent.store();
	store.set_paused_control(true, "ola", false, None).unwrap();
	store
		.set_paused_control(true, "ola", false, Some("[]"))
		.unwrap();
	let lines = file_lines(dir.path());
	let pauses = lines
		.iter()
		.filter(|l| l.contains("\"action\":\"paused\""))
		.count();
	assert_eq!(pauses, 2, "{lines:#?}");
}

#[tokio::test]
async fn a_restart_audits_config_changes_and_rotated_secrets_without_revealing_them() {
	let dir = tempfile::tempdir().unwrap();
	let store = Store::open(dir.path(), "test-start").unwrap();
	let pepper = Secret::new("test-pepper".into());
	let fp = |v: &str| runner::fingerprint(&pepper, "api_key", v);

	runner::audit_start(
		&store,
		Mode::Sandbox,
		"digest-1",
		&[("api_key", fp("test-key-1"))],
	)
	.unwrap();
	assert!(store.last_audit("config_changed").unwrap().is_none());

	// Same config, same secret: nothing new but the start.
	runner::audit_start(
		&store,
		Mode::Sandbox,
		"digest-1",
		&[("api_key", fp("test-key-1"))],
	)
	.unwrap();
	assert!(store.last_audit("config_changed").unwrap().is_none());
	assert!(store.last_audit("secret_rotated").unwrap().is_none());

	runner::audit_start(
		&store,
		Mode::Sandbox,
		"digest-2",
		&[("api_key", fp("test-key-2"))],
	)
	.unwrap();
	let changed = store.last_audit("config_changed").unwrap().unwrap();
	assert!(changed.detail.contains("digest-1") && changed.detail.contains("digest-2"));
	let rotated = store.last_audit("secret_rotated").unwrap().unwrap();
	assert_eq!(rotated.detail, "api_key");

	let text = std::fs::read_to_string(dir.path().join(audit::FILE)).unwrap();
	assert!(!text.contains("test-key-1") && !text.contains("test-key-2"));
	assert!(store.verify_audit().unwrap().ok());
}

// ── readiness ────────────────────────────────────────────────────────────

#[tokio::test]
async fn a_running_agent_is_ready() {
	let dir = tempfile::tempdir().unwrap();
	let world = World::new();
	let agent = world.agent(dir.path()).unwrap();
	let r = agent.check_readiness().await;
	assert!(r.ready(), "{r:?}");
}

#[tokio::test]
async fn readiness_fails_when_the_lease_is_lost() {
	let dir = tempfile::tempdir().unwrap();
	let world = World::new();
	let agent = world.agent(dir.path()).unwrap();
	world.wait_out_the_lease();
	let r = agent.check_readiness().await;
	assert!(!r.lease && !r.ready(), "{r:?}");
	// A tick renews it.
	agent.tick().await.unwrap();
	assert!(agent.check_readiness().await.ready());
}

#[tokio::test]
async fn readiness_fails_when_the_database_cannot_take_writes() {
	let dir = tempfile::tempdir().unwrap();
	let world = World::new();
	let agent = world.agent(dir.path()).unwrap();
	// Another writer holds the database (past the 5 s busy timeout).
	let mut other = raw_db(dir.path());
	let tx = other
		.transaction_with_behavior(rusqlite::TransactionBehavior::Exclusive)
		.unwrap();
	let r = agent.check_readiness().await;
	assert!(!r.database && !r.ready(), "{r:?}");
	tx.rollback().unwrap();
	assert!(agent.check_readiness().await.database);
}

#[tokio::test]
async fn readiness_fails_when_recoilpay_is_unreachable() {
	let dir = tempfile::tempdir().unwrap();
	let world = World::new();
	let agent = world.agent(dir.path()).unwrap();
	*world.platform.down.lock().unwrap() = true;
	let r = agent.check_readiness().await;
	assert!(!r.aggregator && !r.ready(), "{r:?}");
	assert!(r.lease && r.database && r.rpc && r.running);
}

#[tokio::test]
async fn readiness_fails_when_the_rpcs_are_unreachable() {
	let dir = tempfile::tempdir().unwrap();
	let world = World::new();
	let agent = world.agent(dir.path()).unwrap();
	*world.locks.2.lock().unwrap() = true;
	let r = agent.check_readiness().await;
	assert!(!r.rpc && !r.ready(), "{r:?}");
}

#[tokio::test]
async fn readiness_fails_while_payouts_are_paused() {
	let dir = tempfile::tempdir().unwrap();
	let world = World::new();
	let agent = world.agent(dir.path()).unwrap();
	agent.pause("ada@console", false).await.unwrap();
	let r = agent.check_readiness().await;
	assert!(!r.running && !r.ready(), "{r:?}");
	agent.resume("ada@console").await.unwrap();
	assert!(agent.check_readiness().await.ready());
}

// ── metrics ──────────────────────────────────────────────────────────────

#[tokio::test]
async fn a_paid_trade_moves_the_trade_tick_heartbeat_float_and_latency_metrics() {
	let dir = tempfile::tempdir().unwrap();
	let world = World::new();
	let agent = world.agent(dir.path()).unwrap();
	let before = agent.metrics_text().await;
	assert_eq!(
		metric(&before, "merchant_agent_ticks_total").as_deref(),
		Some("0")
	);
	assert_eq!(
		metric(&before, "merchant_agent_lock_to_paid_seconds_count").as_deref(),
		Some("0")
	);
	assert!(
		metric(&before, "merchant_agent_float_kobo").is_none(),
		"not read yet"
	);

	tick_until(&agent, reported).await;
	let after = agent.metrics_text().await;
	assert_eq!(
		metric(&after, "merchant_agent_trades{state=\"reported\"}").as_deref(),
		Some("1"),
		"{after}"
	);
	assert_ne!(
		metric(&after, "merchant_agent_ticks_total").as_deref(),
		Some("0")
	);
	assert_eq!(
		metric(&after, "merchant_agent_heartbeats_total").as_deref(),
		Some("1")
	);
	assert_eq!(world.platform.heartbeats.load(Ordering::SeqCst), 1);
	assert!(
		metric(&after, "merchant_agent_float_kobo").is_some(),
		"{after}"
	);
	assert_eq!(
		metric(&after, "merchant_agent_lock_to_paid_seconds_count").as_deref(),
		Some("1")
	);
	assert_eq!(
		metric(
			&after,
			"merchant_agent_lock_to_paid_seconds_bucket{le=\"+Inf\"}"
		)
		.as_deref(),
		Some("1")
	);
	assert_eq!(metric(&after, "merchant_agent_ready").as_deref(), Some("1"));
	assert_eq!(
		metric(&after, "merchant_agent_paused").as_deref(),
		Some("0")
	);
	// No personal data in any label or value. (The float, in kobo, can
	// contain ten zeros, so the account is matched as a whole value.)
	let values = after.split(|c: char| c.is_whitespace() || c == '"');
	assert!(values.into_iter().all(|v| v != ACCOUNT), "{after}");
	assert!(!after.contains(NAME));
}

#[tokio::test]
async fn the_heartbeat_goes_every_30_seconds_and_failures_are_counted() {
	let dir = tempfile::tempdir().unwrap();
	let world = World::new();
	let agent = world.agent(dir.path()).unwrap();
	agent.tick().await.unwrap();
	agent.tick().await.unwrap();
	assert_eq!(
		world.platform.heartbeats.load(Ordering::SeqCst),
		1,
		"not yet due"
	);
	world.clock.advance(Duration::from_secs(30));
	agent.tick().await.unwrap();
	assert_eq!(world.platform.heartbeats.load(Ordering::SeqCst), 2);

	*world.platform.down.lock().unwrap() = true;
	world.clock.advance(Duration::from_secs(30));
	let _ = agent.tick().await;
	let text = agent.metrics().render(&Default::default(), false, false);
	assert_eq!(
		metric(&text, "merchant_agent_heartbeat_failures_total").as_deref(),
		Some("1")
	);
	assert_eq!(
		metric(&text, "merchant_agent_heartbeats_total").as_deref(),
		Some("2")
	);
}

#[tokio::test]
async fn a_hold_is_counted_by_reason() {
	let dir = tempfile::tempdir().unwrap();
	let world = World::new();
	*world.locks.0.lock().unwrap() = LockOutcome::Problem(HoldReason::LockMismatch);
	let agent = world.agent(dir.path()).unwrap();
	agent.tick().await.unwrap();
	let text = agent.metrics_text().await;
	assert_eq!(
		metric(
			&text,
			&format!(
				"merchant_agent_holds_total{{reason=\"{}\"}}",
				HoldReason::LockMismatch.code()
			)
		)
		.as_deref(),
		Some("1"),
		"{text}"
	);
	assert_eq!(
		metric(&text, "merchant_agent_trades{state=\"held\"}").as_deref(),
		Some("1"),
		"{text}"
	);
}

#[tokio::test]
async fn bank_errors_and_alerts_are_counted() {
	let dir = tempfile::tempdir().unwrap();
	let world = World::new();
	world
		.bank
		.inner
		.set_faults(merchant_agent_core::rail::simulated::Faults {
			errors_after_paying: 1,
			..Default::default()
		});
	*world.bank.status_down.lock().unwrap() = true;
	let agent = world.agent(dir.path()).unwrap();
	tick_until(&agent, |s| matches!(s, AgentState::NeedsPerson(_))).await;
	let text = agent.metrics_text().await;
	assert_eq!(
		metric(&text, "merchant_agent_bank_errors_total{kind=\"send\"}").as_deref(),
		Some("1"),
		"{text}"
	);
	let status: u64 = metric(&text, "merchant_agent_bank_errors_total{kind=\"status\"}")
		.unwrap()
		.parse()
		.unwrap();
	assert!(status >= 3, "{text}");
	let alerts: u64 = metric(&text, "merchant_agent_alerts_total")
		.unwrap()
		.parse()
		.unwrap();
	assert!(alerts >= 1, "{text}");
}

// ── the health endpoints ─────────────────────────────────────────────────

async fn get(agent: Arc<Agent>, path: &str) -> (StatusCode, String) {
	let ops: Arc<dyn AgentOps> = agent;
	let r = merchant_agent_core::health::router(ops)
		.oneshot(Request::get(path).body(Body::empty()).unwrap())
		.await
		.unwrap();
	let status = r.status();
	let body = axum::body::to_bytes(r.into_body(), 1 << 20).await.unwrap();
	(status, String::from_utf8(body.to_vec()).unwrap())
}

#[tokio::test]
async fn the_health_port_serves_liveness_readiness_and_metrics() {
	let dir = tempfile::tempdir().unwrap();
	let world = World::new();
	let agent = Arc::new(world.agent(dir.path()).unwrap());

	assert_eq!(
		get(agent.clone(), "/healthz").await,
		(StatusCode::OK, "ok\n".into())
	);

	let (status, body) = get(agent.clone(), "/readyz").await;
	assert_eq!(status, StatusCode::OK, "{body}");
	let v: Value = serde_json::from_str(&body).unwrap();
	assert_eq!(v["ready"], true);
	assert_eq!(v["checks"]["lease"], true);

	let (status, body) = get(agent.clone(), "/metrics").await;
	assert_eq!(status, StatusCode::OK);
	assert!(
		body.contains("# TYPE merchant_agent_lock_to_paid_seconds histogram"),
		"{body}"
	);
	assert!(body.contains("merchant_agent_up 1"));
}

#[tokio::test]
async fn readyz_answers_503_with_the_failing_condition() {
	let dir = tempfile::tempdir().unwrap();
	let world = World::new();
	let agent = Arc::new(world.agent(dir.path()).unwrap());
	agent.pause("ada@console", false).await.unwrap();
	let (status, body) = get(agent, "/readyz").await;
	assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
	let v: Value = serde_json::from_str(&body).unwrap();
	assert_eq!(v["ready"], false);
	assert_eq!(v["checks"]["running"], false);
	assert_eq!(v["checks"]["aggregator"], true);
}

#[tokio::test]
async fn every_sample_follows_its_own_family_header() {
	let dir = tempfile::tempdir().unwrap();
	let (_world, agent) = paid_run(dir.path()).await;
	let text = agent.metrics_text().await;
	let mut family = String::new();
	let mut seen = std::collections::HashSet::new();
	for line in text.lines() {
		if let Some(rest) = line.strip_prefix("# TYPE ") {
			family = rest.split(' ').next().unwrap().to_string();
			assert!(seen.insert(family.clone()), "{family} declared twice");
		} else if !line.starts_with('#') {
			let name = line.split(['{', ' ']).next().unwrap();
			let base = ["_bucket", "_sum", "_count"]
				.iter()
				.find_map(|s| name.strip_suffix(s).filter(|b| *b == family))
				.unwrap_or(name);
			assert_eq!(base, family, "{line} is outside its family");
		}
	}
}
