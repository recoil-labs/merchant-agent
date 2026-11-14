//! GCP Secret Manager, through its REST API (feature `gcp`).
//!
//! Credentials come from the instance metadata server: the service account
//! of the VM, GKE workload or Cloud Run job the agent runs as. No key file
//! is ever needed. That token request is plain HTTP to the link-local
//! metadata server, which is how Google documents it; it never leaves the
//! host. The Secret Manager call itself is HTTPS with verified TLS.
//!
//! Docs: <https://cloud.google.com/secret-manager/docs/reference/rest/v1/projects.secrets.versions/access>
//! and <https://cloud.google.com/compute/docs/access/authenticate-workloads#applications>.

use base64::Engine;
use serde::Deserialize;
use std::time::Duration;
use zeroize::Zeroizing;

use super::{finish, Secret, SecretError, SecretRef, SecretSource};

const TOKEN_URL: &str =
	"http://metadata.google.internal/computeMetadata/v1/instance/service-accounts/default/token";

pub struct GcpSource;

#[derive(Deserialize)]
struct Token {
	access_token: String,
}

#[derive(Deserialize)]
struct Access {
	payload: Payload,
}

#[derive(Deserialize)]
struct Payload {
	data: String,
}

impl SecretSource for GcpSource {
	async fn fetch(&self, reference: &SecretRef) -> Result<Secret, SecretError> {
		let SecretRef::GcpSecretManager(name) = reference else {
			unreachable!("GcpSource is only given gcp-sm:// references");
		};
		let fail = |why: String| SecretError::Backend {
			reference: reference.to_string(),
			why,
		};
		let http = reqwest::Client::builder()
			.timeout(Duration::from_secs(10))
			.https_only(false)
			.build()
			.map_err(|e| fail(e.to_string()))?;

		let token: Token = http
			.get(TOKEN_URL)
			.header("Metadata-Flavor", "Google")
			.send()
			.await
			.and_then(|r| r.error_for_status())
			.map_err(|e| fail(format!("no metadata server credentials: {e}")))?
			.json()
			.await
			.map_err(|e| fail(e.to_string()))?;
		let token = Zeroizing::new(token.access_token);

		let url = format!("https://secretmanager.googleapis.com/v1/{name}:access");
		let access: Access = http
			.get(url)
			.bearer_auth(token.as_str())
			.send()
			.await
			.and_then(|r| r.error_for_status())
			.map_err(|e| fail(format!("Secret Manager refused: {}", e.without_url())))?
			.json()
			.await
			.map_err(|e| fail(e.to_string()))?;
		let data = Zeroizing::new(access.payload.data);
		let bytes = Zeroizing::new(
			base64::engine::general_purpose::STANDARD
				.decode(data.as_bytes())
				.map_err(|_| fail("the payload is not base64".into()))?,
		);
		let text =
			std::str::from_utf8(&bytes).map_err(|_| fail("the secret is not UTF-8".into()))?;
		finish(reference, text)
	}
}
