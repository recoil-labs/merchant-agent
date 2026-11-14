//! Console logins: who may see and do what.
//!
//! - **Passwords** are argon2id hashes (the `argon2` crate's defaults).
//! - **A second factor is required:** a TOTP code (RFC 6238, 6 digits, 30
//!   s, one step of drift either way). Each accepted step is remembered, so
//!   a code can't be replayed.
//! - **TOTP secrets are encrypted at rest** with AES-256-GCM, under a key
//!   derived (HKDF) from the payee tag pepper. A copy of the database alone
//!   doesn't give away anyone's second factor.
//! - **Lockout:** [`MAX_FAILURES`] wrong attempts lock the account for
//!   [`LOCKOUT`]. A missing user costs the same argon2 work as a real one,
//!   so timing doesn't reveal who exists.
//! - **Sessions** are random 256-bit tokens held in memory, ended after
//!   [`IDLE`] without activity. Every form carries the session's CSRF token.
//! - **Roles**, each including the ones before: `viewer` sees; `operator`
//!   handles exceptions and pauses; `approver` approves payouts and resumes
//!   after an automatic pause; `admin` manages users.

use std::collections::HashMap;
use std::fmt;
use std::sync::Mutex;

use aes_gcm::aead::{Aead, KeyInit};
use aes_gcm::{Aes256Gcm, Nonce};
use argon2::password_hash::{
	rand_core::OsRng, PasswordHash, PasswordHasher, PasswordVerifier, SaltString,
};
use argon2::Argon2;
use chrono::{DateTime, Duration, Utc};
use rand::Rng;
use subtle::ConstantTimeEq;
use totp_rs::{Algorithm, TOTP};
use zeroize::Zeroizing;

use crate::secrets::Secret;
use crate::store::{Store, StoreError, UserRow};

pub const MAX_FAILURES: u32 = 5;
pub const LOCKOUT: Duration = Duration::minutes(15);
pub const IDLE: Duration = Duration::minutes(15);
/// A session ends this long after sign-in however active it is, so a
/// page left open (they refresh themselves) can't keep it alive forever.
pub const MAX_SESSION: Duration = Duration::hours(12);
pub const MIN_PASSWORD_CHARS: usize = 12;
const TOTP_STEP: u64 = 30;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Role {
	Viewer,
	Operator,
	Approver,
	Admin,
}

impl Role {
	pub fn parse(s: &str) -> Option<Self> {
		Some(match s {
			"viewer" => Self::Viewer,
			"operator" => Self::Operator,
			"approver" => Self::Approver,
			"admin" => Self::Admin,
			_ => return None,
		})
	}

	pub fn as_str(&self) -> &'static str {
		match self {
			Self::Viewer => "viewer",
			Self::Operator => "operator",
			Self::Approver => "approver",
			Self::Admin => "admin",
		}
	}
}

impl fmt::Display for Role {
	fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
		f.write_str(self.as_str())
	}
}

/// The key TOTP secrets are encrypted under at rest.
pub struct TotpKey(Zeroizing<[u8; 32]>);

impl TotpKey {
	pub fn from_pepper(pepper: &Secret) -> Self {
		let mut key = Zeroizing::new([0u8; 32]);
		hkdf::Hkdf::<sha2::Sha256>::new(None, pepper.expose().as_bytes())
			.expand(b"recoilpay/merchant-agent/totp-at-rest/v1", &mut key[..])
			.expect("32 bytes is a valid HKDF output");
		Self(key)
	}

	fn cipher(&self) -> Aes256Gcm {
		Aes256Gcm::new_from_slice(&self.0[..]).expect("a 32-byte key")
	}

	pub fn seal(&self, secret: &[u8]) -> String {
		let nonce: [u8; 12] = rand::rng().random();
		let ct = self
			.cipher()
			.encrypt(Nonce::from_slice(&nonce), secret)
			.expect("encryption succeeds");
		format!("{}{}", hex::encode(nonce), hex::encode(ct))
	}

