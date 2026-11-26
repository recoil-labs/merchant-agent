//! `merchant-agent run`: build every part from the config and drive the
//! engine until told to stop.
//!
//! - Every secret is resolved once, at start-up, from its backend.
//! - If another agent holds the lease (a crashed one whose lease hasn't
//!   expired yet, or a second instance), this one waits and says so. It
//!   never acts alongside it.
//! - Each tick polls, reconciles and advances every trade. Alerts are logged
//!   in plain words until the notifier exists (M8).
//! - Ctrl-C (or SIGTERM from a container runtime) stops it between ticks. A
//!   payout already with the bank is unaffected: the next start asks the
//!   bank by key.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use crate::aggregator::Client;
use crate::chain::ChainReader;
use crate::config::{Config, Mode, RailKind};
use crate::console::auth::{Sessions, TotpKey};
use crate::console::Console;
use crate::engine::{
	Alert, ChainLocks, Checks, Clock, Engine, EngineConfig, EngineError, Locks, PayeeChecks,
	Platform, SystemClock,
};
use crate::money::{naira, Minor};
use crate::notify::{Alerter, Email, Notifier, PagerDuty, Webhook};
use crate::payee::PayeePrivateKey;
use crate::policy::screening::DenyList;
use crate::policy::PolicyChecks;
use crate::purchases::{ChainFloats, ChainPin, Purchases, Rules as PurchaseRules};
use crate::rail::manual::ManualRail;
use crate::rail::paystack::{self, PaystackRail};
use crate::rail::simulated::SimulatedRail;
use crate::rail::template::TemplateRail;
use crate::rail::PayoutRail;
use crate::secrets::Secrets;
use crate::store::{Store, StoreError};

/// Sandbox mode's pretend bank balance: ₦100,000,000.00.
const SANDBOX_BALANCE: Minor = Minor(10_000_000_000);

/// One line for people: what happened, what the agent did, what to do.
pub fn describe(alert: &Alert) -> String {
	use crate::machine::AgentState as S;
	if let Some(why) = &alert.purchase_refusal {
		return format!(
			"Purchase {}: refused, {}. {why} The agent signed nothing, so none of your USDC moved; the buyer is told you didn't confirm. Review it under Purchases in the console.",
			alert.trade_id,
			naira(alert.amount),
		);
	}
	let what = match &alert.state {
		S::Held(r) => r.message().to_string(),
		S::NeedsPerson(r) => r.message().to_string(),
		other => format!("The trade is now: {}.", other.label()),
	};
	let did = match &alert.state {
		S::Held(_) => "The agent held it and paid nothing.",
		S::NeedsPerson(_) => "The agent stopped and will not move money for it on its own.",
		_ => "The agent is following it.",
	};
	let todo = match &alert.state {
		S::Held(_) | S::NeedsPerson(_) => {
			"Review it in the exception queue (`merchant-agent status`)."
		}
		_ => "No action needed unless this is unexpected.",
	};
	let account = alert
		.account_masked
		.as_deref()
		.unwrap_or("(account not yet known)");
	let detail = alert
		.detail
		.as_deref()
		.map(|d| format!(" Bank: {d}."))
		.unwrap_or_default();
	format!(
		"Trade {}: {}{detail} {} {} to {account}. {did} {todo}",
		alert.trade_id,
		alert.state.label(),
		what,
		naira(alert.amount),
	)
}

/// A name for this process as a lease holder: unique per start, so a
/// restarted agent waits out a crashed one's lease instead of assuming it.
pub fn holder_id() -> String {
	use rand::Rng;
	format!(
		"agent-{}-{:08x}",
		std::process::id(),
		rand::rng().random::<u32>()
	)
}

/// The config file's digest, recorded at each start so a change is audited.
pub fn config_digest(text: &str) -> String {
	use sha2::{Digest, Sha256};
	hex::encode(Sha256::digest(text.as_bytes()))
}

/// A one-way fingerprint of a secret, keyed with the pepper: changes when
/// the secret does, reveals nothing about it.
pub fn fingerprint(pepper: &crate::secrets::Secret, name: &str, value: &str) -> String {
	use hmac::{Hmac, Mac};
	let mut mac =
		Hmac::<sha2::Sha256>::new_from_slice(pepper.expose().as_bytes()).expect("any key length");
	mac.update(name.as_bytes());
	mac.update(&[0]);
	mac.update(value.as_bytes());
	hex::encode(&mac.finalize().into_bytes()[..8])
}

