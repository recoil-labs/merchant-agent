//! `merchant-agent doctor`: check everything, print pass or fail, and give
//! the fix for each failure.
//!
//! A check that a later milestone builds is listed as skipped, with when it
//! arrives, so the list is honest about what has and hasn't been checked.
//! Production start requires a doctor run with no failures.

use std::fmt;
use std::path::Path;

use std::time::Duration;

use crate::aggregator::{AggregatorError, ChainInfo, Client};
use crate::chain::{ChainError, ChainReader};
use crate::config::{Config, Mode, PayeeKeyForm, RailKind};
use crate::payee::PayeePrivateKey;
use crate::policy::screening::DenyList;
use crate::secrets::Secrets;
use crate::store::{inspect, Inspection, SCHEMA_VERSION};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Outcome {
	Pass(String),
	Warn(String),
	Fail { why: String, fix: String },
	Skip(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Check {
	pub name: String,
	pub outcome: Outcome,
}

#[derive(Debug, Clone, Default)]
pub struct Report {
	pub checks: Vec<Check>,
}

impl Report {
	pub fn add(&mut self, name: impl Into<String>, outcome: Outcome) {
		self.checks.push(Check {
			name: name.into(),
			outcome,
		});
	}

	pub fn failures(&self) -> usize {
		self.checks
			.iter()
			.filter(|c| matches!(c.outcome, Outcome::Fail { .. }))
			.count()
	}

	pub fn passed(&self) -> bool {
		self.failures() == 0
	}
}

impl fmt::Display for Report {
	fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
		for c in &self.checks {
			match &c.outcome {
				Outcome::Pass(note) => writeln!(f, "PASS  {}  {note}", c.name)?,
				Outcome::Warn(note) => writeln!(f, "WARN  {}  {note}", c.name)?,
				Outcome::Fail { why, fix } => {
					writeln!(f, "FAIL  {}  {why}", c.name)?;
					writeln!(f, "        fix: {fix}")?;
				}
				Outcome::Skip(why) => writeln!(f, "SKIP  {}  {why}", c.name)?,
			}
		}
		let fails = self.failures();
		if fails == 0 {
			writeln!(f, "\nNo failures.")
		} else {
			writeln!(
				f,
				"\n{fails} failure(s). Fix them and run `merchant-agent doctor` again."
			)
		}
	}
}

/// Checks against the aggregator, in the order they run.
const AGGREGATOR: [&str; 4] = [
	"Aggregator reachable and API key accepted",
	"Clock agrees with the aggregator",
	"Pinned escrow and USDC match RecoilPay's registry",
	"Payee key matches the key registered with RecoilPay",
];

const RPC: &str = "RPC reachable, right chain id, escrow deployed";

const DATABASE: &str = "Database opens and migrations are current";
const LEASE: &str = "No other agent holds the lease";

const RAIL: &str = "Bank rail reachable and balance readable";
const PRICING: &str = "Reference rates set for the rate check";
const SCREENING: &str = "Screening list loads";

const ALERTS: &str = "Alert channels configured";

pub async fn run(config_path: &Path) -> Report {
	let mut r = Report::default();
	let shown = config_path.display().to_string();

	let text = match std::fs::read_to_string(config_path) {
		Ok(t) => {
			r.add("Config file readable", Outcome::Pass(shown.clone()));
			t
		}
		Err(e) => {
			r.add(
				"Config file readable",
				Outcome::Fail {
					why: format!("{shown}: {}", e.kind()),
					fix: "run `merchant-agent init` to write one, or pass --config <path>".into(),
				},
			);
			skip_rest(&mut r, "needs a config file");
			return r;
		}
	};

	let config = match Config::parse(&text) {
		Ok(c) => {
			r.add("Config is valid", Outcome::Pass(format!("{} mode", c.mode)));
			c
		}
		Err(e) => {
			for problem in e.problems() {
				r.add(
					"Config is valid",
					Outcome::Fail {
						why: problem,
						fix: format!("edit {shown}"),
					},
				);
			}
			skip_rest(&mut r, "needs a valid config");
			return r;
		}
	};

	match config.mode {
		Mode::Sandbox => r.add(
			"Mode",
			Outcome::Warn("sandbox: testnet and the simulated bank; no real money moves".into()),
		),
		Mode::Production => r.add("Mode", Outcome::Pass("production".into())),
	}

	let secrets = Secrets::new(config.mode);
	for (what, reference) in config.secret_refs() {
		let name = format!("Secret readable: {what}");
		match secrets.resolve(reference).await {
			// The value is dropped (and zeroed) at once: doctor only needs to
			// know it can be read.
			Ok(_value) => r.add(name, Outcome::Pass(reference.to_string())),
			Err(e) => r.add(
				name,
				Outcome::Fail {
					why: e.to_string(),
					fix: secret_fix(&e),
				},
			),
		}
	}

	r.add(
		"Payee key is a valid key",
		payee_key_check(&config, &secrets).await,
	);
	r.add(
		"Data directory writable",
		data_dir_check(&config.storage.data_dir),
	);

	aggregator_checks(&mut r, &config, &secrets).await;
	rpc_checks(&mut r, &config, &secrets).await;
	store_checks(&mut r, &config);
	r.add(RAIL, rail_check(&config, &secrets).await);
	r.add(PRICING, pricing_check(&config));
	r.add(SCREENING, screening_check(&config));

	r.add(ALERTS, alerts_check(&config));
	r
}

fn skip_rest(r: &mut Report, why: &str) {
	for name in AGGREGATOR {
		r.add(name, Outcome::Skip(why.into()));
	}
	r.add(RPC, Outcome::Skip(why.into()));
	r.add(DATABASE, Outcome::Skip(why.into()));
	r.add(LEASE, Outcome::Skip(why.into()));
	r.add(RAIL, Outcome::Skip(why.into()));
	r.add(PRICING, Outcome::Skip(why.into()));
	r.add(SCREENING, Outcome::Skip(why.into()));
	r.add(ALERTS, Outcome::Skip(why.into()));
}

/// Doctor names the channels but sends nothing: `merchant-agent alerts
/// test` does that, on purpose, so a routine check never pages anyone.
fn alerts_check(config: &Config) -> Outcome {
	let a = &config.alerts;
	let mut channels = Vec::new();
	if a.webhook.is_some() {
		channels.push("webhook");
	}
	if a.pagerduty_routing_key.is_some() {
		channels.push("pagerduty");
	}
	if a.email.is_some() {
		channels.push("email");
	}
	match (channels.is_empty(), config.mode) {
		(false, _) => Outcome::Pass(format!(
			"{}; run `merchant-agent alerts test` to send a test message",
			channels.join(", ")
		)),
		(true, Mode::Sandbox) => {
			Outcome::Pass("none; alerts are only logged, fine in sandbox".into())
		}
		(true, Mode::Production) => Outcome::Warn(
			"none: alerts are only logged and nobody is told when a payout needs a person".into(),
		),
	}
}

/// Each chain's RPC endpoints: reachable, on the pinned chain, with an
/// `AttestedEscrow` at the pinned address.
async fn rpc_checks(r: &mut Report, config: &Config, secrets: &Secrets) {
	for chain in &config.chains {
		let mut urls = Vec::new();
		for (i, rpc) in chain.rpcs.iter().enumerate() {
			match (&rpc.url, &rpc.secret) {
				(Some(u), _) => urls.push(u.clone()),
				(None, Some(s)) => match secrets.resolve(s).await {
					Ok(v) => urls.push(v.expose().to_string()),
					Err(_) => {
						r.add(
							RPC,
							Outcome::Skip(format!(
								"{} rpc[{i}]: its secret could not be read (see above)",
								chain.name
							)),
						);
						continue;
					}
				},
				(None, None) => {}
			}
		}
		if urls.is_empty() {
			continue;
		}
		let timeout = Duration::from_secs(config.aggregator.request_timeout_secs);
		let outcome = match ChainReader::new(chain, urls, timeout, config.mode == Mode::Production)
		{
			Err(e) => Outcome::Fail {
				why: e.to_string(),
				fix: format!("fix chains ({}) in the config", chain.name),
			},
			Ok(reader) => match reader.check().await {
				Ok(status) => Outcome::Pass(format!(
					"{}: {} endpoint(s), latest block {}",
					chain.name,
					status.len(),
					status.iter().map(|s| s.latest_block).max().unwrap_or(0)
				)),
				Err(e) => {
					let fix = match &e {
						ChainError::WrongChain { .. } => "point this chain's rpcs at the right network".to_string(),
						ChainError::NoEscrowCode(_) => {
							"the pinned escrow has no contract on this network: check the escrow address and the RPC's network".into()
						}
						_ => "check the endpoint, and that this host can reach it".into(),
					};
					Outcome::Fail {
						why: e.to_string(),
						fix,
					}
				}
			},
		};
		r.add(RPC, outcome);
	}
}

async fn aggregator_checks(r: &mut Report, config: &Config, secrets: &Secrets) {
	let [reach, clock, pins, payee] = AGGREGATOR;
	let skip_all = |r: &mut Report, why: &str| {
		for name in AGGREGATOR {
			r.add(name, Outcome::Skip(why.into()));
		}
	};
	let Ok(api_key) = secrets.resolve(&config.aggregator.api_key).await else {
		return skip_all(r, "the API key secret could not be read (see above)");
	};
	let client = match Client::from_config(config, api_key) {
		Ok(c) => c,
		Err(e) => {
			return skip_all(r, &format!("cannot build a client: {e}"));
		}
	};

	// Reachable, and the key is this operator's: reading the operator's own
	// quote book needs both.
	match client.get_quotes().await {
		Ok(book) => r.add(
			reach,
			Outcome::Pass(format!("{} offer(s) in your book", book.len())),
		),
		Err(e) => {
			let fix = match &e {
				AggregatorError::Unauthorized => {
					"the API key was refused: copy the current key from the portal into the secret"
						.into()
				}
				AggregatorError::Forbidden(_) => format!(
					"the API key belongs to another operator: check operator.solver_id ({})",
					config.operator.solver_id
				),
				_ => format!(
					"check aggregator.url ({}) and that this host can reach it",
					config.aggregator.url
				),
			};
			r.add(
				reach,
				Outcome::Fail {
					why: e.to_string(),
					fix,
				},
			);
			for name in [clock, pins, payee] {
				r.add(name, Outcome::Skip("needs the aggregator".into()));
			}
			return;
		}
	}

	r.add(
		clock,
		match client.clock_skew().await {
			Ok(skew) if skew.num_seconds().unsigned_abs() <= config.policy.max_clock_skew_secs => {
				Outcome::Pass(format!("{}s apart", skew.num_seconds()))
			}
			Ok(skew) => Outcome::Fail {
				why: format!(
					"this host's clock is {}s from the aggregator's (allowed: {}s)",
					skew.num_seconds(),
					config.policy.max_clock_skew_secs
				),
				fix: "turn on time sync (NTP / chrony / systemd-timesyncd) on this host".into(),
			},
			Err(e) => Outcome::Fail {
				why: e.to_string(),
				fix: "check that the aggregator's /health answers".into(),
			},
		},
	);

	r.add(
		pins,
		match client.get_chains().await {
			Ok(chains) => pin_check(config, &chains),
			Err(e) => Outcome::Fail {
				why: e.to_string(),
				fix: "retry; the chain registry is public".into(),
			},
		},
	);

	r.add(
		payee,
		payee_registered_check(config, secrets, &client).await,
	);
}

/// Every chain in the config must match RecoilPay's registry exactly. A
/// mismatch stops payouts until a person has confirmed which is right.
fn pin_check(config: &Config, chains: &[ChainInfo]) -> Outcome {
	let mut problems = Vec::new();
	for c in &config.chains {
		let id: Option<u64> = c.caip2.strip_prefix("eip155:").and_then(|n| n.parse().ok());
		let Some(registry) = chains.iter().find(|r| Some(r.chain_id) == id) else {
			problems.push(format!(
				"{} ({}) is not in RecoilPay's registry",
				c.name, c.caip2
			));
			continue;
		};
		if !registry.fiat_escrow.eq_ignore_ascii_case(&c.escrow) {
			problems.push(format!(
				"{}: escrow is pinned to {}, but RecoilPay lists {}",
				c.name,
				c.escrow,
				if registry.fiat_escrow.is_empty() {
					"none"
				} else {
					&registry.fiat_escrow
				}
			));
		}
		match registry.tokens.iter().find(|t| t.symbol == "USDC") {
			Some(t) if t.address.eq_ignore_ascii_case(&c.usdc) && t.decimals == c.usdc_decimals => {
			}
			Some(t) => problems.push(format!(
				"{}: USDC is pinned to {} ({} decimals), but RecoilPay lists {} ({} decimals)",
				c.name, c.usdc, c.usdc_decimals, t.address, t.decimals
			)),
			None => problems.push(format!("{}: RecoilPay lists no USDC", c.name)),
		}
	}
	if problems.is_empty() {
		Outcome::Pass(format!("{} chain(s) match", config.chains.len()))
	} else {
		Outcome::Fail {
			why: problems.join("; "),
			fix: "the agent will not pay while these differ. Confirm the right addresses with RecoilPay \
			      before changing your config: a changed address is exactly what an attack would look like"
				.into(),
		}
	}
}

async fn payee_registered_check(config: &Config, secrets: &Secrets, client: &Client) -> Outcome {
	let Ok(secret) = secrets.resolve(&config.payee.private_key).await else {
		return Outcome::Skip("the payee key secret could not be read (see above)".into());
	};
	let Ok(key) = PayeePrivateKey::load(config.payee.key_form, &config.operator.solver_id, &secret)
	else {
		return Outcome::Skip("the payee key is not a valid key (see above)".into());
	};
	let ours = key.public_key_hex();
	match client.get_payee_key().await {
		Ok(Some(registered)) if registered.eq_ignore_ascii_case(&ours) => Outcome::Pass("they match".into()),
		Ok(Some(registered)) => Outcome::Fail {
			why: format!("RecoilPay has {registered}, but this agent's key is {ours}"),
			fix: "customers seal bank details to RecoilPay's key, so this agent could not open them. \
			      Export the current key from the portal, or find out who changed it"
				.into(),
		},
		Ok(None) => Outcome::Fail {
			why: "no payee key is registered with RecoilPay".into(),
			fix: "set up the payee key in the portal (Payouts), then export it for the agent".into(),
		},
		Err(e) => Outcome::Fail { why: e.to_string(), fix: "retry once the aggregator is reachable".into() },
	}
}

fn secret_fix(e: &crate::secrets::SecretError) -> String {
	use crate::secrets::SecretError as E;
	match e {
		E::EnvMissing { .. } => {
			"export the variable before starting the agent (see `merchant-agent init` output)"
				.into()
		}
		E::EnvInProduction { .. } => {
			"move the secret to a secret manager or a 0600 file and change the reference".into()
		}
		E::FileTooOpen { path, .. } => format!("chmod 600 {path}"),
		E::FileWrongOwner { path, agent, .. } => format!("chown {agent} {path}"),
		E::BackendNotBuilt { feature, .. } => {
			format!("use an image built with --features {feature}")
		}
		_ => "check the reference and that the agent's identity can read it".into(),
	}
}

async fn payee_key_check(config: &Config, secrets: &Secrets) -> Outcome {
	let Ok(secret) = secrets.resolve(&config.payee.private_key).await else {
		return Outcome::Skip("the payee key secret could not be read (see above)".into());
	};
	match PayeePrivateKey::load(config.payee.key_form, &config.operator.solver_id, &secret) {
		// The public half is not secret; showing it lets the person compare
		// it with the key in the portal.
		Ok(key) => Outcome::Pass(format!("public key {}", key.public_key_hex())),
		Err(e) => Outcome::Fail {
			why: e.to_string(),
			fix: format!(
				"export the payee key again from the portal, or check that payee.key_form ({}) matches what the secret holds",
				match config.payee.key_form {
					PayeeKeyForm::PrivateKey => "private_key",
					PayeeKeyForm::Signature => "signature",
				}
			),
		},
	}
}

/// Read-only: doctor never migrates the database or takes the lease.
fn store_checks(r: &mut Report, config: &Config) {
	let dir = &config.storage.data_dir;
	match inspect(dir) {
		Ok(Inspection { schema: None, .. }) => {
			r.add(
				DATABASE,
				Outcome::Pass("none yet; it is created on first run".into()),
			);
			r.add(LEASE, Outcome::Pass("free".into()));
		}
		Ok(Inspection {
			schema: Some(v),
			lease,
		}) => {
			r.add(
				DATABASE,
				match v.cmp(&SCHEMA_VERSION) {
					std::cmp::Ordering::Equal => Outcome::Pass(format!("schema {v}")),
					std::cmp::Ordering::Less => Outcome::Warn(format!(
					"schema {v}; this agent upgrades it to {SCHEMA_VERSION} when it starts. Back up {} first",
					dir.display()
				)),
					std::cmp::Ordering::Greater => Outcome::Fail {
						why: format!(
							"schema {v} is from a newer agent (this one knows {SCHEMA_VERSION})"
						),
						fix: "run the newer agent version; never downgrade over a newer database"
							.into(),
					},
				},
			);
			r.add(
				LEASE,
				match lease {
					Some((holder, until)) if until > chrono::Utc::now() => Outcome::Warn(format!(
					"held by {holder} until {until}. Expected while the agent runs; if it is not running, \
					 a new agent waits for this to expire"
				)),
					_ => Outcome::Pass("free".into()),
				},
			);
		}
		Err(e) => {
			r.add(
				DATABASE,
				Outcome::Fail {
					why: e.to_string(),
					fix: format!(
						"check {} is the agent's data directory and is readable",
						dir.display()
					),
				},
			);
			r.add(LEASE, Outcome::Skip("needs the database".into()));
		}
	}
}

/// Without a reference rate a rail's offers can't be checked against what
/// the merchant believes the market is (invariant 3). Allowed, but said.
fn pricing_check(config: &Config) -> Outcome {
	let missing: Vec<&str> = ["ng.nip"]
		.into_iter()
		.filter(|rail| !config.pricing.reference_rates.contains_key(*rail))
		.collect();
	match (missing.is_empty(), config.mode) {
		(true, _) => Outcome::Pass(format!(
			"{} rail(s), within {} bps",
			config.pricing.reference_rates.len(),
			config.policy.max_rate_deviation_bps
		)),
		(false, Mode::Sandbox) => {
			Outcome::Pass(format!("none for {}; fine in sandbox", missing.join(", ")))
		}
		(false, Mode::Production) => Outcome::Warn(format!(
			"no reference rate for {}, so offers on it are not checked against your market rate. \
			 Set pricing.reference_rates",
			missing.join(", ")
		)),
	}
}

fn screening_check(config: &Config) -> Outcome {
	match &config.screening.deny_list {
		None => Outcome::Pass("no deny-list configured".into()),
		Some(path) => match DenyList::load(path) {
			Ok(list) => Outcome::Pass(format!("{} payee tag(s)", list.len())),
			Err(e) => Outcome::Fail {
				why: e,
				fix: "make the list with `merchant-agent screening hash`".into(),
			},
		},
	}
}

async fn rail_check(config: &Config, secrets: &Secrets) -> Outcome {
	match config.rail.kind {
		RailKind::Simulated => {
			Outcome::Warn("the simulated bank: payouts are pretend, and no real money moves".into())
		}
		RailKind::Manual => Outcome::Pass(
			"manual: a person pays from the bank app. The agent can't check names or the balance on this rail, \
			 so check both in the bank app"
				.into(),
		),
		RailKind::Paystack => paystack_check(config, secrets).await,
		RailKind::Template => template_check(config, secrets).await,
	}
}

/// The adapter answers the protocol, takes the token, and shows a balance.
async fn template_check(config: &Config, secrets: &Secrets) -> Outcome {
	use crate::rail::template::TemplateRail;
	use crate::rail::PayoutRail;
	let (Some(r), Some(url)) = (&config.rail.credentials, config.rail.url.as_deref()) else {
		return Outcome::Skip("rail.url and rail.credentials are needed (see above)".into());
	};
	let Ok(token) = secrets.resolve(r).await else {
		return Outcome::Skip("the adapter token could not be read (see above)".into());
	};
	let rail = match TemplateRail::connect(url, token, Duration::from_secs(15)).await {
		Ok(r) => r,
		Err(why) => {
			return Outcome::Fail {
				why,
				fix: "check rail.url, that the adapter is running, and the token it expects".into(),
			}
		}
	};
	let name = if rail.info().name.is_empty() {
		"the adapter".to_string()
	} else {
		rail.info().name.clone()
	};
	let resend = if rail.info().idempotent_send {
		"idempotent sends"
	} else {
		"NOT idempotent: a lost answer always goes to a person"
	};
	match rail.balance().await {
		Ok(Some(kobo)) => Outcome::Pass(format!(
			"{name}, protocol 1, {resend}; NGN balance {}",
			crate::money::naira(kobo)
		)),
		Ok(None) => Outcome::Warn(format!(
			"{name}, protocol 1, {resend}; it can't show a balance, so the float check is off"
		)),
		Err(e) => Outcome::Fail {
			why: e.to_string(),
			fix: "the adapter's /v1/balance must answer (see docs/adapters.md)".into(),
		},
	}
}

/// The key suits the mode, Paystack accepts it, and the NGN balance reads.
async fn paystack_check(config: &Config, secrets: &Secrets) -> Outcome {
	use crate::rail::paystack::{checked_key, PaystackRail};
	use crate::rail::PayoutRail;
	let Some(r) = &config.rail.credentials else {
		return Outcome::Skip("no rail.credentials (see above)".into());
	};
	let Ok(key) = secrets.resolve(r).await else {
		return Outcome::Skip("the Paystack key could not be read (see above)".into());
	};
	let key = match checked_key(key, config.mode) {
		Ok(k) => k,
		Err(why) => {
			return Outcome::Fail {
				why,
				fix: "set rail.credentials to the right Paystack key".into(),
			}
		}
	};
	let rail = match PaystackRail::new(key) {
		Ok(r) => r,
		Err(why) => {
			return Outcome::Fail {
				why,
				fix: "report this as a bug".into(),
			}
		}
	};
	match rail.balance().await {
		Ok(Some(kobo)) => Outcome::Pass(format!("Paystack answers; NGN balance {}", crate::money::naira(kobo))),
		Ok(None) => Outcome::Warn("Paystack answers, but shows no NGN balance".into()),
		Err(e) => Outcome::Fail {
			why: e.to_string(),
			fix: "check the key in the Paystack dashboard (Settings → API Keys) and that api.paystack.co is reachable".into(),
		},
	}
}

fn data_dir_check(dir: &Path) -> Outcome {
	let shown = dir.display().to_string();
	if !dir.is_dir() {
		return Outcome::Fail {
			why: format!("{shown} does not exist"),
			fix: format!("mkdir -p {shown} and make it owned by the agent's user"),
		};
	}
	let probe = dir.join(".doctor-write-test");
	match std::fs::write(&probe, b"ok").and_then(|_| std::fs::remove_file(&probe)) {
		Ok(()) => Outcome::Pass(shown),
		Err(e) => Outcome::Fail {
			why: format!("{shown} is not writable: {}", e.kind()),
			fix: format!("make {shown} owned by the agent's user"),
		},
	}
}
