//! The agent's durable state: SQLite in WAL mode with `synchronous=FULL`,
//! on the one persistent volume.
//!
//! - **One write per transition.** [`Store::transition`] applies a state
//!   machine transition and its storage effects in one transaction,
//!   guarded by the record's version, so a stale engine can't overwrite a
//!   newer state.
//! - **Write-ahead intent (invariant 1).** [`Store::record_intent`] reserves
//!   the payout and writes its idempotency key before the bank is ever
//!   called.
//! - **One agent at a time (invariant 10).** An OS file lock on the data
//!   directory stops a second process on the same host. A lease row with a
//!   fencing token stops one on another host sharing the volume, where file
//!   locks can't be trusted. Every write checks the lease inside its own
//!   transaction, so an agent that has lost the lease can't write anything.
//! - **No personal data (invariant 7).** No account numbers, names, bank
//!   codes or sealed envelopes. Only the commitment, the keyed payee tag
//!   and a masked account.
//! - **Migrations** are embedded and forward-only. A database from a newer
//!   agent is refused rather than guessed at.

use std::fs::{File, OpenOptions};
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::Duration;

use chrono::{DateTime, Utc};
use rusqlite::{params, Connection, OptionalExtension, Transaction, TransactionBehavior};

use crate::aggregator::types::TradeState;
use crate::audit;
use crate::machine::{AgentState, Effect, HoldReason};
use crate::money::Minor;

/// Forward-only migrations. Index + 1 is the schema version. Never edit one
/// that has shipped; add another.
const MIGRATIONS: &[&str] = &[
	r#"
CREATE TABLE trades (
	trade_id          TEXT PRIMARY KEY,
	chain             TEXT NOT NULL,
	quote_id          TEXT NOT NULL,
	asset_minor       TEXT NOT NULL,
	fiat_minor        TEXT NOT NULL,
	fee_minor         TEXT NOT NULL,
	rate              TEXT NOT NULL,
	user_address      TEXT NOT NULL,
	payee_commitment  TEXT NOT NULL,
	payee_tag         TEXT,
	account_masked    TEXT,
	state             TEXT NOT NULL,
	reason            TEXT,
	attempt           INTEGER NOT NULL DEFAULT 1,
	bank_reference    TEXT,
	lock_protocol_fee TEXT,
	overrides         TEXT NOT NULL DEFAULT '',
	deadline_at       TEXT NOT NULL,
	platform_state    TEXT NOT NULL,
	created_at        TEXT NOT NULL,
	updated_at        TEXT NOT NULL,
	version           INTEGER NOT NULL
);
CREATE INDEX trades_state ON trades(state);

-- The write-ahead intent: one row per payout attempt, written before the
-- bank is called. Limits are counted from here.
CREATE TABLE payouts (
	idempotency_key TEXT PRIMARY KEY,
	trade_id        TEXT NOT NULL REFERENCES trades(trade_id),
	attempt         INTEGER NOT NULL,
	amount_minor    TEXT NOT NULL,
	payee_tag       TEXT NOT NULL,
	day             TEXT NOT NULL,
	status          TEXT NOT NULL,
	bank_reference  TEXT,
	created_at      TEXT NOT NULL,
	updated_at      TEXT NOT NULL,
	UNIQUE (trade_id, attempt)
);
CREATE INDEX payouts_day ON payouts(day);
CREATE INDEX payouts_payee_day ON payouts(payee_tag, day);

-- The published-rate history: every quote the agent has seen, with the
-- merchant's reference rate at the time (design §4.4).
CREATE TABLE quotes_seen (
	quote_id       TEXT PRIMARY KEY,
	chain          TEXT NOT NULL,
	asset          TEXT NOT NULL,
	asset_decimals INTEGER NOT NULL,
	fiat_currency  TEXT NOT NULL,
	rail           TEXT NOT NULL,
	rate           TEXT NOT NULL,
	fee_fixed      TEXT NOT NULL,
	fee_bps        INTEGER NOT NULL,
	min_fiat       TEXT NOT NULL,
	max_fiat       TEXT NOT NULL,
	reference_rate TEXT,
	first_seen     TEXT NOT NULL
);

CREATE TABLE lease (
	id         INTEGER PRIMARY KEY CHECK (id = 1),
	holder     TEXT NOT NULL,
	fencing    INTEGER NOT NULL,
	expires_at TEXT NOT NULL
);

CREATE TABLE approvals (
	id           INTEGER PRIMARY KEY,
	trade_id     TEXT,
	action       TEXT NOT NULL,
	requested_by TEXT NOT NULL,
	approved_by  TEXT,
	created_at   TEXT NOT NULL,
	decided_at   TEXT
);

CREATE TABLE control (
	id          INTEGER PRIMARY KEY CHECK (id = 1),
	paused      INTEGER NOT NULL,
	paused_by   TEXT,
	paused_auto INTEGER NOT NULL,
	last_book   TEXT
);
INSERT INTO control (id, paused, paused_auto) VALUES (1, 0, 0);

CREATE TABLE audit_head (
	id   INTEGER PRIMARY KEY CHECK (id = 1),
	seq  INTEGER NOT NULL,
	hash TEXT NOT NULL
);
"#,
	r#"
-- The manual rail: a person pays from their bank app. No bank details are
-- kept; a reveal re-opens them from the trade, and is counted here.
CREATE TABLE manual_tasks (
	idempotency_key  TEXT PRIMARY KEY,
	trade_id         TEXT NOT NULL REFERENCES trades(trade_id),
	amount_minor     TEXT NOT NULL,
	status           TEXT NOT NULL,
	reference        TEXT,
	reveals          INTEGER NOT NULL DEFAULT 0,
	last_revealed_by TEXT,
	last_revealed_at TEXT,
	closed_by        TEXT,
	created_at       TEXT NOT NULL,
	updated_at       TEXT NOT NULL
);
CREATE INDEX manual_tasks_open ON manual_tasks(status);
"#,
	r#"
-- Console users. Passwords are argon2id hashes; TOTP secrets are encrypted
-- at rest with a key derived from the payee tag pepper.
CREATE TABLE users (
	username        TEXT PRIMARY KEY,
	role            TEXT NOT NULL,
	password_hash   TEXT NOT NULL,
	totp_sealed     TEXT NOT NULL,
	last_totp_step  INTEGER NOT NULL DEFAULT 0,
	failed_attempts INTEGER NOT NULL DEFAULT 0,
	locked_until    TEXT,
	disabled        INTEGER NOT NULL DEFAULT 0,
	created_at      TEXT NOT NULL
);
"#,
	r#"
-- The tamper-evident audit chain (see audit.rs). Written in the same
-- transaction as the change each entry records; mirrored to audit.jsonl.
-- audit_head (migration 1) records how far the file has been written.
CREATE TABLE audit (
	seq      INTEGER PRIMARY KEY,
	at       TEXT NOT NULL,
	actor    TEXT NOT NULL,
	action   TEXT NOT NULL,
	trade_id TEXT,
	detail   TEXT NOT NULL,
	prev     TEXT NOT NULL,
	hash     TEXT NOT NULL
);
"#,
];

/// The schema version this build writes.
pub const SCHEMA_VERSION: usize = MIGRATIONS.len();

pub const DB_FILE: &str = "agent.db";
pub const LOCK_FILE: &str = "agent.lock";

#[derive(Debug, thiserror::Error)]
pub enum StoreError {
	#[error("another merchant-agent is already running on this data directory")]
	AnotherInstance,
	#[error("another agent holds the lease until {until} ({holder})")]
	LeaseHeld {
		holder: String,
		until: DateTime<Utc>,
	},
	#[error("this agent no longer holds the lease; it must stop acting")]
	LeaseLost,
	#[error("the trade changed since it was read; re-read it")]
	Conflict,
	#[error("no such trade")]
	NotFound,
	#[error("the database is from a newer agent (schema {found}, this agent knows {known}); upgrade the agent")]
	TooNew { found: usize, known: usize },
	#[error("the database holds a value this agent does not understand: {0}")]
	Corrupt(String),
	#[error("database error: {0}")]
	Db(#[from] rusqlite::Error),
	#[error("cannot open the data directory: {0}")]
	Io(String),
}

pub type StoreResult<T> = Result<T, StoreError>;

/// What the agent stores about one trade.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TradeRecord {
	pub trade_id: String,
	pub chain: String,
	pub quote_id: String,
	pub asset_minor: Minor,
	pub fiat_minor: Minor,
	pub fee_minor: Minor,
	pub rate: String,
	pub user_address: String,
	pub payee_commitment: String,
	pub payee_tag: Option<String>,
	pub account_masked: Option<String>,
	pub state: AgentState,
	pub attempt: u32,
	pub bank_reference: Option<String>,
	pub lock_protocol_fee: Option<String>,
	/// Holds a person approved away (design: approvals restart at Detected).
	pub overrides: Vec<HoldReason>,
	pub deadline_at: DateTime<Utc>,
	pub platform_state: TradeState,
	pub created_at: DateTime<Utc>,
	pub updated_at: DateTime<Utc>,
	pub version: i64,
}

