//! Secrets: where they live, and the one type that holds them.
//!
//! Invariant 8: secrets come from a secret backend, never from config text.
//! A config field that takes a secret takes a [`SecretRef`] (`gcp-sm://`,
//! `aws-sm://`, `file:///` or `env://`) and parsing refuses anything else, so
//! a key pasted into the config is an error rather than a leak.
//!
//! A resolved value is a [`Secret`]: it zeroes itself when dropped, prints
//! as `Secret([REDACTED])`, and cannot be serialised or displayed.

#[cfg(feature = "aws")]
mod aws;
mod env;
mod file;
#[cfg(feature = "gcp")]
mod gcp;

pub use env::EnvSource;
pub use file::FileSource;

use secrecy::{ExposeSecret, SecretString};
use serde::Deserialize;
use std::fmt;
use std::path::PathBuf;

use crate::config::Mode;

/// Where a secret lives. Never the secret itself.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(try_from = "String")]
pub enum SecretRef {
	/// `gcp-sm://projects/<p>/secrets/<s>/versions/<v>`
	GcpSecretManager(String),
	/// `aws-sm://<name or ARN>`
	AwsSecretsManager(String),
	/// `file:///absolute/path`
	File(PathBuf),
	/// `env://NAME`. Sandbox only.
	Env(String),
}

impl SecretRef {
	pub fn parse(s: &str) -> Result<Self, String> {
		let s = s.trim();
		let bad = || {
			"a secret field takes a reference (gcp-sm://, aws-sm://, file:///, env://), \
			 never the secret itself"
				.to_string()
		};
		if let Some(rest) = s.strip_prefix("gcp-sm://") {
			let parts: Vec<&str> = rest.split('/').collect();
			let ok = parts.len() == 6
				&& parts[0] == "projects"
				&& parts[2] == "secrets"
				&& parts[4] == "versions"
				&& parts.iter().all(|p| !p.is_empty());
			return if ok {
				Ok(Self::GcpSecretManager(rest.to_string()))
			} else {
				Err("gcp-sm:// must be gcp-sm://projects/<project>/secrets/<name>/versions/<version>".into())
			};
		}
		if let Some(rest) = s.strip_prefix("aws-sm://") {
			return if rest.is_empty() {
				Err("aws-sm:// needs a secret name or ARN".into())
			} else {
				Ok(Self::AwsSecretsManager(rest.to_string()))
			};
		}
		if let Some(rest) = s.strip_prefix("file://") {
			let path = PathBuf::from(rest);
			return if path.is_absolute() {
				Ok(Self::File(path))
			} else {
				Err(
					"file:// needs an absolute path, such as file:///etc/merchant-agent/api-key"
						.into(),
				)
			};
		}
		if let Some(name) = s.strip_prefix("env://") {
			let valid = !name.is_empty()
				&& !name.starts_with(|c: char| c.is_ascii_digit())
				&& name
					.chars()
					.all(|c| c.is_ascii_uppercase() || c.is_ascii_digit() || c == '_');
			return if valid {
				Ok(Self::Env(name.to_string()))
			} else {
				Err("env:// needs an upper-case variable name, such as env://MERCHANT_AGENT_API_KEY".into())
			};
		}
		Err(bad())
	}

	pub fn is_env(&self) -> bool {
		matches!(self, Self::Env(_))
	}
}

impl TryFrom<String> for SecretRef {
	type Error = String;
	fn try_from(s: String) -> Result<Self, String> {
		Self::parse(&s)
	}
}

/// Shows the reference, which is not secret: `file:///etc/x`.
impl fmt::Display for SecretRef {
	fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
		match self {
			Self::GcpSecretManager(p) => write!(f, "gcp-sm://{p}"),
			Self::AwsSecretsManager(p) => write!(f, "aws-sm://{p}"),
			Self::File(p) => write!(f, "file://{}", p.display()),
			Self::Env(n) => write!(f, "env://{n}"),
		}
	}
}

/// A resolved secret. Zeroed on drop; never printed, displayed or
/// serialised. Read it with [`Secret::expose`] at the one place it is used.
pub struct Secret(SecretString);

impl Secret {
	pub fn new(value: String) -> Self {
		Self(SecretString::from(value))
	}

	pub fn expose(&self) -> &str {
		self.0.expose_secret()
	}
}

impl fmt::Debug for Secret {
	fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
		f.write_str("Secret([REDACTED])")
	}
}