	pub fn open(&self, sealed: &str) -> Option<Zeroizing<Vec<u8>>> {
		let bytes = hex::decode(sealed).ok()?;
		if bytes.len() < 12 + 16 {
			return None;
		}
		let (nonce, ct) = bytes.split_at(12);
		self.cipher()
			.decrypt(Nonce::from_slice(nonce), ct)
			.ok()
			.map(Zeroizing::new)
	}
}

fn totp(secret: Vec<u8>, username: &str) -> TOTP {
	TOTP::new_unchecked(
		Algorithm::SHA1,
		6,
		1,
		TOTP_STEP,
		secret,
		Some("RecoilPay Merchant Agent".into()),
		username.into(),
	)
}

/// What a new user needs to set up their authenticator app. Shown once.
pub struct Enrolment {
	/// `otpauth://` URI for a QR code.
	pub uri: Zeroizing<String>,
	/// The same secret, for typing in by hand.
	pub secret_base32: Zeroizing<String>,
}

fn valid_username(s: &str) -> bool {
	(1..=64).contains(&s.len())
		&& s.chars()
			.all(|c| c.is_ascii_alphanumeric() || "._@-".contains(c))
}

pub fn hash_password(password: &str) -> Result<String, String> {
	if password.chars().count() < MIN_PASSWORD_CHARS {
		return Err(format!(
			"a password needs at least {MIN_PASSWORD_CHARS} characters"
		));
	}
	let salt = SaltString::generate(&mut OsRng);
	Argon2::default()
		.hash_password(password.as_bytes(), &salt)
		.map(|h| h.to_string())
		.map_err(|e| e.to_string())
}

/// Add a user with a new TOTP secret. Returns what to show them, once.
pub fn add_user(
	store: &Store,
	key: &TotpKey,
	username: &str,
	role: Role,
	password: &str,
	now: DateTime<Utc>,
) -> Result<Enrolment, String> {
	if !valid_username(username) {
		return Err("a username is 1 to 64 letters, digits or . _ @ -".into());
	}
	let password_hash = hash_password(password)?;
	let secret: Zeroizing<[u8; 20]> = Zeroizing::new(rand::rng().random());
	let t = totp(secret.to_vec(), username);
	let row = UserRow {
		username: username.into(),
		role: role.as_str().into(),
		password_hash,
		totp_sealed: key.seal(&secret[..]),
		last_totp_step: 0,
		failed_attempts: 0,
		locked_until: None,
		disabled: false,
	};
	if !store.insert_user(&row, now).map_err(|e| e.to_string())? {
		return Err(format!("{username} already exists"));
	}
	Ok(Enrolment {
		uri: Zeroizing::new(t.get_url()),
		secret_base32: Zeroizing::new(t.get_secret_base32()),
	})
}

/// The current code for a user. Only for `users add`'s confirmation step
/// and tests.
pub fn current_code(
	store: &Store,
	key: &TotpKey,
	username: &str,
	now: DateTime<Utc>,
) -> Option<String> {
	let user = store.user(username).ok()??;
	let secret = key.open(&user.totp_sealed)?;
	Some(totp(secret.to_vec(), username).generate(now.timestamp().max(0) as u64))
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum LoginError {
	/// Deliberately vague: never says which part was wrong.
	#[error(
		"wrong username, password or code (5 wrong tries in a row lock an account for 15 minutes)"
	)]
	Invalid,
	/// Reads exactly like `Invalid`, so the reply never reveals that a
	/// username exists (only an existing account can be locked).
	#[error(
		"wrong username, password or code (5 wrong tries in a row lock an account for 15 minutes)"
	)]
	Locked(DateTime<Utc>),
	#[error("the console is unavailable: {0}")]
	Unavailable(String),
}

/// An argon2id hash of a random password, so a missing user costs the
/// same work as a real one.
fn dummy_hash() -> &'static str {
	static HASH: std::sync::OnceLock<String> = std::sync::OnceLock::new();
	HASH.get_or_init(|| hash_password("not-a-real-password-for-timing").expect("hashing works"))
}