impl TradeRecord {
	/// The idempotency key for this attempt: deterministic from the trade
	/// id, so a restart always asks the bank about the same key.
	pub fn idempotency_key(&self) -> String {
		idempotency_key(&self.trade_id, self.attempt)
	}
}

pub fn idempotency_key(trade_id: &str, attempt: u32) -> String {
	if attempt <= 1 {
		format!("rpma-{trade_id}")
	} else {
		format!("rpma-{trade_id}-{attempt}")
	}
}

/// A trade seen for the first time.
#[derive(Debug, Clone)]
pub struct NewTrade {
	pub trade_id: String,
	pub chain: String,
	pub quote_id: String,
	pub asset_minor: Minor,
	pub fiat_minor: Minor,
	pub fee_minor: Minor,
	pub rate: String,
	pub user_address: String,
	pub payee_commitment: String,
	pub deadline_at: DateTime<Utc>,
	pub platform_state: TradeState,
}

/// Fields the engine may update along with a transition.
#[derive(Debug, Clone, Default)]
pub struct Patch {
	pub platform_state: Option<TradeState>,
	pub deadline_at: Option<DateTime<Utc>>,
	pub lock_protocol_fee: Option<String>,
	pub payee_tag: Option<String>,
	pub account_masked: Option<String>,
	/// Who decided this, for the audit log. `None` means the agent itself.
	pub actor: Option<String>,
}

/// The payout to reserve and write ahead.
#[derive(Debug, Clone)]
pub struct Intent {
	pub amount: Minor,
	pub payee_tag: String,
	/// The merchant-local day the payout counts against.
	pub day: String,
}

/// One payout attempt, as the limits read it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PayoutRow {
	pub idempotency_key: String,
	pub trade_id: String,
	pub amount: Minor,
	pub payee_tag: String,
	pub day: String,
	pub status: String,
	pub created_at: DateTime<Utc>,
}

pub struct Store {
	conn: Mutex<Connection>,
	holder: String,
	fencing: Mutex<Option<i64>>,
	_os_lock: Option<File>,
	path: PathBuf,
}

fn ts(t: DateTime<Utc>) -> String {
	t.to_rfc3339_opts(chrono::SecondsFormat::Micros, true)
}

fn parse_ts(s: &str) -> StoreResult<DateTime<Utc>> {
	DateTime::parse_from_rfc3339(s)
		.map(|t| t.with_timezone(&Utc))
		.map_err(|_| StoreError::Corrupt(format!("timestamp {s}")))
}

fn parse_platform(s: &str) -> StoreResult<TradeState> {
	serde_json::from_value(serde_json::Value::String(s.into()))
		.map_err(|_| StoreError::Corrupt(format!("platform state {s}")))
}

fn parse_minor(s: &str) -> StoreResult<Minor> {
	Minor::parse(s).map_err(|_| StoreError::Corrupt(format!("amount {s}")))
}

/// Create the data directory readable only by the agent's user (0700), and
/// tighten an existing one this user owns. It holds the ledger, the console
/// users' password hashes and the lock file: another local account must
/// not be able to read them, or hold the lock and keep the agent from
/// starting. The container image already makes it 0700; this covers the
/// binary run directly on a host.
pub fn private_dir(dir: &Path) -> std::io::Result<()> {
	use std::os::unix::fs::{DirBuilderExt, PermissionsExt};
	std::fs::DirBuilder::new()
		.recursive(true)
		.mode(0o700)
		.create(dir)?;
	let mode = std::fs::metadata(dir)?.permissions().mode();
	if mode & 0o077 != 0 {
		// Fails (and is left alone) when another user owns it, as a
		// mounted volume's root may be.
		let _ = std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700));
	}
	Ok(())
}

impl Store {
	/// Open (creating if needed) the store in `data_dir`, taking the OS file
	/// lock. A second process on this directory gets
	/// [`StoreError::AnotherInstance`] at once.
	pub fn open(data_dir: &Path, holder: &str) -> StoreResult<Self> {
		private_dir(data_dir).map_err(|e| StoreError::Io(e.to_string()))?;
		let lock = OpenOptions::new()
			.create(true)
			.truncate(false)
			.write(true)
			.mode(0o600)
			.open(data_dir.join(LOCK_FILE))
			.map_err(|e| StoreError::Io(e.to_string()))?;
		rustix::fs::flock(&lock, rustix::fs::FlockOperation::NonBlockingLockExclusive)
			.map_err(|_| StoreError::AnotherInstance)?;
		Self::open_db(&data_dir.join(DB_FILE), holder, Some(lock))
	}

	/// Open without the OS file lock, relying on the lease alone: what an
	/// agent on another host sharing the volume sees. For tests of the lease.
	/// For `merchant-agent pause/resume/status/users` and the console's
	/// user checks: no file lock and no lease, so it works beside a running
	/// agent. It can read everything, but every write that moves a trade or
	/// money checks the lease and so fails here. Only the control writes
	/// below (the pause flag and users), which move no money, succeed.
	pub fn open_control(data_dir: &Path) -> StoreResult<Self> {
		Self::open_shared(data_dir, "control")
	}

	pub fn open_shared(data_dir: &Path, holder: &str) -> StoreResult<Self> {
		private_dir(data_dir).map_err(|e| StoreError::Io(e.to_string()))?;
		Self::open_db(&data_dir.join(DB_FILE), holder, None)
	}

	fn open_db(path: &Path, holder: &str, os_lock: Option<File>) -> StoreResult<Self> {
		let mut conn = Connection::open(path)?;
		conn.busy_timeout(Duration::from_secs(5))?;
		conn.pragma_update(None, "journal_mode", "WAL")?;
		conn.pragma_update(None, "synchronous", "FULL")?;
		// Temporary tables and sorts in memory: in a container the root
		// filesystem is read-only, so there is no /tmp to spill to.
		conn.pragma_update(None, "temp_store", "MEMORY")?;
		conn.pragma_update(None, "foreign_keys", "ON")?;
		migrate(&mut conn, path)?;
		Ok(Self {
			conn: Mutex::new(conn),
			holder: holder.to_string(),
			fencing: Mutex::new(None),
			_os_lock: os_lock,
			path: path.to_path_buf(),
		})
	}

	pub fn path(&self) -> &Path {
		&self.path
	}

	/// This store's lease holder name.
	pub fn holder(&self) -> &str {
		&self.holder
	}

	/// Can the database take a write right now? Tries one and rolls it back.
	pub fn write_probe(&self) -> StoreResult<()> {
		let mut conn = self.conn();
		let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
		tx.execute("UPDATE control SET id = id WHERE id = 1", [])?;
		tx.rollback()?;
		Ok(())
	}

