//! Paystack Transfers: the first real rail.
//!
//! Checked against Paystack's API spec (github.com/PaystackOSS/openapi,
//! `dist/paystack.yaml`, updated 2026-06-09) and its docs
//! (docs-v2.paystack.com, read 2026-11-14). The calls, in order:
//!
//! 1. **Resolve Account Number**, `GET /bank/resolve?account_number=&bank_code=`
//!    → `data.account_name`. For the name check (invariant 4).
//! 2. **Create Transfer Recipient**, `POST /transferrecipient`
//!    `{type: "nuban", name, account_number, bank_code, currency}` →
//!    `data.recipient_code`. The spec: "A duplicate account number will lead
//!    to the retrieval of the existing record", so this is safe to repeat.
//! 3. **Initiate Transfer**, `POST /transfer`
//!    `{source: "balance", amount, recipient, reference, reason, currency}`.
//! 4. **Verify Transfer**, `GET /transfer/verify/{reference}` → `data.status`.
//! 5. **Fetch Balance**, `GET /balance` → `data[] {currency, balance}`.
//!
//! **The reference is the idempotency key.** The spec's `reference`: "To
//! ensure idempotency, you need to provide a unique identifier for the
//! request. The identifier should be a lowercase alphanumeric string with
//! only -,_ symbols allowed." The single-transfers guide: "If there is an
//! error with the transfer request, kindly retry the transaction with the
//! same reference in order to avoid double crediting", and it asks for "at
//! least 16 alphanumeric characters". A second transfer with a used
//! reference is refused ("Please provide a unique reference...", in the
//! transfer errors list). So a repeated send can never pay twice, and the
//! agent's key (`rpma-ft-…`, lowercase) is used as the reference unchanged.
//!
//! **Statuses** (the docs name `pending`, `otp`, `success`, `received`, and
//! the `transfer.failed` / `transfer.reversed` events; "reversed" is sent
//! "when we refund a previously debited amount for a transfer that couldn't
//! be completed"):
//! - `success` → succeeded;
//! - `pending`, `received`, `queued`, `processing` → pending;
//! - `failed`, `reversed`, `abandoned`, `blocked`, `rejected` → failed;
//! - `otp` → unknown: Paystack is waiting for someone to type a one-time
//!   code, and the money moves the moment they do. The agent can't, and
//!   must not report it as paid, so a person decides. Turn off OTP for
//!   transfers in the Paystack dashboard to run unattended;
//! - anything else → unknown.
//!
//! **Personal data.** Recipient creation and the lookup need the account
//! number and name; nothing else does. Paystack's answers carry them back
//! (`data.recipient.details`): those fields are never deserialised, and
//! no body is ever logged or put in an error.
//!
//! Webhooks are not used: the agent accepts no inbound connections and asks
//! by reference instead.

use std::time::Duration;

use reqwest::{Method, StatusCode};
use serde::de::DeserializeOwned;
use serde::Deserialize;
use serde_json::json;

use super::{clean, AccountLookup, PayoutRail, PayoutRequest, RailError};
use crate::machine::{PayoutStatus, SendOutcome};
use crate::money::Minor;
use crate::pii::{AccountNumber, HolderName};
use crate::secrets::Secret;

pub const API: &str = "https://api.paystack.co";

/// Which kind of key this is. Sandbox takes only test keys and production
/// only live ones, so a sandbox can never move real money.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KeyMode {
	Test,
	Live,
}

impl KeyMode {
	pub fn of(key: &Secret) -> Option<Self> {
		let k = key.expose();
		if k.starts_with("sk_test_") {
			Some(Self::Test)
		} else if k.starts_with("sk_live_") {
			Some(Self::Live)
		} else {
			None
		}
	}
}

/// The key, if it suits the mode: sandbox takes only `sk_test_`, production
/// only `sk_live_`.
pub fn checked_key(key: Secret, mode: crate::config::Mode) -> Result<Secret, String> {
	use crate::config::Mode;
	match (KeyMode::of(&key), mode) {
		(Some(KeyMode::Test), Mode::Sandbox) | (Some(KeyMode::Live), Mode::Production) => Ok(key),
		(Some(KeyMode::Live), Mode::Sandbox) => {
			Err("rail.credentials is a live Paystack key, but this is sandbox mode: use an sk_test_ key".into())
		}
		(Some(KeyMode::Test), Mode::Production) => {
			Err("rail.credentials is a Paystack test key, but this is production: use the sk_live_ key".into())
		}
		(None, _) => Err("rail.credentials is not a Paystack secret key (sk_test_… or sk_live_…)".into()),
	}
}

