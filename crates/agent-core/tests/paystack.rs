//! The Paystack rail against a mock Paystack: every response shape the
//! adapter handles, including the documented errors, and the opt-in
//! test-mode run against the real API.

use std::time::Duration;

use merchant_agent_core::machine::{PayoutStatus, SendOutcome};
use merchant_agent_core::money::Minor;
use merchant_agent_core::pii::{AccountNumber, HolderName};
use merchant_agent_core::rail::paystack::PaystackRail;
use merchant_agent_core::rail::{AccountLookup, PayoutRail, PayoutRequest};
use merchant_agent_core::secrets::Secret;
use serde_json::{json, Value};
use wiremock::matchers::{bearer_token, body_partial_json, method, path, query_param};
use wiremock::{Mock, MockServer, ResponseTemplate};

const KEY: &str = "sk_test_0000000000000000000000000000000000000000";
const REF: &str = "rpma-ft-7c1e9a2b4d6f48e0a1b2c3d4e5f60718";
const TRADE: &str = "ft-7c1e9a2b4d6f48e0a1b2c3d4e5f60718";
/// Obviously fake bank details.
const ACCOUNT: &str = "0000000000";
const NAME: &str = "Test Payee";

async fn mock() -> (MockServer, PaystackRail) {
	let server = MockServer::start().await;
	let rail = PaystackRail::with_base(
		Secret::new(KEY.into()),
		&server.uri(),
		Duration::from_secs(2),
	)
	.unwrap();
	(server, rail)
}

fn ok(data: Value) -> ResponseTemplate {
	ResponseTemplate::new(200).set_body_json(json!({"status": true, "message": "ok", "data": data}))
}

fn refused(code: u16, message: &str) -> ResponseTemplate {
	ResponseTemplate::new(code)
		.set_body_json(json!({"status": false, "message": message, "code": "validation_error"}))
}

/// A transfer as Paystack returns it, recipient details and all.
fn transfer(status: &str) -> Value {
	json!({
		"amount": 1_490_500, "currency": "NGN", "domain": "test", "failures": null, "id": 1, "integration": 1,
		"reason": "RecoilPay cash-out", "reference": REF, "source": "balance", "source_details": null,
		"status": status, "titan_code": null, "transfer_code": "TRF_test0000000000", "transferred_at": null,
		"recipient": {"recipient_code": "RCP_test0000000000", "name": NAME,
			"details": {"account_number": ACCOUNT, "account_name": NAME, "bank_code": "058", "bank_name": "Test Bank"}},
		"gateway_response": "Approved", "createdAt": "2026-11-14T10:00:00.000Z", "updatedAt": "2026-11-14T10:00:00.000Z"
	})
}

async fn send(
	rail: &PaystackRail,
	key: &str,
) -> Result<SendOutcome, merchant_agent_core::rail::RailError> {
	let account = AccountNumber::new(ACCOUNT.into());
	let name = HolderName::new(NAME.into());
	rail.send(PayoutRequest {
		idempotency_key: key,
		trade_id: TRADE,
		amount: Minor(1_490_500),
		bank_code: "058",
		account: &account,
		name: &name,
		narration: "RecoilPay cash-out",
	})
	.await
}

async fn not_yet_sent(server: &MockServer) {
	Mock::given(method("GET"))
		.and(path(format!("/transfer/verify/{REF}")))
		.respond_with(refused(404, "Transfer not found"))
		.up_to_n_times(1)
		.with_priority(1)
		.mount(server)
		.await;
}

async fn recipient_ok(server: &MockServer) {
	Mock::given(method("POST"))
		.and(path("/transferrecipient"))
		.and(body_partial_json(json!({"type": "nuban", "account_number": ACCOUNT, "bank_code": "058", "name": NAME, "currency": "NGN"})))
		.respond_with(ResponseTemplate::new(201).set_body_json(json!({"status": true, "message": "Transfer recipient created successfully",
			"data": {"recipient_code": "RCP_test0000000000", "details": {"account_number": ACCOUNT, "account_name": NAME}}})))
		.mount(server)
		.await;
}

