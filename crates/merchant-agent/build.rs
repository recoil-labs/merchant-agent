//! Records the git commit the binary was built from, for `merchant-agent version`.

use std::process::Command;

fn main() {
	// A container build has no .git; the Dockerfile passes the commit in.
	#[allow(clippy::disallowed_methods)]
	let given = std::env::var("MERCHANT_AGENT_GIT_COMMIT")
		.ok()
		.filter(|s| !s.is_empty());
	let commit = given
		.or_else(|| {
			Command::new("git")
				.args(["rev-parse", "--short=12", "HEAD"])
				.output()
				.ok()
				.filter(|o| o.status.success())
				.and_then(|o| String::from_utf8(o.stdout).ok())
				.map(|s| s.trim().to_string())
				.filter(|s| !s.is_empty())
		})
		.unwrap_or_else(|| "unknown".into());
	println!("cargo:rustc-env=MERCHANT_AGENT_GIT_COMMIT={commit}");
	println!("cargo:rerun-if-changed=../../.git/HEAD");
	println!("cargo:rerun-if-env-changed=MERCHANT_AGENT_GIT_COMMIT");
}
