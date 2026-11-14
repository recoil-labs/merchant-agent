//! A typed client for exactly the aggregator routes the agent needs.
//!
//! - **Authentication:** `x-api-key`, marked sensitive so it never appears
//!   in debug output.
//! - **Timeouts** on every call.
//! - **Redirects are never followed**, so the key can't be carried to
//!   another host.
//! - **Bodies are capped** at [`MAX_BODY_BYTES`].
//! - **Retries:** bounded, with jitter, and only for calls that are safe to
//!   repeat: reads, the heartbeat, and replacing the quote book.
//! - **`payment-sent` is never retried blindly.** After any doubt the trade
//!   is re-read, and the report is sent again only if the re-read shows it
//!   didn't take effect ([`Client::report_payment`]).
//! - **Strict parsing** ([`types`]): unknown fields, unknown states and
//!   malformed amounts are errors.

pub mod types;

use std::time::{Duration, Instant};

use chrono::{DateTime, Utc};
use rand::Rng;
use reqwest::header::{HeaderMap, HeaderValue, DATE};
use reqwest::{Method, StatusCode};
use serde::de::DeserializeOwned;
use serde::Serialize;

use crate::config::{Config, Mode};
use crate::secrets::Secret;
pub use types::{ChainInfo, ErrorBody, Quote, QuoteInput, Trade, TradeState};

/// Largest response the agent will read. A trade list is a few kB per
/// trade; anything far beyond this is not a response the agent asked for.
pub const MAX_BODY_BYTES: usize = 8 * 1024 * 1024;

/// What the aggregator accepts as a payment reference
/// (`plain_text("paymentReference", …, 64)`): 1 to 64 printable ASCII
/// characters.
pub const MAX_REFERENCE_CHARS: usize = 64;

/// A statement is 1 to 2000 printable characters (`MAX_STATEMENT_CHARS`).
pub const MAX_STATEMENT_CHARS: usize = 2000;

#[derive(Debug, thiserror::Error)]
pub enum AggregatorError {
	#[error("the aggregator did not answer in time")]
	Timeout,
	#[error("could not reach the aggregator: {0}")]
	Transport(String),
	#[error("the aggregator refused the API key")]
	Unauthorized,
	#[error("forbidden: {}", .0.message)]
	Forbidden(ErrorBody),
	#[error("not found: {}", .0.message)]
	NotFound(ErrorBody),
	#[error("conflict ({}): {}", .0.error, .0.message)]
	Conflict(ErrorBody),
	#[error("rejected ({}): {}", .0.error, .0.message)]
	BadRequest(ErrorBody),
	#[error("the aggregator is rate limiting this agent")]
	RateLimited,
	#[error("the aggregator failed with HTTP {0}")]
	Server(u16),
	#[error("unexpected HTTP {0} from the aggregator")]
	UnexpectedStatus(u16),
	#[error("the aggregator's response was not understood: {0}")]
	Decode(String),
	#[error("the aggregator's response was larger than {MAX_BODY_BYTES} bytes")]
	TooLarge,
	#[error("{0}")]
	InvalidInput(String),
}

impl AggregatorError {
	/// Worth trying again for a call that is safe to repeat.
	pub fn is_transient(&self) -> bool {
		matches!(
			self,
			Self::Timeout | Self::Transport(_) | Self::Server(_) | Self::RateLimited
		)
	}

	/// After a write, an error that leaves it unknown whether the write
	/// happened.
	fn is_ambiguous(&self) -> bool {
		self.is_transient() || matches!(self, Self::Decode(_) | Self::TooLarge)
	}

	/// The aggregator's error code, when it sent one.
	pub fn code(&self) -> Option<&str> {
		match self {
			Self::Forbidden(b) | Self::NotFound(b) | Self::Conflict(b) | Self::BadRequest(b) => {
				Some(b.error.as_str())
			}
			_ => None,
		}
	}
}

#[derive(Debug, Clone, Copy)]
pub struct RetryPolicy {
	/// Total attempts, including the first.
	pub attempts: u32,
	/// The first backoff; each retry doubles it, plus up to as much again
	/// in jitter.
	pub base_delay: Duration,
}

impl Default for RetryPolicy {
	fn default() -> Self {
		Self {
			attempts: 3,
			base_delay: Duration::from_millis(250),
		}
	}
}

/// What reporting a payment came to.
#[derive(Debug)]
pub enum ReportOutcome {
	/// The aggregator holds our reference: the trade is waiting on the
	/// customer, or has already moved on from there.
	Reported(Box<Trade>),
	/// The trade can no longer take our report: it was refunded, settled
	/// without it, or holds another reference. A person must look.
	NotReportable(Box<Trade>),
}

enum Judgement {
	Reported,
	StillAwaitingFiat,
	Elsewhere,
}

