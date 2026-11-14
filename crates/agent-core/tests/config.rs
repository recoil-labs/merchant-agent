//! Config parsing and validation: one test per rejection rule.

use merchant_agent_core::config::{Config, ConfigError, Mode, NameMatch, RailKind};
use merchant_agent_core::init::{render_sandbox_config, Answers};
use serde_yaml_ng::Value;

fn sandbox_yaml() -> String {
	render_sandbox_config(&Answers {
		solver_id: "solver-1a2b3c4d5e6f".into(),
		wallet: "0x1111111111111111111111111111111111111111".into(),
		aggregator_url: "http://127.0.0.1:4000".into(),
		data_dir: "/var/lib/merchant-agent".into(),
		bind_all: false,
		secrets_dir: None,
	})
}

fn doc(yaml: &str) -> Value {
	serde_yaml_ng::from_str(yaml).unwrap()
}

fn set(v: &mut Value, path: &str, new: Value) {
	let mut cur = v;
	let parts: Vec<&str> = path.split('.').collect();
	for (i, part) in parts.iter().enumerate() {
		let last = i == parts.len() - 1;
		cur = match part.parse::<usize>() {
			Ok(n) => &mut cur[n],
			Err(_) => {
				if last {
					cur.as_mapping_mut()
						.unwrap()
						.insert(Value::from(*part), new);
					return;
				}
				cur.get_mut(*part)
					.unwrap_or_else(|| panic!("no {part} in {path}"))
			}
		};
	}
	*cur = new;
}

fn remove(v: &mut Value, path: &str) {
	let (parent, key) = path.rsplit_once('.').map_or(("", path), |(p, k)| (p, k));
	let mut cur = &mut *v;
	if !parent.is_empty() {
		for part in parent.split('.') {
			cur = cur.get_mut(part).unwrap();
		}
	}
	cur.as_mapping_mut().unwrap().remove(key);
}

/// A valid production config: https, a real rail, file secrets, strict names.
fn production() -> Value {
	let mut v = doc(&sandbox_yaml());
	set(&mut v, "mode", "production".into());
	set(&mut v, "aggregator.url", "https://api.recoilpay.com".into());
	set(
		&mut v,
		"aggregator.api_key",
		"file:///etc/merchant-agent/api-key".into(),
	);
	set(
		&mut v,
		"payee.private_key",
		"gcp-sm://projects/p/secrets/payee/versions/1".into(),
	);
	set(
		&mut v,
		"payee.tag_pepper",
		"aws-sm://merchant-agent/pepper".into(),
	);
	set(&mut v, "rail.kind", "paystack".into());
	set(
		&mut v,
		"rail.credentials",
		"file:///etc/merchant-agent/paystack".into(),
	);
	set(&mut v, "policy.name_match", "strict".into());
	v
}

fn parse(v: &Value) -> Result<Config, ConfigError> {
	Config::parse(&serde_yaml_ng::to_string(v).unwrap())
}

/// Parse `v`, expect a rejection, and return all its messages joined.
fn rejected(v: &Value) -> String {
	match parse(v) {
		Ok(_) => panic!("the config should have been rejected"),
		Err(e) => e.problems().join("\n"),
	}
}

#[test]
fn the_generated_sandbox_config_is_valid() {
	let c = Config::parse(&sandbox_yaml()).unwrap();
	assert_eq!(c.mode, Mode::Sandbox);
	assert_eq!(c.rail.kind, RailKind::Simulated);
	assert_eq!(c.limits.per_trade.get(), 5_000_000);
	assert_eq!(c.policy.name_match, NameMatch::TokenSet);
	assert!(c.server.console_bind.ip().is_loopback());
}

#[test]
fn a_well_formed_production_config_is_valid() {
	let c = parse(&production()).unwrap();
	assert_eq!(c.mode, Mode::Production);
}

#[test]
fn unknown_fields_are_rejected() {
	let mut v = doc(&sandbox_yaml());
	set(&mut v, "limits.per_day_minr", "1".into());
	let err = rejected(&v);
	assert!(err.contains("per_day_minr"), "{err}");
}

#[test]
fn a_wrong_config_version_is_rejected() {
	let mut v = doc(&sandbox_yaml());
	set(&mut v, "config_version", 2.into());
	assert!(rejected(&v).contains("config_version"));
}

#[test]
fn production_rejects_the_simulated_bank() {
	let mut v = production();
	set(&mut v, "rail.kind", "simulated".into());
	assert!(rejected(&v).contains("simulated is for sandbox only"));
}

#[test]
fn production_rejects_name_match_off() {
	let mut v = production();
	set(&mut v, "policy.name_match", "off".into());
	assert!(rejected(&v).contains("name_match off is for sandbox only"));
}

#[test]
fn production_rejects_env_secrets() {
	let mut v = production();
	set(
		&mut v,
		"aggregator.api_key",
		"env://MERCHANT_AGENT_API_KEY".into(),
	);
	let err = rejected(&v);
	assert!(
		err.contains("aggregator.api_key") && err.contains("env://"),
		"{err}"
	);
}

#[test]
fn production_rejects_a_plain_http_aggregator() {
	let mut v = production();
	set(&mut v, "aggregator.url", "http://api.recoilpay.com".into());
	assert!(rejected(&v).contains("https:// in production"));
}

#[test]
fn production_rejects_a_plain_http_rpc() {
	let mut v = production();
	let mut rpc = serde_yaml_ng::Mapping::new();
	rpc.insert("url".into(), "http://sepolia.base.org".into());
	set(
		&mut v,
		"chains.0.rpcs",
		Value::Sequence(vec![Value::Mapping(rpc)]),
	);
	assert!(rejected(&v).contains("https:// in production"));
}