/// Check a login. On success, the user's name and role.
pub fn login(
	store: &Store,
	key: &TotpKey,
	username: &str,
	password: &str,
	code: &str,
	now: DateTime<Utc>,
) -> Result<(String, Role), LoginError> {
	let unavailable = |e: StoreError| LoginError::Unavailable(e.to_string());
	let user = store.user(username).map_err(unavailable)?;

	let hash = user
		.as_ref()
		.map_or(dummy_hash(), |u| u.password_hash.as_str());
	let password_ok = PasswordHash::new(hash)
		.map(|h| {
			Argon2::default()
				.verify_password(password.as_bytes(), &h)
				.is_ok()
		})
		.unwrap_or(false);

	let Some(user) = user else {
		return Err(LoginError::Invalid);
	};
	if let Some(until) = user.locked_until.filter(|u| *u > now) {
		return Err(LoginError::Locked(until));
	}

	let step = if password_ok && !user.disabled {
		totp_step(key, &user, code, now)
	} else {
		None
	};
	// The state change is made atomically in the store, against the row as
	// it is then, not as it was read above: concurrent attempts each count,
	// and one TOTP code signs in once.
	if let (Some(step), Some(role)) = (step, Role::parse(&user.role)) {
		if store
			.record_login_success(&user.username, step, now)
			.map_err(unavailable)?
		{
			return Ok((user.username, role));
		}
	}
	match store
		.record_login_failure(&user.username, MAX_FAILURES, LOCKOUT, now)
		.map_err(unavailable)?
	{
		Some(until) => {
			tracing::warn!(username = %user.username, "console account locked after repeated failed logins");
			Err(LoginError::Locked(until))
		}
		None => Err(LoginError::Invalid),
	}
}

/// The time step `code` is valid for, if it is valid now (±1 step) and
/// later than the last one this user used.
fn totp_step(key: &TotpKey, user: &UserRow, code: &str, now: DateTime<Utc>) -> Option<u64> {
	let secret = key.open(&user.totp_sealed)?;
	let t = totp(secret.to_vec(), &user.username);
	let current = now.timestamp().max(0) as u64 / TOTP_STEP;
	(current.saturating_sub(1)..=current + 1).find(|&s| {
		s > user.last_totp_step
			&& bool::from(
				t.generate(s * TOTP_STEP)
					.as_bytes()
					.ct_eq(code.trim().as_bytes()),
			)
	})
}

#[derive(Debug, Clone)]
pub struct Session {
	pub username: String,
	pub role: Role,
	pub csrf: String,
	pub last_seen: DateTime<Utc>,
	pub signed_in: DateTime<Utc>,
}

fn token() -> String {
	let bytes: [u8; 32] = rand::rng().random();
	hex::encode(bytes)
}

/// Live sessions, in memory: a restart signs everyone out.
#[derive(Default)]
pub struct Sessions(Mutex<HashMap<String, Session>>);

impl Sessions {
	fn map(&self) -> std::sync::MutexGuard<'_, HashMap<String, Session>> {
		self.0.lock().unwrap_or_else(|e| e.into_inner())
	}

	pub fn create(&self, username: &str, role: Role, now: DateTime<Utc>) -> String {
		let t = token();
		self.map().insert(
			t.clone(),
			Session {
				username: username.into(),
				role,
				csrf: token(),
				last_seen: now,
				signed_in: now,
			},
		);
		t
	}

	/// The session for `token`, if it hasn't been idle too long. Touches it.
	pub fn get(&self, token: &str, now: DateTime<Utc>) -> Option<Session> {
		let mut map = self.map();
		let live = map
			.get(token)
			.is_some_and(|s| now - s.last_seen < IDLE && now - s.signed_in < MAX_SESSION);
		if !live {
			map.remove(token);
			return None;
		}
		let s = map.get_mut(token)?;
		s.last_seen = now;
		Some(s.clone())
	}

	pub fn end(&self, token: &str) {
		self.map().remove(token);
	}

	/// End every session of `username` (it was disabled, removed or
	/// changed). Each request also re-checks the user, so this is just
	/// prompt cleanup.
	pub fn end_user(&self, username: &str) {
		self.map().retain(|_, s| s.username != username);
	}
}

/// Constant-time comparison for CSRF tokens.
pub fn same_token(a: &str, b: &str) -> bool {
	bool::from(a.as_bytes().ct_eq(b.as_bytes()))
}
