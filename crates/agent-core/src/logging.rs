//! Structured logs, with personal data kept out.
//!
//! Two layers of defence (invariant 7):
//!
//! 1. **The type rule** ([`crate::pii`]): personal-data types cannot be
//!    formatted at all, so they cannot be logged by accident.
//! 2. **This redactor**, a backstop that rewrites every log line before it
//!    is written, replacing anything shaped like a bank account number or a
//!    payment provider secret key.
//!
//! The redactor matches shapes, not values, so it errs towards hiding. A
//! standalone run of 10 to 13 digits is masked whether it is an account
//! number, a phone number or something else. The agent therefore never logs
//! raw Unix timestamps, and amounts of ₦10,000,000.00 or more in kobo show
//! masked. Hashes and addresses are `0x`-prefixed hex and pass untouched.
//!
//! Panics are logged without their message, which could hold anything
//! (see [`install_panic_hook`]).

use regex::Regex;
use std::io::{self, Write};
use std::sync::OnceLock;
use tracing_subscriber::fmt::MakeWriter;
use tracing_subscriber::EnvFilter;

use crate::config::LogFormat;

struct Rule {
	pattern: Regex,
	replacement: &'static str,
}

fn rules() -> &'static [Rule] {
	static RULES: OnceLock<Vec<Rule>> = OnceLock::new();
	RULES.get_or_init(|| {
		let rule = |p: &str, replacement| Rule {
			pattern: Regex::new(p).expect("redaction patterns are valid"),
			replacement,
		};
		vec![
			// Paystack and Stripe-style secret and public keys.
			rule(r"\b(sk|pk|rk)_(live|test)_[A-Za-z0-9]+", "[redacted-key]"),
			// Bearer tokens in a header someone formatted.
			rule(r"(?i)bearer\s+[A-Za-z0-9._~+/=-]{8,}", "Bearer [redacted]"),
			// Account and phone numbers: 10–13 digits standing alone,
			// optionally with a leading +.
			rule(r"\+?\b[0-9]{10,13}\b", "[redacted-number]"),
		]
	})
}

/// Replace everything that looks like personal data or a credential.
pub fn redact(line: &str) -> String {
	let mut out = line.to_string();
	for r in rules() {
		if r.pattern.is_match(&out) {
			out = r.pattern.replace_all(&out, r.replacement).into_owned();
		}
	}
	out
}

/// A [`MakeWriter`] that redacts each log event before passing it on.
///
/// The formatter makes one writer per event and drops it when the event is
/// written, so buffering until drop redacts whole lines, even when a value
/// was written in several pieces.
#[derive(Clone)]
pub struct RedactingMakeWriter<M> {
	inner: M,
}

impl<M> RedactingMakeWriter<M> {
	pub fn new(inner: M) -> Self {
		Self { inner }
	}
}

pub struct RedactingWriter<W: Write> {
	inner: W,
	buf: Vec<u8>,
}

impl<W: Write> Write for RedactingWriter<W> {
	fn write(&mut self, data: &[u8]) -> io::Result<usize> {
		self.buf.extend_from_slice(data);
		Ok(data.len())
	}

	fn flush(&mut self) -> io::Result<()> {
		if self.buf.is_empty() {
			return Ok(());
		}
		let text = String::from_utf8_lossy(&self.buf);
		let clean = redact(&text);
		self.buf.clear();
		self.inner.write_all(clean.as_bytes())?;
		self.inner.flush()
	}
}

impl<W: Write> Drop for RedactingWriter<W> {
	fn drop(&mut self) {
		let _ = self.flush();
	}
}

impl<'a, M> MakeWriter<'a> for RedactingMakeWriter<M>
where
	M: MakeWriter<'a>,
{
	type Writer = RedactingWriter<M::Writer>;

	fn make_writer(&'a self) -> Self::Writer {
		RedactingWriter {
			inner: self.inner.make_writer(),
			buf: Vec::new(),
		}
	}
}

/// Install the global subscriber: JSON (or pretty) lines on stderr, through
/// the redactor.
pub fn init(level: &str, format: LogFormat) -> Result<(), String> {
	let filter = EnvFilter::try_new(level).map_err(|e| e.to_string())?;
	let writer = RedactingMakeWriter::new(io::stderr);
	let builder = tracing_subscriber::fmt()
		.with_env_filter(filter)
		.with_writer(writer);
	let result = match format {
		LogFormat::Json => builder
			.json()
			.flatten_event(true)
			.with_current_span(false)
			.try_init(),
		LogFormat::Pretty => builder.try_init(),
	};
	result.map_err(|e| e.to_string())
}

/// Log a panic's location only. The message is dropped: a panic can carry
/// any value that was in scope, including a decrypted account number, and
/// crash reports are shipped to places personal data must not go.
pub fn install_panic_hook() {
	std::panic::set_hook(Box::new(|info| {
		let at = info
			.location()
			.map(|l| format!("{}:{}", l.file(), l.line()))
			.unwrap_or_else(|| "unknown".into());
		tracing::error!(
			location = %at,
			"the agent stopped on an internal error; the message is withheld because it may hold personal data"
		);
	}));
}

#[cfg(test)]
mod tests {
	use super::*;
	use std::sync::{Arc, Mutex};

	#[test]
	fn masks_account_numbers_and_keys() {
		let line = r#"{"msg":"paying 0123456789 via sk_test_abc123DEF456 for +2348012345678"}"#;
		let out = redact(line);
		assert!(!out.contains("0123456789"), "{out}");
		assert!(!out.contains("abc123DEF456"), "{out}");
		assert!(!out.contains("8012345678"), "{out}");
	}

	#[test]
	fn leaves_hashes_addresses_ids_and_small_amounts() {
		let line = "trade ft-0123456789abcdef0123456789abcdef lock 0x975ddf097dddbb27fc7a7f09bee0d6a767b08d52159f6787b12a0b0ca5f11f1e \
		            wallet 0x1234567890123456789012345678901234567890 amount 15081152";
		assert_eq!(redact(line), line);
	}

	#[derive(Clone, Default)]
	struct Capture(Arc<Mutex<Vec<u8>>>);

	impl Write for Capture {
		fn write(&mut self, d: &[u8]) -> io::Result<usize> {
			self.0.lock().unwrap().extend_from_slice(d);
			Ok(d.len())
		}
		fn flush(&mut self) -> io::Result<()> {
			Ok(())
		}
	}

	impl<'a> MakeWriter<'a> for Capture {
		type Writer = Capture;
		fn make_writer(&'a self) -> Capture {
			self.clone()
		}
	}

	#[test]
	fn events_written_through_the_subscriber_are_redacted() {
		let capture = Capture::default();
		let subscriber = tracing_subscriber::fmt()
			.json()
			.with_writer(RedactingMakeWriter::new(capture.clone()))
			.finish();
		tracing::subscriber::with_default(subscriber, || {
			tracing::info!(account = "0123456789", "about to pay");
		});
		let out = String::from_utf8(capture.0.lock().unwrap().clone()).unwrap();
		assert!(out.contains("about to pay"), "{out}");
		assert!(!out.contains("0123456789"), "{out}");
	}
}