/// Audit this start, and anything that changed since the last one: the
/// config file, or a secret (by fingerprint).
pub fn audit_start(
	store: &Store,
	mode: Mode,
	digest: &str,
	fingerprints: &[(&str, String)],
) -> Result<(), String> {
	let now = chrono::Utc::now();
	let err = |e: StoreError| e.to_string();
	if let Some(prev) = store.last_audit("agent_started").map_err(err)? {
		let field = |key: &str| {
			prev.detail
				.split("; ")
				.find_map(|p| p.strip_prefix(key))
				.map(str::to_string)
		};
		if let Some(old) = field("config ").filter(|old| old != digest) {
			store
				.audit_event(
					"agent",
					"config_changed",
					None,
					&format!("config file digest {old} became {digest}"),
					now,
				)
				.map_err(err)?;
		}
		let old: HashMap<String, String> = field("secrets ")
			.unwrap_or_default()
			.split(',')
			.filter_map(|kv| {
				kv.split_once('=')
					.map(|(k, v)| (k.to_string(), v.to_string()))
			})
			.collect();
		for (name, fp) in fingerprints {
			if old.get(*name).is_some_and(|o| o != fp) {
				store
					.audit_event("agent", "secret_rotated", None, name, now)
					.map_err(err)?;
			}
		}
	}
	let fps: Vec<String> = fingerprints
		.iter()
		.map(|(n, f)| format!("{n}={f}"))
		.collect();
	store
		.audit_event(
			"agent",
			"agent_started",
			None,
			&format!(
				"version {}; mode {mode}; config {digest}; secrets {}",
				env!("CARGO_PKG_VERSION"),
				fps.join(",")
			),
			now,
		)
		.map_err(err)
}

