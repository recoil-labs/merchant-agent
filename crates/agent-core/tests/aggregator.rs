//! The aggregator client against an in-process fake aggregator.
//!
//! The fake serves responses built from `vectors/merchant-agent-api.json`,
//! whose shapes the aggregator's own `merchant_agent_contract` test pins to
//! the real handlers. Each test scripts the replies it needs and checks what
//! the client sent.
//!
//! One opt-in test at the end runs against a real local aggregator.

use std::collections::{HashMap, VecDeque};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use axum::body::Bytes;
use axum::extract::State;
use axum::http::{HeaderMap, Method, StatusCode, Uri};
use axum::response::{IntoResponse, Response};
use merchant_agent_core::aggregator::types::Direction;
use merchant_agent_core::aggregator::{
	AggregatorError, Client, ReportOutcome, RetryPolicy, TradeState,
};
use merchant_agent_core::secrets::Secret;
use serde_json::{json, Value};

const FIXTURE: &str = include_str!("../../../vectors/merchant-agent-api.json");
const SOLVER: &str = "solver-1111111111111111111111111111111111111111";
const KEY: &str = "test-api-key-not-real";
const TRADE_ID: &str = "ft-7c1e9a2b4d6f48e0a1b2c3d4e5f60718";

fn fixture(key: &str) -> Value {
	let all: Value = serde_json::from_str(FIXTURE).unwrap();
	all[key].clone()
}

/// An off-ramp trade as the operator's list returns it.
fn trade() -> Value {
	let mut t = fixture("ownFiatTrade");
	let m = t.as_object_mut().unwrap();
	m.remove("payer_name");
	for k in [
		"payment_reference",
		"proof_kind",
		"proof",
		"proof_payment_id",
		"release_tx_hash",
		"settled_at",
	] {
		m.insert(k.into(), Value::Null);
	}
	t
}

fn trade_with(edit: impl FnOnce(&mut serde_json::Map<String, Value>)) -> Value {
	let mut t = trade();
	edit(t.as_object_mut().unwrap());
	t
}

#[derive(Clone)]
enum Reply {
	Json(StatusCode, Value),
	Empty(StatusCode),
	Slow(Duration, Value),
	Redirect(String),
	Raw(StatusCode, Vec<u8>),
}

#[derive(Clone, Debug)]
struct Seen {
	method: String,
	path: String,
	api_key: Option<String>,
	body: String,
}

#[derive(Default)]
struct Fake {
	script: Mutex<HashMap<String, VecDeque<Reply>>>,
	seen: Mutex<Vec<Seen>>,
}

impl Fake {
	/// Queue replies for `"METHOD /path"`. The last one repeats.
	fn on(&self, route: &str, replies: Vec<Reply>) {
		self.script
			.lock()
			.unwrap()
			.insert(route.into(), replies.into());
	}

	fn seen(&self) -> Vec<Seen> {
		self.seen.lock().unwrap().clone()
	}

	fn count(&self, route: &str) -> usize {
		self.seen()
			.iter()
			.filter(|s| format!("{} {}", s.method, s.path) == route)
			.count()
	}
}

async fn handle(
	State(fake): State<Arc<Fake>>,
	method: Method,
	uri: Uri,
	headers: HeaderMap,
	body: Bytes,
) -> Response {
	let path = uri
		.path_and_query()
		.map(|p| p.as_str())
		.unwrap_or("")
		.to_string();
	fake.seen.lock().unwrap().push(Seen {
		method: method.to_string(),
		path: path.clone(),
		api_key: headers
			.get("x-api-key")
			.map(|v| v.to_str().unwrap().to_string()),
		body: String::from_utf8_lossy(&body).into_owned(),
	});
	let reply = {
		let mut script = fake.script.lock().unwrap();
		match script.get_mut(&format!("{method} {path}")) {
			Some(q) if q.len() > 1 => q.pop_front(),
			Some(q) => q.front().cloned(),
			None => None,
		}
	};
	match reply {
		Some(Reply::Json(s, v)) => (s, axum::Json(v)).into_response(),
		Some(Reply::Empty(s)) => s.into_response(),
		Some(Reply::Slow(d, v)) => {
			tokio::time::sleep(d).await;
			axum::Json(v).into_response()
		}
		Some(Reply::Redirect(to)) => (StatusCode::FOUND, [("location", to)]).into_response(),
		Some(Reply::Raw(s, b)) => (s, b).into_response(),
		None => (
			StatusCode::NOT_FOUND,
			axum::Json(json!({"error": "NO_ROUTE", "message": path, "timestamp": 0})),
		)
			.into_response(),
	}
}