// ── account lookup ───────────────────────────────────────────────────────

#[tokio::test]
async fn a_resolved_account_gives_the_banks_name() {
	let (server, rail) = mock().await;
	Mock::given(method("GET"))
		.and(path("/bank/resolve"))
		.and(query_param("account_number", ACCOUNT))
		.and(query_param("bank_code", "058"))
		.and(bearer_token(KEY))
		.respond_with(ok(
			json!({"account_number": ACCOUNT, "account_name": "TEST PAYEE", "bank_id": 9}),
		))
		.mount(&server)
		.await;
	match rail
		.verify_account("058", &AccountNumber::new(ACCOUNT.into()))
		.await
		.unwrap()
	{
		AccountLookup::Name(n) => assert_eq!(n.expose(), "TEST PAYEE"),
		_ => panic!("expected a name"),
	}
}

#[tokio::test]
async fn an_unresolvable_account_is_not_found() {
	let (server, rail) = mock().await;
	Mock::given(method("GET"))
		.and(path("/bank/resolve"))
		.respond_with(refused(
			422,
			"Could not resolve account name. Check parameters or try again.",
		))
		.mount(&server)
		.await;
	assert!(matches!(
		rail.verify_account("058", &AccountNumber::new(ACCOUNT.into()))
			.await
			.unwrap(),
		AccountLookup::NotFound
	));
}

#[tokio::test]
async fn a_refused_key_or_a_server_error_is_an_error_not_an_answer() {
	let (server, rail) = mock().await;
	Mock::given(method("GET"))
		.and(path("/bank/resolve"))
		.respond_with(refused(401, "Invalid key"))
		.mount(&server)
		.await;
	let e = rail
		.verify_account("058", &AccountNumber::new(ACCOUNT.into()))
		.await
		.err()
		.unwrap();
	assert!(e.to_string().contains("401"), "{e}");
	assert!(
		!e.to_string().contains(ACCOUNT),
		"the URL carries the account; errors must not"
	);
}

// ── sending ──────────────────────────────────────────────────────────────

#[tokio::test]
async fn a_transfer_uses_the_key_as_its_reference_and_is_accepted() {
	let (server, rail) = mock().await;
	not_yet_sent(&server).await;
	recipient_ok(&server).await;
	Mock::given(method("POST"))
		.and(path("/transfer"))
		.and(bearer_token(KEY))
		.and(body_partial_json(
			json!({"source": "balance", "amount": 1_490_500, "recipient": "RCP_test0000000000",
			"reference": REF, "reason": "RecoilPay cash-out", "currency": "NGN"}),
		))
		.respond_with(ok(transfer("pending")))
		.expect(1)
		.mount(&server)
		.await;
	assert_eq!(
		send(&rail, REF).await.unwrap(),
		SendOutcome::Accepted {
			reference: REF.into()
		}
	);
}

#[tokio::test]
async fn a_retry_of_a_transfer_paystack_already_has_never_initiates_again() {
	let (server, rail) = mock().await;
	Mock::given(method("GET"))
		.and(path(format!("/transfer/verify/{REF}")))
		.respond_with(ok(transfer("success")))
		.mount(&server)
		.await;
	Mock::given(method("POST"))
		.respond_with(ok(json!({})))
		.expect(0)
		.mount(&server)
		.await;
	assert_eq!(
		send(&rail, REF).await.unwrap(),
		SendOutcome::Accepted {
			reference: REF.into()
		}
	);
}

#[tokio::test]
async fn a_duplicate_reference_refusal_is_checked_not_taken_as_nothing_sent() {
	// The first answer was lost; the second send is refused as a duplicate.
	// That refusal means the transfer exists, so it is never "rejected".
	let (server, rail) = mock().await;
	not_yet_sent(&server).await;
	recipient_ok(&server).await;
	Mock::given(method("POST"))
		.and(path("/transfer"))
		.respond_with(refused(
			400,
			"Please provide a unique reference. Reference already exists on a transfer",
		))
		.mount(&server)
		.await;
	Mock::given(method("GET"))
		.and(path(format!("/transfer/verify/{REF}")))
		.respond_with(ok(transfer("success")))
		.with_priority(5)
		.mount(&server)
		.await;
	assert_eq!(
		send(&rail, REF).await.unwrap(),
		SendOutcome::Accepted {
			reference: REF.into()
		}
	);
}