	/// How many trades are in each agent state, for the metrics.
	pub fn trade_state_counts(&self) -> StoreResult<std::collections::BTreeMap<String, u64>> {
		let conn = self.conn();
		let mut stmt = conn.prepare("SELECT state, COUNT(*) FROM trades GROUP BY state")?;
		let rows = stmt
			.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?)))?
			.collect::<Result<Vec<_>, _>>()?;
		Ok(rows
			.into_iter()
			.map(|(k, v)| (k, v.max(0) as u64))
			.collect())
	}

	/// When a trade last reached `action` (a state code), from the audit log.
	pub fn audit_time(&self, trade_id: &str, action: &str) -> StoreResult<Option<DateTime<Utc>>> {
		let at: Option<String> = self
			.conn()
			.query_row(
				"SELECT at FROM audit WHERE trade_id = ?1 AND action = ?2 ORDER BY seq DESC LIMIT 1",
				params![trade_id, action],
				|r| r.get(0),
			)
			.optional()?;
		at.as_deref().map(parse_ts).transpose()
	}

	fn dir(&self) -> &Path {
		self.path.parent().unwrap_or(Path::new("."))
	}

	/// Bring `audit.jsonl` up to date with the committed entries.
	fn mirror(&self) {
		let mut conn = self.conn();
		if let Err(e) = audit::mirror(&mut conn, self.path.parent().unwrap_or(Path::new("."))) {
			tracing::error!(error = %e, "could not mirror the audit log");
		}
	}

	/// Record an event that changes no other state (the agent starting, a
	/// config change, a rotated secret). No lease needed: it moves nothing.
	pub fn audit_event(
		&self,
		actor: &str,
		action: &str,
		trade_id: Option<&str>,
		detail: &str,
		now: DateTime<Utc>,
	) -> StoreResult<()> {
		{
			let mut conn = self.conn();
			let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
			audit::append(&tx, actor, action, trade_id, detail, now)?;
			tx.commit()?;
		}
		self.mirror();
		Ok(())
	}

	/// The most recent entry with `action`, for comparing at start-up.
	pub fn last_audit(&self, action: &str) -> StoreResult<Option<audit::Entry>> {
		let conn = self.conn();
		let seq: Option<i64> = conn
			.query_row(
				"SELECT MAX(seq) FROM audit WHERE action = ?1",
				[action],
				|r| r.get(0),
			)
			.optional()?
			.flatten();
		Ok(match seq {
			Some(seq) => audit::entries(&conn, seq as u64)?.into_iter().next(),
			None => None,
		})
	}

	pub fn verify_audit(&self) -> StoreResult<audit::Verification> {
		self.mirror();
		Ok(audit::verify(&self.conn(), self.dir())?)
	}

	pub fn export_audit(&self, from: DateTime<Utc>, to: DateTime<Utc>) -> StoreResult<Vec<String>> {
		Ok(audit::export(&self.conn(), from, to)?)
	}

	pub fn schema_version(&self) -> StoreResult<usize> {
		let v: i64 = self
			.conn()
			.query_row("PRAGMA user_version", [], |r| r.get(0))?;
		Ok(v as usize)
	}

	fn conn(&self) -> std::sync::MutexGuard<'_, Connection> {
		self.conn.lock().unwrap_or_else(|e| e.into_inner())
	}

	// ── lease ────────────────────────────────────────────────────────────

	/// Take the lease if it is free, expired, or already ours. Each take by
	/// a new holder raises the fencing token.
	pub fn acquire_lease(&self, ttl: Duration, now: DateTime<Utc>) -> StoreResult<i64> {
		let mut conn = self.conn();
		let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
		let current: Option<(String, i64, String)> = tx
			.query_row(
				"SELECT holder, fencing, expires_at FROM lease WHERE id = 1",
				[],
				|r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
			)
			.optional()?;
		let expires = now + chrono::Duration::from_std(ttl).unwrap_or_default();
		let fencing = match current {
			Some((holder, fencing, _)) if holder == self.holder => fencing,
			Some((holder, fencing, until)) => {
				let until = parse_ts(&until)?;
				if until > now {
					return Err(StoreError::LeaseHeld { holder, until });
				}
				fencing + 1
			}
			None => 1,
		};
		tx.execute(
			"INSERT INTO lease (id, holder, fencing, expires_at) VALUES (1, ?1, ?2, ?3)
			 ON CONFLICT(id) DO UPDATE SET holder = ?1, fencing = ?2, expires_at = ?3",
			params![self.holder, fencing, ts(expires)],
		)?;
		tx.commit()?;
		*self.fencing.lock().unwrap_or_else(|e| e.into_inner()) = Some(fencing);
		Ok(fencing)
	}

	/// Extend our lease. Fails with [`StoreError::LeaseLost`] if another
	/// holder took it.
	///
	/// A lease that expired while this agent was stalled is still renewable
	/// if the row still names us with our fencing token: any other agent
	/// would have had to rewrite the row to act, so nobody did. Writes, by
	/// contrast, need an unexpired lease, so a stalled agent writes nothing
	/// until it has renewed.
	pub fn renew_lease(&self, ttl: Duration, now: DateTime<Utc>) -> StoreResult<()> {
		let mut conn = self.conn();
		let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
		let ours = *self.fencing.lock().unwrap_or_else(|e| e.into_inner());
		let row: Option<(String, i64)> = tx
			.query_row("SELECT holder, fencing FROM lease WHERE id = 1", [], |r| {
				Ok((r.get(0)?, r.get(1)?))
			})
			.optional()?;
		match (row, ours) {
			(Some((holder, fencing)), Some(ours)) if holder == self.holder && fencing == ours => {}
			_ => return Err(StoreError::LeaseLost),
		}
		let expires = now + chrono::Duration::from_std(ttl).unwrap_or_default();
		tx.execute(
			"UPDATE lease SET expires_at = ?1 WHERE id = 1",
			params![ts(expires)],
		)?;
		tx.commit()?;
		Ok(())
	}

	/// Who holds the lease, and until when.
	pub fn lease(&self) -> StoreResult<Option<(String, i64, DateTime<Utc>)>> {
		let row: Option<(String, i64, String)> = self
			.conn()
			.query_row(
				"SELECT holder, fencing, expires_at FROM lease WHERE id = 1",
				[],
				|r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
			)
			.optional()?;
		row.map(|(h, f, e)| Ok((h, f, parse_ts(&e)?))).transpose()
	}

	/// Inside a write: we still hold the lease, with our fencing token.
	fn check_lease(&self, tx: &Transaction<'_>, now: DateTime<Utc>) -> StoreResult<()> {
		let ours = *self.fencing.lock().unwrap_or_else(|e| e.into_inner());
		let Some(ours) = ours else {
			return Err(StoreError::LeaseLost);
		};
		let row: Option<(String, i64, String)> = tx
			.query_row(
				"SELECT holder, fencing, expires_at FROM lease WHERE id = 1",
				[],
				|r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
			)
			.optional()?;
		match row {
			Some((holder, fencing, until))
				if holder == self.holder && fencing == ours && parse_ts(&until)? > now =>
			{
				Ok(())
			}
			_ => Err(StoreError::LeaseLost),
		}
	}

	// ── trades ───────────────────────────────────────────────────────────

	/// Record a trade the first time it is seen, as `Detected`. Returns
	/// false if it was already known.
	pub fn insert_detected(&self, t: &NewTrade, now: DateTime<Utc>) -> StoreResult<bool> {
		let mut conn = self.conn();
		let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
		self.check_lease(&tx, now)?;
		let (state, reason) = AgentState::Detected.to_codes();
		let n = tx.execute(
			"INSERT OR IGNORE INTO trades (trade_id, chain, quote_id, asset_minor, fiat_minor, fee_minor, rate,
			   user_address, payee_commitment, state, reason, deadline_at, platform_state, created_at, updated_at, version)
			 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?14, 1)",
			params![
				t.trade_id,
				t.chain,
				t.quote_id,
				t.asset_minor.to_string(),
				t.fiat_minor.to_string(),
				t.fee_minor.to_string(),
				t.rate,
				t.user_address,
				t.payee_commitment.to_ascii_lowercase(),
				state,
				reason,
				ts(t.deadline_at),
				t.platform_state.as_str(),
				ts(now),
			],
		)?;
		if n == 1 {
			audit::append(
				&tx,
				"agent",
				"detected",
				Some(&t.trade_id),
				&format!(
					"{} kobo for {} minor units on {}, quote {}",
					t.fiat_minor, t.asset_minor, t.chain, t.quote_id
				),
				now,
			)?;
		}
		tx.commit()?;
		drop(conn);
		self.mirror();
		Ok(n == 1)
	}

	pub fn get(&self, trade_id: &str) -> StoreResult<Option<TradeRecord>> {
		let conn = self.conn();
		let mut stmt = conn.prepare(&format!("{SELECT_TRADE} WHERE trade_id = ?1"))?;
		let row = stmt.query_row([trade_id], raw_trade).optional()?;
		row.map(TryInto::try_into).transpose()
	}

	/// Every trade not yet settled or refunded.
	pub fn active(&self) -> StoreResult<Vec<TradeRecord>> {
		let conn = self.conn();
		let mut stmt = conn.prepare(&format!(
			"{SELECT_TRADE} WHERE state NOT IN ('settled', 'refunded') ORDER BY created_at"
		))?;
		let rows = stmt
			.query_map([], raw_trade)?
			.collect::<Result<Vec<_>, _>>()?;
		rows.into_iter().map(TryInto::try_into).collect()
	}

	/// The most recent trades, newest first.
	pub fn recent_trades(&self, limit: u32) -> StoreResult<Vec<TradeRecord>> {
		let conn = self.conn();
		let mut stmt =
			conn.prepare(&format!("{SELECT_TRADE} ORDER BY created_at DESC LIMIT ?1"))?;
		let rows = stmt
			.query_map([limit], raw_trade)?
			.collect::<Result<Vec<_>, _>>()?;
		rows.into_iter().map(TryInto::try_into).collect()
	}

	pub fn all(&self) -> StoreResult<Vec<TradeRecord>> {
		let conn = self.conn();
		let mut stmt = conn.prepare(&format!("{SELECT_TRADE} ORDER BY created_at"))?;
		let rows = stmt
			.query_map([], raw_trade)?
			.collect::<Result<Vec<_>, _>>()?;
		rows.into_iter().map(TryInto::try_into).collect()
	}

	/// Write one transition: the new state, the patch, and the transition's
	/// storage effects, in one transaction guarded by `record.version` and
	/// the lease.
	pub fn transition(
		&self,
		record: &TradeRecord,
		next: &AgentState,
		effects: &[Effect],
		patch: &Patch,
		now: DateTime<Utc>,
	) -> StoreResult<TradeRecord> {
		let mut conn = self.conn();
		let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
		self.check_lease(&tx, now)?;
		let key = record.idempotency_key();
		let mut attempt = record.attempt;
		let mut bank_reference = record.bank_reference.clone();
		let mut overrides = record.overrides.clone();

		for effect in effects {
			match effect {
				Effect::SetBankReference(r) => bank_reference = Some(r.clone()),
				Effect::NewAttempt => attempt += 1,
				Effect::Override(r) => {
					if !overrides.contains(r) {
						overrides.push(*r);
					}
				}
				Effect::ReleaseReservation => {
					tx.execute(
						"UPDATE payouts SET status = 'cancelled', updated_at = ?2
						 WHERE idempotency_key = ?1 AND status IN ('recorded', 'sending')",
						params![key, ts(now)],
					)?;
				}
				Effect::Alert { .. } | Effect::AddDisputeEvidence | Effect::PauseOffers => {}
			}
		}

		// The payout row follows the money.
		let payout_status = match (&record.state, next) {
			(AgentState::PayoutIntentRecorded, AgentState::Paying) => Some("sending"),
			// The bank took it (and, for a real bank, debited the account).
			(AgentState::Paying, AgentState::Paying)
				if effects
					.iter()
					.any(|e| matches!(e, Effect::SetBankReference(_))) =>
			{
				Some("accepted")
			}
			(AgentState::Paying, AgentState::Paid)
			| (
				AgentState::Reported {
					bank_confirmed: false,
				},
				AgentState::Reported {
					bank_confirmed: true,
				},
			) => Some("succeeded"),
			(AgentState::Paying, AgentState::PayoutIntentRecorded) => Some("recorded"),
			(_, AgentState::NeedsPerson(_)) if record.state == AgentState::Paying => {
				Some("unknown")
			}
			_ => None,
		};
		if let Some(status) = payout_status {
			tx.execute(
				"UPDATE payouts SET status = ?2, bank_reference = COALESCE(?3, bank_reference), updated_at = ?4
				 WHERE idempotency_key = ?1",
				params![key, status, bank_reference, ts(now)],
			)?;
		}

		let (state, reason) = next.to_codes();
		let overrides_text = overrides
			.iter()
			.map(|r| r.code())
			.collect::<Vec<_>>()
			.join(",");
		let n = tx.execute(
			"UPDATE trades SET state = ?2, reason = ?3, attempt = ?4, bank_reference = ?5, overrides = ?6,
			   platform_state = COALESCE(?7, platform_state), deadline_at = COALESCE(?8, deadline_at),
			   lock_protocol_fee = COALESCE(?9, lock_protocol_fee), payee_tag = COALESCE(?10, payee_tag),
			   account_masked = COALESCE(?11, account_masked), updated_at = ?12, version = version + 1
			 WHERE trade_id = ?1 AND version = ?13",
			params![
				record.trade_id,
				state,
				reason,
				attempt,
				bank_reference,
				overrides_text,
				patch.platform_state.map(|p| p.as_str()),
				patch.deadline_at.map(ts),
				patch.lock_protocol_fee,
				patch.payee_tag,
				patch.account_masked,
				ts(now),
				record.version,
			],
		)?;
		if n != 1 {
			return Err(StoreError::Conflict);
		}
		let platform_moved = patch
			.platform_state
			.is_some_and(|p| p != record.platform_state);
		if *next != record.state || !effects.is_empty() || platform_moved {
			let mut detail = format!("from {}", record.state.to_codes().0);
			if let Some(p) = patch.platform_state.filter(|_| platform_moved) {
				detail.push_str(&format!("; RecoilPay says {}", p.as_str()));
			}
			for e in effects {
				match e {
					Effect::SetBankReference(r) => {
						detail.push_str(&format!("; bank reference {r}"))
					}
					Effect::NewAttempt => detail.push_str(&format!("; new attempt {attempt}")),
					Effect::Override(r) => detail.push_str(&format!("; waived {}", r.code())),
					Effect::ReleaseReservation => detail.push_str("; reservation released"),
					Effect::PauseOffers => detail.push_str("; pausing offers"),
					Effect::Alert { urgent: true } => detail.push_str("; urgent alert"),
					Effect::Alert { urgent: false } => detail.push_str("; alert"),
					Effect::AddDisputeEvidence => detail.push_str("; dispute evidence"),
				}
			}
			let action = match reason {
				Some(r) => format!("{state}:{r}"),
				None => state.to_string(),
			};
			audit::append(
				&tx,
				patch.actor.as_deref().unwrap_or("agent"),
				&action,
				Some(&record.trade_id),
				&detail,
				now,
			)?;
		}
		tx.commit()?;
		drop(conn);
		self.mirror();
		self.get(&record.trade_id)?.ok_or(StoreError::NotFound)
	}

	/// The write-ahead intent (invariant 1): in one transaction, run
	/// `reserve` (the limit checks, against the payouts already written),
	/// then write the payout row with its idempotency key and move the trade
	/// to `PayoutIntentRecorded`. If `reserve` refuses, nothing is written
	/// and its reason comes back.
	pub fn record_intent<F>(
		&self,
		record: &TradeRecord,
		intent: &Intent,
		now: DateTime<Utc>,
		reserve: F,
	) -> StoreResult<Result<TradeRecord, HoldReason>>
	where
		F: FnOnce(&Transaction<'_>) -> StoreResult<Result<(), HoldReason>>,
	{
		let mut conn = self.conn();
		let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
		self.check_lease(&tx, now)?;
		if paused(&tx)? {
			return Ok(Err(HoldReason::Paused));
		}
		if let Err(r) = reserve(&tx)? {
			return Ok(Err(r));
		}
		tx.execute(
			"INSERT INTO payouts (idempotency_key, trade_id, attempt, amount_minor, payee_tag, day, status, created_at, updated_at)
			 VALUES (?1, ?2, ?3, ?4, ?5, ?6, 'recorded', ?7, ?7)",
			params![
				record.idempotency_key(),
				record.trade_id,
				record.attempt,
				intent.amount.to_string(),
				intent.payee_tag,
				intent.day,
				ts(now),
			],
		)?;
		let (state, reason) = AgentState::PayoutIntentRecorded.to_codes();
		let n = tx.execute(
			"UPDATE trades SET state = ?2, reason = ?3, updated_at = ?4, version = version + 1
			 WHERE trade_id = ?1 AND version = ?5 AND state = 'checks_passed'",
			params![record.trade_id, state, reason, ts(now), record.version],
		)?;
		if n != 1 {
			return Err(StoreError::Conflict);
		}
		audit::append(
			&tx,
			"agent",
			"payout_intent_recorded",
			Some(&record.trade_id),
			&format!(
				"key {} for {} kobo; limits and float checked",
				record.idempotency_key(),
				intent.amount
			),
			now,
		)?;
		tx.commit()?;
		drop(conn);
		self.mirror();
		Ok(Ok(self
			.get(&record.trade_id)?
			.ok_or(StoreError::NotFound)?))
	}

	/// Every payout attempt not cancelled, for the limits.
	pub fn payouts(&self) -> StoreResult<Vec<PayoutRow>> {
		let conn = self.conn();
		let mut stmt = conn.prepare(
			"SELECT idempotency_key, trade_id, amount_minor, payee_tag, day, status, created_at FROM payouts ORDER BY created_at",
		)?;
		let rows = stmt
			.query_map([], |r| {
				Ok((
					r.get::<_, String>(0)?,
					r.get::<_, String>(1)?,
					r.get::<_, String>(2)?,
					r.get::<_, String>(3)?,
					r.get::<_, String>(4)?,
					r.get::<_, String>(5)?,
					r.get::<_, String>(6)?,
				))
			})?
			.collect::<Result<Vec<_>, _>>()?;
		rows.into_iter()
			.map(|(k, t, a, p, d, s, c)| {
				Ok(PayoutRow {
					idempotency_key: k,
					trade_id: t,
					amount: parse_minor(&a)?,
					payee_tag: p,
					day: d,
					status: s,
					created_at: parse_ts(&c)?,
				})
			})
			.collect()
	}

	// ── control ──────────────────────────────────────────────────────────

	/// Pause or resume. `last_book` is the quote book withdrawn by the
	/// pause, kept so a resume can republish it; `None` leaves it as it is.
	pub fn set_paused(
		&self,
		paused: bool,
		by: &str,
		auto: bool,
		last_book: Option<&str>,
		now: DateTime<Utc>,
	) -> StoreResult<()> {
		let mut conn = self.conn();
		let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
		self.check_lease(&tx, now)?;
		let was = self::paused(&tx)?;
		tx.execute(
			// Pausing while already paused keeps who paused and whether it was
			// automatic: a manual pause must not turn an automatic one (which
			// only an approver may lift) into one an operator can resume. It
			// may still save a real book (the CLI saves it in a second write),
			// but never an empty one over the book saved when payouts paused.
			"UPDATE control SET
			   paused = ?1,
			   paused_by = CASE WHEN paused = 1 AND ?1 = 1 THEN paused_by ELSE ?2 END,
			   paused_auto = CASE WHEN paused = 1 AND ?1 = 1 THEN paused_auto ELSE ?3 END,
			   last_book = CASE WHEN paused = 1 AND ?1 = 1 AND (?4 IS NULL OR ?4 = '[]')
			                    THEN last_book ELSE COALESCE(?4, last_book) END
			 WHERE id = 1",
			params![paused, by, auto, last_book],
		)?;
		// Audited when the switch moves, not when the saved book is updated.
		if was != paused {
			audit::append(
				&tx,
				by,
				if paused { "paused" } else { "resumed" },
				None,
				if auto { "automatically" } else { "by hand" },
				now,
			)?;
		}
		tx.commit()?;
		drop(conn);
		self.mirror();
		Ok(())
	}

	pub fn is_paused(&self) -> StoreResult<bool> {
		let conn = self.conn();
		let tx = conn.unchecked_transaction()?;
		paused(&tx)
	}

	/// `(paused, by, automatic, last book)`.
	pub fn control(&self) -> StoreResult<(bool, Option<String>, bool, Option<String>)> {
		Ok(self.conn().query_row(
			"SELECT paused, paused_by, paused_auto, last_book FROM control WHERE id = 1",
			[],
			|r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
		)?)
	}

	// ── control writes: no lease, no money ───────────────────────────────

	/// Set the pause flag. Pausing stops new payout intents at once (the
	/// intent transaction reads it); it never touches a payout in flight.
	pub fn set_paused_control(
		&self,
		paused: bool,
		by: &str,
		auto: bool,
		last_book: Option<&str>,
	) -> StoreResult<()> {
		{
			let mut conn = self.conn();
			let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
			let was = self::paused(&tx)?;
			tx.execute(
				// As in set_paused: a pause over a pause keeps who paused and
				// whether it was automatic, and never saves an empty book.
				"UPDATE control SET
				   paused = ?1,
				   paused_by = CASE WHEN paused = 1 AND ?1 = 1 THEN paused_by ELSE ?2 END,
				   paused_auto = CASE WHEN paused = 1 AND ?1 = 1 THEN paused_auto ELSE ?3 END,
				   last_book = CASE WHEN paused = 1 AND ?1 = 1 AND (?4 IS NULL OR ?4 = '[]')
				                    THEN last_book ELSE COALESCE(?4, last_book) END
				 WHERE id = 1",
				params![paused, by, auto, last_book],
			)?;
			if was != paused {
				let action = if paused { "paused" } else { "resumed" };
				audit::append(
					&tx,
					by,
					action,
					None,
					if auto { "automatically" } else { "by hand" },
					Utc::now(),
				)?;
			}
			tx.commit()?;
		}
		self.mirror();
		Ok(())
	}

	pub fn insert_user(&self, user: &UserRow, now: DateTime<Utc>) -> StoreResult<bool> {
		let mut conn = self.conn();
		let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
		let n = tx.execute(
			"INSERT OR IGNORE INTO users (username, role, password_hash, totp_sealed, created_at)
			 VALUES (?1, ?2, ?3, ?4, ?5)",
			params![
				user.username,
				user.role,
				user.password_hash,
				user.totp_sealed,
				ts(now)
			],
		)?;
		if n == 1 {
			audit::append(
				&tx,
				"cli",
				"user_added",
				None,
				&format!("{} as {}", user.username, user.role),
				now,
			)?;
		}
		tx.commit()?;
		drop(conn);
		self.mirror();
		Ok(n == 1)
	}

	pub fn user(&self, username: &str) -> StoreResult<Option<UserRow>> {
		let conn = self.conn();
		let mut stmt = conn.prepare(&format!("{SELECT_USER} WHERE username = ?1"))?;
		stmt.query_row([username], raw_user)
			.optional()?
			.map(TryInto::try_into)
			.transpose()
	}

	pub fn users(&self) -> StoreResult<Vec<UserRow>> {
		let conn = self.conn();
		let mut stmt = conn.prepare(&format!("{SELECT_USER} ORDER BY username"))?;
		let rows = stmt
			.query_map([], raw_user)?
			.collect::<Result<Vec<_>, _>>()?;
		rows.into_iter().map(TryInto::try_into).collect()
	}

	/// After a login attempt: the new failure count, lockout and last
	/// accepted TOTP step.
	pub fn update_login_state(
		&self,
		username: &str,
		failed_attempts: u32,
		locked_until: Option<DateTime<Utc>>,
		last_totp_step: Option<u64>,
	) -> StoreResult<()> {
		{
			let mut conn = self.conn();
			let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
			tx.execute(
				"UPDATE users SET failed_attempts = ?2, locked_until = ?3, last_totp_step = COALESCE(?4, last_totp_step)
				 WHERE username = ?1",
				params![username, failed_attempts, locked_until.map(ts), last_totp_step.map(|s| s as i64)],
			)?;
			if let Some(until) = locked_until {
				audit::append(
					&tx,
					username,
					"console_locked",
					None,
					&format!("until {}", ts(until)),
					Utc::now(),
				)?;
			} else if last_totp_step.is_some() {
				audit::append(&tx, username, "console_signin", None, "", Utc::now())?;
			}
			tx.commit()?;
		}
		self.mirror();
		Ok(())
	}

	/// Count one failed sign-in, atomically. Concurrent attempts each count,
	/// and a lock already in force is never cleared by a failure. Returns
	/// when the account is locked until, if it is (now or already).
	pub fn record_login_failure(
		&self,
		username: &str,
		max_failures: u32,
		lockout: chrono::Duration,
		now: DateTime<Utc>,
	) -> StoreResult<Option<DateTime<Utc>>> {
		let locked = {
			let mut conn = self.conn();
			let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
			let row: Option<(u32, Option<String>)> = tx
				.query_row(
					"SELECT failed_attempts, locked_until FROM users WHERE username = ?1",
					[username],
					|r| Ok((r.get(0)?, r.get(1)?)),
				)
				.optional()?;
			let Some((failed, locked_until)) = row else {
				return Ok(None);
			};
			let locked_until = locked_until.as_deref().map(parse_ts).transpose()?;
			if let Some(until) = locked_until.filter(|u| *u > now) {
				tx.commit()?;
				return Ok(Some(until));
			}
			let failures = failed.saturating_add(1);
			let locked = if failures >= max_failures {
				let until = now + lockout;
				tx.execute(
					"UPDATE users SET failed_attempts = 0, locked_until = ?2 WHERE username = ?1",
					params![username, ts(until)],
				)?;
				audit::append(
					&tx,
					username,
					"console_locked",
					None,
					&format!("until {}", ts(until)),
					now,
				)?;
				Some(until)
			} else {
				tx.execute(
					"UPDATE users SET failed_attempts = ?2, locked_until = NULL WHERE username = ?1",
					params![username, failures],
				)?;
				None
			};
			tx.commit()?;
			locked
		};
		self.mirror();
		Ok(locked)
	}

	/// Record a successful sign-in with TOTP step `step`, atomically: only
	/// if the step is later than the last one used and the account is not
	/// locked. `false` means the code was already used (or the account
	/// locked) by a concurrent sign-in, so this one must fail.
	pub fn record_login_success(
		&self,
		username: &str,
		step: u64,
		now: DateTime<Utc>,
	) -> StoreResult<bool> {
		let ok = {
			let mut conn = self.conn();
			let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
			let row: Option<(i64, Option<String>)> = tx
				.query_row(
					"SELECT last_totp_step, locked_until FROM users WHERE username = ?1",
					[username],
					|r| Ok((r.get(0)?, r.get(1)?)),
				)
				.optional()?;
			let Some((last, locked_until)) = row else {
				return Ok(false);
			};
			let locked_until = locked_until.as_deref().map(parse_ts).transpose()?;
			let fresh = u64::try_from(last).map_or(true, |last| step > last);
			let unlocked = locked_until.is_none_or(|u| u <= now);
			let ok = fresh && unlocked;
			if ok {
				tx.execute(
					"UPDATE users SET failed_attempts = 0, locked_until = NULL, last_totp_step = ?2 WHERE username = ?1",
					params![username, step as i64],
				)?;
				audit::append(&tx, username, "console_signin", None, "", now)?;
			}
			tx.commit()?;
			ok
		};
		self.mirror();
		Ok(ok)
	}

	pub fn set_user_disabled(&self, username: &str, disabled: bool, by: &str) -> StoreResult<bool> {
		let n = {
			let mut conn = self.conn();
			let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
			let n = tx.execute(
				"UPDATE users SET disabled = ?2 WHERE username = ?1",
				params![username, disabled],
			)?;
			if n == 1 {
				let action = if disabled {
					"user_disabled"
				} else {
					"user_enabled"
				};
				audit::append(&tx, by, action, None, username, Utc::now())?;
			}
			tx.commit()?;
			n
		};
		self.mirror();
		Ok(n == 1)
	}

	pub fn remove_user(&self, username: &str, by: &str) -> StoreResult<bool> {
		let n = {
			let mut conn = self.conn();
			let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
			let n = tx.execute("DELETE FROM users WHERE username = ?1", [username])?;
			if n == 1 {
				audit::append(&tx, by, "user_removed", None, username, Utc::now())?;
			}
			tx.commit()?;
			n
		};
		self.mirror();
		Ok(n == 1)
	}

	/// The approvals and reveals people made, newest first, for the console.
	pub fn recent_decisions(&self, limit: u32) -> StoreResult<Vec<Decision>> {
		let conn = self.conn();
		let mut stmt = conn.prepare(
			"SELECT created_at, COALESCE(trade_id, ''), action || ' requested by ' || requested_by,
			   CASE WHEN approved_by IS NULL THEN NULL ELSE 'confirmed by ' || approved_by END
			 FROM approvals
			 UNION ALL
			 SELECT COALESCE(last_revealed_at, created_at), trade_id,
			   'bank details revealed ' || reveals || ' time(s), last by ' || COALESCE(last_revealed_by, '-'),
			   CASE status WHEN 'open' THEN NULL ELSE status || ' by ' || COALESCE(closed_by, '-') END
			 FROM manual_tasks
			 ORDER BY 1 DESC LIMIT ?1",
		)?;
		let rows = stmt
			.query_map([limit], |r| {
				Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?))
			})?
			.collect::<Result<Vec<_>, _>>()?;
		Ok(rows)
	}

	// ── quote history ────────────────────────────────────────────────────

	/// Record every quote not seen before, with the merchant's reference
	/// rate for its rail at this moment. A quote id is recorded once: its
	/// first-seen terms are what the agent prices trades on it with, so a
	/// later answer can't change them.
	pub fn record_quotes(
		&self,
		quotes: &[crate::aggregator::Quote],
		reference_rate: impl Fn(&crate::aggregator::Quote) -> Option<String>,
		now: DateTime<Utc>,
	) -> StoreResult<usize> {
		let mut conn = self.conn();
		let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
		self.check_lease(&tx, now)?;
		let mut added = 0;
		for q in quotes {
			let n = tx.execute(
				"INSERT OR IGNORE INTO quotes_seen (quote_id, chain, asset, asset_decimals, fiat_currency, rail, rate,
				   fee_fixed, fee_bps, min_fiat, max_fiat, reference_rate, first_seen)
				 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13)",
				params![
					q.id,
					q.chain,
					q.asset,
					q.asset_decimals,
					q.fiat_currency,
					q.rail,
					q.rate.as_str(),
					q.fee_fixed.to_string(),
					q.fee_bps,
					q.min_fiat.to_string(),
					q.max_fiat.to_string(),
					reference_rate(q),
					ts(now),
				],
			)?;
			if n == 1 {
				audit::append(
					&tx,
					"agent",
					"quote_recorded",
					None,
					&format!(
						"{} on {} {}: rate {}, fee {} + {} bps, {}..{} kobo, reference {}",
						q.id,
						q.chain,
						q.rail,
						q.rate.as_str(),
						q.fee_fixed,
						q.fee_bps,
						q.min_fiat,
						q.max_fiat,
						reference_rate(q).unwrap_or_else(|| "none".into())
					),
					now,
				)?;
			}
			added += n;
		}
		tx.commit()?;
		drop(conn);
		self.mirror();
		Ok(added)
	}

	/// The terms of a quote the agent has seen, as first seen.
	pub fn quote(&self, quote_id: &str) -> StoreResult<Option<crate::policy::pricing::QuoteTerms>> {
		type Row = (
			String,
			String,
			String,
			i64,
			String,
			String,
			String,
			String,
			i64,
			String,
			String,
			Option<String>,
		);
		let row: Option<Row> = self
			.conn()
			.query_row(
				"SELECT quote_id, chain, asset, asset_decimals, fiat_currency, rail, rate, fee_fixed, fee_bps,
				   min_fiat, max_fiat, reference_rate FROM quotes_seen WHERE quote_id = ?1",
				[quote_id],
				|r| {
					Ok((
						r.get(0)?,
						r.get(1)?,
						r.get(2)?,
						r.get(3)?,
						r.get(4)?,
						r.get(5)?,
						r.get(6)?,
						r.get(7)?,
						r.get(8)?,
						r.get(9)?,
						r.get(10)?,
						r.get(11)?,
					))
				},
			)
			.optional()?;
		row.map(
			|(id, chain, asset, dec, cur, rail, rate, fixed, bps, min, max, reference)| {
				Ok(crate::policy::pricing::QuoteTerms {
					quote_id: id,
					chain,
					asset,
					asset_decimals: u8::try_from(dec)
						.map_err(|_| StoreError::Corrupt("asset_decimals".into()))?,
					fiat_currency: cur,
					rail,
					rate,
					fee_fixed: parse_minor(&fixed)?,
					fee_bps: u16::try_from(bps)
						.map_err(|_| StoreError::Corrupt("fee_bps".into()))?,
					min_fiat: parse_minor(&min)?,
					max_fiat: parse_minor(&max)?,
					reference_rate: reference,
				})
			},
		)
		.transpose()
	}

	// ── two-person approvals ─────────────────────────────────────────────

	/// One person's approval of `action` on a trade. The first person's
	/// request is recorded; a **different** person confirms it.
	pub fn approve(
		&self,
		trade_id: &str,
		action: &str,
		by: &str,
		now: DateTime<Utc>,
	) -> StoreResult<ApprovalStep> {
		let mut conn = self.conn();
		let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
		self.check_lease(&tx, now)?;
		let pending: Option<(i64, String)> = tx
			.query_row(
				"SELECT id, requested_by FROM approvals
				 WHERE trade_id = ?1 AND action = ?2 AND approved_by IS NULL ORDER BY id DESC LIMIT 1",
				params![trade_id, action],
				|r| Ok((r.get(0)?, r.get(1)?)),
			)
			.optional()?;
		let step = match pending {
			Some((_, first)) if first == by => ApprovalStep::AlreadyRequested,
			Some((id, first)) => {
				tx.execute(
					"UPDATE approvals SET approved_by = ?2, decided_at = ?3 WHERE id = ?1",
					params![id, by, ts(now)],
				)?;
				ApprovalStep::Confirmed { first }
			}
			None => {
				tx.execute(
					"INSERT INTO approvals (trade_id, action, requested_by, created_at) VALUES (?1, ?2, ?3, ?4)",
					params![trade_id, action, by, ts(now)],
				)?;
				ApprovalStep::Requested
			}
		};
		let what = match &step {
			ApprovalStep::Requested => format!("{action}_requested"),
			ApprovalStep::AlreadyRequested => format!("{action}_repeated"),
			ApprovalStep::Confirmed { .. } => format!("{action}_confirmed"),
		};
		let detail = match &step {
			ApprovalStep::Confirmed { first } => format!("second approver; first was {first}"),
			_ => String::new(),
		};
		audit::append(&tx, by, &what, Some(trade_id), &detail, now)?;
		tx.commit()?;
		drop(conn);
		self.mirror();
		Ok(step)
	}

	// ── manual rail ──────────────────────────────────────────────────────

	/// Open a task for a person to pay. Idempotent by key.
	pub fn open_manual_task(
		&self,
		key: &str,
		trade_id: &str,
		amount: Minor,
		now: DateTime<Utc>,
	) -> StoreResult<()> {
		let mut conn = self.conn();
		let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
		self.check_lease(&tx, now)?;
		let n = tx.execute(
			"INSERT OR IGNORE INTO manual_tasks (idempotency_key, trade_id, amount_minor, status, created_at, updated_at)
			 VALUES (?1, ?2, ?3, 'open', ?4, ?4)",
			params![key, trade_id, amount.to_string(), ts(now)],
		)?;
		if n == 1 {
			audit::append(
				&tx,
				"agent",
				"manual_task_opened",
				Some(trade_id),
				&format!("{amount} kobo, key {key}"),
				now,
			)?;
		}
		tx.commit()?;
		drop(conn);
		self.mirror();
		Ok(())
	}

	pub fn manual_task(&self, key: &str) -> StoreResult<Option<ManualTask>> {
		let conn = self.conn();
		let mut stmt = conn.prepare(&format!("{SELECT_TASK} WHERE idempotency_key = ?1"))?;
		stmt.query_row([key], raw_task)
			.optional()?
			.map(TryInto::try_into)
			.transpose()
	}

	/// The open task for a trade, if any.
	pub fn open_task_for(&self, trade_id: &str) -> StoreResult<Option<ManualTask>> {
		let conn = self.conn();
		let mut stmt = conn.prepare(&format!(
			"{SELECT_TASK} WHERE trade_id = ?1 AND status = 'open'"
		))?;
		stmt.query_row([trade_id], raw_task)
			.optional()?
			.map(TryInto::try_into)
			.transpose()
	}

	pub fn open_manual_tasks(&self) -> StoreResult<Vec<ManualTask>> {
		let conn = self.conn();
		let mut stmt = conn.prepare(&format!(
			"{SELECT_TASK} WHERE status = 'open' ORDER BY created_at"
		))?;
		let rows = stmt
			.query_map([], raw_task)?
			.collect::<Result<Vec<_>, _>>()?;
		rows.into_iter().map(TryInto::try_into).collect()
	}

	/// Count a reveal of an open task's bank details.
	pub fn note_reveal(&self, key: &str, by: &str, now: DateTime<Utc>) -> StoreResult<ManualTask> {
		let mut conn = self.conn();
		let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
		self.check_lease(&tx, now)?;
		let n = tx.execute(
			"UPDATE manual_tasks SET reveals = reveals + 1, last_revealed_by = ?2, last_revealed_at = ?3, updated_at = ?3
			 WHERE idempotency_key = ?1 AND status = 'open'",
			params![key, by, ts(now)],
		)?;
		if n != 1 {
			return Err(StoreError::NotFound);
		}
		let trade: String = tx.query_row(
			"SELECT trade_id FROM manual_tasks WHERE idempotency_key = ?1",
			[key],
			|r| r.get(0),
		)?;
		audit::append(&tx, by, "bank_details_revealed", Some(&trade), "", now)?;
		tx.commit()?;
		drop(conn);
		self.mirror();
		self.manual_task(key)?.ok_or(StoreError::NotFound)
	}

	/// A person paid the task (with the bank's reference) or could not.
	pub fn close_manual_task(
		&self,
		key: &str,
		paid_reference: Option<&str>,
		by: &str,
		now: DateTime<Utc>,
	) -> StoreResult<()> {
		let mut conn = self.conn();
		let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
		self.check_lease(&tx, now)?;
		let status = if paid_reference.is_some() {
			"paid"
		} else {
			"failed"
		};
		let n = tx.execute(
			"UPDATE manual_tasks SET status = ?2, reference = ?3, closed_by = ?4, updated_at = ?5
			 WHERE idempotency_key = ?1 AND status = 'open'",
			params![key, status, paid_reference, by, ts(now)],
		)?;
		if n != 1 {
			return Err(StoreError::NotFound);
		}
		let trade: String = tx.query_row(
			"SELECT trade_id FROM manual_tasks WHERE idempotency_key = ?1",
			[key],
			|r| r.get(0),
		)?;
		let detail = paid_reference
			.map(|r| format!("bank reference {r}"))
			.unwrap_or_default();
		audit::append(
			&tx,
			by,
			&format!("manual_{status}"),
			Some(&trade),
			&detail,
			now,
		)?;
		tx.commit()?;
		drop(conn);
		self.mirror();
		Ok(())
	}
}

