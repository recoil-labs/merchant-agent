//! `merchant-agent init`: write a commented sandbox config, and say exactly
//! which secrets to create and where.
//!
//! The wizard (in the binary) asks the questions; this module only renders
//! the answers, so the output can be tested to parse and validate.
//!
//! Init only ever writes a **sandbox** config: testnet, the simulated bank,
//! low limits and `env://` secrets. Production is a deliberate edit,
//! followed by a passing `merchant-agent doctor`.

use std::path::PathBuf;

/// What the wizard asks.
#[derive(Debug, Clone)]
pub struct Answers {
	pub solver_id: String,
	pub wallet: String,
	pub aggregator_url: String,
	pub data_dir: PathBuf,
	/// Listen on every interface, for a container: its ports are reached
	/// through the container runtime, never 127.0.0.1 inside it.
	pub bind_all: bool,
	/// Read the three secrets from files in this directory (`api-key`,
	/// `payee-key`, `payee-tag-pepper`) instead of environment variables.
	pub secrets_dir: Option<PathBuf>,
}

impl Answers {
	/// The reference for one of the three secrets.
	fn secret(&self, env: &str, file: &str) -> String {
		match &self.secrets_dir {
			Some(d) => format!(
				"file://{}/{file}",
				d.display().to_string().trim_end_matches('/')
			),
			None => format!("env://{env}"),
		}
	}
}

impl Answers {
	/// Check answers before rendering, with a message for each bad one.
	pub fn check(&self) -> Result<(), Vec<String>> {
		let mut p = Vec::new();
		if !self.solver_id.starts_with("solver-") || self.solver_id.len() < 8 {
			p.push(
				"the operator id looks like solver-1a2b3c…; copy it from the RecoilPay portal"
					.into(),
			);
		}
		let w = &self.wallet;
		if !(w.len() == 42 && w.starts_with("0x") && w[2..].bytes().all(|b| b.is_ascii_hexdigit()))
		{
			p.push("the wallet is your registered wallet: 0x followed by 40 hex digits".into());
		}
		if !(self.aggregator_url.starts_with("https://")
			|| self.aggregator_url.starts_with("http://"))
		{
			p.push("the aggregator URL starts with https://".into());
		}
		if self.secrets_dir.as_ref().is_some_and(|d| !d.is_absolute()) {
			p.push("the secrets directory must be an absolute path".into());
		}
		if p.is_empty() {
			Ok(())
		} else {
			Err(p)
		}
	}
}

/// Environment variables the sandbox config references.
pub const SANDBOX_SECRETS: [(&str, &str); 3] = [
	(
		"MERCHANT_AGENT_API_KEY",
		"your operator API key, from the RecoilPay portal",
	),
	(
		"MERCHANT_AGENT_PAYEE_KEY",
		"your payee private key, from the portal (Payouts, Export payee key for the Merchant Agent)",
	),
	(
		"MERCHANT_AGENT_PAYEE_TAG_PEPPER",
		"a random value only this agent knows: run `openssl rand -hex 32`",
	),
];