#[tokio::test]
async fn a_documented_refusal_with_no_transfer_is_rejected_with_its_reason() {
	let (server, rail) = mock().await;
	Mock::given(method("GET"))
		.and(path(format!("/transfer/verify/{REF}")))
		.respond_with(refused(404, "Transfer not found"))
		.mount(&server)
		.await;
	recipient_ok(&server).await;
	Mock::given(method("POST"))
		.and(path("/transfer"))
		.respond_with(refused(
			400,
			"Your balance is not enough to fulfil this request",
		))
		.mount(&server)
		.await;
	match send(&rail, REF).await.unwrap() {
		SendOutcome::Rejected { reason } => {
			assert!(reason.contains("balance is not enough"), "{reason}")
		}
		other => panic!("expected a rejection, got {other:?}"),
	}
}

#[tokio::test]
async fn a_refused_recipient_is_rejected_and_no_transfer_is_tried() {
	let (server, rail) = mock().await;
	not_yet_sent(&server).await;
	Mock::given(method("POST"))
		.and(path("/transferrecipient"))
		.respond_with(refused(400, "Account number 0000000000 is invalid"))
		.mount(&server)
		.await;
	Mock::given(method("POST"))
		.and(path("/transfer"))
		.respond_with(ok(json!({})))
		.expect(0)
		.mount(&server)
		.await;
	match send(&rail, REF).await.unwrap() {
		SendOutcome::Rejected { reason } => {
			assert!(reason.contains("recipient"), "{reason}");
			assert!(
				!reason.contains(ACCOUNT),
				"the account number is masked: {reason}"
			);
		}
		other => panic!("expected a rejection, got {other:?}"),
	}
}

#[tokio::test]
async fn a_server_error_or_a_timeout_leaves_the_send_unknown() {
	let (server, rail) = mock().await;
	not_yet_sent(&server).await;
	recipient_ok(&server).await;
	Mock::given(method("POST"))
		.and(path("/transfer"))
		.respond_with(refused(500, "System Malfunction"))
		.up_to_n_times(1)
		.mount(&server)
		.await;
	assert!(send(&rail, REF).await.is_err(), "a 500 is not an answer");

	let (server, rail) = mock().await;
	not_yet_sent(&server).await;
	recipient_ok(&server).await;
	Mock::given(method("POST"))
		.and(path("/transfer"))
		.respond_with(ok(transfer("pending")).set_delay(Duration::from_secs(5)))
		.mount(&server)
		.await;
	let e = send(&rail, REF).await.err().unwrap();
	assert!(e.to_string().contains("timed out"), "{e}");
}

#[tokio::test]
async fn otp_at_initiation_is_unknown_never_accepted() {
	let (server, rail) = mock().await;
	not_yet_sent(&server).await;
	recipient_ok(&server).await;
	Mock::given(method("POST"))
		.and(path("/transfer"))
		.respond_with(ok(transfer("otp")))
		.mount(&server)
		.await;
	assert_eq!(send(&rail, REF).await.unwrap(), SendOutcome::Unknown);
}

#[tokio::test]
async fn a_different_amount_on_record_is_unknown() {
	let (server, rail) = mock().await;
	not_yet_sent(&server).await;
	recipient_ok(&server).await;
	let mut t = transfer("pending");
	t["amount"] = json!(999);
	Mock::given(method("POST"))
		.and(path("/transfer"))
		.respond_with(ok(t))
		.mount(&server)
		.await;
	assert_eq!(send(&rail, REF).await.unwrap(), SendOutcome::Unknown);
}