async fn start() -> (Arc<Fake>, String) {
	let fake = Arc::new(Fake::default());
	let app = axum::Router::new()
		.fallback(handle)
		.with_state(fake.clone());
	let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
	let url = format!("http://{}", listener.local_addr().unwrap());
	tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
	(fake, url)
}

fn client(url: &str) -> Client {
	Client::new(
		url,
		SOLVER,
		Secret::new(KEY.into()),
		Duration::from_millis(500),
		false,
	)
	.unwrap()
	.with_retry(RetryPolicy {
		attempts: 3,
		base_delay: Duration::from_millis(5),
	})
}

fn own_trades_route() -> String {
	format!("GET /solver-api/operators/{SOLVER}/fiat-trades?state=awaiting_fiat")
}
fn trade_route() -> String {
	format!("GET /api/v1/fiat/trades/{TRADE_ID}")
}
fn report_route() -> String {
	format!("POST /api/v1/fiat/trades/{TRADE_ID}/payment-sent")
}
fn ok(v: Value) -> Reply {
	Reply::Json(StatusCode::OK, v)
}
fn err(status: u16, code: &str) -> Reply {
	Reply::Json(
		StatusCode::from_u16(status).unwrap(),
		json!({"error": code, "message": "test", "timestamp": 0}),
	)
}

// ── reads ────────────────────────────────────────────────────────────────

#[tokio::test]
async fn lists_own_trades_with_the_api_key() {
	let (fake, url) = start().await;
	fake.on(&own_trades_route(), vec![ok(json!([trade()]))]);
	let trades = client(&url)
		.list_own_trades(TradeState::AwaitingFiat)
		.await
		.unwrap();
	assert_eq!(trades.len(), 1);
	let t = &trades[0];
	assert_eq!(t.id, TRADE_ID);
	assert_eq!(t.direction, Direction::Offramp);
	assert_eq!(t.state, TradeState::AwaitingFiat);
	assert_eq!(t.asset_minor_units.get(), 100_000_000);
	assert_eq!(t.fiat_minor_units.get(), 15_081_152);
	assert_eq!(t.fee_minor_units.get(), 47_823);
	assert_eq!(t.rate.as_str(), "1520.50");
	assert!(t.payee_sealed.is_some());

	let seen = fake.seen();
	assert_eq!(seen.len(), 1);
	assert_eq!(seen[0].api_key.as_deref(), Some(KEY));
}

#[tokio::test]
async fn reads_the_full_fixtures_with_every_optional_field() {
	let (fake, url) = start().await;
	fake.on(
		&own_trades_route(),
		vec![ok(json!([fixture("ownFiatTrade")]))],
	);
	fake.on(&trade_route(), vec![ok(fixture("fiatTradeView"))]);
	let c = client(&url);
	assert_eq!(
		c.list_own_trades(TradeState::AwaitingFiat)
			.await
			.unwrap()
			.len(),
		1
	);
	let t = c.get_trade(TRADE_ID).await.unwrap();
	assert_eq!(t.payment_reference.as_deref(), Some("RP-TEST-0001"));
	// On-ramp-only personal data is discarded, never kept.
	assert_eq!(format!("{:?}", t.pay_to), "Some([discarded])");
	assert!(!format!("{t:?}").contains("Test Merchant"));
}

#[tokio::test]
async fn invariant_13_an_unknown_state_is_refused() {
	let (fake, url) = start().await;
	fake.on(
		&trade_route(),
		vec![ok(trade_with(|m| {
			m.insert("state".into(), json!("awaiting_crypto"));
		}))],
	);
	let e = client(&url).get_trade(TRADE_ID).await.unwrap_err();
	assert!(matches!(e, AggregatorError::Decode(_)), "{e}");
	assert_eq!(
		fake.count(&trade_route()),
		1,
		"a parse error is not retried"
	);
}

#[tokio::test]
async fn invariant_13_an_unknown_field_is_refused() {
	let (fake, url) = start().await;
	fake.on(
		&trade_route(),
		vec![ok(trade_with(|m| {
			m.insert("platform_fee_minor_units".into(), json!("100"));
		}))],
	);
	let e = client(&url).get_trade(TRADE_ID).await.unwrap_err();
	assert!(e.to_string().contains("platform_fee_minor_units"), "{e}");
}

