//! Alerts: what each channel sends, the rate limit, and that no alert ever
//! carries personal data.

use std::sync::{Arc, Mutex};

use axum::extract::State;
use axum::routing::post;
use chrono::{DateTime, Utc};
use merchant_agent_core::engine::Alert;
use merchant_agent_core::machine::{AgentState, HoldReason, PersonReason};
use merchant_agent_core::money::Minor;
use merchant_agent_core::notify::{Alerter, Email, Message, Notifier, PagerDuty, Webhook};
use merchant_agent_core::secrets::Secret;
use serde_json::Value;

/// What the agent knows and must never say.
const ACCOUNT: &str = "0000000000";
const NAME: &str = "Test Payee";

fn alert(urgent: bool, state: AgentState) -> Alert {
	Alert {
		trade_id: "ft-7c1e9a2b4d6f48e0a1b2c3d4e5f60718".into(),
		state,
		urgent,
		account_masked: Some("******0000".into()),
		amount: Minor(15_081_152),
		detail: Some("insufficient funds in the payout account".into()),
		purchase_refusal: None,
	}
}

async fn capture() -> (String, Arc<Mutex<Vec<Value>>>) {
	let seen = Arc::new(Mutex::new(Vec::new()));
	let app = axum::Router::new()
		.route(
			"/hook",
			post(
				|State(s): State<Arc<Mutex<Vec<Value>>>>, axum::Json(v): axum::Json<Value>| async move {
					s.lock().unwrap().push(v);
					"ok"
				},
			),
		)
		.with_state(seen.clone());
	let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
	let url = format!("http://{}/hook", listener.local_addr().unwrap());
	tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
	(url, seen)
}

fn t(s: &str) -> DateTime<Utc> {
	s.parse().unwrap()
}

#[test]
fn every_alert_says_what_happened_what_was_done_and_what_to_do() {
	let m = Message::from_alert(&alert(
		true,
		AgentState::NeedsPerson(PersonReason::BankUnknown),
	));
	assert!(m.title.starts_with("URGENT"));
	assert!(
		m.body.contains("can't say whether this transfer happened"),
		"what happened: {}",
		m.body
	);
	assert!(
		m.body.contains("stopped and will not move money"),
		"what it did: {}",
		m.body
	);
	assert!(
		m.body.contains("merchant-agent status"),
		"what to do: {}",
		m.body
	);
	assert!(
		m.body.contains("******0000") && m.body.contains("150,811.52"),
		"{}",
		m.body
	);
}

#[tokio::test]
async fn invariant_7_a_webhook_alert_carries_no_personal_data() {
	let (url, seen) = capture().await;
	let alerter = Alerter::new(vec![Box::new(Webhook::new(Secret::new(url)))], 20);
	alerter
		.deliver(
			&[alert(false, AgentState::Held(HoldReason::NameMismatch))],
			t("2026-11-13T12:00:00Z"),
		)
		.await;
	let sent = seen.lock().unwrap().clone();
	assert_eq!(sent.len(), 1);
	let text = sent[0]["text"].as_str().unwrap();
	assert!(text.contains("Held for review"), "{text}");
	assert!(!text.contains(ACCOUNT) && !text.contains(NAME), "{text}");
}

#[tokio::test]
async fn pagerduty_pages_only_for_urgent_alerts() {
	let (url, seen) = capture().await;
	let pd = PagerDuty::with_endpoint(Secret::new("test-routing-key".into()), &url);
	let alerter = Alerter::new(vec![Box::new(pd)], 20);
	alerter
		.deliver(
			&[
				alert(false, AgentState::Held(HoldReason::OverDailyLimit)),
				alert(true, AgentState::NeedsPerson(PersonReason::PaidButRefunded)),
			],
			t("2026-11-13T12:00:00Z"),
		)
		.await;
	let sent = seen.lock().unwrap().clone();
	assert_eq!(sent.len(), 1, "only the urgent one pages");
	assert_eq!(sent[0]["routing_key"], "test-routing-key");
	assert_eq!(sent[0]["event_action"], "trigger");
	assert_eq!(
		sent[0]["dedup_key"],
		"ft-7c1e9a2b4d6f48e0a1b2c3d4e5f60718:needs_person"
	);
	assert!(!sent[0].to_string().contains(ACCOUNT));
}

#[tokio::test]
async fn a_burst_is_rate_limited_and_summarised_never_dropped_silently() {
	let (url, seen) = capture().await;
	let alerter = Alerter::new(vec![Box::new(Webhook::new(Secret::new(url)))], 2);
	let burst: Vec<Alert> = (0..5)
		.map(|_| alert(false, AgentState::Held(HoldReason::Screening)))
		.collect();
	assert_eq!(alerter.deliver(&burst, t("2026-11-13T12:00:10Z")).await, 2);
	assert_eq!(seen.lock().unwrap().len(), 2);

	// The next minute opens with a summary of what was held back.
	alerter.deliver(&[], t("2026-11-13T12:01:00Z")).await;
	let sent = seen.lock().unwrap().clone();
	assert_eq!(sent.len(), 3);
	assert!(
		sent[2]["text"]
			.as_str()
			.unwrap()
			.contains("3 more alert(s) held back"),
		"{}",
		sent[2]
	);
}

#[test]
fn an_email_alert_is_plain_and_carries_no_personal_data() {
	let email = Email::new(
		&Secret::new("smtps://user:pass@smtp.example.com:465".into()),
		"agent@example.com",
		&["ops@example.com".into(), "oncall@example.com".into()],
	)
	.unwrap();
	let m = Message::from_alert(&alert(
		true,
		AgentState::NeedsPerson(PersonReason::BankFailed),
	));
	let raw = String::from_utf8(email.build(&m).unwrap().formatted()).unwrap();
	assert!(
		raw.contains("To: ops@example.com, oncall@example.com"),
		"{raw}"
	);
	assert!(raw.contains("URGENT"), "{raw}");
	assert!(!raw.contains(ACCOUNT) && !raw.contains(NAME), "{raw}");
	assert_eq!(email.name(), "email");
}

#[test]
fn a_bad_email_setting_is_refused_without_echoing_the_smtp_url() {
	let e = Email::new(
		&Secret::new("not a url with secret-password".into()),
		"agent@example.com",
		&[],
	)
	.err()
	.unwrap();
	assert!(!e.contains("secret-password"), "{e}");
}

#[tokio::test]
async fn an_urgent_alert_is_never_held_back_by_the_rate_limit() {
	let (url, seen) = capture().await;
	let alerter = Alerter::new(vec![Box::new(Webhook::new(Secret::new(url)))], 2);
	let mut burst: Vec<Alert> = (0..5)
		.map(|_| alert(false, AgentState::Held(HoldReason::Screening)))
		.collect();
	burst.push(alert(true, AgentState::Refunded));
	assert_eq!(alerter.deliver(&burst, t("2026-11-13T12:00:10Z")).await, 3);
	let sent = seen.lock().unwrap().clone();
	assert!(
		sent.iter().any(|m| m.to_string().contains("URGENT")),
		"the urgent alert went out: {sent:?}"
	);
}