#[tokio::test]
async fn a_key_paystack_cannot_take_as_a_reference_is_rejected_before_any_call() {
	let (server, rail) = mock().await;
	Mock::given(wiremock::matchers::any())
		.respond_with(ok(json!({})))
		.expect(0)
		.mount(&server)
		.await;
	assert!(matches!(
		send(&rail, "RPMA-not.valid").await.unwrap(),
		SendOutcome::Rejected { .. }
	));
}

// ── status ───────────────────────────────────────────────────────────────

#[tokio::test]
async fn every_transfer_status_maps_to_what_it_means() {
	let cases = [
		("success", "succeeded"),
		("pending", "pending"),
		("received", "pending"),
		("queued", "pending"),
		("failed", "failed"),
		("reversed", "failed"),
		("abandoned", "failed"),
		("blocked", "failed"),
		("rejected", "failed"),
		("otp", "unknown"),
		("something_new", "unknown"),
	];
	for (status, want) in cases {
		let (server, rail) = mock().await;
		Mock::given(method("GET"))
			.and(path(format!("/transfer/verify/{REF}")))
			.respond_with(ok(transfer(status)))
			.mount(&server)
			.await;
		let got = match rail.status(REF).await.unwrap() {
			PayoutStatus::Succeeded { reference } => {
				assert_eq!(reference, REF);
				"succeeded"
			}
			PayoutStatus::Pending => "pending",
			PayoutStatus::Failed { reason } => {
				assert!(
					!reason.contains(ACCOUNT) && !reason.contains(NAME),
					"{reason}"
				);
				"failed"
			}
			PayoutStatus::NotFound => "not found",
			PayoutStatus::Unknown => "unknown",
		};
		assert_eq!(got, want, "status {status}");
	}
}

#[tokio::test]
async fn an_unknown_reference_is_not_found_as_a_404_or_a_400() {
	for code in [404, 400] {
		let (server, rail) = mock().await;
		Mock::given(method("GET"))
			.and(path(format!("/transfer/verify/{REF}")))
			.respond_with(refused(code, "Transfer not found"))
			.mount(&server)
			.await;
		assert_eq!(
			rail.status(REF).await.unwrap(),
			PayoutStatus::NotFound,
			"{code}"
		);
	}
}

#[tokio::test]
async fn a_reversal_says_the_money_came_back() {
	let (server, rail) = mock().await;
	Mock::given(method("GET"))
		.and(path(format!("/transfer/verify/{REF}")))
		.respond_with(ok(transfer("reversed")))
		.mount(&server)
		.await;
	match rail.status(REF).await.unwrap() {
		PayoutStatus::Failed { reason } => {
			assert!(reason.contains("refunded the balance"), "{reason}")
		}
		other => panic!("{other:?}"),
	}
}

// ── balance ──────────────────────────────────────────────────────────────

#[tokio::test]
async fn the_balance_is_the_ngn_entry() {
	let (server, rail) = mock().await;
	Mock::given(method("GET"))
		.and(path("/balance"))
		.respond_with(ok(
			json!([{"currency": "GHS", "balance": 5}, {"currency": "NGN", "balance": 123_456_789}]),
		))
		.mount(&server)
		.await;
	assert_eq!(rail.balance().await.unwrap(), Some(Minor(123_456_789)));

	let (server, rail) = mock().await;
	Mock::given(method("GET"))
		.and(path("/balance"))
		.respond_with(ok(json!([{"currency": "KES", "balance": 5}])))
		.mount(&server)
		.await;
	assert_eq!(rail.balance().await.unwrap(), None);
}

#[tokio::test]
async fn an_unexpected_answer_shape_is_an_error() {
	let (server, rail) = mock().await;
	Mock::given(method("GET"))
		.and(path("/balance"))
		.respond_with(ResponseTemplate::new(200).set_body_string("<html>maintenance</html>"))
		.mount(&server)
		.await;
	assert!(rail.balance().await.is_err());
}

// ── the real thing, in test mode ─────────────────────────────────────────

