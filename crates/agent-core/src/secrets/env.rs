//! Secrets in environment variables. **Sandbox mode only.**
//!
//! Environment variables leak: into child processes, `/proc/<pid>/environ`,
//! crash dumps and the "env" panel of most container platforms. That is fine
//! for a sandbox test key and wrong for a bank credential, so production
//! refuses them twice: config validation rejects `env://` references, and
//! [`super::Secrets`] refuses to resolve one.
//!
//! The only place in the agent that reads the environment (see
//! `clippy.toml`).

use zeroize::Zeroizing;

use super::{finish, Secret, SecretError, SecretRef, SecretSource};

pub struct EnvSource;

impl SecretSource for EnvSource {
	async fn fetch(&self, reference: &SecretRef) -> Result<Secret, SecretError> {
		let SecretRef::Env(name) = reference else {
			unreachable!("EnvSource is only given env:// references");
		};
		#[allow(clippy::disallowed_methods)]
		let raw = std::env::var(name)
			.map(Zeroizing::new)
			.map_err(|_| SecretError::EnvMissing {
				reference: reference.to_string(),
			})?;
		finish(reference, &raw)
	}
}