pub async fn run(
	config: Config,
	config_digest: &str,
	stop: impl std::future::Future<Output = ()>,
) -> Result<(), String> {
	let secrets = Secrets::new(config.mode);
	let prod = config.mode == Mode::Production;
	let read = |what: &'static str| move |e: crate::secrets::SecretError| format!("{what}: {e}");
	let pepper = secrets
		.resolve(&config.payee.tag_pepper)
		.await
		.map_err(read("the payee tag pepper"))?;
	let mut fingerprints = vec![("pepper", fingerprint(&pepper, "pepper", "pepper"))];

	let api_key = secrets
		.resolve(&config.aggregator.api_key)
		.await
		.map_err(read("the API key"))?;
	fingerprints.push(("api_key", fingerprint(&pepper, "api_key", api_key.expose())));
	let payee_secret = secrets
		.resolve(&config.payee.private_key)
		.await
		.map_err(read("the payee key"))?;
	fingerprints.push((
		"payee_key",
		fingerprint(&pepper, "payee_key", payee_secret.expose()),
	));
	let key = PayeePrivateKey::load(
		config.payee.key_form,
		&config.operator.solver_id,
		&payee_secret,
	)
	.map_err(|e| format!("the payee key: {e}"))?;
	// The float wallet's key: its own secret, never the payee key.
	let mut purchase_key = None;
	if config.purchases.enabled {
		let r = config
			.purchases
			.signer
			.as_ref()
			.ok_or("purchases.signer must reference your float wallet's private key")?;
		let signer = secrets
			.resolve(r)
			.await
			.map_err(read("the purchase float wallet key"))?;
		if signer.expose().trim() == payee_secret.expose().trim() {
			return Err(
				"purchases.signer holds the payee key; the float wallet needs its own key".into(),
			);
		}
		fingerprints.push((
			"purchase_signer",
			fingerprint(&pepper, "purchase_signer", signer.expose()),
		));
		purchase_key = Some(crate::purchases::permit::signing_key(signer.expose())?);
	}
	drop(payee_secret);
	let mut rail_key = None;
	if let Some(r) = &config.rail.credentials {
		let c = secrets
			.resolve(r)
			.await
			.map_err(read("the bank rail credentials"))?;
		fingerprints.push((
			"rail_credentials",
			fingerprint(&pepper, "rail_credentials", c.expose()),
		));
		rail_key = Some(c);
	}

	// The purchase watcher has its own client, so its 2-second poll never
	// waits behind the engine's calls.
	let purchase_api_key = config
		.purchases
		.enabled
		.then(|| crate::secrets::Secret::new(api_key.expose().to_string()));
	let client = Client::from_config(&config, api_key).map_err(|e| e.to_string())?;
	let registered = client
		.get_payee_key()
		.await
		.map_err(|e| format!("reading the registered payee key: {e}"))?;
	if !registered.is_some_and(|k| k.eq_ignore_ascii_case(&key.public_key_hex())) {
		return Err("the payee key does not match the one registered with RecoilPay; run `merchant-agent doctor`".into());
	}

	let timeout = Duration::from_secs(config.aggregator.request_timeout_secs);
	let mut chains = HashMap::new();
	let mut floats = HashMap::new();
	for chain in &config.chains {
		let mut urls = Vec::new();
		for rpc in &chain.rpcs {
			match (&rpc.url, &rpc.secret) {
				(Some(u), _) => urls.push(u.clone()),
				(None, Some(s)) => urls.push(
					secrets
						.resolve(s)
						.await
						.map_err(read("an RPC URL"))?
						.expose()
						.to_string(),
				),
				(None, None) => {}
			}
		}
		if purchase_key.is_some() {
			let reader =
				ChainReader::new(chain, urls.clone(), timeout, prod).map_err(|e| e.to_string())?;
			floats.insert(chain.caip2.clone(), Arc::new(reader));
		}
		let reader = ChainReader::new(chain, urls, timeout, prod).map_err(|e| e.to_string())?;
		chains.insert(chain.caip2.clone(), (chain.clone(), reader));
	}
	let locks = ChainLocks {
		chains,
		wallet: config.operator.wallet.clone(),
	};

	let screening = match &config.screening.deny_list {
		Some(path) => DenyList::load(path)?,
		None => DenyList::parse("")?,
	};
	let screening_for_purchases = Arc::new(match &config.screening.deny_list {
		Some(path) => DenyList::load(path)?,
		None => DenyList::parse("")?,
	});

	let alerter = alerter(&config, &secrets).await?;
	let console = ConsoleParts {
		key: TotpKey::from_pepper(&pepper),
		settings: settings_rows(&config, &alerter),
		limits: Some(config.limits.clone()),
		two_person: config.policy.two_person.clone(),
		bind: config.server.console_bind,
		health: config.server.health_bind,
		hostnames: config.server.console_hostnames.clone(),
	};

	crate::store::private_dir(&config.storage.data_dir)
		.map_err(|e| format!("{}: {e}", config.storage.data_dir.display()))?;
	let store =
		Arc::new(Store::open(&config.storage.data_dir, &holder_id()).map_err(|e| e.to_string())?);
	audit_start(&store, config.mode, config_digest, &fingerprints)?;
	let checks = PolicyChecks {
		payee: PayeeChecks { key, pepper },
		store: store.clone(),
		screening,
		amount_tolerance_bps: config.policy.amount_tolerance_bps,
		max_rate_deviation_bps: config.policy.max_rate_deviation_bps,
	};
	let cfg = EngineConfig::from_config(&config);
	let poll = Duration::from_secs(config.aggregator.poll_secs);
	let purchases = match (purchase_key, purchase_api_key) {
		(Some(key), Some(api_key)) => {
			let client = Client::from_config(&config, api_key).map_err(|e| e.to_string())?;
			Some(Arc::new(
				purchase_watcher(
					&config,
					&store,
					client,
					floats,
					key,
					&screening_for_purchases,
				)
				.await?,
			))
		}
		_ => None,
	};
	let shell = Shell {
		alerter,
		console,
		poll,
		purchases,
		purchase_poll: Duration::from_secs(config.purchases.poll_secs),
	};

	match config.rail.kind {
		RailKind::Simulated => {
			drive(
				store,
				client,
				locks,
				checks,
				SimulatedRail::sandbox_in(&config.storage.data_dir, SANDBOX_BALANCE)?,
				cfg,
				shell,
				stop,
			)
			.await
		}
		RailKind::Manual => {
			let rail = ManualRail::new(store.clone(), SystemClock);
			drive(store, client, locks, checks, rail, cfg, shell, stop).await
		}
		RailKind::Paystack => {
			let key = rail_key.ok_or("rail.credentials must reference your Paystack secret key")?;
			let rail = PaystackRail::new(paystack::checked_key(key, config.mode)?)?;
			drive(store, client, locks, checks, rail, cfg, shell, stop).await
		}
		RailKind::Template => {
			let token =
				rail_key.ok_or("rail.credentials must reference your adapter's bearer token")?;
			let url = config
				.rail
				.url
				.as_deref()
				.ok_or("rail.url must be your adapter's address")?;
			let rail = TemplateRail::connect(url, token, Duration::from_secs(30)).await?;
			tracing::info!(
				adapter = %rail.info().name,
				idempotent_send = rail.info().idempotent_send,
				"connected to the payout adapter"
			);
			drive(store, client, locks, checks, rail, cfg, shell, stop).await
		}
	}
}

