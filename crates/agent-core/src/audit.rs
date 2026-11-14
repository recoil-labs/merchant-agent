//! The tamper-evident audit log (invariant 12).
//!
//! Every decision is one entry: what happened, who did it (the agent, a
//! console user, the CLI), the trade if any, and a short detail with no
//! personal data. Each entry carries the hash of the one before it:
//!
//! ```text
//! hash = SHA-256(canonical JSON of {seq, at, actor, action, trade_id, detail, prev})
//! ```
//!
//! **Where it lives.** Entries are written to the `audit` table **in the
//! same SQLite transaction as the change they record**, so a decision and
//! its entry can never disagree after a crash. After each commit the new
//! entries are appended to `audit.jsonl` in the data directory: append-only
//! JSON lines, for the merchant's log shipping.
//!
//! **What `verify` catches.** It recomputes both chains (database and
//! file) and requires them to match line for line. An edited entry breaks
//! its hash; a deleted one breaks the next entry's `prev` and the sequence.
//! A thorough attacker with root could rewrite everything consistently;
//! shipping `audit.jsonl` to write-once storage as it grows is what closes
//! that, and the docs say so.

use std::fs::OpenOptions;
use std::io::{BufRead, Write};
use std::path::Path;

use chrono::{DateTime, Utc};
use rusqlite::{params, OptionalExtension, Transaction};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

/// The `prev` of the first entry: `SHA-256("recoilpay/merchant-agent/audit/v1")`,
/// so a chain is tied to this format and version.
pub const GENESIS: &str = "1d04ee8f88bb452337dd93259cac2ce32783f272a808dd8197c161cb30e3da72";

pub const FILE: &str = "audit.jsonl";

/// One audit entry. Field order is the canonical order.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Entry {
	pub seq: u64,
	pub at: String,
	pub actor: String,
	pub action: String,
	pub trade_id: Option<String>,
	pub detail: String,
	pub prev: String,
	pub hash: String,
}

#[derive(Serialize)]
struct Hashed<'a> {
	seq: u64,
	at: &'a str,
	actor: &'a str,
	action: &'a str,
	trade_id: Option<&'a str>,
	detail: &'a str,
	prev: &'a str,
}

impl Entry {
	/// The hash this entry should have.
	pub fn compute_hash(&self) -> String {
		let canonical = serde_json::to_string(&Hashed {
			seq: self.seq,
			at: &self.at,
			actor: &self.actor,
			action: &self.action,
			trade_id: self.trade_id.as_deref(),
			detail: &self.detail,
			prev: &self.prev,
		})
		.expect("an audit entry serialises");
		hex::encode(Sha256::digest(canonical.as_bytes()))
	}
}

/// Append an entry inside `tx`, chained to the last one.
pub fn append(
	tx: &Transaction<'_>,
	actor: &str,
	action: &str,
	trade_id: Option<&str>,
	detail: &str,
	now: DateTime<Utc>,
) -> rusqlite::Result<()> {
	let last: Option<(i64, String)> = tx
		.query_row(
			"SELECT seq, hash FROM audit ORDER BY seq DESC LIMIT 1",
			[],
			|r| Ok((r.get(0)?, r.get(1)?)),
		)
		.optional()?;
	let (seq, prev) = match last {
		Some((s, h)) => (s as u64 + 1, h),
		None => (1, GENESIS.to_string()),
	};
	let mut e = Entry {
		seq,
		at: now.to_rfc3339_opts(chrono::SecondsFormat::Micros, true),
		actor: actor.into(),
		action: action.into(),
		trade_id: trade_id.map(Into::into),
		detail: detail.into(),
		prev,
		hash: String::new(),
	};
	e.hash = e.compute_hash();
	tx.execute(
		"INSERT INTO audit (seq, at, actor, action, trade_id, detail, prev, hash) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
		params![e.seq as i64, e.at, e.actor, e.action, e.trade_id, e.detail, e.prev, e.hash],
	)?;
	Ok(())
}

pub fn entries(conn: &rusqlite::Connection, from_seq: u64) -> rusqlite::Result<Vec<Entry>> {
	let mut stmt = conn.prepare(
		"SELECT seq, at, actor, action, trade_id, detail, prev, hash FROM audit WHERE seq >= ?1 ORDER BY seq",
	)?;
	let rows = stmt.query_map([from_seq as i64], |r| {
		Ok(Entry {
			seq: r.get::<_, i64>(0)? as u64,
			at: r.get(1)?,
			actor: r.get(2)?,
			action: r.get(3)?,
			trade_id: r.get(4)?,
			detail: r.get(5)?,
			prev: r.get(6)?,
			hash: r.get(7)?,
		})
	})?;
	rows.collect()
}

/// Append to `audit.jsonl` every entry committed but not yet there. Runs in
/// its own immediate transaction, so two processes never write the same
/// line twice; `audit_head.seq` is how far the file goes.
pub fn mirror(conn: &mut rusqlite::Connection, dir: &Path) -> rusqlite::Result<usize> {
	let tx = conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
	let mirrored: i64 = tx
		.query_row("SELECT seq FROM audit_head WHERE id = 1", [], |r| r.get(0))
		.optional()?
		.unwrap_or(0);
	let new = entries(&tx, mirrored as u64 + 1)?;
	if new.is_empty() {
		return Ok(0);
	}
	let write = || -> std::io::Result<()> {
		let mut f = OpenOptions::new()
			.create(true)
			.append(true)
			.open(dir.join(FILE))?;
		for e in &new {
			let line = serde_json::to_string(e).map_err(std::io::Error::other)?;
			writeln!(f, "{line}")?;
		}
		f.sync_all()
	};
	if let Err(e) = write() {
		// The entries are safe in the database; the file catches up next time.
		tracing::error!(error = %e, "could not append to audit.jsonl");
		return Ok(0);
	}
	let last = new.last().expect("not empty");
	tx.execute(
		"INSERT INTO audit_head (id, seq, hash) VALUES (1, ?1, ?2) ON CONFLICT(id) DO UPDATE SET seq = ?1, hash = ?2",
		params![last.seq as i64, last.hash],
	)?;
	tx.commit()?;
	Ok(new.len())
}