/// One decision a person made: `(when, trade, what, outcome)`.
pub type Decision = (String, String, String, Option<String>);

/// A console user as stored.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UserRow {
	pub username: String,
	pub role: String,
	/// argon2id, PHC string.
	pub password_hash: String,
	/// The TOTP secret, encrypted (see `console::auth`).
	pub totp_sealed: String,
	pub last_totp_step: u64,
	pub failed_attempts: u32,
	pub locked_until: Option<DateTime<Utc>>,
	pub disabled: bool,
}

const SELECT_USER: &str =
	"SELECT username, role, password_hash, totp_sealed, last_totp_step, failed_attempts,
	locked_until, disabled FROM users";

type RawUser = (
	String,
	String,
	String,
	String,
	i64,
	i64,
	Option<String>,
	bool,
);

fn raw_user(r: &rusqlite::Row<'_>) -> rusqlite::Result<RawUser> {
	Ok((
		r.get(0)?,
		r.get(1)?,
		r.get(2)?,
		r.get(3)?,
		r.get(4)?,
		r.get(5)?,
		r.get(6)?,
		r.get(7)?,
	))
}

impl TryFrom<RawUser> for UserRow {
	type Error = StoreError;
	fn try_from(
		(username, role, hash, totp, step, failed, locked, disabled): RawUser,
	) -> StoreResult<Self> {
		Ok(Self {
			username,
			role,
			password_hash: hash,
			totp_sealed: totp,
			last_totp_step: step.max(0) as u64,
			failed_attempts: failed.max(0) as u32,
			locked_until: locked.as_deref().map(parse_ts).transpose()?,
			disabled,
		})
	}
}

