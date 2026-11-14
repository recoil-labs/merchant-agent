//! Telling people: alerts to a webhook (Slack or Teams), PagerDuty and email.
//!
//! - **Every alert answers three questions:** what happened, what the agent
//!   did, and what to do now ([`crate::runner::describe`]).
//! - **No personal data:** the trade id, the masked account and the amount,
//!   never an account number or a name. The tests grep for them.
//! - **Rate-limited:** at most `alerts.max_per_minute` across all channels.
//!   Anything over is counted and reported in one summary, so a burst never
//!   floods a channel and never silently disappears.
//! - **Logs are the floor:** every alert is logged whether or not a channel
//!   took it.

use std::future::Future;
use std::pin::Pin;
use std::sync::Mutex;
use std::time::Duration;

use chrono::{DateTime, Utc};
use serde_json::json;

use crate::engine::Alert;
use crate::secrets::Secret;

pub type BoxFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

/// One alert, ready to send. No personal data.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Message {
	pub title: String,
	pub body: String,
	pub urgent: bool,
	/// Lets a channel (PagerDuty) fold repeats of the same alert together.
	pub dedup_key: String,
}

impl Message {
	pub fn from_alert(a: &Alert) -> Self {
		Self {
			title: format!(
				"{}RecoilPay agent: trade {} is {}",
				if a.urgent { "URGENT: " } else { "" },
				a.trade_id,
				a.state.label()
			),
			body: crate::runner::describe(a),
			urgent: a.urgent,
			dedup_key: format!("{}:{}", a.trade_id, a.state.to_codes().0),
		}
	}
}

pub trait Notifier: Send + Sync {
	fn name(&self) -> &'static str;
	fn send<'a>(&'a self, m: &'a Message) -> BoxFuture<'a, Result<(), String>>;
}

fn http() -> reqwest::Client {
	reqwest::Client::builder()
		.timeout(Duration::from_secs(10))
		.redirect(reqwest::redirect::Policy::none())
		.build()
		.expect("an HTTP client builds")
}

/// A Slack or Teams incoming webhook. Both accept `{"text": …}`.
pub struct Webhook {
	url: Secret,
	http: reqwest::Client,
}

impl Webhook {
	pub fn new(url: Secret) -> Self {
		Self { url, http: http() }
	}
}

impl Notifier for Webhook {
	fn name(&self) -> &'static str {
		"webhook"
	}
	fn send<'a>(&'a self, m: &'a Message) -> BoxFuture<'a, Result<(), String>> {
		Box::pin(async move {
			self.http
				.post(self.url.expose())
				.json(&json!({ "text": format!("*{}*\n{}", m.title, m.body) }))
				.send()
				.await
				.and_then(|r| r.error_for_status())
				.map(|_| ())
				.map_err(|e| e.without_url().to_string())
		})
	}
}

/// PagerDuty Events v2. Only urgent alerts page someone.
pub struct PagerDuty {
	routing_key: Secret,
	endpoint: String,
	http: reqwest::Client,
}

impl PagerDuty {
	pub const ENDPOINT: &'static str = "https://events.pagerduty.com/v2/enqueue";

	pub fn new(routing_key: Secret) -> Self {
		Self::with_endpoint(routing_key, Self::ENDPOINT)
	}

	/// For tests: a local endpoint.
	pub fn with_endpoint(routing_key: Secret, endpoint: &str) -> Self {
		Self {
			routing_key,
			endpoint: endpoint.into(),
			http: http(),
		}
	}
}

impl Notifier for PagerDuty {
	fn name(&self) -> &'static str {
		"pagerduty"
	}
	fn send<'a>(&'a self, m: &'a Message) -> BoxFuture<'a, Result<(), String>> {
		Box::pin(async move {
			if !m.urgent {
				return Ok(());
			}
			let body = json!({
				"routing_key": self.routing_key.expose(),
				"event_action": "trigger",
				"dedup_key": m.dedup_key,
				"payload": {
					"summary": m.title,
					"severity": "critical",
					"source": "recoilpay-merchant-agent",
					"custom_details": { "what_to_do": m.body },
				},
			});
			self.http
				.post(&self.endpoint)
				.json(&body)
				.send()
				.await
				.and_then(|r| r.error_for_status())
				.map(|_| ())
				.map_err(|e| e.without_url().to_string())
		})
	}
}