#[tokio::test]
async fn invariant_13_malformed_amounts_are_refused() {
	for bad in [
		json!("1.5"),
		json!(100000000),
		json!("-5"),
		json!(""),
		json!("1e8"),
		json!(null),
	] {
		let (fake, url) = start().await;
		fake.on(
			&trade_route(),
			vec![ok(trade_with(|m| {
				m.insert("fiat_minor_units".into(), bad.clone());
			}))],
		);
		let e = client(&url).get_trade(TRADE_ID).await.unwrap_err();
		assert!(matches!(e, AggregatorError::Decode(_)), "{bad}: {e}");
	}
}

#[tokio::test]
async fn a_malformed_rate_or_direction_is_refused() {
	let (fake, url) = start().await;
	fake.on(
		&trade_route(),
		vec![ok(trade_with(|m| {
			m.insert("rate".into(), json!("1,520.50"));
		}))],
	);
	assert!(matches!(
		client(&url).get_trade(TRADE_ID).await,
		Err(AggregatorError::Decode(_))
	));

	let (fake, url) = start().await;
	fake.on(
		&trade_route(),
		vec![ok(trade_with(|m| {
			m.insert("direction".into(), json!("sideways"));
		}))],
	);
	assert!(matches!(
		client(&url).get_trade(TRADE_ID).await,
		Err(AggregatorError::Decode(_))
	));
}

#[tokio::test]
async fn a_401_is_unauthorized_and_not_retried() {
	let (fake, url) = start().await;
	// The auth middleware answers 401 with no body.
	fake.on(
		&own_trades_route(),
		vec![Reply::Empty(StatusCode::UNAUTHORIZED)],
	);
	let e = client(&url)
		.list_own_trades(TradeState::AwaitingFiat)
		.await
		.unwrap_err();
	assert!(matches!(e, AggregatorError::Unauthorized), "{e}");
	assert_eq!(fake.count(&own_trades_route()), 1);
}

#[tokio::test]
async fn a_404_and_a_409_carry_the_aggregators_code() {
	let (fake, url) = start().await;
	fake.on(&trade_route(), vec![err(404, "NO_SUCH_TRADE")]);
	let e = client(&url).get_trade(TRADE_ID).await.unwrap_err();
	assert!(matches!(e, AggregatorError::NotFound(_)));
	assert_eq!(e.code(), Some("NO_SUCH_TRADE"));
	assert_eq!(fake.count(&trade_route()), 1, "a 404 is not retried");

	let route = format!("PUT /solver-api/operators/{SOLVER}/fiat-quotes");
	fake.on(&route, vec![err(409, "SOMETHING")]);
	let e = client(&url).put_quotes(&[]).await.unwrap_err();
	assert_eq!(e.code(), Some("SOMETHING"));
}

#[tokio::test]
async fn a_slow_server_times_out_after_bounded_retries() {
	let (fake, url) = start().await;
	fake.on(
		&trade_route(),
		vec![Reply::Slow(Duration::from_secs(3), trade())],
	);
	let started = Instant::now();
	let e = client(&url).get_trade(TRADE_ID).await.unwrap_err();
	assert!(matches!(e, AggregatorError::Timeout), "{e}");
	assert_eq!(
		fake.count(&trade_route()),
		3,
		"three attempts, then give up"
	);
	assert!(
		started.elapsed() < Duration::from_secs(3),
		"{:?}",
		started.elapsed()
	);
}

#[tokio::test]
async fn a_server_error_on_a_read_is_retried() {
	let (fake, url) = start().await;
	fake.on(
		&trade_route(),
		vec![Reply::Empty(StatusCode::BAD_GATEWAY), ok(trade())],
	);
	assert!(client(&url).get_trade(TRADE_ID).await.is_ok());
	assert_eq!(fake.count(&trade_route()), 2);
}

#[tokio::test]
async fn invariant_9_redirects_are_never_followed() {
	let (target, target_url) = start().await;
	let (fake, url) = start().await;
	fake.on(
		&trade_route(),
		vec![Reply::Redirect(format!(
			"{target_url}/api/v1/fiat/trades/{TRADE_ID}"
		))],
	);
	let e = client(&url).get_trade(TRADE_ID).await.unwrap_err();
	assert!(matches!(e, AggregatorError::UnexpectedStatus(302)), "{e}");
	assert!(
		target.seen().is_empty(),
		"the API key must not follow a redirect"
	);
}