#[test]
fn missing_limits_are_rejected_in_every_mode() {
	for mut v in [doc(&sandbox_yaml()), production()] {
		remove(&mut v, "limits");
		assert!(rejected(&v).contains("no defaults for money"));
	}
}

#[test]
fn a_missing_limit_is_named() {
	let mut v = doc(&sandbox_yaml());
	remove(&mut v, "limits.per_payee_per_day_minor");
	remove(&mut v, "limits.min_float_minor");
	let err = rejected(&v);
	assert!(err.contains("limits.per_payee_per_day_minor"), "{err}");
	assert!(err.contains("limits.min_float_minor"), "{err}");
}

#[test]
fn inconsistent_limits_are_rejected() {
	let mut v = doc(&sandbox_yaml());
	set(&mut v, "limits.per_trade_minor", "60000000".into());
	assert!(rejected(&v).contains("per_trade_minor cannot be more than"));

	let mut v = doc(&sandbox_yaml());
	set(&mut v, "limits.per_day_minor", "0".into());
	assert!(rejected(&v).contains("per_day_minor must be more than 0"));
}

#[test]
fn limits_must_be_naira() {
	let mut v = doc(&sandbox_yaml());
	set(&mut v, "limits.currency", "KES".into());
	assert!(rejected(&v).contains("must be NGN"));
}

#[test]
fn money_cannot_be_a_float() {
	let mut v = doc(&sandbox_yaml());
	set(&mut v, "limits.per_trade_minor", Value::from(5000000.5));
	assert!(parse(&v).is_err());
}

#[test]
fn a_public_bind_needs_an_explicit_setting() {
	let mut v = doc(&sandbox_yaml());
	set(&mut v, "server.console_bind", "0.0.0.0:8740".into());
	assert!(rejected(&v).contains("reachable from other machines"));
	set(&mut v, "server.bind_public", true.into());
	assert!(parse(&v).is_ok());
}

#[test]
fn paystack_needs_credentials() {
	let mut v = production();
	remove(&mut v, "rail.credentials");
	assert!(rejected(&v).contains("Paystack secret key"));
}

#[test]
fn a_secret_pasted_into_the_config_is_rejected_without_echoing_it() {
	let mut v = doc(&sandbox_yaml());
	set(
		&mut v,
		"aggregator.api_key",
		"sk_live_abcdef0123456789".into(),
	);
	let err = rejected(&v);
	assert!(err.contains("never the secret itself"), "{err}");
	assert!(!err.contains("abcdef0123456789"), "{err}");
}

#[test]
fn a_bad_wallet_or_escrow_is_rejected() {
	let mut v = doc(&sandbox_yaml());
	set(&mut v, "operator.wallet", "0x1234".into());
	set(&mut v, "chains.0.escrow", "not-an-address".into());
	let err = rejected(&v);
	assert!(
		err.contains("operator.wallet") && err.contains("escrow must be"),
		"{err}"
	);
}

#[test]
fn only_evm_chains_are_accepted() {
	let mut v = doc(&sandbox_yaml());
	set(
		&mut v,
		"chains.0.caip2",
		"solana:EtWTRABZaYq6iMfeYKouRu166VU2xqa1".into(),
	);
	assert!(rejected(&v).contains("only EVM chains"));
}

#[test]
fn two_person_needs_a_threshold() {
	let mut v = doc(&sandbox_yaml());
	set(&mut v, "policy.two_person.enabled", true.into());
	assert!(rejected(&v).contains("approve_over_minor is required"));
}

#[test]
fn every_problem_is_reported_at_once() {
	let mut v = production();
	set(&mut v, "rail.kind", "simulated".into());
	set(&mut v, "policy.name_match", "off".into());
	set(&mut v, "aggregator.url", "http://x".into());
	let Err(ConfigError::Invalid(problems)) = parse(&v) else {
		panic!("expected validation problems");
	};
	assert!(problems.len() >= 3, "{problems:?}");
}

#[test]
fn the_shipped_example_config_is_valid() {
	let text = include_str!("../../../config.example.yaml");
	let c = Config::parse(text).unwrap();
	assert_eq!(c.mode, Mode::Sandbox);
}

#[test]
fn a_template_rail_needs_a_url_and_a_token_and_https_in_production() {
	let mut v = production();
	set(&mut v, "rail.kind", "template".into());
	remove(&mut v, "rail.credentials");
	let e = rejected(&v);
	assert!(
		e.contains("rail.credentials must reference your adapter's bearer token"),
		"{e}"
	);
	assert!(e.contains("rail.url must be your adapter's address"), "{e}");

	set(
		&mut v,
		"rail.credentials",
		"file:///etc/merchant-agent/adapter-token".into(),
	);
	set(&mut v, "rail.url", "http://payouts.internal:8750".into());
	assert!(rejected(&v).contains("must be https://"));

	set(&mut v, "rail.url", "https://payouts.internal:8443".into());
	let c = parse(&v).unwrap();
	assert_eq!(c.rail.kind, RailKind::Template);

	// Sandbox may use http, for an adapter on the same machine.
	let mut s = doc(&sandbox_yaml());
	set(&mut s, "rail.kind", "template".into());
	set(&mut s, "rail.credentials", "env://ADAPTER_TOKEN".into());
	set(&mut s, "rail.url", "http://127.0.0.1:8750".into());
	assert!(parse(&s).is_ok());

	// A url on another rail is a mistake, said out loud.
	let mut p = production();
	set(&mut p, "rail.url", "https://payouts.internal".into());
	assert!(rejected(&p).contains("rail.url is only for rail.kind template"));
}
