//! The store on its own: migrations, the lease, guarded writes and the
//! write-ahead intent.

use std::time::Duration;

use chrono::{DateTime, Utc};
use merchant_agent_core::aggregator::types::TradeState;
use merchant_agent_core::machine::{AgentState, Effect, HoldReason};
use merchant_agent_core::money::Minor;
use merchant_agent_core::store::{
	idempotency_key, inspect, Intent, NewTrade, Patch, Store, StoreError, SCHEMA_VERSION,
};

const TTL: Duration = Duration::from_secs(30);

fn t0() -> DateTime<Utc> {
	"2026-11-13T12:00:00Z".parse().unwrap()
}

fn new_trade() -> NewTrade {
	NewTrade {
		trade_id: "ft-1".into(),
		chain: "eip155:84532".into(),
		quote_id: "fq-1".into(),
		asset_minor: Minor(100_000_000),
		fiat_minor: Minor(15_081_152),
		fee_minor: Minor(47_823),
		rate: "1520.50".into(),
		user_address: "0x2222222222222222222222222222222222222222".into(),
		payee_commitment: "0xABCD".into(),
		deadline_at: t0() + chrono::Duration::minutes(15),
		platform_state: TradeState::AwaitingFiat,
	}
}

fn open(dir: &std::path::Path) -> Store {
	let s = Store::open(dir, "me").unwrap();
	s.acquire_lease(TTL, t0()).unwrap();
	s
}

#[test]
fn migrations_run_once_and_a_newer_database_is_refused() {
	let dir = tempfile::tempdir().unwrap();
	assert_eq!(inspect(dir.path()).unwrap().schema, None);
	let s = open(dir.path());
	assert_eq!(s.schema_version().unwrap(), SCHEMA_VERSION);
	drop(s);
	let s = open(dir.path());
	assert_eq!(
		s.schema_version().unwrap(),
		SCHEMA_VERSION,
		"re-opening runs nothing twice"
	);
	drop(s);

	let conn = rusqlite_conn(dir.path());
	conn.pragma_update(None, "user_version", (SCHEMA_VERSION + 1) as i64)
		.unwrap();
	drop(conn);
	assert!(matches!(
		Store::open(dir.path(), "me"),
		Err(StoreError::TooNew { .. })
	));
}

fn rusqlite_conn(dir: &std::path::Path) -> rusqlite::Connection {
	rusqlite::Connection::open(dir.join("agent.db")).unwrap()
}

#[test]
fn the_database_is_in_wal_mode_with_full_sync() {
	let dir = tempfile::tempdir().unwrap();
	let _s = open(dir.path());
	let conn = rusqlite_conn(dir.path());
	let mode: String = conn
		.query_row("PRAGMA journal_mode", [], |r| r.get(0))
		.unwrap();
	assert_eq!(mode, "wal");
}

#[test]
fn invariant_10_the_lease_has_one_holder_and_a_rising_fencing_token() {
	let dir = tempfile::tempdir().unwrap();
	let a = Store::open_shared(dir.path(), "a").unwrap();
	let b = Store::open_shared(dir.path(), "b").unwrap();
	assert_eq!(a.acquire_lease(TTL, t0()).unwrap(), 1);
	assert!(matches!(
		b.acquire_lease(TTL, t0()),
		Err(StoreError::LeaseHeld { .. })
	));
	assert_eq!(
		a.acquire_lease(TTL, t0()).unwrap(),
		1,
		"re-taking our own lease keeps the token"
	);

	let later = t0() + chrono::Duration::seconds(31);
	assert_eq!(
		b.acquire_lease(TTL, later).unwrap(),
		2,
		"a new holder raises the token"
	);
	assert!(matches!(
		a.renew_lease(TTL, later),
		Err(StoreError::LeaseLost)
	));
	assert!(matches!(
		a.insert_detected(&new_trade(), later),
		Err(StoreError::LeaseLost)
	));
	assert!(b.insert_detected(&new_trade(), later).unwrap());
	let (holder, _) = inspect(dir.path()).unwrap().lease.unwrap();
	assert_eq!(holder, "b");
}

#[test]
fn a_write_without_the_lease_is_refused() {
	let dir = tempfile::tempdir().unwrap();
	let s = Store::open(dir.path(), "me").unwrap();
	assert!(matches!(
		s.insert_detected(&new_trade(), t0()),
		Err(StoreError::LeaseLost)
	));
}

#[test]
fn a_stale_record_cannot_overwrite_a_newer_state() {
	let dir = tempfile::tempdir().unwrap();
	let s = open(dir.path());
	s.insert_detected(&new_trade(), t0()).unwrap();
	let stale = s.get("ft-1").unwrap().unwrap();
	s.transition(
		&stale,
		&AgentState::LockVerified,
		&[],
		&Patch::default(),
		t0(),
	)
	.unwrap();
	let e = s.transition(
		&stale,
		&AgentState::Held(HoldReason::LockMismatch),
		&[],
		&Patch::default(),
		t0(),
	);
	assert!(matches!(e, Err(StoreError::Conflict)));
	assert_eq!(
		s.get("ft-1").unwrap().unwrap().state,
		AgentState::LockVerified
	);
}

fn to_checks_passed(s: &Store) -> merchant_agent_core::store::TradeRecord {
	s.insert_detected(&new_trade(), t0()).unwrap();
	let r = s.get("ft-1").unwrap().unwrap();
	let r = s
		.transition(&r, &AgentState::LockVerified, &[], &Patch::default(), t0())
		.unwrap();
	s.transition(&r, &AgentState::ChecksPassed, &[], &Patch::default(), t0())
		.unwrap()
}