#[tokio::test]
async fn invariant_9_a_production_client_refuses_plain_http() {
	let (fake, url) = start().await;
	let c = Client::new(
		&url,
		SOLVER,
		Secret::new(KEY.into()),
		Duration::from_millis(500),
		true,
	)
	.unwrap();
	assert!(c.get_trade(TRADE_ID).await.is_err());
	assert!(fake.seen().is_empty());
}

#[tokio::test]
async fn an_oversized_response_is_refused() {
	let (fake, url) = start().await;
	let huge = vec![b' '; merchant_agent_core::aggregator::MAX_BODY_BYTES + 1];
	fake.on(&trade_route(), vec![Reply::Raw(StatusCode::OK, huge)]);
	assert!(matches!(
		client(&url).get_trade(TRADE_ID).await,
		Err(AggregatorError::TooLarge)
	));
}

#[tokio::test]
async fn ids_that_could_change_the_url_are_refused_locally() {
	let (fake, url) = start().await;
	for bad in ["../admin", "ft-1?x=1", "ft 1", ""] {
		assert!(
			matches!(
				client(&url).get_trade(bad).await,
				Err(AggregatorError::InvalidInput(_))
			),
			"{bad}"
		);
	}
	assert!(fake.seen().is_empty());
}

#[tokio::test]
async fn the_payee_key_is_read_or_absent() {
	let (fake, url) = start().await;
	let route = format!("GET /solver-api/operators/{SOLVER}/fiat-payee-key");
	fake.on(&route, vec![ok(fixture("fiatPayeeKey"))]);
	let key = client(&url).get_payee_key().await.unwrap();
	assert_eq!(
		key.as_deref(),
		fixture("fiatPayeeKey")["publicKey"].as_str()
	);

	fake.on(&route, vec![err(404, "NO_KEY")]);
	assert_eq!(client(&url).get_payee_key().await.unwrap(), None);

	fake.on(&route, vec![err(404, "NOT_FOUND")]);
	assert!(
		client(&url).get_payee_key().await.is_err(),
		"only NO_KEY means no key"
	);
}

#[tokio::test]
async fn quotes_are_strict_and_chains_are_lenient() {
	let (fake, url) = start().await;
	let quotes = format!("GET /solver-api/operators/{SOLVER}/fiat-quotes");
	fake.on(&quotes, vec![ok(fixture("fiatQuotes"))]);
	let q = client(&url).get_quotes().await.unwrap();
	assert_eq!(q[0].fee_fixed.get(), 10_000);
	assert_eq!(q[0].fee_bps, 25);

	let mut unknown = fixture("fiatQuotes");
	unknown["quotes"][0]["fee_kind"] = json!("new");
	fake.on(&quotes, vec![ok(unknown)]);
	assert!(matches!(
		client(&url).get_quotes().await,
		Err(AggregatorError::Decode(_))
	));

	let mut chains = fixture("chains");
	chains["data"][0]["some_new_contract"] = json!("0x00");
	fake.on("GET /api/v1/chains", vec![ok(chains)]);
	let c = client(&url).get_chains().await.unwrap();
	assert_eq!(
		c[0].fiat_escrow,
		"0x112a534104ac10E73D57f7C5a4C6469632392173"
	);
	assert_eq!(c[0].tokens[0].symbol, "USDC");
}

#[tokio::test]
async fn the_clock_skew_is_read_from_the_date_header() {
	let (fake, url) = start().await;
	fake.on("GET /health", vec![Reply::Empty(StatusCode::OK)]);
	let skew = client(&url).clock_skew().await.unwrap();
	assert!(skew.num_seconds().abs() <= 2, "{skew:?}");
}

// ── writes ───────────────────────────────────────────────────────────────