/// Where a two-person approval stands.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ApprovalStep {
	/// Recorded; a second, different person must confirm.
	Requested,
	/// This person already asked; it needs someone else.
	AlreadyRequested,
	/// A second person confirmed `first`'s request.
	Confirmed { first: String },
}

/// A payout a person makes by hand (the manual rail).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ManualTask {
	pub idempotency_key: String,
	pub trade_id: String,
	pub amount: Minor,
	/// `open`, `paid` or `failed`.
	pub status: String,
	pub reference: Option<String>,
	pub reveals: u32,
	pub last_revealed_by: Option<String>,
	pub created_at: DateTime<Utc>,
}

const SELECT_TASK: &str =
	"SELECT idempotency_key, trade_id, amount_minor, status, reference, reveals,
	last_revealed_by, created_at FROM manual_tasks";

type RawTask = (
	String,
	String,
	String,
	String,
	Option<String>,
	i64,
	Option<String>,
	String,
);

fn raw_task(r: &rusqlite::Row<'_>) -> rusqlite::Result<RawTask> {
	Ok((
		r.get(0)?,
		r.get(1)?,
		r.get(2)?,
		r.get(3)?,
		r.get(4)?,
		r.get(5)?,
		r.get(6)?,
		r.get(7)?,
	))
}