fn judge(trade: &Trade, reference: &str) -> Judgement {
	use TradeState as S;
	let ours = trade.payment_reference.as_deref() == Some(reference);
	match trade.state {
		S::AwaitingConfirmation | S::Disputed | S::SettledToFiatPayer if ours => {
			Judgement::Reported
		}
		S::AwaitingFiat => Judgement::StillAwaitingFiat,
		_ => Judgement::Elsewhere,
	}
}

/// A path segment from the aggregator or config, checked before it goes
/// into a URL.
fn segment(s: &str) -> Result<&str, AggregatorError> {
	let ok = (1..=128).contains(&s.len())
		&& s.bytes()
			.all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_');
	if ok {
		Ok(s)
	} else {
		Err(AggregatorError::InvalidInput(
			"an id contains characters a URL can't carry".into(),
		))
	}
}

/// The reference as the aggregator will store it, or why it would refuse.
pub fn check_reference(reference: &str) -> Result<String, AggregatorError> {
	let r = reference.trim();
	if r.is_empty()
		|| r.len() > MAX_REFERENCE_CHARS
		|| !r.chars().all(|c| c.is_ascii_graphic() || c == ' ')
	{
		return Err(AggregatorError::InvalidInput(format!(
			"a payment reference is 1 to {MAX_REFERENCE_CHARS} printable ASCII characters"
		)));
	}
	Ok(r.to_string())
}

pub struct Client {
	http: reqwest::Client,
	base: String,
	solver_id: String,
	api_key: Secret,
	retry: RetryPolicy,
}

impl Client {
	/// `https_only` must be true in production (invariant 9).
	pub fn new(
		base_url: &str,
		solver_id: &str,
		api_key: Secret,
		timeout: Duration,
		https_only: bool,
	) -> Result<Self, AggregatorError> {
		segment(solver_id)?;
		let http = reqwest::Client::builder()
			.timeout(timeout)
			.connect_timeout(timeout)
			.redirect(reqwest::redirect::Policy::none())
			.https_only(https_only)
			.user_agent(concat!(
				"recoilpay-merchant-agent/",
				env!("CARGO_PKG_VERSION")
			))
			.build()
			.map_err(|e| AggregatorError::Transport(e.to_string()))?;
		Ok(Self {
			http,
			base: base_url.trim().trim_end_matches('/').to_string(),
			solver_id: solver_id.to_string(),
			api_key,
			retry: RetryPolicy::default(),
		})
	}

	pub fn from_config(config: &Config, api_key: Secret) -> Result<Self, AggregatorError> {
		Self::new(
			&config.aggregator.url,
			&config.operator.solver_id,
			api_key,
			Duration::from_secs(config.aggregator.request_timeout_secs),
			config.mode == Mode::Production,
		)
	}

	pub fn with_retry(mut self, retry: RetryPolicy) -> Self {
		self.retry = retry;
		self
	}

	fn operator_path(&self, rest: &str) -> String {
		format!("/solver-api/operators/{}/{rest}", self.solver_id)
	}

	// ── transport ────────────────────────────────────────────────────────

	async fn send<B: Serialize + ?Sized>(
		&self,
		method: Method,
		path: &str,
		body: Option<&B>,
	) -> Result<(HeaderMap, Vec<u8>), AggregatorError> {
		let mut key = HeaderValue::from_str(self.api_key.expose()).map_err(|_| {
			AggregatorError::InvalidInput("the API key has characters a header can't carry".into())
		})?;
		key.set_sensitive(true);
		let mut req = self
			.http
			.request(method, format!("{}{path}", self.base))
			.header("x-api-key", key);
		if let Some(b) = body {
			req = req.json(b);
		}
		let mut resp = req.send().await.map_err(from_reqwest)?;
		let status = resp.status();
		let headers = resp.headers().clone();

		let mut bytes = Vec::new();
		while let Some(chunk) = resp.chunk().await.map_err(from_reqwest)? {
			if bytes.len() + chunk.len() > MAX_BODY_BYTES {
				return Err(AggregatorError::TooLarge);
			}
			bytes.extend_from_slice(&chunk);
		}

		if status.is_success() {
			return Ok((headers, bytes));
		}
		let body = || serde_json::from_slice::<ErrorBody>(&bytes).unwrap_or_default();
		Err(match status {
			StatusCode::UNAUTHORIZED => AggregatorError::Unauthorized,
			StatusCode::FORBIDDEN => AggregatorError::Forbidden(body()),
			StatusCode::NOT_FOUND => AggregatorError::NotFound(body()),
			StatusCode::CONFLICT => AggregatorError::Conflict(body()),
			StatusCode::BAD_REQUEST | StatusCode::UNPROCESSABLE_ENTITY => {
				AggregatorError::BadRequest(body())
			}
			StatusCode::TOO_MANY_REQUESTS => AggregatorError::RateLimited,
			s if s.is_server_error() => AggregatorError::Server(s.as_u16()),
			s => AggregatorError::UnexpectedStatus(s.as_u16()),
		})
	}