/// The alert channels the config names, behind the rate limit.
pub async fn alerter(config: &Config, secrets: &Secrets) -> Result<Alerter, String> {
	let read = |what: &'static str| move |e: crate::secrets::SecretError| format!("{what}: {e}");
	let a = &config.alerts;
	let prod = config.mode == crate::config::Mode::Production;
	let mut channels: Vec<Box<dyn Notifier>> = Vec::new();
	if let Some(r) = &a.webhook {
		let url = secrets
			.resolve(r)
			.await
			.map_err(read("the alert webhook"))?;
		// Production refuses plain-HTTP connections (invariant 9), alert
		// channels included: a webhook URL carries its own credential.
		if prod
			&& !url
				.expose()
				.trim()
				.to_ascii_lowercase()
				.starts_with("https://")
		{
			return Err("the alert webhook must be an https:// URL in production".into());
		}
		channels.push(Box::new(Webhook::new(url)));
	}
	if let Some(r) = &a.pagerduty_routing_key {
		channels.push(Box::new(PagerDuty::new(
			secrets
				.resolve(r)
				.await
				.map_err(read("the PagerDuty key"))?,
		)));
	}
	if let Some(e) = &a.email {
		let url = secrets
			.resolve(&e.smtp_url)
			.await
			.map_err(read("the SMTP URL"))?;
		if prod && !smtp_url_is_encrypted(url.expose()) {
			return Err(
				"the SMTP URL must use TLS in production: smtps://… (port 465), or smtp://…?tls=required for STARTTLS".into(),
			);
		}
		channels.push(Box::new(Email::new(&url, &e.from, &e.to)?));
	}
	Ok(Alerter::new(channels, a.max_per_minute))
}

/// Whether an SMTP URL makes lettre encrypt the connection: implicit TLS
/// (`smtps://`), or `smtp://` with `tls=required` (STARTTLS, refused if the
/// server doesn't offer it). A bare `smtp://` may send mail and the
/// password in the clear.
fn smtp_url_is_encrypted(url: &str) -> bool {
	let u = url.trim().to_ascii_lowercase();
	u.starts_with("smtps://")
		|| (u.starts_with("smtp://")
			&& u.split_once('?')
				.is_some_and(|(_, q)| q.split('&').any(|kv| kv == "tls=required")))
}

/// The read-only settings the console shows. References, never secrets.
fn settings_rows(config: &Config, alerter: &Alerter) -> Vec<(String, String)> {
	let mut rows = vec![
		("Mode".to_string(), config.mode.to_string()),
		("Operator".into(), config.operator.solver_id.clone()),
		(
			"Your wallet (pinned)".into(),
			config.operator.wallet.clone(),
		),
		("RecoilPay".into(), config.aggregator.url.clone()),
		(
			"Bank rail".into(),
			format!("{:?}", config.rail.kind).to_lowercase(),
		),
		(
			"Name check".into(),
			match config.policy.name_match {
				crate::config::NameMatch::Strict => "strict",
				crate::config::NameMatch::TokenSet => "token_set (same words, any order)",
				crate::config::NameMatch::Off => "off",
			}
			.into(),
		),
		(
			"Start no payout with less than".into(),
			format!("{} s left", config.policy.start_margin_secs),
		),
		(
			"Amount tolerance".into(),
			format!("{} bps", config.policy.amount_tolerance_bps),
		),
		(
			"Rate band".into(),
			format!(
				"{} bps of your reference",
				config.policy.max_rate_deviation_bps
			),
		),
		(
			"Alert channels".into(),
			if alerter.channels().is_empty() {
				"none (logged only)".into()
			} else {
				alerter.channels().join(", ")
			},
		),
	];
	for c in &config.chains {
		rows.push((
			format!("Chain {}", c.name),
			format!(
				"{} · escrow {} · USDC {} · {} confirmation(s)",
				c.caip2, c.escrow, c.usdc, c.confirmations
			),
		));
	}
	for (rail, rate) in &config.pricing.reference_rates {
		rows.push((
			format!("Reference rate {rail}"),
			format!("NGN {} per USDC", rate.as_str()),
		));
	}
	rows
}