impl TryFrom<RawTask> for ManualTask {
	type Error = StoreError;
	fn try_from(
		(key, trade, amount, status, reference, reveals, by, created): RawTask,
	) -> StoreResult<Self> {
		Ok(Self {
			idempotency_key: key,
			trade_id: trade,
			amount: parse_minor(&amount)?,
			status,
			reference,
			reveals: reveals as u32,
			last_revealed_by: by,
			created_at: parse_ts(&created)?,
		})
	}
}

/// Money reserved by payouts written ahead but not yet taken by the bank:
/// a balance read now does not reflect them yet.
pub fn reserved_unsent(tx: &Transaction<'_>) -> StoreResult<u128> {
	let mut stmt =
		tx.prepare("SELECT amount_minor FROM payouts WHERE status IN ('recorded', 'sending')")?;
	let amounts = stmt
		.query_map([], |r| r.get::<_, String>(0))?
		.collect::<Result<Vec<_>, _>>()?;
	amounts.iter().try_fold(
		0u128,
		|sum, a| Ok(sum.saturating_add(parse_minor(a)?.get())),
	)
}

fn paused(tx: &Transaction<'_>) -> StoreResult<bool> {
	Ok(
		tx.query_row("SELECT paused FROM control WHERE id = 1", [], |r| {
			r.get::<_, bool>(0)
		})?,
	)
}