/// Sends one transfer in Paystack test mode, sends it again under the same
/// reference (it must not pay twice), and reads its status. Needs
/// `PAYSTACK_TEST_SECRET` (an `sk_test_` key; `PAYSTACK_TEST_SECRET_KEY` also works) and, because test mode
/// resolves recipients against real accounts, `PAYSTACK_TEST_BANK_CODE`,
/// `PAYSTACK_TEST_ACCOUNT` and `PAYSTACK_TEST_NAME`. Test mode moves no money.
///
///   cargo test -p merchant-agent-core --test paystack -- --ignored --nocapture
#[tokio::test]
#[ignore = "needs PAYSTACK_TEST_SECRET and a real account; run by hand"]
#[allow(clippy::disallowed_methods)] // a test reads its own environment
async fn test_mode_a_repeated_reference_never_pays_twice() {
	let env = |k: &str| std::env::var(k).unwrap_or_else(|_| panic!("set {k}"));
	let key = std::env::var("PAYSTACK_TEST_SECRET")
		.or_else(|_| std::env::var("PAYSTACK_TEST_SECRET_KEY"))
		.expect("set PAYSTACK_TEST_SECRET (or PAYSTACK_TEST_SECRET_KEY)");
	assert!(
		key.starts_with("sk_test_"),
		"PAYSTACK_TEST_SECRET must be a test key"
	);
	let rail_key = || key.clone();
	let rail = PaystackRail::new(Secret::new(key.clone())).unwrap();
	let (bank, account, name) = (
		env("PAYSTACK_TEST_BANK_CODE"),
		env("PAYSTACK_TEST_ACCOUNT"),
		env("PAYSTACK_TEST_NAME"),
	);
	let account = AccountNumber::new(account);
	let name = HolderName::new(name);

	match rail.verify_account(&bank, &account).await.unwrap() {
		AccountLookup::Name(n) => println!("resolved: {} characters of name", n.expose().len()),
		_ => panic!("Paystack could not resolve the test account"),
	}
	println!("balance before: {:?}", rail.balance().await.unwrap());

	let reference = format!(
		"rpma-ft-e2e{:x}",
		std::time::SystemTime::now()
			.duration_since(std::time::UNIX_EPOCH)
			.unwrap()
			.as_nanos()
	);
	let req = || PayoutRequest {
		idempotency_key: &reference,
		trade_id: "ft-e2e",
		amount: Minor(10_000),
		bank_code: &bank,
		account: &account,
		name: &name,
		narration: "RecoilPay agent test",
	};
	let first = rail.send(req()).await.unwrap();
	println!("first send: {first:?}");
	assert!(matches!(first, SendOutcome::Accepted { .. }), "{first:?}");

	// The adapter would see the transfer and not initiate; prove Paystack
	// itself refuses the reference too, by initiating again directly.
	let http = reqwest::Client::new();
	let recipient: Value = http
		.post("https://api.paystack.co/transferrecipient")
		.bearer_auth(rail_key())
		.json(&json!({"type": "nuban", "name": name.expose(), "account_number": account.expose(), "bank_code": bank, "currency": "NGN"}))
		.send().await.unwrap().json().await.unwrap();
	let code = recipient["data"]["recipient_code"]
		.as_str()
		.unwrap()
		.to_string();
	let again = http
		.post("https://api.paystack.co/transfer")
		.bearer_auth(rail_key())
		.json(&json!({"source": "balance", "amount": 10_000, "recipient": code, "reference": reference, "reason": "RecoilPay agent test", "currency": "NGN"}))
		.send().await.unwrap();
	let status = again.status();
	let body: Value = again.json().await.unwrap();
	println!(
		"second initiate with the same reference: {status} {}",
		body["message"]
	);
	assert!(
		!status.is_success() || body["status"] == false,
		"Paystack accepted a repeated reference: {body}"
	);

	let second = rail.send(req()).await.unwrap();
	println!("second send through the adapter: {second:?}");
	assert!(matches!(second, SendOutcome::Accepted { .. }));

	println!("status: {:?}", rail.status(&reference).await.unwrap());
}