#[tokio::test]
async fn payment_sent_sends_the_reference() {
	let (fake, url) = start().await;
	let reported = trade_with(|m| {
		m.insert("state".into(), json!("awaiting_confirmation"));
		m.insert("payment_reference".into(), json!("PSK-REF-0001"));
	});
	fake.on(&report_route(), vec![ok(reported)]);
	let out = client(&url)
		.report_payment(TRADE_ID, "  PSK-REF-0001 ")
		.await
		.unwrap();
	assert!(matches!(out, ReportOutcome::Reported(_)));
	let seen = fake.seen();
	assert_eq!(seen.len(), 1);
	assert_eq!(seen[0].api_key.as_deref(), Some(KEY));
	let body: Value = serde_json::from_str(&seen[0].body).unwrap();
	assert_eq!(body, json!({"paymentReference": "PSK-REF-0001"}));
}

#[tokio::test]
async fn a_reference_the_aggregator_would_refuse_is_refused_before_sending() {
	let (fake, url) = start().await;
	let c = client(&url);
	assert!(
		c.report_payment(TRADE_ID, &"R".repeat(64)).await.is_err(),
		"64 is allowed but no route is scripted"
	);
	let before = fake.seen().len();
	for bad in [
		"R".repeat(65),
		String::new(),
		"   ".into(),
		"REF\u{00e9}".into(),
		"REF\n1".into(),
	] {
		let e = c.report_payment(TRADE_ID, &bad).await.unwrap_err();
		assert!(
			matches!(e, AggregatorError::InvalidInput(_)),
			"{bad:?}: {e}"
		);
	}
	assert_eq!(
		fake.seen().len(),
		before,
		"nothing was sent for a bad reference"
	);
}

#[tokio::test]
async fn a_report_that_timed_out_but_landed_is_not_sent_again() {
	let (fake, url) = start().await;
	let landed = trade_with(|m| {
		m.insert("state".into(), json!("awaiting_confirmation"));
		m.insert("payment_reference".into(), json!("PSK-REF-0001"));
	});
	fake.on(
		&report_route(),
		vec![Reply::Slow(Duration::from_secs(3), landed.clone())],
	);
	fake.on(&trade_route(), vec![ok(landed)]);
	let out = client(&url)
		.report_payment(TRADE_ID, "PSK-REF-0001")
		.await
		.unwrap();
	assert!(matches!(out, ReportOutcome::Reported(_)));
	assert_eq!(
		fake.count(&report_route()),
		1,
		"the re-read showed it landed; no second report"
	);
}

#[tokio::test]
async fn a_report_that_failed_is_sent_again_only_after_the_re_read_says_so() {
	let (fake, url) = start().await;
	let landed = trade_with(|m| {
		m.insert("state".into(), json!("awaiting_confirmation"));
		m.insert("payment_reference".into(), json!("PSK-REF-0001"));
	});
	fake.on(
		&report_route(),
		vec![Reply::Empty(StatusCode::INTERNAL_SERVER_ERROR), ok(landed)],
	);
	fake.on(&trade_route(), vec![ok(trade())]);
	let out = client(&url)
		.report_payment(TRADE_ID, "PSK-REF-0001")
		.await
		.unwrap();
	assert!(matches!(out, ReportOutcome::Reported(_)));
	assert_eq!(fake.count(&report_route()), 2);
	assert_eq!(fake.count(&trade_route()), 1);
}

#[tokio::test]
async fn a_report_on_a_refunded_trade_goes_to_a_person() {
	let (fake, url) = start().await;
	fake.on(&report_route(), vec![err(409, "TRADE_STATE")]);
	fake.on(
		&trade_route(),
		vec![ok(trade_with(|m| {
			m.insert("state".into(), json!("refunded_to_funder"));
		}))],
	);
	let out = client(&url)
		.report_payment(TRADE_ID, "PSK-REF-0001")
		.await
		.unwrap();
	let ReportOutcome::NotReportable(t) = out else {
		panic!("expected NotReportable")
	};
	assert_eq!(t.state, TradeState::RefundedToFunder);
	assert_eq!(fake.count(&report_route()), 1);
}

#[tokio::test]
async fn a_trade_holding_another_reference_is_not_ours_to_claim() {
	let (fake, url) = start().await;
	fake.on(&report_route(), vec![err(409, "TRADE_STATE")]);
	fake.on(
		&trade_route(),
		vec![ok(trade_with(|m| {
			m.insert("state".into(), json!("awaiting_confirmation"));
			m.insert("payment_reference".into(), json!("SOMEONE-ELSE"));
		}))],
	);
	let out = client(&url)
		.report_payment(TRADE_ID, "PSK-REF-0001")
		.await
		.unwrap();
	assert!(matches!(out, ReportOutcome::NotReportable(_)));
}

