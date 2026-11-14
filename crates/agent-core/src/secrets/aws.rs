//! AWS Secrets Manager, through the official SDK (feature `aws`).
//!
//! Credentials and region come from the SDK's default chain: the instance
//! or task role, `AWS_REGION`, and so on. The agent never reads an AWS key
//! from its own config.

use super::{finish, Secret, SecretError, SecretRef, SecretSource};

pub struct AwsSource;

impl SecretSource for AwsSource {
	async fn fetch(&self, reference: &SecretRef) -> Result<Secret, SecretError> {
		let SecretRef::AwsSecretsManager(id) = reference else {
			unreachable!("AwsSource is only given aws-sm:// references");
		};
		let fail = |why: String| SecretError::Backend {
			reference: reference.to_string(),
			why,
		};
		let config = aws_config::load_defaults(aws_config::BehaviorVersion::latest()).await;
		let client = aws_sdk_secretsmanager::Client::new(&config);
		let out = client
			.get_secret_value()
			.secret_id(id)
			.send()
			.await
			.map_err(|e| {
				fail(format!(
					"Secrets Manager refused: {}",
					e.into_service_error()
				))
			})?;
		let value = out
			.secret_string()
			.ok_or_else(|| fail("the secret has no string value".into()))?;
		finish(reference, value)
	}
}