/// Email over SMTP (TLS verified).
pub struct Email {
	transport: lettre::AsyncSmtpTransport<lettre::Tokio1Executor>,
	from: lettre::message::Mailbox,
	to: Vec<lettre::message::Mailbox>,
}

impl Email {
	pub fn new(smtp_url: &Secret, from: &str, to: &[String]) -> Result<Self, String> {
		let transport =
			lettre::AsyncSmtpTransport::<lettre::Tokio1Executor>::from_url(smtp_url.expose())
				.map_err(|_| "the SMTP URL is not valid".to_string())?
				.timeout(Some(Duration::from_secs(15)))
				.build();
		let from = from
			.parse()
			.map_err(|_| "alerts.email.from is not an email address".to_string())?;
		let to = to
			.iter()
			.map(|t| {
				t.parse().map_err(|_| {
					"alerts.email.to holds something that is not an email address".to_string()
				})
			})
			.collect::<Result<_, _>>()?;
		Ok(Self {
			transport,
			from,
			to,
		})
	}

	/// The email for `m`, as it would be sent.
	pub fn build(&self, m: &Message) -> Result<lettre::Message, String> {
		let mut builder = lettre::Message::builder()
			.from(self.from.clone())
			.subject(&m.title);
		for to in &self.to {
			builder = builder.to(to.clone());
		}
		builder.body(m.body.clone()).map_err(|e| e.to_string())
	}
}

impl Notifier for Email {
	fn name(&self) -> &'static str {
		"email"
	}
	fn send<'a>(&'a self, m: &'a Message) -> BoxFuture<'a, Result<(), String>> {
		Box::pin(async move {
			use lettre::AsyncTransport;
			let email = self.build(m)?;
			self.transport
				.send(email)
				.await
				.map(|_| ())
				.map_err(|e| e.to_string())
		})
	}
}

#[derive(Default)]
struct Window {
	minute: i64,
	sent: u32,
	held_back: u32,
	held_back_urgent: bool,
}

/// Every configured channel, behind one rate limit.
pub struct Alerter {
	channels: Vec<Box<dyn Notifier>>,
	max_per_minute: u32,
	window: Mutex<Window>,
}

impl Alerter {
	pub fn new(channels: Vec<Box<dyn Notifier>>, max_per_minute: u32) -> Self {
		Self {
			channels,
			max_per_minute: max_per_minute.max(1),
			window: Mutex::new(Window::default()),
		}
	}

	pub fn channels(&self) -> Vec<&'static str> {
		self.channels.iter().map(|c| c.name()).collect()
	}

	/// Send `alerts`. Returns how many messages went out.
	pub async fn deliver(&self, alerts: &[Alert], now: DateTime<Utc>) -> usize {
		let mut out = Vec::new();
		{
			let mut w = self.window.lock().unwrap_or_else(|e| e.into_inner());
			let minute = now.timestamp() / 60;
			if minute != w.minute {
				if w.held_back > 0 {
					out.push(Message {
						title: format!(
							"{}RecoilPay agent: {} more alert(s) held back",
							if w.held_back_urgent { "URGENT: " } else { "" },
							w.held_back
						),
						body: format!(
							"{} alert(s) were held back to avoid flooding this channel. The agent kept working. \\
							 Open the exception queue (`merchant-agent status`) to see them all.",
							w.held_back
						),
						urgent: w.held_back_urgent,
						dedup_key: format!("held-back:{}", w.minute),
					});
				}
				*w = Window {
					minute,
					..Default::default()
				};
			}
			for a in alerts {
				if a.urgent {
					tracing::error!(trade_id = %a.trade_id, "{}", crate::runner::describe(a));
				} else {
					tracing::warn!(trade_id = %a.trade_id, "{}", crate::runner::describe(a));
				}
				// Urgent alerts always go out: they mean money may already
				// have moved wrongly, and a flood of others must not bury them.
				if a.urgent || w.sent < self.max_per_minute {
					w.sent += 1;
					out.push(Message::from_alert(a));
				} else {
					w.held_back += 1;
					w.held_back_urgent |= a.urgent;
				}
			}
		}
		for m in &out {
			for c in &self.channels {
				if let Err(e) = c.send(m).await {
					tracing::error!(channel = c.name(), error = %e, "could not deliver an alert");
				}
			}
		}
		out.len()
	}
}