#[tokio::test]
async fn a_refused_report_is_an_error_without_a_re_read() {
	let (fake, url) = start().await;
	fake.on(
		&report_route(),
		vec![Reply::Empty(StatusCode::UNAUTHORIZED)],
	);
	let e = client(&url)
		.report_payment(TRADE_ID, "PSK-REF-0001")
		.await
		.unwrap_err();
	assert!(matches!(e, AggregatorError::Unauthorized));
	assert_eq!(fake.count(&trade_route()), 0);
}

#[tokio::test]
async fn a_report_that_never_takes_effect_gives_up() {
	let (fake, url) = start().await;
	fake.on(
		&report_route(),
		vec![Reply::Empty(StatusCode::SERVICE_UNAVAILABLE)],
	);
	fake.on(&trade_route(), vec![ok(trade())]);
	assert!(client(&url)
		.report_payment(TRADE_ID, "PSK-REF-0001")
		.await
		.is_err());
	assert_eq!(fake.count(&report_route()), 3, "bounded");
}

#[tokio::test]
async fn pausing_puts_an_empty_book() {
	let (fake, url) = start().await;
	let route = format!("PUT /solver-api/operators/{SOLVER}/fiat-quotes");
	fake.on(&route, vec![ok(json!({"quotes": []}))]);
	assert!(client(&url).put_quotes(&[]).await.unwrap().is_empty());
	let body: Value = serde_json::from_str(&fake.seen()[0].body).unwrap();
	assert_eq!(body, json!({"quotes": []}));
}

#[tokio::test]
async fn a_quote_round_trips_into_the_input_shape() {
	let q: merchant_agent_core::aggregator::Quote =
		serde_json::from_value(fixture("fiatQuotes")["quotes"][0].clone()).unwrap();
	let input =
		serde_json::to_value(merchant_agent_core::aggregator::QuoteInput::from(&q)).unwrap();
	assert_eq!(input["assetDecimals"], 6);
	assert_eq!(input["feeFixed"], "10000");
	assert_eq!(input["kycTierRequired"], "none");
	assert_eq!(input["expiry"], 1_795_132_800); // 2026-11-20T00:00:00Z
}

#[tokio::test]
async fn the_heartbeat_posts_to_the_operator() {
	let (fake, url) = start().await;
	let route = format!("POST /solver-api/operators/{SOLVER}/heartbeat");
	fake.on(&route, vec![Reply::Empty(StatusCode::NO_CONTENT)]);
	client(&url).heartbeat().await.unwrap();
	assert_eq!(fake.count(&route), 1);
}

#[tokio::test]
async fn a_statement_carries_the_reference_and_is_checked_first() {
	let (fake, url) = start().await;
	let route = format!("POST /api/v1/fiat/trades/{TRADE_ID}/evidence");
	fake.on(&route, vec![Reply::Json(StatusCode::CREATED, json!({}))]);
	let c = client(&url);
	c.add_statement(
		TRADE_ID,
		"Paid ******0000 by bank transfer.",
		Some("PSK-REF-0001"),
	)
	.await
	.unwrap();
	let body: Value = serde_json::from_str(&fake.seen()[0].body).unwrap();
	assert_eq!(
		body,
		json!({"kind": "statement", "text": "Paid ******0000 by bank transfer.", "reference": "PSK-REF-0001"})
	);
	assert!(c.add_statement(TRADE_ID, "", None).await.is_err());
	assert!(c
		.add_statement(TRADE_ID, "ok", Some("has space"))
		.await
		.is_err());
	assert_eq!(fake.count(&route), 1);
}

// ── opt-in: a real local aggregator ──────────────────────────────────────

