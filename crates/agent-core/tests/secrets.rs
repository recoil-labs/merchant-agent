//! Secret backends: file permission rules, env only in sandbox, and secrets
//! never shown in Debug output.

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;

use merchant_agent_core::config::Mode;
use merchant_agent_core::secrets::{FileSource, SecretError, SecretRef, SecretSource, Secrets};

const VALUE: &str = "sk_test_THISISAFAKEVALUEFORTESTS0000";

fn secret_file(dir: &Path, mode: u32, body: &str) -> SecretRef {
	let path = dir.join("secret");
	fs::write(&path, body).unwrap();
	fs::set_permissions(&path, fs::Permissions::from_mode(mode)).unwrap();
	SecretRef::File(path)
}

#[tokio::test]
async fn a_0600_file_owned_by_the_agent_is_read_without_its_newline() {
	let dir = tempfile::tempdir().unwrap();
	let r = secret_file(dir.path(), 0o600, &format!("{VALUE}\n"));
	let s = Secrets::new(Mode::Production).resolve(&r).await.unwrap();
	assert_eq!(s.expose(), VALUE);
}

#[tokio::test]
async fn a_0400_file_is_also_accepted() {
	let dir = tempfile::tempdir().unwrap();
	let r = secret_file(dir.path(), 0o400, VALUE);
	assert!(FileSource::for_current_user().fetch(&r).await.is_ok());
}

#[tokio::test]
async fn invariant_8_a_world_readable_secret_file_is_refused() {
	let dir = tempfile::tempdir().unwrap();
	for mode in [0o644, 0o640, 0o604, 0o660] {
		let r = secret_file(dir.path(), mode, VALUE);
		match FileSource::for_current_user().fetch(&r).await {
			Err(SecretError::FileTooOpen { mode: m, .. }) => assert_eq!(m, mode),
			other => panic!("mode {mode:o}: expected FileTooOpen, got {other:?}"),
		}
	}
}

#[tokio::test]
async fn invariant_8_a_secret_file_owned_by_someone_else_is_refused() {
	let dir = tempfile::tempdir().unwrap();
	let r = secret_file(dir.path(), 0o600, VALUE);
	let other_uid = my_uid().wrapping_add(1);
	match FileSource::for_uid(other_uid).fetch(&r).await {
		Err(SecretError::FileWrongOwner { agent, .. }) => assert_eq!(agent, other_uid),
		other => panic!("expected FileWrongOwner, got {other:?}"),
	}
}

fn my_uid() -> u32 {
	use std::os::unix::fs::MetadataExt;
	// The owner of a file this process just made is this process's uid.
	let f = tempfile::NamedTempFile::new().unwrap();
	f.as_file().metadata().unwrap().uid()
}

#[tokio::test]
async fn missing_empty_and_directory_secrets_are_refused() {
	let dir = tempfile::tempdir().unwrap();
	let src = FileSource::for_current_user();

	let missing = SecretRef::File(dir.path().join("nope"));
	assert!(matches!(
		src.fetch(&missing).await,
		Err(SecretError::FileUnreadable { .. })
	));

	let empty = secret_file(dir.path(), 0o600, "\n");
	assert!(matches!(
		src.fetch(&empty).await,
		Err(SecretError::Empty { .. })
	));

	let sub = dir.path().join("d");
	fs::create_dir(&sub).unwrap();
	fs::set_permissions(&sub, fs::Permissions::from_mode(0o700)).unwrap();
	let r = SecretRef::File(sub);
	assert!(src.fetch(&r).await.is_err());
}

#[tokio::test]
async fn invariant_8_production_refuses_env_secrets() {
	let r = SecretRef::Env("MA_TEST_PRODUCTION_REFUSES".into());
	match Secrets::new(Mode::Production).resolve(&r).await {
		Err(SecretError::EnvInProduction { .. }) => {}
		other => panic!("expected EnvInProduction, got {other:?}"),
	}
}

#[tokio::test]
async fn sandbox_reads_env_secrets() {
	// A name no other test uses: tests run in parallel in one process.
	let name = "MA_TEST_SANDBOX_READS_ENV";
	// No other test reads or writes this variable.
	std::env::set_var(name, VALUE);
	let s = Secrets::new(Mode::Sandbox)
		.resolve(&SecretRef::Env(name.into()))
		.await
		.unwrap();
	assert_eq!(s.expose(), VALUE);

	let missing = SecretRef::Env("MA_TEST_DEFINITELY_UNSET".into());
	assert!(matches!(
		Secrets::new(Mode::Sandbox).resolve(&missing).await,
		Err(SecretError::EnvMissing { .. })
	));
}

#[tokio::test]
async fn a_cloud_backend_missing_from_the_build_says_how_to_get_it() {
	let r = SecretRef::parse("gcp-sm://projects/p/secrets/s/versions/1").unwrap();
	let result = Secrets::new(Mode::Production).resolve(&r).await;
	if cfg!(feature = "gcp") {
		return; // with the feature on, this would try to reach GCP
	}
	match result {
		Err(e @ SecretError::BackendNotBuilt { .. }) => {
			assert!(e.to_string().contains("--features gcp"))
		}
		other => panic!("expected BackendNotBuilt, got {other:?}"),
	}
}

#[tokio::test]
async fn invariant_8_secrets_never_appear_in_debug_output() {
	let dir = tempfile::tempdir().unwrap();
	let r = secret_file(dir.path(), 0o600, VALUE);
	let s = FileSource::for_current_user().fetch(&r).await.unwrap();
	for shown in [
		format!("{s:?}"),
		format!("{s:#?}"),
		format!("{:?}", Some(&s)),
	] {
		assert!(!shown.contains("THISISAFAKE"), "{shown}");
		assert!(shown.contains("REDACTED"), "{shown}");
	}
}

#[tokio::test]
async fn errors_never_contain_the_secret() {
	let dir = tempfile::tempdir().unwrap();
	let r = secret_file(dir.path(), 0o644, VALUE);
	let err = FileSource::for_current_user().fetch(&r).await.unwrap_err();
	assert!(!format!("{err} {err:?}").contains("THISISAFAKE"));
}