/// Is `key` usable as a Paystack transfer reference? Lowercase letters,
/// digits, `-` and `_`; 16 to 100 characters.
pub fn valid_reference(key: &str) -> bool {
	(16..=100).contains(&key.len())
		&& key
			.bytes()
			.all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-' || b == b'_')
		&& key.bytes().filter(u8::is_ascii_alphanumeric).count() >= 16
}

pub struct PaystackRail {
	http: reqwest::Client,
	base: String,
	key: Secret,
}

/// Paystack's envelope: `{status, message, data}`.
#[derive(Deserialize)]
struct Envelope<T> {
	status: bool,
	#[serde(default)]
	message: String,
	data: Option<T>,
}

#[derive(Deserialize)]
struct Resolved {
	account_name: String,
}

#[derive(Deserialize)]
struct Recipient {
	recipient_code: String,
}

/// The transfer fields the agent reads; the recipient's details are not.
#[derive(Deserialize)]
struct Transfer {
	status: String,
	reference: Option<String>,
	#[serde(default)]
	amount: Option<u64>,
	#[serde(default)]
	gateway_response: Option<String>,
	#[serde(default)]
	reason: Option<String>,
}

#[derive(Deserialize)]
struct Balance {
	currency: String,
	balance: i64,
}

/// What an HTTP exchange came to, before it means anything to the rail.
enum Answer<T> {
	Ok(T),
	/// Paystack refused the request (a 4xx other than 401/429): no change
	/// was made. Its message, which never repeats personal data back.
	Refused(StatusCode, String),
}

impl PaystackRail {
	pub fn new(key: Secret) -> Result<Self, String> {
		Self::with_base(key, API, Duration::from_secs(30))
	}

	/// Against another base URL and timeout: the tests' mock server.
	pub fn with_base(key: Secret, base: &str, timeout: Duration) -> Result<Self, String> {
		let http = reqwest::Client::builder()
			.timeout(timeout)
			.connect_timeout(Duration::from_secs(10))
			.redirect(reqwest::redirect::Policy::none())
			.build()
			.map_err(|e| format!("could not build the Paystack client: {e}"))?;
		Ok(Self {
			http,
			base: base.trim_end_matches('/').to_string(),
			key,
		})
	}

	async fn call<T: DeserializeOwned>(
		&self,
		method: Method,
		path: &str,
		body: Option<serde_json::Value>,
	) -> Result<Answer<T>, RailError> {
		let mut req = self
			.http
			.request(method, format!("{}{path}", self.base))
			.bearer_auth(self.key.expose());
		if let Some(b) = body {
			req = req.json(&b);
		}
		// Errors from reqwest name the URL, which may carry an account
		// number in its query: say what kind of failure it was, nothing more.
		let res = req.send().await.map_err(|e| {
			RailError(if e.is_timeout() {
				"Paystack timed out".into()
			} else if e.is_connect() {
				"could not connect to Paystack".into()
			} else {
				"the request to Paystack failed".into()
			})
		})?;
		let status = res.status();
		let text = res
			.text()
			.await
			.map_err(|_| RailError("Paystack's answer was cut off".into()))?;
		if status == StatusCode::UNAUTHORIZED {
			return Err(RailError("Paystack refused the secret key (401)".into()));
		}
		if status == StatusCode::TOO_MANY_REQUESTS {
			return Err(RailError("Paystack is rate limiting (429)".into()));
		}
		if status.is_server_error() {
			return Err(RailError(format!("Paystack answered {status}")));
		}
		if status.is_client_error() {
			let message = serde_json::from_str::<Envelope<serde_json::Value>>(&text)
				.map(|e| e.message)
				.unwrap_or_default();
			return Ok(Answer::Refused(status, clean(&message)));
		}
		let env: Envelope<T> = serde_json::from_str(&text).map_err(|_| {
			RailError(format!(
				"Paystack's answer ({status}) was not the expected shape"
			))
		})?;
		match (env.status, env.data) {
			(true, Some(d)) => Ok(Answer::Ok(d)),
			_ => Ok(Answer::Refused(status, clean(&env.message))),
		}
	}