/// Problems with a chain of entries, starting at `first_seq` after `prev`.
fn check_chain(
	entries: &[Entry],
	first_seq: u64,
	mut prev: String,
	source: &str,
	out: &mut Vec<String>,
) {
	let mut expect = first_seq;
	for e in entries {
		if e.seq != expect {
			out.push(format!(
				"{source}: entry {expect} is missing (found {} next)",
				e.seq
			));
			expect = e.seq;
		}
		if e.prev != prev {
			out.push(format!("{source}: entry {} does not follow the entry before it (an entry was removed or changed)", e.seq));
		}
		if e.compute_hash() != e.hash {
			out.push(format!(
				"{source}: entry {} was changed after it was written",
				e.seq
			));
		}
		prev = e.hash.clone();
		expect += 1;
	}
}

#[derive(Debug, Clone, Default)]
pub struct Verification {
	pub entries: usize,
	pub in_file: usize,
	pub problems: Vec<String>,
}

impl Verification {
	pub fn ok(&self) -> bool {
		self.problems.is_empty()
	}
}

fn read_lines(path: &Path, out: &mut Vec<String>) -> Vec<Entry> {
	let Ok(f) = std::fs::File::open(path) else {
		return Vec::new();
	};
	let mut lines = Vec::new();
	for (i, line) in std::io::BufReader::new(f).lines().enumerate() {
		// A read error repeats forever (a directory in the file's place, a
		// bad disk): report it once and stop.
		let line = match line {
			Ok(l) => l,
			Err(e) => {
				out.push(format!("{}: cannot be read: {}", path.display(), e.kind()));
				break;
			}
		};
		match serde_json::from_str::<Entry>(&line) {
			Ok(e) => lines.push(e),
			Err(_) => out.push(format!(
				"{}: line {} is not an audit entry",
				path.display(),
				i + 1
			)),
		}
	}
	lines
}

/// Check the database chain, the file chain, and that they agree.
pub fn verify(conn: &rusqlite::Connection, dir: &Path) -> rusqlite::Result<Verification> {
	let mut v = Verification::default();
	let db = entries(conn, 1)?;
	v.entries = db.len();
	check_chain(&db, 1, GENESIS.into(), "database", &mut v.problems);

	let file = read_lines(&dir.join(FILE), &mut v.problems);
	v.in_file = file.len();
	check_chain(&file, 1, GENESIS.into(), FILE, &mut v.problems);

	let mirrored: i64 = conn
		.query_row("SELECT seq FROM audit_head WHERE id = 1", [], |r| r.get(0))
		.optional()?
		.unwrap_or(0);
	if (file.len() as i64) < mirrored {
		v.problems.push(format!(
			"{FILE} has {} entries but {} were written to it",
			file.len(),
			mirrored
		));
	}
	for f in &file {
		match db.iter().find(|d| d.seq == f.seq) {
			Some(d) if d == f => {}
			Some(_) => v.problems.push(format!(
				"{FILE} entry {} differs from the database's",
				f.seq
			)),
			None => v
				.problems
				.push(format!("{FILE} entry {} is not in the database", f.seq)),
		}
	}
	Ok(v)
}

/// Entries in `[from, to)`, as JSON lines. Each carries its `prev` and
/// `hash`, so the slice can be checked on its own (each line's hash, and
/// each line against the one before).
pub fn export(
	conn: &rusqlite::Connection,
	from: DateTime<Utc>,
	to: DateTime<Utc>,
) -> rusqlite::Result<Vec<String>> {
	let (from, to) = (
		from.to_rfc3339_opts(chrono::SecondsFormat::Micros, true),
		to.to_rfc3339_opts(chrono::SecondsFormat::Micros, true),
	);
	Ok(entries(conn, 1)?
		.into_iter()
		.filter(|e| e.at >= from && e.at < to)
		.map(|e| serde_json::to_string(&e).expect("an entry serialises"))
		.collect())
}

/// Check an exported slice: every hash, and every link inside it.
pub fn verify_export(lines: &[String]) -> Vec<String> {
	let mut problems = Vec::new();
	let mut entries = Vec::new();
	for (i, l) in lines.iter().enumerate() {
		match serde_json::from_str::<Entry>(l) {
			Ok(e) => entries.push(e),
			Err(_) => problems.push(format!("line {} is not an audit entry", i + 1)),
		}
	}
	if let Some(first) = entries.first() {
		check_chain(
			&entries,
			first.seq,
			first.prev.clone(),
			"export",
			&mut problems,
		);
	}
	problems
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn the_genesis_is_the_domain_hash() {
		assert_eq!(
			GENESIS,
			hex::encode(Sha256::digest(b"recoilpay/merchant-agent/audit/v1"))
		);
	}
}