/// Why a secret could not be read. Messages name the reference and the fix,
/// never the value.
#[derive(Debug, thiserror::Error)]
pub enum SecretError {
	#[error(
		"{reference}: env:// secrets are refused in production; use a secret manager or a file"
	)]
	EnvInProduction { reference: String },
	#[error("{reference}: the environment variable is not set")]
	EnvMissing { reference: String },
	#[error("{reference}: cannot read the file: {why}")]
	FileUnreadable { reference: String, why: String },
	#[error("{reference}: not a regular file")]
	NotAFile { reference: String },
	#[error("{reference}: mode {mode:04o} lets other users read it; run `chmod 600 {path}`")]
	FileTooOpen {
		reference: String,
		mode: u32,
		path: String,
	},
	#[error("{reference}: owned by uid {owner}, but the agent runs as uid {agent}; run `chown {agent} {path}`")]
	FileWrongOwner {
		reference: String,
		owner: u32,
		agent: u32,
		path: String,
	},
	#[error("{reference}: the secret is empty")]
	Empty { reference: String },
	#[error(
		"{reference}: this build has no {backend} support; rebuild with `--features {feature}`"
	)]
	BackendNotBuilt {
		reference: String,
		backend: &'static str,
		feature: &'static str,
	},
	#[error("{reference}: {why}")]
	Backend { reference: String, why: String },
}

/// One place a secret can live.
pub trait SecretSource {
	/// Fetch the value behind `reference`, which is always of this source's
	/// own scheme.
	fn fetch(
		&self,
		reference: &SecretRef,
	) -> impl std::future::Future<Output = Result<Secret, SecretError>> + Send;
}

/// Resolves any [`SecretRef`] with the backend for its scheme, refusing
/// `env://` outside sandbox mode even if validation was somehow skipped.
pub struct Secrets {
	mode: Mode,
	file: FileSource,
	env: EnvSource,
}

impl Secrets {
	pub fn new(mode: Mode) -> Self {
		Self {
			mode,
			file: FileSource::for_current_user(),
			env: EnvSource,
		}
	}

	pub async fn resolve(&self, reference: &SecretRef) -> Result<Secret, SecretError> {
		match reference {
			SecretRef::Env(_) if self.mode == Mode::Production => {
				Err(SecretError::EnvInProduction {
					reference: reference.to_string(),
				})
			}
			SecretRef::Env(_) => self.env.fetch(reference).await,
			SecretRef::File(_) => self.file.fetch(reference).await,
			SecretRef::GcpSecretManager(_) => {
				#[cfg(feature = "gcp")]
				{
					gcp::GcpSource.fetch(reference).await
				}
				#[cfg(not(feature = "gcp"))]
				{
					Err(SecretError::BackendNotBuilt {
						reference: reference.to_string(),
						backend: "GCP Secret Manager",
						feature: "gcp",
					})
				}
			}
			SecretRef::AwsSecretsManager(_) => {
				#[cfg(feature = "aws")]
				{
					aws::AwsSource.fetch(reference).await
				}
				#[cfg(not(feature = "aws"))]
				{
					Err(SecretError::BackendNotBuilt {
						reference: reference.to_string(),
						backend: "AWS Secrets Manager",
						feature: "aws",
					})
				}
			}
		}
	}
}

/// Trim the trailing newline an editor or `echo` leaves, and refuse an
/// empty value.
pub(crate) fn finish(reference: &SecretRef, raw: &str) -> Result<Secret, SecretError> {
	let value = raw.trim_end_matches(['\n', '\r']);
	if value.trim().is_empty() {
		return Err(SecretError::Empty {
			reference: reference.to_string(),
		});
	}
	Ok(Secret::new(value.to_string()))
}

#[cfg(test)]
mod tests {
	use super::*;
	use static_assertions::assert_not_impl_any;

	assert_not_impl_any!(Secret: fmt::Display, Clone, serde::Serialize);

	#[test]
	fn parses_every_scheme() {
		assert_eq!(
			SecretRef::parse("gcp-sm://projects/p/secrets/s/versions/latest"),
			Ok(SecretRef::GcpSecretManager(
				"projects/p/secrets/s/versions/latest".into()
			))
		);
		assert_eq!(
			SecretRef::parse("aws-sm://prod/agent/api-key"),
			Ok(SecretRef::AwsSecretsManager("prod/agent/api-key".into()))
		);
		assert_eq!(
			SecretRef::parse("file:///etc/merchant-agent/api-key"),
			Ok(SecretRef::File("/etc/merchant-agent/api-key".into()))
		);
		assert_eq!(
			SecretRef::parse("env://API_KEY_1"),
			Ok(SecretRef::Env("API_KEY_1".into()))
		);
	}

	#[test]
	fn refuses_values_and_malformed_references() {
		for bad in [
			"sk_live_abcdef0123456789",
			"0x4c0883a69102937d6231471b5dbb6204fe5129617082792ae468d01a3f362318",
			"file://relative/path",
			"env://lower",
			"env://1ABC",
			"gcp-sm://my-secret",
			"aws-sm://",
			"https://vault.example/secret",
		] {
			assert!(SecretRef::parse(bad).is_err(), "{bad}");
		}
	}

	#[test]
	fn an_error_for_a_pasted_secret_does_not_echo_it() {
		let err = SecretRef::parse("sk_live_abcdef0123456789").unwrap_err();
		assert!(!err.contains("abcdef"), "{err}");
	}
}