struct ConsoleParts {
	key: TotpKey,
	settings: Vec<(String, String)>,
	limits: Option<crate::config::Limits>,
	two_person: crate::config::TwoPerson,
	bind: std::net::SocketAddr,
	health: std::net::SocketAddr,
	hostnames: Vec<String>,
}

/// What runs around the engine: alerts, the console, the pace, and the
/// purchase watcher when purchases are enabled.
struct Shell {
	alerter: Alerter,
	console: ConsoleParts,
	poll: Duration,
	purchases: Option<Arc<Purchases<Client, ChainFloats>>>,
	purchase_poll: Duration,
}

/// The purchase watcher, once its float wallet is the one RecoilPay has
/// registered on every chain set to merchant-signed.
async fn purchase_watcher(
	config: &Config,
	store: &Arc<Store>,
	client: Client,
	floats: HashMap<String, Arc<ChainReader>>,
	key: k256::ecdsa::SigningKey,
	deny: &Arc<DenyList>,
) -> Result<Purchases<Client, ChainFloats>, String> {
	let mut pins = Vec::new();
	for c in &config.chains {
		let parse = |what: &str, s: &str| {
			s.parse()
				.map_err(|_| format!("{}: {what} is not an address", c.name))
		};
		pins.push(ChainPin {
			caip2: c.caip2.clone(),
			chain_id: c
				.caip2
				.strip_prefix("eip155:")
				.and_then(|n| n.parse().ok())
				.ok_or_else(|| format!("{} is not an EVM chain", c.caip2))?,
			escrow: parse("escrow", &c.escrow)?,
			usdc: parse("usdc", &c.usdc)?,
			usdc_decimals: c.usdc_decimals,
			float_floor: config
				.purchases
				.float_floor
				.get(&c.caip2)
				.map(|m| m.get())
				.unwrap_or(u128::MAX),
		});
	}
	let rules = PurchaseRules {
		solver_id: config.operator.solver_id.clone(),
		max_per_trade: config.purchases.max_per_trade_minor.unwrap_or_default(),
		max_per_day: config.purchases.max_per_day_minor.unwrap_or_default(),
		max_deadline_secs: config.purchases.max_deadline_secs as i64,
		max_rate_deviation_bps: config.policy.max_rate_deviation_bps,
	};
	let float_wallet = format!("{:#x}", crate::purchases::permit::address_of(&key));
	let registered = client
		.get_purchase_funding()
		.await
		.map_err(|e| format!("reading the registered purchase funding: {e}"))?;
	let signed: Vec<_> = registered
		.iter()
		.filter(|f| f.mode == crate::aggregator::PurchaseFundingMode::MerchantSigned)
		.collect();
	for f in &signed {
		if !f
			.float_wallet
			.as_deref()
			.is_some_and(|w| w.eq_ignore_ascii_case(&float_wallet))
		{
			return Err(format!(
				"purchases.signer is {float_wallet}, but RecoilPay has {} as the float wallet on {}; run `merchant-agent doctor`",
				f.float_wallet.as_deref().unwrap_or("none"),
				f.chain
			));
		}
	}
	if signed.is_empty() {
		tracing::warn!("purchases are enabled, but no chain is set to merchant-signed in RecoilPay; nothing will be asked to sign until you set it in the portal");
	}
	tracing::info!(%float_wallet, chains = signed.len(), "signing merchant-signed purchases");
	Ok(Purchases::new(
		store.clone(),
		client,
		ChainFloats(floats),
		key,
		pins,
		rules,
		deny.clone(),
	))
}