/// Bring the schema up to date, forward only. A database that already has
/// a schema is backed up first, beside it, as `agent.db.backup-v<old>-<unix
/// time>`: a consistent copy (`VACUUM INTO`), taken before any migration
/// touches it. Migrations never run backwards; to undo one, stop the agent
/// and restore the backup.
fn migrate(conn: &mut Connection, path: &Path) -> StoreResult<()> {
	let found: i64 = conn.query_row("PRAGMA user_version", [], |r| r.get(0))?;
	let found = found as usize;
	if found > MIGRATIONS.len() {
		return Err(StoreError::TooNew {
			found,
			known: MIGRATIONS.len(),
		});
	}
	if found > 0 && found < MIGRATIONS.len() {
		let backup = path.with_file_name(format!(
			"{DB_FILE}.backup-v{found}-{}",
			Utc::now().timestamp()
		));
		conn.execute("VACUUM INTO ?1", [backup.to_string_lossy().as_ref()])?;
		tracing::warn!(
			from = found,
			to = MIGRATIONS.len(),
			backup = %backup.display(),
			"database backed up; migrating"
		);
	}
	for (i, sql) in MIGRATIONS.iter().enumerate().skip(found) {
		let tx = conn.transaction_with_behavior(TransactionBehavior::Exclusive)?;
		tx.execute_batch(sql)?;
		tx.pragma_update(None, "user_version", (i + 1) as i64)?;
		tx.commit()?;
	}
	Ok(())
}