	async fn verify(&self, reference: &str) -> Result<Option<Transfer>, RailError> {
		match self
			.call::<Transfer>(Method::GET, &format!("/transfer/verify/{reference}"), None)
			.await?
		{
			Answer::Ok(t) => Ok(Some(t)),
			Answer::Refused(StatusCode::NOT_FOUND, _) => Ok(None),
			// "Transfer not found" can also come as a 400.
			Answer::Refused(_, m) if m.to_ascii_lowercase().contains("not found") => Ok(None),
			Answer::Refused(s, m) => Err(RailError(format!(
				"Paystack would not verify the transfer ({s}): {m}"
			))),
		}
	}
}

fn status_of(t: &Transfer, reference: &str) -> PayoutStatus {
	let why = || {
		clean(
			t.gateway_response
				.as_deref()
				.or(t.reason.as_deref())
				.unwrap_or("no reason given"),
		)
	};
	match t.status.as_str() {
		"success" => PayoutStatus::Succeeded {
			reference: t.reference.clone().unwrap_or_else(|| reference.to_string()),
		},
		"pending" | "received" | "queued" | "processing" => PayoutStatus::Pending,
		"reversed" => PayoutStatus::Failed {
			reason: format!(
				"Paystack reversed the transfer and refunded the balance ({})",
				why()
			),
		},
		"failed" | "abandoned" | "blocked" | "rejected" => PayoutStatus::Failed {
			reason: format!("Paystack says {} ({})", t.status, why()),
		},
		"otp" => {
			tracing::error!(
				reference,
				"Paystack is waiting for an OTP on this transfer; the agent can't enter one. Turn off OTP for transfers in the Paystack dashboard"
			);
			PayoutStatus::Unknown
		}
		other => {
			tracing::error!(
				reference,
				status = other,
				"Paystack gave a transfer status the agent does not know"
			);
			PayoutStatus::Unknown
		}
	}
}

impl PayoutRail for PaystackRail {
	fn idempotent_send(&self) -> bool {
		// The reference: a repeat is refused, never paid twice (module docs).
		true
	}

	async fn verify_account(
		&self,
		bank_code: &str,
		account: &AccountNumber,
	) -> Result<AccountLookup, RailError> {
		if !bank_code.bytes().all(|b| b.is_ascii_alphanumeric()) {
			return Ok(AccountLookup::NotFound);
		}
		let path = format!(
			"/bank/resolve?account_number={}&bank_code={bank_code}",
			account.expose()
		);
		match self.call::<Resolved>(Method::GET, &path, None).await? {
			Answer::Ok(r) => Ok(AccountLookup::Name(HolderName::new(r.account_name))),
			// "Could not resolve account name", "Account number is invalid",
			// "Bank code is invalid", "Cannot resolve account": all mean the
			// bank has no such account to pay.
			Answer::Refused(_, _) => Ok(AccountLookup::NotFound),
		}
	}

	async fn send(&self, req: PayoutRequest<'_>) -> Result<SendOutcome, RailError> {
		let reference = req.idempotency_key;
		if !valid_reference(reference) {
			return Ok(SendOutcome::Rejected {
				reason: "the payout key is not a valid Paystack reference".into(),
			});
		}
		let amount = u64::try_from(req.amount.get())
			.map_err(|_| RailError("the amount is too large for Paystack".into()))?;

		// A retry after a crash: if Paystack already has this reference, its
		// answer is the outcome; never initiate again.
		if let Some(t) = self.verify(reference).await? {
			return Ok(outcome_of(&t, reference));
		}

		let recipient = match self
			.call::<Recipient>(
				Method::POST,
				"/transferrecipient",
				Some(json!({
					"type": "nuban",
					"name": req.name.expose(),
					"account_number": req.account.expose(),
					"bank_code": req.bank_code,
					"currency": "NGN",
				})),
			)
			.await?
		{
			Answer::Ok(r) => r.recipient_code,
			Answer::Refused(s, m) => {
				return Ok(SendOutcome::Rejected {
					reason: format!("Paystack would not create the recipient ({s}): {m}"),
				})
			}
		};

		let initiated = self
			.call::<Transfer>(
				Method::POST,
				"/transfer",
				Some(json!({
					"source": "balance",
					"amount": amount,
					"recipient": recipient,
					"reference": reference,
					"reason": req.narration,
					"currency": "NGN",
				})),
			)
			.await?;
		match initiated {
			Answer::Ok(t) => {
				if t.amount.is_some_and(|a| a != amount) {
					tracing::error!(
						reference,
						"Paystack recorded a different amount for this transfer"
					);
					return Ok(SendOutcome::Unknown);
				}
				Ok(outcome_of(&t, reference))
			}
			// Refused: but a refusal can also mean "that reference is taken"
			// (a lost answer to an earlier send). Ask by reference before
			// saying no money moved.
			Answer::Refused(s, m) => match self.verify(reference).await? {
				Some(t) => Ok(outcome_of(&t, reference)),
				None => Ok(SendOutcome::Rejected {
					reason: format!("Paystack refused the transfer ({s}): {m}"),
				}),
			},
		}
	}