/// Runs against a local aggregator. Start one (in-memory store, loopback
/// solvers allowed) and point `AGGREGATOR_URL` at it:
///
/// ```text
/// AGGREGATOR_URL=http://127.0.0.1:3999 cargo test -p merchant-agent-core \
///   --test aggregator -- --ignored real_aggregator
/// ```
///
/// It registers a throwaway operator with a throwaway key, so never point it
/// at a shared aggregator.
#[tokio::test]
#[ignore = "needs AGGREGATOR_URL pointing at a local aggregator"]
async fn real_aggregator() {
	#[allow(clippy::disallowed_methods)]
	let url = std::env::var("AGGREGATOR_URL").expect("set AGGREGATOR_URL");
	assert!(
		url.contains("127.0.0.1") || url.contains("localhost"),
		"only a local aggregator: this registers operators"
	);
	let (solver_id, api_key) = register(&url).await;
	let c = Client::new(
		&url,
		&solver_id,
		Secret::new(api_key.clone()),
		Duration::from_secs(10),
		false,
	)
	.unwrap();

	assert!(c
		.list_own_trades(TradeState::AwaitingFiat)
		.await
		.unwrap()
		.is_empty());
	assert_eq!(c.get_payee_key().await.unwrap(), None);
	assert!(c.get_quotes().await.unwrap().is_empty());
	assert!(c.put_quotes(&[]).await.unwrap().is_empty());
	c.heartbeat().await.unwrap();
	assert!(c.clock_skew().await.unwrap().num_seconds().abs() <= 2);

	let chains = c.get_chains().await.unwrap();
	let base = chains
		.iter()
		.find(|ch| ch.chain_id == 84532)
		.expect("Base Sepolia is registered");
	assert_eq!(
		base.fiat_escrow.to_lowercase(),
		"0x112a534104ac10e73d57f7c5a4c6469632392173"
	);

	let e = c
		.get_trade("ft-00000000000000000000000000000000")
		.await
		.unwrap_err();
	assert_eq!(e.code(), Some("NO_SUCH_TRADE"));
	let e = c
		.report_payment("ft-00000000000000000000000000000000", "REF-1")
		.await
		.unwrap_err();
	assert_eq!(e.code(), Some("NO_SUCH_TRADE"));

	// The payee key the agent reads is the one the operator registered.
	let public =
		merchant_agent_core::payee::PayeePrivateKey::from_hex(&format!("0x{}", "22".repeat(32)))
			.unwrap()
			.public_key_hex();
	let http = reqwest::Client::new();
	let put = http
		.put(format!(
			"{url}/solver-api/operators/{solver_id}/fiat-payee-key"
		))
		.header("x-api-key", &api_key)
		.json(&json!({ "publicKey": public }))
		.send()
		.await
		.unwrap();
	assert!(put.status().is_success(), "{}", put.status());
	assert_eq!(c.get_payee_key().await.unwrap(), Some(public));

	let wrong = Client::new(
		&url,
		&solver_id,
		Secret::new("not-the-key".into()),
		Duration::from_secs(10),
		false,
	)
	.unwrap();
	assert!(matches!(
		wrong.get_quotes().await,
		Err(AggregatorError::Unauthorized)
	));
}

/// Register a throwaway operator: sign the registration message with a
/// random key (EIP-191) and return its solver id and API key.
async fn register(url: &str) -> (String, String) {
	use k256::ecdsa::SigningKey;
	use rand::Rng;
	use sha3::{Digest, Keccak256};

	let key = loop {
		let bytes: [u8; 32] = rand::rng().random();
		if let Ok(k) = SigningKey::from_slice(&bytes) {
			break k;
		}
	};
	let point = key.verifying_key().to_sec1_point(false);
	let hash = Keccak256::digest(&point.as_bytes()[1..]);
	let address = format!("0x{}", hex::encode(&hash[12..]));

	let http = reqwest::Client::new();
	let challenge: Value = http
		.get(format!("{url}/api/v1/solver/register/message"))
		.send()
		.await
		.unwrap()
		.json()
		.await
		.unwrap();
	let message = challenge["data"]["message"].as_str().unwrap().to_string();
	let prefixed = format!("\x19Ethereum Signed Message:\n{}{message}", message.len());
	let digest = Keccak256::digest(prefixed.as_bytes());
	let (sig, recid) = key.sign_prehash_recoverable(&digest);
	let mut bytes = sig.to_bytes().to_vec();
	bytes.push(27 + recid.to_byte());

	let reg: Value = http
		.post(format!("{url}/solver-api/account/register"))
		.json(&json!({
			"address": address,
			"message": message,
			"signature": format!("0x{}", hex::encode(bytes)),
			"endpoint": "http://127.0.0.1:4099",
		}))
		.send()
		.await
		.unwrap()
		.json()
		.await
		.unwrap();
	(
		reg["solverId"]
			.as_str()
			.unwrap_or_else(|| panic!("registration: {reg}"))
			.to_string(),
		reg["apiKey"].as_str().unwrap().to_string(),
	)
}