/// The sandbox config for `answers`, with comments.
pub fn render_sandbox_config(a: &Answers) -> String {
	format!(
		r#"# RecoilPay Merchant Agent config.
#
# Written by `merchant-agent init` in SANDBOX mode: Base Sepolia testnet,
# the simulated bank, and low limits. No real money moves.
#
# Secrets are never written in this file. Each secret field holds a
# reference to where the secret lives:
#   gcp-sm://projects/<p>/secrets/<s>/versions/<v>   GCP Secret Manager
#   aws-sm://<name or ARN>                           AWS Secrets Manager
#   file:///absolute/path                            a file, mode 0600, owned by the agent's user
#   env://NAME                                       an environment variable (sandbox only)
#
# Check everything with:  merchant-agent doctor

config_version: 1

# sandbox | production. Production needs real secrets (not env://), a real
# bank rail, https everywhere and a passing `merchant-agent doctor`.
mode: sandbox

operator:
  # Your operator id in RecoilPay.
  solver_id: "{solver_id}"
  # Your registered wallet. Every escrow lock you are paid from names this
  # address. It is pinned here and never taken from the aggregator.
  wallet: "{wallet}"

aggregator:
  url: "{url}"
  api_key: {api_key}
  # How often to look for new cash-outs, in seconds.
  poll_secs: 5
  request_timeout_secs: 10

payee:
  # Opens the bank details customers seal to you. Export it from the
  # portal (Payouts) and set key_form: private_key. Or, if you signed the
  # payee-key message yourself, store the signature and set
  # key_form: signature.
  private_key: {payee_key}
  key_form: private_key
  # Lets the agent recognise the same bank account across trades (for the
  # per-payee limit) without storing the account number.
  tag_pepper: {pepper}

chains:
  - caip2: "eip155:84532"
    name: base-sepolia
    # Pinned addresses. At start-up the agent checks them against
    # RecoilPay's registry and refuses to pay if they differ.
    escrow: "0x112a534104ac10E73D57f7C5a4C6469632392173"
    usdc: "0x73c83DAcc74bB8a704717AC09703b959E74b9705"
    usdc_decimals: 6
    # Your own RPC endpoints. Use `secret:` instead of `url:` for a URL
    # that carries an API key.
    rpcs:
      - url: "https://sepolia.base.org"
    # Blocks a lock must be buried under before the agent pays.
    confirmations: 3
    require_rpc_agreement: false

rail:
  # simulated (sandbox only) | manual | paystack | template
  kind: simulated

# Local limits, enforced by the agent whatever RecoilPay says. Amounts are
# in kobo (₦1 = 100 kobo), written as strings. There are no defaults: every
# limit must be set.
limits:
  currency: NGN
  per_trade_minor: "5000000"            # ₦50,000.00
  per_day_minor: "50000000"             # ₦500,000.00
  per_payee_per_day_minor: "10000000"   # ₦100,000.00
  velocity_max_payouts: 20              # at most 20 payouts ...
  velocity_window_secs: 3600            # ... per hour
  min_float_minor: "0"                  # never pay below this balance

policy:
  # Don't start a payout with less than this many seconds left in the
  # 15-minute window. A late payout can be refunded to the customer AND
  # paid by you.
  start_margin_secs: 180
  # If the bank still says "pending" with this many seconds left, report
  # the payment with the bank's reference anyway.
  report_margin_secs: 120
  max_clock_skew_secs: 5
  # How far a quote's rate may be from your reference rate (below), in
  # basis points.
  max_rate_deviation_bps: 300
  # How far a trade's naira may differ from the agent's own recomputation
  # from your published offer, in basis points. 0 means exact.
  amount_tolerance_bps: 0
  # strict | token_set | off (sandbox only)
  name_match: token_set
  two_person:
    enabled: false
    # approve_over_minor: "20000000"

# Your reference rates, NGN per USDC, by rail. A trade on an offer whose
# rate is further from this than policy.max_rate_deviation_bps is held.
# Without one, that check is off (doctor warns in production).
pricing:
  reference_rates:
    ng.nip: "1500.00"

# Payees on a deny-list are held for review. Make the list with
# `merchant-agent screening hash`, then uncomment:
# screening:
#   deny_list: /etc/merchant-agent/deny-list.txt

# Where alerts go. Each address is a secret reference (it carries a
# credential). With none set, alerts are only logged. Uncomment to use:
# alerts:
#   webhook: env://MERCHANT_AGENT_ALERT_WEBHOOK        # Slack or Teams incoming webhook
#   pagerduty_routing_key: env://MERCHANT_AGENT_PAGERDUTY_KEY
#   email:
#     smtp_url: env://MERCHANT_AGENT_SMTP_URL          # smtps://user:password@host:465
#     from: agent@example.com
#     to: [ops@example.com]
#   max_per_minute: 20

server:
{server}
storage:
  # The agent's database and audit log. Back this directory up.
  data_dir: "{data_dir}"

logging:
  level: info
  # json | pretty
  format: json
"#,
		solver_id = a.solver_id,
		wallet = a.wallet,
		url = a.aggregator_url,
		data_dir = a.data_dir.display(),
		api_key = a.secret("MERCHANT_AGENT_API_KEY", "api-key"),
		payee_key = a.secret("MERCHANT_AGENT_PAYEE_KEY", "payee-key"),
		pepper = a.secret("MERCHANT_AGENT_PAYEE_TAG_PEPPER", "payee-tag-pepper"),
		server = if a.bind_all {
			"  # In a container: listen on every interface inside it, and publish\n\
			 \x20 # the console only on the host's 127.0.0.1 (the compose file does).\n\
			 \x20 console_bind: \"0.0.0.0:8740\"\n\
			 \x20 health_bind: \"0.0.0.0:8741\"\n\
			 \x20 bind_public: true\n"
		} else {
			"  # The local console and health endpoints. Keep them on 127.0.0.1.\n\
			 \x20 console_bind: \"127.0.0.1:8740\"\n\
			 \x20 health_bind: \"127.0.0.1:8741\"\n\
			 \x20 bind_public: false\n"
		},
	)
}

/// What to do after init, for the person at the terminal.
pub fn next_steps(config_path: &str) -> String {
	let mut s = format!("Wrote a sandbox config to {config_path}.\n\nCreate these secrets before you run the agent:\n");
	for (name, what) in SANDBOX_SECRETS {
		s.push_str(&format!("  export {name}=…   # {what}\n"));
	}
	s.push_str(&format!(
		"\nThen check everything:\n  merchant-agent --config {config_path} doctor\n\n\
		 In production, put each secret in GCP Secret Manager, AWS Secrets Manager or a\n\
		 0600 file owned by the agent's user, and change the env:// references to point at it.\n"
	));
	s
}