	async fn status(&self, idempotency_key: &str) -> Result<PayoutStatus, RailError> {
		if !valid_reference(idempotency_key) {
			return Ok(PayoutStatus::NotFound);
		}
		Ok(match self.verify(idempotency_key).await? {
			Some(t) => status_of(&t, idempotency_key),
			None => PayoutStatus::NotFound,
		})
	}

	async fn balance(&self) -> Result<Option<Minor>, RailError> {
		match self
			.call::<Vec<Balance>>(Method::GET, "/balance", None)
			.await?
		{
			Answer::Ok(list) => {
				let ngn = list.iter().find(|b| b.currency.eq_ignore_ascii_case("NGN"));
				Ok(ngn.map(|b| Minor(u128::try_from(b.balance.max(0)).unwrap_or(0))))
			}
			Answer::Refused(s, m) => Err(RailError(format!(
				"Paystack would not show the balance ({s}): {m}"
			))),
		}
	}
}

/// A send's outcome from the transfer Paystack holds under the reference.
fn outcome_of(t: &Transfer, reference: &str) -> SendOutcome {
	match status_of(t, reference) {
		PayoutStatus::Succeeded { reference } => SendOutcome::Accepted { reference },
		// Accepted and moving: the engine asks by reference until it settles.
		PayoutStatus::Pending => SendOutcome::Accepted {
			reference: reference.to_string(),
		},
		// Accepted, then failed: the engine's status check sees the failure
		// and hands it to a person, as for any bank.
		PayoutStatus::Failed { .. } => SendOutcome::Accepted {
			reference: reference.to_string(),
		},
		PayoutStatus::NotFound | PayoutStatus::Unknown => SendOutcome::Unknown,
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn the_agents_keys_are_valid_references() {
		assert!(valid_reference("rpma-ft-7c1e9a2b4d6f48e0a1b2c3d4e5f60718"));
		assert!(
			!valid_reference("rpma-FT-7c1e9a2b4d6f48e0a1b2c3d4e5f60718"),
			"uppercase"
		);
		assert!(!valid_reference("rpma-ft-7c1e9a2b.4d6f48e0"), "a dot");
		assert!(!valid_reference("rpma-ft-short"), "under 16");
		assert!(
			!valid_reference("----------------------"),
			"no alphanumerics"
		);
	}

	#[test]
	fn messages_are_kept_without_account_numbers() {
		assert_eq!(
			clean("Account 0123456789 is invalid"),
			"Account ********** is invalid"
		);
		assert_eq!(clean("Error 422"), "Error 422");
		assert_eq!(clean(""), "no message");
	}

	#[test]
	fn sandbox_takes_only_test_keys_and_production_only_live_ones() {
		use crate::config::Mode;
		let k = |s: &str| Secret::new(s.into());
		assert!(checked_key(k("sk_test_abc"), Mode::Sandbox).is_ok());
		assert!(checked_key(k("sk_live_abc"), Mode::Production).is_ok());
		assert!(checked_key(k("sk_live_abc"), Mode::Sandbox)
			.unwrap_err()
			.contains("sandbox"));
		assert!(checked_key(k("sk_test_abc"), Mode::Production)
			.unwrap_err()
			.contains("production"));
		let e = checked_key(k("pk_live_secret123"), Mode::Production).unwrap_err();
		assert!(!e.contains("secret123"), "never echo the key: {e}");
	}

	#[test]
	fn key_modes() {
		assert_eq!(
			KeyMode::of(&Secret::new("sk_test_abc".into())),
			Some(KeyMode::Test)
		);
		assert_eq!(
			KeyMode::of(&Secret::new("sk_live_abc".into())),
			Some(KeyMode::Live)
		);
		assert_eq!(KeyMode::of(&Secret::new("pk_test_abc".into())), None);
	}
}