	async fn call<T: DeserializeOwned, B: Serialize + ?Sized>(
		&self,
		method: Method,
		path: &str,
		body: Option<&B>,
	) -> Result<T, AggregatorError> {
		let (_, bytes) = self.send(method, path, body).await?;
		decode(&bytes)
	}

	/// Repeat `f` while it fails transiently, up to the retry policy. Only
	/// for calls that are safe to repeat.
	async fn retrying<T, F, Fut>(&self, f: F) -> Result<T, AggregatorError>
	where
		F: Fn() -> Fut,
		Fut: std::future::Future<Output = Result<T, AggregatorError>>,
	{
		let mut attempt = 0;
		loop {
			match f().await {
				Err(e) if e.is_transient() && attempt + 1 < self.retry.attempts => {
					self.backoff(attempt).await;
					attempt += 1;
				}
				other => return other,
			}
		}
	}

	async fn backoff(&self, attempt: u32) {
		let base = self.retry.base_delay.saturating_mul(1 << attempt.min(6));
		let jitter_ms = rand::rng().random_range(0..=base.as_millis() as u64);
		tokio::time::sleep(base + Duration::from_millis(jitter_ms)).await;
	}

	// ── reads ────────────────────────────────────────────────────────────

	/// The operator's own trades in `state`, newest first.
	pub async fn list_own_trades(&self, state: TradeState) -> Result<Vec<Trade>, AggregatorError> {
		let path = self.operator_path(&format!("fiat-trades?state={}", state.as_str()));
		self.retrying(|| self.call::<Vec<Trade>, ()>(Method::GET, &path, None))
			.await
	}

	/// One trade by id.
	pub async fn get_trade(&self, trade_id: &str) -> Result<Trade, AggregatorError> {
		let path = format!("/api/v1/fiat/trades/{}", segment(trade_id)?);
		self.retrying(|| self.call::<Trade, ()>(Method::GET, &path, None))
			.await
	}

	/// The operator's quote book.
	pub async fn get_quotes(&self) -> Result<Vec<Quote>, AggregatorError> {
		let path = self.operator_path("fiat-quotes");
		let r: types::QuotesResponse = self
			.retrying(|| self.call(Method::GET, &path, None::<&()>))
			.await?;
		Ok(r.quotes)
	}

	/// The payee public key registered with RecoilPay, if one is set.
	pub async fn get_payee_key(&self) -> Result<Option<String>, AggregatorError> {
		let path = self.operator_path("fiat-payee-key");
		match self
			.retrying(|| self.call::<types::PayeeKeyResponse, ()>(Method::GET, &path, None))
			.await
		{
			Ok(r) => Ok(Some(r.public_key)),
			Err(AggregatorError::NotFound(b)) if b.error == "NO_KEY" => Ok(None),
			Err(e) => Err(e),
		}
	}

	/// RecoilPay's chain registry.
	pub async fn get_chains(&self) -> Result<Vec<ChainInfo>, AggregatorError> {
		let r: types::ChainsResponse = self
			.retrying(|| self.call(Method::GET, "/api/v1/chains", None::<&()>))
			.await?;
		Ok(r.data)
	}

	/// How far this host's clock is from the aggregator's: positive when
	/// ours is ahead. From the `Date` header, so whole seconds at best;
	/// the request's round trip is split evenly.
	pub async fn clock_skew(&self) -> Result<chrono::Duration, AggregatorError> {
		let started = Utc::now();
		let timer = Instant::now();
		let (headers, _) = self
			.retrying(|| self.send::<()>(Method::GET, "/health", None))
			.await?;
		let half_trip = chrono::Duration::from_std(timer.elapsed() / 2).unwrap_or_default();
		let date = headers
			.get(DATE)
			.and_then(|v| v.to_str().ok())
			.and_then(|v| DateTime::parse_from_rfc2822(v).ok())
			.ok_or_else(|| AggregatorError::Decode("no Date header".into()))?;
		Ok((started + half_trip) - date.with_timezone(&Utc))
	}

	// ── writes ───────────────────────────────────────────────────────────