const SELECT_TRADE: &str = "SELECT trade_id, chain, quote_id, asset_minor, fiat_minor, fee_minor, rate, user_address,
	payee_commitment, payee_tag, account_masked, state, reason, attempt, bank_reference, lock_protocol_fee, overrides,
	deadline_at, platform_state, created_at, updated_at, version FROM trades";

/// A row as SQLite hands it over, before checking.
struct RawTrade {
	cols: [Option<String>; 21],
	version: i64,
}

fn raw_trade(r: &rusqlite::Row<'_>) -> rusqlite::Result<RawTrade> {
	let mut cols: [Option<String>; 21] = Default::default();
	for (i, col) in cols.iter_mut().enumerate() {
		*col = if i == 13 {
			Some(r.get::<_, i64>(i)?.to_string())
		} else {
			r.get(i)?
		};
	}
	Ok(RawTrade {
		cols,
		version: r.get(21)?,
	})
}

impl TryFrom<RawTrade> for TradeRecord {
	type Error = StoreError;

	fn try_from(raw: RawTrade) -> StoreResult<Self> {
		let [trade_id, chain, quote_id, asset, fiat, fee, rate, user, commitment, tag, masked, state, reason, attempt, bank_ref, lock_fee, overrides, deadline, platform, created, updated] =
			raw.cols;
		let need = |v: Option<String>, what: &str| {
			v.ok_or_else(|| StoreError::Corrupt(format!("{what} is empty")))
		};
		let state_code = need(state, "state")?;
		let state = AgentState::from_codes(&state_code, reason.as_deref())
			.ok_or_else(|| StoreError::Corrupt(format!("state {state_code}")))?;
		let overrides = need(overrides, "overrides")?
			.split(',')
			.filter(|s| !s.is_empty())
			.map(|c| {
				HoldReason::from_code(c).ok_or_else(|| StoreError::Corrupt(format!("override {c}")))
			})
			.collect::<StoreResult<Vec<_>>>()?;
		Ok(Self {
			trade_id: need(trade_id, "trade_id")?,
			chain: need(chain, "chain")?,
			quote_id: need(quote_id, "quote_id")?,
			asset_minor: parse_minor(&need(asset, "asset_minor")?)?,
			fiat_minor: parse_minor(&need(fiat, "fiat_minor")?)?,
			fee_minor: parse_minor(&need(fee, "fee_minor")?)?,
			rate: need(rate, "rate")?,
			user_address: need(user, "user_address")?,
			payee_commitment: need(commitment, "payee_commitment")?,
			payee_tag: tag,
			account_masked: masked,
			state,
			attempt: need(attempt, "attempt")?
				.parse()
				.map_err(|_| StoreError::Corrupt("attempt".into()))?,
			bank_reference: bank_ref,
			lock_protocol_fee: lock_fee,
			overrides,
			deadline_at: parse_ts(&need(deadline, "deadline_at")?)?,
			platform_state: parse_platform(&need(platform, "platform_state")?)?,
			created_at: parse_ts(&need(created, "created_at")?)?,
			updated_at: parse_ts(&need(updated, "updated_at")?)?,
			version: raw.version,
		})
	}
}

/// What `doctor` sees, read-only: never migrates, never takes the lock.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Inspection {
	/// `None` when there is no database yet.
	pub schema: Option<usize>,
	/// `(holder, expires_at)` when a lease was ever taken.
	pub lease: Option<(String, DateTime<Utc>)>,
}

pub fn inspect(data_dir: &Path) -> StoreResult<Inspection> {
	let path = data_dir.join(DB_FILE);
	if !path.exists() {
		return Ok(Inspection {
			schema: None,
			lease: None,
		});
	}
	let conn = Connection::open_with_flags(&path, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)?;
	let schema: i64 = conn.query_row("PRAGMA user_version", [], |r| r.get(0))?;
	let has_lease: bool = conn.query_row(
		"SELECT COUNT(*) > 0 FROM sqlite_master WHERE type = 'table' AND name = 'lease'",
		[],
		|r| r.get(0),
	)?;
	let lease = if has_lease {
		conn.query_row(
			"SELECT holder, expires_at FROM lease WHERE id = 1",
			[],
			|r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)),
		)
		.optional()?
		.map(|(h, e)| Ok::<_, StoreError>((h, parse_ts(&e)?)))
		.transpose()?
	} else {
		None
	};
	Ok(Inspection {
		schema: Some(schema as usize),
		lease,
	})
}

#[cfg(test)]
mod private_dir_tests {
	use std::os::unix::fs::PermissionsExt;

	#[test]
	fn the_data_directory_is_private_to_the_agent() {
		let base = tempfile::tempdir().unwrap();
		let dir = base.path().join("data");
		super::private_dir(&dir).unwrap();
		let mode = std::fs::metadata(&dir).unwrap().permissions().mode() & 0o777;
		assert_eq!(mode, 0o700);
		// An existing one this user owns is tightened.
		std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o755)).unwrap();
		super::private_dir(&dir).unwrap();
		let mode = std::fs::metadata(&dir).unwrap().permissions().mode() & 0o777;
		assert_eq!(mode, 0o700);
	}
}
