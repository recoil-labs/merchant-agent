//! Prometheus metrics, in the text exposition format, served on the health
//! port (`/metrics`). No labels ever carry personal data: states, reasons
//! and error kinds only.

use std::collections::BTreeMap;
use std::fmt::Write;
use std::sync::atomic::{AtomicI64, AtomicU64, Ordering};
use std::sync::Mutex;

/// Upper bounds of the lock-to-paid histogram, in seconds. The payment
/// window is 15 minutes, so the buckets crowd towards it.
pub const LOCK_TO_PAID_BUCKETS: [u64; 9] = [5, 10, 30, 60, 120, 300, 600, 900, 1800];

#[derive(Default)]
struct Histogram {
	counts: [u64; LOCK_TO_PAID_BUCKETS.len()],
	sum_ms: u64,
	count: u64,
}

#[derive(Default)]
pub struct Metrics {
	holds: Mutex<BTreeMap<&'static str, u64>>,
	bank_errors: Mutex<BTreeMap<&'static str, u64>>,
	lock_to_paid: Mutex<Histogram>,
	/// The payout account's last-read balance in kobo; -1 if never read.
	float_kobo: AtomicI64,
	last_tick: AtomicI64,
	ticks: AtomicU64,
	heartbeats: AtomicU64,
	heartbeat_failures: AtomicU64,
	alerts: AtomicU64,
}

impl Metrics {
	pub fn new() -> Self {
		let m = Self::default();
		m.float_kobo.store(-1, Ordering::Relaxed);
		m
	}

	pub fn hold(&self, reason: &'static str) {
		*self
			.holds
			.lock()
			.unwrap_or_else(|e| e.into_inner())
			.entry(reason)
			.or_default() += 1;
	}

	/// `kind` is `send`, `status`, `lookup` or `balance`.
	pub fn bank_error(&self, kind: &'static str) {
		*self
			.bank_errors
			.lock()
			.unwrap_or_else(|e| e.into_inner())
			.entry(kind)
			.or_default() += 1;
	}

	pub fn lock_to_paid(&self, millis: u64) {
		let mut h = self.lock_to_paid.lock().unwrap_or_else(|e| e.into_inner());
		let secs = millis / 1000;
		for (i, bound) in LOCK_TO_PAID_BUCKETS.iter().enumerate() {
			if secs <= *bound {
				h.counts[i] += 1;
			}
		}
		h.sum_ms += millis;
		h.count += 1;
	}

	pub fn float(&self, kobo: u128) {
		self.float_kobo
			.store(i64::try_from(kobo).unwrap_or(i64::MAX), Ordering::Relaxed);
	}

	pub fn tick(&self, unix_secs: i64) {
		self.last_tick.store(unix_secs, Ordering::Relaxed);
		self.ticks.fetch_add(1, Ordering::Relaxed);
	}

	pub fn last_tick(&self) -> i64 {
		self.last_tick.load(Ordering::Relaxed)
	}

	pub fn heartbeat(&self, ok: bool) {
		if ok {
			self.heartbeats.fetch_add(1, Ordering::Relaxed);
		} else {
			self.heartbeat_failures.fetch_add(1, Ordering::Relaxed);
		}
	}

	pub fn alerts(&self, n: u64) {
		self.alerts.fetch_add(n, Ordering::Relaxed);
	}

	/// The exposition text. `trades` is the count per agent state. Each
	/// family's samples follow its own HELP and TYPE lines.
	pub fn render(&self, trades: &BTreeMap<String, u64>, paused: bool, ready: bool) -> String {
		let mut out = String::new();
		let mut family = |name: &str, kind: &str, help: &str, samples: &[(String, String)]| {
			let _ = writeln!(out, "# HELP {name} {help}\n# TYPE {name} {kind}");
			for (suffix, value) in samples {
				let _ = writeln!(out, "{name}{suffix} {value}");
			}
		};
		let one = |v: String| vec![(String::new(), v)];
		let labelled = |label: &str, m: &mut dyn Iterator<Item = (String, u64)>| {
			m.map(|(k, v)| (format!("{{{label}=\"{k}\"}}"), v.to_string()))
				.collect::<Vec<_>>()
		};

		family(
			"merchant_agent_up",
			"gauge",
			"1 while the agent process runs.",
			&one("1".into()),
		);
		family(
			"merchant_agent_ready",
			"gauge",
			"1 when /readyz passes.",
			&one(u8::from(ready).to_string()),
		);
		family(
			"merchant_agent_paused",
			"gauge",
			"1 while payouts are paused.",
			&one(u8::from(paused).to_string()),
		);
		family(
			"merchant_agent_trades",
			"gauge",
			"Trades the agent knows, by state.",
			&labelled("state", &mut trades.iter().map(|(k, v)| (k.clone(), *v))),
		);
		let holds = self.holds.lock().unwrap_or_else(|e| e.into_inner()).clone();
		family(
			"merchant_agent_holds_total",
			"counter",
			"Trades held for review, by reason.",
			&labelled(
				"reason",
				&mut holds.into_iter().map(|(k, v)| (k.to_string(), v)),
			),
		);
		let errors = self
			.bank_errors
			.lock()
			.unwrap_or_else(|e| e.into_inner())
			.clone();
		family(
			"merchant_agent_bank_errors_total",
			"counter",
			"Calls to the bank that failed, by kind.",
			&labelled(
				"kind",
				&mut errors.into_iter().map(|(k, v)| (k.to_string(), v)),
			),
		);
		let float = self.float_kobo.load(Ordering::Relaxed);
		family(
			"merchant_agent_float_kobo",
			"gauge",
			"The payout account's last-read balance, in kobo. Absent until first read.",
			&if float >= 0 {
				one(float.to_string())
			} else {
				Vec::new()
			},
		);
		family(
			"merchant_agent_last_tick_timestamp_seconds",
			"gauge",
			"When the agent last checked for trades.",
			&one(self.last_tick().to_string()),
		);
		family(
			"merchant_agent_ticks_total",
			"counter",
			"Passes over the trades.",
			&one(self.ticks.load(Ordering::Relaxed).to_string()),
		);
		family(
			"merchant_agent_heartbeats_total",
			"counter",
			"Heartbeats RecoilPay accepted.",
			&one(self.heartbeats.load(Ordering::Relaxed).to_string()),
		);
		family(
			"merchant_agent_heartbeat_failures_total",
			"counter",
			"Heartbeats that failed.",
			&one(self.heartbeat_failures.load(Ordering::Relaxed).to_string()),
		);
		family(
			"merchant_agent_alerts_total",
			"counter",
			"Alerts raised.",
			&one(self.alerts.load(Ordering::Relaxed).to_string()),
		);
		let h = self.lock_to_paid.lock().unwrap_or_else(|e| e.into_inner());
		let mut samples: Vec<(String, String)> = LOCK_TO_PAID_BUCKETS
			.iter()
			.zip(h.counts)
			.map(|(bound, n)| (format!("_bucket{{le=\"{bound}\"}}"), n.to_string()))
			.collect();
		samples.push(("_bucket{le=\"+Inf\"}".into(), h.count.to_string()));
		samples.push((
			"_sum".into(),
			format!("{}.{:03}", h.sum_ms / 1000, h.sum_ms % 1000),
		));
		samples.push(("_count".into(), h.count.to_string()));
		family(
			"merchant_agent_lock_to_paid_seconds",
			"histogram",
			"From the escrow lock being verified to the bank confirming the payout.",
			&samples,
		);
		out
	}
}