#[allow(clippy::too_many_arguments)]
async fn drive<Pl, Lo, Ch, Ra>(
	store: Arc<Store>,
	platform: Pl,
	locks: Lo,
	checks: Ch,
	rail: Ra,
	cfg: EngineConfig,
	shell: Shell,
	stop: impl std::future::Future<Output = ()>,
) -> Result<(), String>
where
	Pl: Platform + 'static,
	Lo: Locks + 'static,
	Ch: Checks + 'static,
	Ra: PayoutRail + 'static,
{
	// Wait out another holder's lease rather than act beside it.
	loop {
		match store.acquire_lease(cfg.lease_ttl, SystemClock.now()) {
			Ok(_) => break,
			Err(StoreError::LeaseHeld { holder, until }) => {
				tracing::warn!(%holder, %until, "another agent holds the lease; waiting for it to expire");
				tokio::time::sleep(Duration::from_secs(5)).await;
			}
			Err(e) => return Err(e.to_string()),
		}
	}
	let engine = Arc::new(
		Engine::start(
			store.clone(),
			platform,
			locks,
			checks,
			rail,
			SystemClock,
			cfg,
		)
		.map_err(|e| e.to_string())?,
	);

	let Shell {
		alerter,
		console,
		poll,
		purchases,
		purchase_poll,
	} = shell;
	// Started only now, holding the lease: every purchase write checks it.
	if let Some(p) = purchases.clone() {
		tokio::spawn(async move {
			loop {
				match p.tick(chrono::Utc::now()).await {
					Ok(()) => {}
					Err(crate::purchases::PurchaseError::Store(StoreError::LeaseLost)) => {
						tracing::error!("the purchase watcher lost the lease and stopped signing");
						return;
					}
					Err(e) => tracing::error!(error = %e, "purchase pass failed; trying again"),
				}
				tokio::time::sleep(purchase_poll).await;
			}
		});
	}
	let bind = console.bind;
	let health_bind = console.health;
	let console = Arc::new(Console {
		store,
		ops: engine.clone(),
		sessions: Sessions::default(),
		key: console.key,
		settings: console.settings,
		limits: console.limits,
		two_person: console.two_person,
		clock: Arc::new(chrono::Utc::now),
		hostnames: console.hostnames,
	});
	let listener = tokio::net::TcpListener::bind(bind)
		.await
		.map_err(|e| format!("the console can't listen on {bind}: {e}"))?;
	if !bind.ip().is_loopback() {
		tracing::warn!(%bind, "the console is reachable from other machines; put it behind TLS and a firewall");
	}
	tokio::spawn(async move {
		if let Err(e) = axum::serve(listener, crate::console::router(console)).await {
			tracing::error!(error = %e, "the console stopped");
		}
	});
	let health = tokio::net::TcpListener::bind(health_bind)
		.await
		.map_err(|e| format!("the health endpoints can't listen on {health_bind}: {e}"))?;
	let ops: Arc<dyn crate::console::AgentOps> = engine.clone();
	tokio::spawn(async move {
		if let Err(e) = axum::serve(health, crate::health::router(ops)).await {
			tracing::error!(error = %e, "the health endpoints stopped");
		}
	});
	tracing::info!(
		console = %format!("http://{bind}"),
		health = %format!("http://{health_bind}/readyz"),
		channels = ?alerter.channels(),
		"merchant agent started"
	);

	tokio::pin!(stop);
	loop {
		match engine.tick().await {
			Ok(()) => {}
			Err(EngineError::Store(StoreError::LeaseLost)) => {
				return Err("this agent lost the lease to another one and stopped acting".into());
			}
			Err(e) => tracing::error!(error = %e, "tick failed; trying again next tick"),
		}
		// Every tick, even with nothing new: a summary of alerts held back
		// last minute goes out as soon as the minute turns, not only when
		// some later alert happens to arrive.
		let mut alerts = engine.take_alerts();
		if let Some(p) = &purchases {
			alerts.extend(p.take_alerts());
		}
		alerter.deliver(&alerts, chrono::Utc::now()).await;
		tokio::select! {
			_ = &mut stop => {
				tracing::info!("stopping; a payout already with the bank is checked by key on the next start");
				return Ok(());
			}
			_ = tokio::time::sleep(poll) => {}
		}
	}
}

#[cfg(test)]
mod tests {
	use super::smtp_url_is_encrypted;

	#[test]
	fn only_encrypted_smtp_urls_pass_in_production() {
		assert!(smtp_url_is_encrypted("smtps://u:p@mail.example.com:465"));
		assert!(smtp_url_is_encrypted(
			"smtp://u:p@mail.example.com:587?tls=required"
		));
		assert!(!smtp_url_is_encrypted("smtp://u:p@mail.example.com:587"));
		assert!(!smtp_url_is_encrypted(
			"smtp://u:p@mail.example.com:587?tls=opportunistic"
		));
	}
}
