//! Secrets in files: the simplest backend, and the one a single VM uses.
//!
//! A secret file must be a regular file that only the agent's own user can
//! read: no group or other permission bits at all (`0600` or `0400`), and
//! owned by the uid the agent runs as. Anything looser and the agent refuses
//! to start, because another account on the host could read the key.
//!
//! The file is opened first and its metadata read from the open handle, so
//! the checks and the read are about the same file even if the path is
//! swapped in between. Symlinks are followed (Kubernetes mounts secrets
//! through them); the checks apply to the file they lead to.

use std::fs::File;
use std::io::Read;
use std::os::unix::fs::MetadataExt;

use zeroize::Zeroizing;

use super::{finish, Secret, SecretError, SecretRef, SecretSource};

pub struct FileSource {
	/// The uid a secret file must be owned by.
	owner: u32,
}

impl FileSource {
	/// Files must be owned by the user this process runs as.
	pub fn for_current_user() -> Self {
		Self {
			owner: rustix::process::geteuid().as_raw(),
		}
	}

	/// Files must be owned by `uid`. For tests.
	pub fn for_uid(owner: u32) -> Self {
		Self { owner }
	}
}

impl SecretSource for FileSource {
	async fn fetch(&self, reference: &SecretRef) -> Result<Secret, SecretError> {
		let SecretRef::File(path) = reference else {
			unreachable!("FileSource is only given file:// references");
		};
		let r = || reference.to_string();
		let shown = path.display().to_string();

		let mut file = File::open(path).map_err(|e| SecretError::FileUnreadable {
			reference: r(),
			why: e.kind().to_string(),
		})?;
		let meta = file.metadata().map_err(|e| SecretError::FileUnreadable {
			reference: r(),
			why: e.kind().to_string(),
		})?;
		if !meta.is_file() {
			return Err(SecretError::NotAFile { reference: r() });
		}
		let mode = meta.mode() & 0o7777;
		if mode & 0o077 != 0 {
			return Err(SecretError::FileTooOpen {
				reference: r(),
				mode,
				path: shown,
			});
		}
		if meta.uid() != self.owner {
			return Err(SecretError::FileWrongOwner {
				reference: r(),
				owner: meta.uid(),
				agent: self.owner,
				path: shown,
			});
		}

		let mut raw = Zeroizing::new(String::new());
		file.read_to_string(&mut raw)
			.map_err(|e| SecretError::FileUnreadable {
				reference: r(),
				why: e.kind().to_string(),
			})?;
		finish(reference, &raw)
	}
}