	/// Replace the quote book. Safe to repeat: the result is the same book.
	/// An empty list withdraws every offer, which is how the kill switch
	/// pauses (design §3.4).
	pub async fn put_quotes(&self, quotes: &[QuoteInput]) -> Result<Vec<Quote>, AggregatorError> {
		#[derive(Serialize)]
		struct Body<'a> {
			quotes: &'a [QuoteInput],
		}
		let path = self.operator_path("fiat-quotes");
		let body = Body { quotes };
		let r: types::QuotesResponse = self
			.retrying(|| self.call(Method::PUT, &path, Some(&body)))
			.await?;
		Ok(r.quotes)
	}

	/// Tell the platform the agent is alive. Safe to repeat.
	pub async fn heartbeat(&self) -> Result<(), AggregatorError> {
		let path = self.operator_path("heartbeat");
		self.retrying(|| async { self.send::<()>(Method::POST, &path, None).await.map(|_| ()) })
			.await
	}

	/// Report that the payout was sent, with the bank's reference.
	///
	/// Never sent twice on a guess. After a timeout, a server error, an
	/// unreadable answer or a conflict, the trade is re-read:
	/// - it already holds our reference → done;
	/// - it is still waiting for payment → send again (bounded);
	/// - anything else → [`ReportOutcome::NotReportable`], for a person.
	///
	/// A refusal that re-reading can't change (bad key, not our trade, bad
	/// reference) is returned as an error.
	pub async fn report_payment(
		&self,
		trade_id: &str,
		reference: &str,
	) -> Result<ReportOutcome, AggregatorError> {
		#[derive(Serialize)]
		#[serde(rename_all = "camelCase")]
		struct Body<'a> {
			payment_reference: &'a str,
		}
		let reference = check_reference(reference)?;
		let path = format!("/api/v1/fiat/trades/{}/payment-sent", segment(trade_id)?);
		let body = Body {
			payment_reference: &reference,
		};

		let mut attempt = 0;
		loop {
			let trade = match self
				.call::<Trade, _>(Method::POST, &path, Some(&body))
				.await
			{
				Ok(trade) => trade,
				Err(e) if e.is_ambiguous() || matches!(e, AggregatorError::Conflict(_)) => {
					// Did it land? Ask, rather than guess.
					let seen = self.get_trade(trade_id).await?;
					tracing::warn!(trade_id, error = %e, state = seen.state.as_str(), "payment report was uncertain; re-read the trade");
					seen
				}
				Err(e) => return Err(e),
			};
			match judge(&trade, &reference) {
				Judgement::Reported => return Ok(ReportOutcome::Reported(Box::new(trade))),
				Judgement::Elsewhere => return Ok(ReportOutcome::NotReportable(Box::new(trade))),
				Judgement::StillAwaitingFiat if attempt + 1 < self.retry.attempts => {
					self.backoff(attempt).await;
					attempt += 1;
				}
				Judgement::StillAwaitingFiat => {
					return Err(AggregatorError::Transport(
						"the payment report did not take effect after every attempt".into(),
					))
				}
			}
		}
	}

	/// Add a statement to a disputed trade: what was paid and the bank's
	/// reference, for the person who rules on it. Not retried: a repeat
	/// would file the statement twice.
	pub async fn add_statement(
		&self,
		trade_id: &str,
		text: &str,
		reference: Option<&str>,
	) -> Result<(), AggregatorError> {
		#[derive(Serialize)]
		struct Body<'a> {
			kind: &'static str,
			text: &'a str,
			#[serde(skip_serializing_if = "Option::is_none")]
			reference: Option<&'a str>,
		}
		let text = text.trim();
		if text.is_empty()
			|| text.chars().count() > MAX_STATEMENT_CHARS
			|| !text
				.chars()
				.all(|c| c.is_ascii_graphic() || c == ' ' || c == '\n')
		{
			return Err(AggregatorError::InvalidInput(format!(
				"a statement is 1 to {MAX_STATEMENT_CHARS} printable characters"
			)));
		}
		let reference = reference.map(str::trim).filter(|r| !r.is_empty());
		if let Some(r) = reference {
			if r.len() > 100
				|| !r
					.chars()
					.all(|c| c.is_ascii_alphanumeric() || "-_./".contains(c))
			{
				return Err(AggregatorError::InvalidInput(
					"an evidence reference is up to 100 letters, digits or -_./".into(),
				));
			}
		}
		let path = format!("/api/v1/fiat/trades/{}/evidence", segment(trade_id)?);
		self.send(
			Method::POST,
			&path,
			Some(&Body {
				kind: "statement",
				text,
				reference,
			}),
		)
		.await
		.map(|_| ())
	}
}

fn decode<T: DeserializeOwned>(bytes: &[u8]) -> Result<T, AggregatorError> {
	serde_json::from_slice(bytes).map_err(|e| AggregatorError::Decode(e.to_string()))
}

fn from_reqwest(e: reqwest::Error) -> AggregatorError {
	if e.is_timeout() {
		AggregatorError::Timeout
	} else {
		AggregatorError::Transport(e.without_url().to_string())
	}
}
