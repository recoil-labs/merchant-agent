//! The agent's config file: YAML, parsed strictly, then validated.
//!
//! - **Strict.** Every section denies unknown fields, so a misspelt
//!   `per_day_minr` is an error, not a limit silently left unset.
//! - **No secrets.** Secret fields are [`SecretRef`]s; a value pasted in
//!   their place does not parse.
//! - **No silent defaults for money.** Every limit must be written down. A
//!   missing one is an error that names it, in every mode.
//! - **All problems at once.** Validation collects every problem before
//!   failing, so fixing a config is one pass, not one error at a time.
//!
//! [`Config::parse`] returns a [`Config`] only if it passed validation, so
//! code that holds one never re-checks it.

use serde::Deserialize;
use std::fmt;
use std::net::SocketAddr;
use std::path::PathBuf;

use crate::money::Minor;
use crate::secrets::SecretRef;
use crate::CONFIG_SCHEMA_VERSION;

/// Sandbox (testnet, simulated bank, low limits) or production.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Mode {
	Sandbox,
	Production,
}

impl fmt::Display for Mode {
	fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
		f.write_str(match self {
			Self::Sandbox => "sandbox",
			Self::Production => "production",
		})
	}
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Operator {
	/// The operator id in RecoilPay, e.g. `solver-1a2b…`.
	pub solver_id: String,
	/// The merchant's registered wallet. Pinned here and never taken from
	/// the aggregator: a lock that pays anyone else is not this merchant's.
	pub wallet: String,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Aggregator {
	pub url: String,
	pub api_key: SecretRef,
	#[serde(default = "default_poll_secs")]
	pub poll_secs: u64,
	#[serde(default = "default_timeout_secs")]
	pub request_timeout_secs: u64,
}

fn default_poll_secs() -> u64 {
	5
}
fn default_timeout_secs() -> u64 {
	10
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Payee {
	/// The payee private key: the derived key exported from the dashboard,
	/// or the wallet's signature over the payee-key message (design §3.8).
	pub private_key: SecretRef,
	/// Whether `private_key` holds the derived key or the signature.
	pub key_form: PayeeKeyForm,
	/// Keys the per-payee tag (design §3.7), so the tag cannot be reversed
	/// by trying every account number.
	pub tag_pepper: SecretRef,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PayeeKeyForm {
	PrivateKey,
	Signature,
}

/// An RPC endpoint: a plain URL, or a secret reference for URLs that carry
/// an API key.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Rpc {
	pub url: Option<String>,
	pub secret: Option<SecretRef>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Chain {
	/// CAIP-2, e.g. `eip155:84532`.
	pub caip2: String,
	pub name: String,
	/// The fiat `AttestedEscrow`. Pinned; checked against the aggregator's
	/// registry at start-up.
	pub escrow: String,
	/// The chain's USDC. Pinned likewise.
	pub usdc: String,
	pub usdc_decimals: u8,
	pub rpcs: Vec<Rpc>,
	pub confirmations: u64,
	#[serde(default)]
	pub require_rpc_agreement: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RailKind {
	/// In-memory bank for tests and sandbox. Never in production.
	Simulated,
	/// A person pays from the bank app; the agent does everything else.
	Manual,
	Paystack,
	/// The merchant's own payout adapter: a small HTTPS service speaking the
	/// adapter protocol (docs/adapters.md).
	Template,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Rail {
	pub kind: RailKind,
	/// Paystack: the secret key. Template: the adapter's bearer token.
	pub credentials: Option<SecretRef>,
	/// Template: the adapter's base URL, e.g. https://payouts.internal:8443.
	pub url: Option<String>,
}

/// Limits as written. Every field is optional here only so a missing one
/// can be reported by name; [`Limits`] is what the agent uses.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawLimits {
	currency: Option<String>,
	per_trade_minor: Option<Minor>,
	per_day_minor: Option<Minor>,
	per_payee_per_day_minor: Option<Minor>,
	velocity_max_payouts: Option<u32>,
	velocity_window_secs: Option<u64>,
	min_float_minor: Option<Minor>,
}

/// Local limits (invariant 5). Going over any of them holds the trade.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Limits {
	pub currency: String,
	pub per_trade: Minor,
	pub per_day: Minor,
	pub per_payee_per_day: Minor,
	pub velocity_max_payouts: u32,
	pub velocity_window_secs: u64,
	/// Never pay below this balance in the payout account.
	pub min_float: Minor,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum NameMatch {
	/// Normalised names must be equal.
	Strict,
	/// The same set of name words, in any order ("OSAHON GINO" = "Gino Osahon").
	TokenSet,
	/// No name check. Sandbox only.
	Off,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawTwoPerson {
	enabled: bool,
	approve_over_minor: Option<Minor>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TwoPerson {
	pub enabled: bool,
	/// Approving a held payout above this needs a second approver.
	pub approve_over: Option<Minor>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawPolicy {
	start_margin_secs: u64,
	report_margin_secs: u64,
	max_clock_skew_secs: u64,
	max_rate_deviation_bps: u32,
	amount_tolerance_bps: u32,
	name_match: NameMatch,
	/// On a trade that carries the seller's verified name, the bank's name
	/// for the account must match it. On unless set; off is sandbox only.
	#[serde(default = "on")]
	require_verified_name_match: bool,
	two_person: RawTwoPerson,
}

fn on() -> bool {
	true
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Policy {
	/// Don't start a payout with less than this left before the deadline
	/// (invariant 6).
	pub start_margin_secs: u64,
	/// Report a still-pending payout with this much left (design §9.4).
	pub report_margin_secs: u64,
	pub max_clock_skew_secs: u64,
	/// How far a quote's rate may sit from the merchant's reference rate.
	pub max_rate_deviation_bps: u32,
	/// How far the trade's naira may differ from the agent's own
	/// recomputation. 0 (exact) unless you have a reason.
	pub amount_tolerance_bps: u32,
	pub name_match: NameMatch,
	/// When a trade carries the seller's verified name (an offer that pays
	/// only verified sellers), the bank's name for the account must match it
	/// under RecoilPay's shared rule, or the trade is held.
	pub require_verified_name_match: bool,
	pub two_person: TwoPerson,
}

/// Your own reference rates, NGN per USDC, by rail. A quote whose rate is
/// further from its rail's reference than `policy.max_rate_deviation_bps`
/// is held. The `Pricing` seam is where a live pricing engine plugs in
/// later; for now these are set here.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Pricing {
	#[serde(default)]
	pub reference_rates: std::collections::BTreeMap<String, crate::aggregator::types::Rate>,
}

/// Where alerts go. Every channel's address is a secret reference: a
/// webhook URL or an SMTP URL carries a credential. With none configured,
/// alerts are only logged (doctor warns in production).
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Alerts {
	/// A Slack or Teams incoming-webhook URL (both accept `{"text": …}`).
	pub webhook: Option<SecretRef>,
	/// A PagerDuty Events v2 routing key. Only urgent alerts page.
	pub pagerduty_routing_key: Option<SecretRef>,
	pub email: Option<EmailAlerts>,
	/// At most this many alerts a minute, all channels together. Beyond it,
	/// one summary says how many were held back.
	#[serde(default = "default_alerts_per_minute")]
	pub max_per_minute: u32,
}

fn default_alerts_per_minute() -> u32 {
	20
}

impl Default for Alerts {
	fn default() -> Self {
		Self {
			webhook: None,
			pagerduty_routing_key: None,
			email: None,
			max_per_minute: default_alerts_per_minute(),
		}
	}
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EmailAlerts {
	/// `smtps://user:password@host:465` (or `smtp://…` with STARTTLS).
	pub smtp_url: SecretRef,
	pub from: String,
	pub to: Vec<String>,
}

/// Screening (design §3.7): payees on the deny-list are held.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Screening {
	/// A file of payee tags, one per line (`#` starts a comment). Make it
	/// with `merchant-agent screening hash`.
	pub deny_list: Option<PathBuf>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Server {
	#[serde(default = "default_console_bind")]
	pub console_bind: SocketAddr,
	#[serde(default = "default_health_bind")]
	pub health_bind: SocketAddr,
	/// Must be set to bind anywhere but loopback (invariant 9).
	#[serde(default)]
	pub bind_public: bool,
	/// Host names, besides localhost, the console answers to: the name of
	/// a TLS proxy in front of it, if you use one. Requests for any other
	/// host are refused, so a web page can't reach the console through DNS
	/// rebinding.
	#[serde(default)]
	pub console_hostnames: Vec<String>,
}

fn default_console_bind() -> SocketAddr {
	SocketAddr::from(([127, 0, 0, 1], 8740))
}
fn default_health_bind() -> SocketAddr {
	SocketAddr::from(([127, 0, 0, 1], 8741))
}

impl Default for Server {
	fn default() -> Self {
		Self {
			console_bind: default_console_bind(),
			health_bind: default_health_bind(),
			bind_public: false,
			console_hostnames: Vec::new(),
		}
	}
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Storage {
	pub data_dir: PathBuf,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum LogFormat {
	#[default]
	Json,
	Pretty,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Logging {
	#[serde(default = "default_level")]
	pub level: String,
	#[serde(default)]
	pub format: LogFormat,
}

fn default_level() -> String {
	"info".into()
}

impl Default for Logging {
	fn default() -> Self {
		Self {
			level: default_level(),
			format: LogFormat::Json,
		}
	}
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawConfig {
	config_version: u32,
	mode: Mode,
	operator: Operator,
	aggregator: Aggregator,
	payee: Payee,
	chains: Vec<Chain>,
	rail: Rail,
	limits: Option<RawLimits>,
	policy: RawPolicy,
	#[serde(default)]
	pricing: Pricing,
	#[serde(default)]
	screening: Screening,
	#[serde(default)]
	alerts: Alerts,
	#[serde(default)]
	server: Server,
	storage: Storage,
	#[serde(default)]
	logging: Logging,
}

/// A config that parsed and passed validation.
#[derive(Debug, Clone)]
pub struct Config {
	pub mode: Mode,
	pub operator: Operator,
	pub aggregator: Aggregator,
	pub payee: Payee,
	pub chains: Vec<Chain>,
	pub rail: Rail,
	pub limits: Limits,
	pub policy: Policy,
	pub pricing: Pricing,
	pub screening: Screening,
	pub alerts: Alerts,
	pub server: Server,
	pub storage: Storage,
	pub logging: Logging,
}

#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
	#[error("the config could not be read: {0}")]
	Parse(String),
	#[error("the config has {} problem(s):\n{}", .0.len(), .0.iter().map(|p| format!("  - {p}")).collect::<Vec<_>>().join("\n"))]
	Invalid(Vec<String>),
}

impl ConfigError {
	pub fn problems(&self) -> Vec<String> {
		match self {
			Self::Parse(p) => vec![p.clone()],
			Self::Invalid(ps) => ps.clone(),
		}
	}
}

impl Config {
	pub fn parse(yaml: &str) -> Result<Self, ConfigError> {
		let raw: RawConfig =
			serde_yaml_ng::from_str(yaml).map_err(|e| ConfigError::Parse(e.to_string()))?;
		raw.validate()
	}

	/// Every secret reference in the config, with what it is for.
	pub fn secret_refs(&self) -> Vec<(&'static str, &SecretRef)> {
		let mut out = vec![
			("aggregator API key", &self.aggregator.api_key),
			("payee key", &self.payee.private_key),
			("payee tag pepper", &self.payee.tag_pepper),
		];
		if let Some(c) = &self.rail.credentials {
			out.push(("bank rail credentials", c));
		}
		if let Some(w) = &self.alerts.webhook {
			out.push(("alert webhook URL", w));
		}
		if let Some(k) = &self.alerts.pagerduty_routing_key {
			out.push(("PagerDuty routing key", k));
		}
		if let Some(e) = &self.alerts.email {
			out.push(("alert SMTP URL", &e.smtp_url));
		}
		for chain in &self.chains {
			for rpc in &chain.rpcs {
				if let Some(s) = &rpc.secret {
					out.push(("RPC URL", s));
				}
			}
		}
		out
	}
}

fn is_evm_address(s: &str) -> bool {
	s.len() == 42 && s.starts_with("0x") && s[2..].bytes().all(|b| b.is_ascii_hexdigit())
}

fn is_eip155(s: &str) -> bool {
	s.strip_prefix("eip155:").is_some_and(|id| {
		!id.is_empty() && id.bytes().all(|b| b.is_ascii_digit()) && !id.starts_with('0')
	})
}

impl RawConfig {
	fn validate(self) -> Result<Config, ConfigError> {
		let mut p: Vec<String> = Vec::new();
		let prod = self.mode == Mode::Production;

		if self.config_version != CONFIG_SCHEMA_VERSION {
			p.push(format!(
				"config_version is {}, but this agent reads version {CONFIG_SCHEMA_VERSION}",
				self.config_version
			));
		}

		// Operator.
		let sid = &self.operator.solver_id;
		if !sid.starts_with("solver-") || sid.len() < 8 {
			p.push(
				"operator.solver_id must be your RecoilPay operator id, such as solver-1a2b3c…"
					.into(),
			);
		}
		if !is_evm_address(&self.operator.wallet) {
			p.push(
				"operator.wallet must be your registered wallet, 0x followed by 40 hex digits"
					.into(),
			);
		}

		// Aggregator.
		let url = self.aggregator.url.trim();
		let https = url.starts_with("https://");
		if !https && !url.starts_with("http://") {
			p.push("aggregator.url must start with https://".into());
		} else if prod && !https {
			p.push("aggregator.url must use https:// in production".into());
		}
		if !(1..=60).contains(&self.aggregator.poll_secs) {
			p.push("aggregator.poll_secs must be between 1 and 60".into());
		}
		if !(1..=60).contains(&self.aggregator.request_timeout_secs) {
			p.push("aggregator.request_timeout_secs must be between 1 and 60".into());
		}

		// Chains.
		if self.chains.is_empty() {
			p.push("chains must list at least one chain".into());
		}
		let mut seen = std::collections::HashSet::new();
		for (i, c) in self.chains.iter().enumerate() {
			let at = format!("chains[{i}] ({})", c.name);
			if !is_eip155(&c.caip2) {
				p.push(format!(
					"{at}: caip2 must be eip155:<chain id>; only EVM chains are supported in v1"
				));
			}
			if !seen.insert(c.caip2.clone()) {
				p.push(format!("{at}: {} is listed twice", c.caip2));
			}
			if !is_evm_address(&c.escrow) {
				p.push(format!("{at}: escrow must be 0x followed by 40 hex digits"));
			}
			if !is_evm_address(&c.usdc) {
				p.push(format!("{at}: usdc must be 0x followed by 40 hex digits"));
			}
			if c.usdc_decimals != 6 {
				p.push(format!(
					"{at}: usdc_decimals is {}, but USDC has 6",
					c.usdc_decimals
				));
			}
			if c.rpcs.is_empty() {
				p.push(format!("{at}: rpcs must list at least one endpoint"));
			}
			if c.require_rpc_agreement && c.rpcs.len() < 2 {
				p.push(format!(
					"{at}: require_rpc_agreement needs at least two rpcs"
				));
			}
			if c.confirmations == 0 || c.confirmations > 64 {
				p.push(format!("{at}: confirmations must be between 1 and 64"));
			}
			for (j, rpc) in c.rpcs.iter().enumerate() {
				match (&rpc.url, &rpc.secret) {
					(Some(u), None) => {
						if !u.starts_with("https://") && !u.starts_with("http://") {
							p.push(format!("{at}: rpcs[{j}].url must be an http(s) URL"));
						} else if prod && !u.starts_with("https://") {
							p.push(format!(
								"{at}: rpcs[{j}].url must use https:// in production"
							));
						}
					}
					(None, Some(_)) => {}
					_ => p.push(format!(
						"{at}: rpcs[{j}] needs exactly one of url or secret"
					)),
				}
			}
		}

		// Pricing: in production every quote's rate must be checkable against
		// a reference, and quotes on a rail with no reference are refused.
		if prod && self.pricing.reference_rates.is_empty() {
			p.push(
				"pricing.reference_rates needs a reference rate for every rail you publish offers on (for example ng.nip) in production"
					.into(),
			);
		}

		// Rail.
		match self.rail.kind {
			RailKind::Simulated if prod => p.push(
				"rail.kind simulated is for sandbox only; production needs a real rail".into(),
			),
			RailKind::Paystack if self.rail.credentials.is_none() => {
				p.push("rail.credentials must reference your Paystack secret key".into())
			}
			RailKind::Template => {
				if self.rail.credentials.is_none() {
					p.push("rail.credentials must reference your adapter's bearer token".into());
				}
				match self.rail.url.as_deref() {
					None => p.push("rail.url must be your adapter's address, such as https://payouts.internal:8443".into()),
					Some(u) if u.starts_with("https://") => {}
					Some(u) if u.starts_with("http://") && !prod => {}
					Some(u) if u.starts_with("http://") => p.push(format!(
						"rail.url is {u}: production sends bank details to the adapter, so it must be https://"
					)),
					Some(u) => p.push(format!("rail.url is {u}, which is not an http(s) URL")),
				}
			}
			_ => {}
		}
		if self.rail.url.is_some() && self.rail.kind != RailKind::Template {
			p.push("rail.url is only for rail.kind template".into());
		}

		// Limits: every one written down, in every mode.
		let limits = match self.limits {
			None => {
				p.push("limits are missing; every limit must be set explicitly (there are no defaults for money)".into());
				None
			}
			Some(l) => validate_limits(l, &mut p),
		};

		// Policy.
		let pol = self.policy;
		if prod && pol.name_match == NameMatch::Off {
			p.push("policy.name_match off is for sandbox only".into());
		}
		if prod && !pol.require_verified_name_match {
			p.push("policy.require_verified_name_match off is for sandbox only".into());
		}
		if pol.start_margin_secs < 30 {
			p.push("policy.start_margin_secs must be at least 30".into());
		}
		if pol.report_margin_secs < 30 {
			p.push("policy.report_margin_secs must be at least 30".into());
		}
		if pol.max_clock_skew_secs == 0 || pol.max_clock_skew_secs > 60 {
			p.push("policy.max_clock_skew_secs must be between 1 and 60".into());
		}
		if pol.max_rate_deviation_bps > 2_000 {
			p.push("policy.max_rate_deviation_bps must be at most 2000 (20%)".into());
		}
		if !(1..=600).contains(&self.alerts.max_per_minute) {
			p.push("alerts.max_per_minute must be between 1 and 600".into());
		}
		if let Some(e) = &self.alerts.email {
			if e.to.is_empty() || !e.from.contains('@') || e.to.iter().any(|t| !t.contains('@')) {
				p.push("alerts.email needs a from address and at least one to address".into());
			}
		}
		if pol.amount_tolerance_bps > 100 {
			p.push(
				"policy.amount_tolerance_bps must be at most 100 (1%); the recomputation is exact"
					.into(),
			);
		}
		for rail in self.pricing.reference_rates.keys() {
			let ok = rail.split_once('.').is_some_and(|(c, r)| {
				c.len() == 2 && c.chars().all(|c| c.is_ascii_lowercase()) && !r.is_empty()
			});
			if !ok {
				p.push(format!(
					"pricing.reference_rates: {rail} is not a rail id such as ng.nip"
				));
			}
		}
		for (rail, rate) in &self.pricing.reference_rates {
			if rate.as_str().chars().all(|c| c == '0' || c == '.') {
				p.push(format!(
					"pricing.reference_rates.{rail} must be more than 0"
				));
			}
		}
		if pol.two_person.enabled && pol.two_person.approve_over_minor.is_none() {
			p.push(
				"policy.two_person.approve_over_minor is required when two_person is enabled"
					.into(),
			);
		}
		let policy = Policy {
			start_margin_secs: pol.start_margin_secs,
			report_margin_secs: pol.report_margin_secs,
			max_clock_skew_secs: pol.max_clock_skew_secs,
			max_rate_deviation_bps: pol.max_rate_deviation_bps,
			amount_tolerance_bps: pol.amount_tolerance_bps,
			name_match: pol.name_match,
			require_verified_name_match: pol.require_verified_name_match,
			two_person: TwoPerson {
				enabled: pol.two_person.enabled,
				approve_over: pol.two_person.approve_over_minor,
			},
		};

		// Server (invariant 9).
		for (name, addr) in [
			("console_bind", self.server.console_bind),
			("health_bind", self.server.health_bind),
		] {
			if !addr.ip().is_loopback() && !self.server.bind_public {
				p.push(format!(
					"server.{name} is {addr}, which is reachable from other machines; \
					 bind to 127.0.0.1 or set server.bind_public: true"
				));
			}
		}

		// Secrets: env:// only in sandbox (invariant 8).
		if prod {
			let mut refs = vec![
				("aggregator.api_key", &self.aggregator.api_key),
				("payee.private_key", &self.payee.private_key),
				("payee.tag_pepper", &self.payee.tag_pepper),
			];
			if let Some(c) = &self.rail.credentials {
				refs.push(("rail.credentials", c));
			}
			for (field, r) in [
				("alerts.webhook", self.alerts.webhook.as_ref()),
				(
					"alerts.pagerduty_routing_key",
					self.alerts.pagerduty_routing_key.as_ref(),
				),
				(
					"alerts.email.smtp_url",
					self.alerts.email.as_ref().map(|e| &e.smtp_url),
				),
			] {
				if let Some(r) = r {
					refs.push((field, r));
				}
			}
			for c in &self.chains {
				for rpc in &c.rpcs {
					if let Some(s) = &rpc.secret {
						refs.push(("chains[].rpcs[].secret", s));
					}
				}
			}
			for (field, r) in refs {
				if r.is_env() {
					p.push(format!(
						"{field} is {r}; env:// secrets are refused in production"
					));
				}
			}
		}

		if self.logging.level.parse::<tracing::Level>().is_err() {
			p.push("logging.level must be one of error, warn, info, debug, trace".into());
		}

		match (p.is_empty(), limits) {
			(true, Some(limits)) => Ok(Config {
				mode: self.mode,
				operator: self.operator,
				aggregator: self.aggregator,
				payee: self.payee,
				chains: self.chains,
				rail: self.rail,
				limits,
				policy,
				pricing: self.pricing,
				screening: self.screening,
				alerts: self.alerts,
				server: self.server,
				storage: self.storage,
				logging: self.logging,
			}),
			_ => Err(ConfigError::Invalid(p)),
		}
	}
}

fn validate_limits(l: RawLimits, p: &mut Vec<String>) -> Option<Limits> {
	let mut missing = Vec::new();
	macro_rules! need {
		($f:ident) => {
			match l.$f.clone() {
				Some(v) => Some(v),
				None => {
					missing.push(stringify!($f));
					None
				}
			}
		};
	}
	let currency = need!(currency);
	let per_trade = need!(per_trade_minor);
	let per_day = need!(per_day_minor);
	let per_payee = need!(per_payee_per_day_minor);
	let vmax = need!(velocity_max_payouts);
	let vwin = need!(velocity_window_secs);
	let float = need!(min_float_minor);
	if !missing.is_empty() {
		p.push(format!(
			"limits.{} {} missing; every limit must be set explicitly (there are no defaults for money)",
			missing.join(", limits."),
			if missing.len() == 1 { "is" } else { "are" }
		));
		return None;
	}
	let (currency, per_trade, per_day, per_payee, vmax, vwin, float) = (
		currency?, per_trade?, per_day?, per_payee?, vmax?, vwin?, float?,
	);

	let before = p.len();
	if currency != "NGN" {
		p.push("limits.currency must be NGN; v1 pays out naira only".into());
	}
	for (name, v) in [
		("per_trade_minor", per_trade),
		("per_day_minor", per_day),
		("per_payee_per_day_minor", per_payee),
	] {
		if v.get() == 0 {
			p.push(format!("limits.{name} must be more than 0"));
		}
	}
	if per_trade > per_day {
		p.push("limits.per_trade_minor cannot be more than limits.per_day_minor".into());
	}
	if per_payee > per_day {
		p.push("limits.per_payee_per_day_minor cannot be more than limits.per_day_minor".into());
	}
	if vmax == 0 {
		p.push("limits.velocity_max_payouts must be more than 0".into());
	}
	if vwin == 0 {
		p.push("limits.velocity_window_secs must be more than 0".into());
	}
	(p.len() == before).then_some(Limits {
		currency,
		per_trade,
		per_day,
		per_payee_per_day: per_payee,
		velocity_max_payouts: vmax,
		velocity_window_secs: vwin,
		min_float: float,
	})
}