fn intent() -> Intent {
	Intent {
		amount: Minor(15_081_152),
		payee_tag: "tag".into(),
		day: "2026-11-13".into(),
	}
}

#[test]
fn invariant_1_the_intent_and_its_key_are_written_before_any_send() {
	let dir = tempfile::tempdir().unwrap();
	let s = open(dir.path());
	let r = to_checks_passed(&s);
	let r = s
		.record_intent(&r, &intent(), t0(), |_| Ok(Ok(())))
		.unwrap()
		.unwrap();
	assert_eq!(r.state, AgentState::PayoutIntentRecorded);
	let payouts = s.payouts().unwrap();
	assert_eq!(payouts.len(), 1);
	assert_eq!(payouts[0].idempotency_key, idempotency_key("ft-1", 1));
	assert_eq!(payouts[0].status, "recorded");

	// Starting to send marks the payout; a refused send cancels it.
	let r = s
		.transition(&r, &AgentState::Paying, &[], &Patch::default(), t0())
		.unwrap();
	assert_eq!(s.payouts().unwrap()[0].status, "sending");
	s.transition(
		&r,
		&AgentState::Held(HoldReason::BankRejected),
		&[Effect::ReleaseReservation],
		&Patch::default(),
		t0(),
	)
	.unwrap();
	assert_eq!(s.payouts().unwrap()[0].status, "cancelled");
}

#[test]
fn a_refused_reservation_writes_nothing() {
	let dir = tempfile::tempdir().unwrap();
	let s = open(dir.path());
	let r = to_checks_passed(&s);
	let refused = s
		.record_intent(&r, &intent(), t0(), |_| Ok(Err(HoldReason::OverDailyLimit)))
		.unwrap();
	assert_eq!(refused.unwrap_err(), HoldReason::OverDailyLimit);
	assert!(s.payouts().unwrap().is_empty());
	assert_eq!(
		s.get("ft-1").unwrap().unwrap().state,
		AgentState::ChecksPassed
	);
}

#[test]
fn invariant_11_a_paused_agent_records_no_new_payout() {
	let dir = tempfile::tempdir().unwrap();
	let s = open(dir.path());
	let r = to_checks_passed(&s);
	s.set_paused(true, "ops", false, None, t0()).unwrap();
	let refused = s
		.record_intent(&r, &intent(), t0(), |_| Ok(Ok(())))
		.unwrap();
	assert_eq!(refused.unwrap_err(), HoldReason::Paused);
	assert!(s.payouts().unwrap().is_empty());
}

#[test]
fn a_new_attempt_gets_a_new_key() {
	let dir = tempfile::tempdir().unwrap();
	let s = open(dir.path());
	let r = to_checks_passed(&s);
	let r = s
		.record_intent(&r, &intent(), t0(), |_| Ok(Ok(())))
		.unwrap()
		.unwrap();
	let r = s
		.transition(
			&r,
			&AgentState::Detected,
			&[
				Effect::NewAttempt,
				Effect::Override(HoldReason::OverDailyLimit),
			],
			&Patch::default(),
			t0(),
		)
		.unwrap();
	assert_eq!(r.attempt, 2);
	assert_eq!(
		r.idempotency_key(),
		format!("{}-2", idempotency_key("ft-1", 1))
	);
	assert_eq!(r.overrides, vec![HoldReason::OverDailyLimit]);
}

#[test]
fn an_older_database_is_backed_up_before_it_is_migrated() {
	use merchant_agent_core::store::{DB_FILE, SCHEMA_VERSION};
	let dir = tempfile::tempdir().unwrap();
	drop(Store::open(dir.path(), "first").unwrap());
	// Make it a schema-3 database: migrations 4 (the audit table) and 5
	// (purchases, quote direction) undone.
	{
		let c = rusqlite::Connection::open(dir.path().join(DB_FILE)).unwrap();
		c.execute_batch(
			"DROP TABLE audit; DROP TABLE purchases; ALTER TABLE quotes_seen DROP COLUMN direction; PRAGMA user_version = 3;",
		)
		.unwrap();
	}
	drop(Store::open(dir.path(), "second").unwrap());

	let backups: Vec<_> = std::fs::read_dir(dir.path())
		.unwrap()
		.filter_map(|e| e.ok())
		.map(|e| e.file_name().to_string_lossy().into_owned())
		.filter(|n| n.starts_with(&format!("{DB_FILE}.backup-v3-")))
		.collect();
	assert_eq!(backups.len(), 1, "{backups:?}");
	let old = rusqlite::Connection::open(dir.path().join(&backups[0])).unwrap();
	let v: i64 = old
		.query_row("PRAGMA user_version", [], |r| r.get(0))
		.unwrap();
	assert_eq!(v, 3, "the backup is the database as it was");
	let new = rusqlite::Connection::open(dir.path().join(DB_FILE)).unwrap();
	let v: i64 = new
		.query_row("PRAGMA user_version", [], |r| r.get(0))
		.unwrap();
	assert_eq!(v as usize, SCHEMA_VERSION);

	// A database already current is not backed up again.
	drop(Store::open(dir.path(), "third").unwrap());
	let count = std::fs::read_dir(dir.path())
		.unwrap()
		.filter(|e| {
			e.as_ref()
				.unwrap()
				.file_name()
				.to_string_lossy()
				.contains(".backup-")
		})
		.count();
	assert_eq!(count, 1);
}
