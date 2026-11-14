//! `merchant-agent`: the command line.
//!
//! All logic lives in `merchant-agent-core`; this file parses arguments,
//! talks to the person at the terminal, and sets the exit code.

use std::io::{self, BufRead, Write};
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use clap::{Parser, Subcommand};
use merchant_agent_core::aggregator::Client;
use merchant_agent_core::config::{Config, Mode};
use merchant_agent_core::console::auth;
use merchant_agent_core::engine::Platform;
use merchant_agent_core::machine::AgentState;
use merchant_agent_core::money;
use merchant_agent_core::policy::screening;
use merchant_agent_core::secrets::Secrets;
use merchant_agent_core::store::Store;
use merchant_agent_core::{doctor, init, logging, runner, CONFIG_SCHEMA_VERSION};

#[derive(Parser)]
#[command(
	name = "merchant-agent",
	about = "Pays RecoilPay naira cash-outs through your own bank rails."
)]
struct Cli {
	/// The config file.
	#[arg(
		long,
		short,
		global = true,
		env = "MERCHANT_AGENT_CONFIG",
		default_value = "merchant-agent.yaml"
	)]
	config: PathBuf,
	#[command(subcommand)]
	command: Command,
}

#[derive(Subcommand)]
enum Command {
	/// Write a commented sandbox config, asking a few questions.
	Init {
		/// Accept the defaults and take the answers from flags, without asking.
		#[arg(long)]
		yes: bool,
		/// Overwrite an existing config file.
		#[arg(long)]
		force: bool,
		#[arg(long)]
		solver_id: Option<String>,
		#[arg(long)]
		wallet: Option<String>,
		#[arg(long, default_value = "http://127.0.0.1:4000")]
		aggregator_url: String,
		#[arg(long, default_value = "./merchant-agent-data")]
		data_dir: PathBuf,
		/// Listen on every interface (for a container) instead of 127.0.0.1.
		#[arg(long)]
		bind_all: bool,
		/// For Kubernetes: health on every interface, console on 127.0.0.1
		/// only (reach it with kubectl port-forward).
		#[arg(long, conflicts_with = "bind_all")]
		public_health: bool,
		/// Read the secrets from files in this directory (api-key,
		/// payee-key, payee-tag-pepper) instead of environment variables.
		#[arg(long)]
		secrets_dir: Option<PathBuf>,
		/// Print the config instead of writing the file (for a container,
		/// whose config directory it can't write: redirect it on the host).
		#[arg(long, requires = "yes")]
		stdout: bool,
	},
	/// Exit 0 if the running agent's /healthz answers. For a container's
	/// HEALTHCHECK, which has no shell or curl to call it with. Reads only
	/// `server.health_bind` from the config, so it needs no secrets.
	Healthcheck {
		/// Ask this address instead of the config's (e.g. 127.0.0.1:8741).
		#[arg(long)]
		addr: Option<std::net::SocketAddr>,
	},
	/// Run the conformance suite against your payout adapter (rail.kind
	/// template). It sends a few ₦1 test payouts, so point a SANDBOX config
	/// at your adapter's test environment. See docs/adapters.md.
	AdapterTest {
		/// The test account's bank code.
		#[arg(long, default_value = "058")]
		bank_code: String,
		/// An account your adapter's test environment knows and pays.
		#[arg(long, default_value = "0000000001")]
		account: String,
		/// An account it says does not exist.
		#[arg(long, default_value = "0000000009")]
		missing_account: String,
		/// Each test payout, in kobo.
		#[arg(long, default_value_t = 100)]
		amount_kobo: u64,
		/// How long a payout may stay pending, in seconds.
		#[arg(long, default_value_t = 60)]
		settle_secs: u64,
	},
	/// Check everything and print what passed, failed or was skipped.
	Doctor,
	/// Run the agent.
	Run,
	/// Show the agent's state and the exception queue.
	Status,
	/// Stop new payouts now and withdraw your RecoilPay offers. Works whether
	/// or not the agent is running. Payouts already with the bank carry on.
	Pause {
		/// Who is pausing, for the record.
		#[arg(long, default_value = "cli")]
		by: String,
	},
	/// Resume after a pause you made by hand. An automatic pause needs an
	/// approver: resume it from the console.
	Resume {
		#[arg(long, default_value = "cli")]
		by: String,
	},
	/// Console users.
	Users {
		#[command(subcommand)]
		command: UsersCommand,
	},
	/// Alert channels.
	Alerts {
		#[command(subcommand)]
		command: AlertsCommand,
	},
	/// Work with the tamper-evident audit log.
	Audit {
		#[command(subcommand)]
		command: AuditCommand,
	},
	/// Screening tools.
	Screening {
		#[command(subcommand)]
		command: ScreeningCommand,
	},
	/// Print the version, git commit, and schema versions.
	Version,
}

#[derive(Subcommand)]
enum UsersCommand {
	/// Add a console user. Sets a password and shows an authenticator-app
	/// secret once.
	Add {
		username: String,
		/// viewer, operator, approver or admin.
		#[arg(long)]
		role: String,
		/// Read the password from the first line of stdin instead of asking.
		#[arg(long)]
		password_stdin: bool,
		/// Skip confirming a code from the authenticator app. Sandbox only.
		#[arg(long)]
		no_confirm: bool,
	},
	List,
	Disable {
		username: String,
	},
	Enable {
		username: String,
	},
	Remove {
		username: String,
	},
}

#[derive(Subcommand)]
enum AlertsCommand {
	/// Send a test alert to every configured channel.
	Test,
}

#[derive(Subcommand)]
enum ScreeningCommand {
	/// Turn a list of accounts (`bank_code,account` or
	/// `rail,bank_code,account` per line, on stdin) into payee tags for
	/// `screening.deny_list`. Uses this agent's pepper, so the tags match
	/// only this agent's payees and reveal no account numbers.
	Hash,
}

#[derive(Subcommand)]
enum AuditCommand {
	/// Check that no audit entry was edited or deleted: the database's
	/// chain, `audit.jsonl`'s chain, and that they match. With `--export`,
	/// check an exported file instead.
	Verify {
		#[arg(long)]
		export: Option<PathBuf>,
	},
	/// Write the entries between two times (RFC 3339, e.g.
	/// 2026-11-01T00:00:00Z) as JSON lines, for compliance. Each line keeps
	/// its hash and the previous one's, so the export can be verified alone.
	Export {
		#[arg(long)]
		from: String,
		#[arg(long)]
		to: String,
		/// Write here instead of stdout.
		#[arg(long)]
		out: Option<PathBuf>,
	},
}

fn main() -> ExitCode {
	logging::install_panic_hook();
	let cli = Cli::parse();
	match cli.command {
		Command::Init {
			yes,
			force,
			solver_id,
			wallet,
			aggregator_url,
			data_dir,
			bind_all,
			public_health,
			secrets_dir,
			stdout,
		} => cmd_init(
			&cli.config,
			InitFlags {
				yes,
				force,
				solver_id,
				wallet,
				aggregator_url,
				data_dir,
				bind_all,
				public_health,
				secrets_dir,
				stdout,
			},
		),
		Command::Doctor => cmd_doctor(&cli.config),
		Command::Run => cmd_run(&cli.config),
		Command::Screening {
			command: ScreeningCommand::Hash,
		} => cmd_screening_hash(&cli.config),
		Command::Status => cmd_status(&cli.config),
		Command::Pause { by } => cmd_pause(&cli.config, &by),
		Command::Resume { by } => cmd_resume(&cli.config, &by),
		Command::Users { command } => cmd_users(&cli.config, command),
		Command::Alerts {
			command: AlertsCommand::Test,
		} => cmd_alerts_test(&cli.config),
		Command::Audit { command } => cmd_audit(&cli.config, command),
		Command::Healthcheck { addr } => cmd_healthcheck(&cli.config, addr),
		Command::AdapterTest {
			bank_code,
			account,
			missing_account,
			amount_kobo,
			settle_secs,
		} => cmd_adapter_test(
			&cli.config,
			merchant_agent_core::rail::conformance::Accounts {
				bank_code,
				account,
				missing_account,
				name: "Test Payee One".into(),
				amount: money::Minor(u128::from(amount_kobo)),
				settle_within: std::time::Duration::from_secs(settle_secs),
			},
		),
		Command::Version => {
			println!("merchant-agent {}", env!("CARGO_PKG_VERSION"));
			println!("git commit      {}", env!("MERCHANT_AGENT_GIT_COMMIT"));
			println!(
				"database schema {}",
				merchant_agent_core::store::SCHEMA_VERSION
			);
			println!("config schema   {CONFIG_SCHEMA_VERSION}");
			ExitCode::SUCCESS
		}
	}
}

struct InitFlags {
	yes: bool,
	force: bool,
	solver_id: Option<String>,
	wallet: Option<String>,
	aggregator_url: String,
	data_dir: PathBuf,
	bind_all: bool,
	public_health: bool,
	secrets_dir: Option<PathBuf>,
	stdout: bool,
}

fn cmd_init(path: &Path, flags: InitFlags) -> ExitCode {
	let InitFlags {
		yes,
		force,
		solver_id,
		wallet,
		aggregator_url,
		data_dir,
		bind_all,
		public_health,
		secrets_dir,
		stdout,
	} = flags;
	if !stdout && path.exists() && !force {
		println!(
			"{} already exists. Pass --force to overwrite it.",
			path.display()
		);
		return ExitCode::FAILURE;
	}

	let answers = if yes {
		init::Answers {
			solver_id: solver_id.unwrap_or_default(),
			wallet: wallet.unwrap_or_default(),
			aggregator_url,
			data_dir,
			bind_all,
			public_health,
			secrets_dir: secrets_dir.clone(),
		}
	} else {
		println!(
			"Setting up a SANDBOX agent: Base Sepolia testnet, the simulated bank, low limits."
		);
		println!("Nothing here is secret. Secrets are set up afterwards.\n");
		let stdin = io::stdin();
		let mut lines = stdin.lock().lines();
		let mut ask = |q: &str, default: Option<&str>| -> String {
			loop {
				match default {
					Some(d) if !d.is_empty() => print!("{q} [{d}]: "),
					_ => print!("{q}: "),
				}
				let _ = io::stdout().flush();
				let line = lines.next().and_then(Result::ok).unwrap_or_default();
				let line = line.trim().to_string();
				if !line.is_empty() {
					return line;
				}
				if let Some(d) = default.filter(|d| !d.is_empty()) {
					return d.to_string();
				}
			}
		};
		init::Answers {
			solver_id: ask(
				"Your RecoilPay operator id (solver-…)",
				solver_id.as_deref(),
			),
			wallet: ask("Your registered wallet (0x…)", wallet.as_deref()),
			aggregator_url: ask("RecoilPay aggregator URL", Some(&aggregator_url)),
			data_dir: PathBuf::from(ask(
				"Where the agent keeps its database",
				Some(&data_dir.display().to_string()),
			)),
			bind_all,
			public_health,
			secrets_dir: secrets_dir.clone(),
		}
	};

	if let Err(problems) = answers.check() {
		// On stderr with --stdout, so a redirect doesn't swallow it.
		let say = |s: String| {
			if stdout {
				let _ = writeln!(io::stderr(), "{s}");
			} else {
				println!("{s}");
			}
		};
		say("Not written:".into());
		for p in problems {
			say(format!("  - {p}"));
		}
		return ExitCode::FAILURE;
	}

	let text = init::render_sandbox_config(&answers);
	// Rendered configs always validate; refuse to write one that doesn't
	// rather than hand the person a broken file.
	if let Err(e) = Config::parse(&text) {
		println!("internal error: the generated config does not validate:\n{e}");
		return ExitCode::FAILURE;
	}
	if stdout {
		print!("{text}");
		return ExitCode::SUCCESS;
	}
	if let Err(e) = std::fs::write(path, text) {
		println!("could not write {}: {e}", path.display());
		return ExitCode::FAILURE;
	}
	print!("{}", init::next_steps(&path.display().to_string()));
	// Not fatal: a production data directory is often made by the deploy,
	// as root, and `doctor` checks it either way.
	if let Err(e) = merchant_agent_core::store::private_dir(&answers.data_dir) {
		println!(
			"\nNote: could not create the data directory {} ({}). Create it, owned by the \
			 agent's user, before running the agent.",
			answers.data_dir.display(),
			e.kind()
		);
	}
	ExitCode::SUCCESS
}

fn runtime() -> tokio::runtime::Runtime {
	tokio::runtime::Builder::new_multi_thread()
		.enable_all()
		.build()
		.expect("the async runtime starts")
}

fn cmd_doctor(path: &Path) -> ExitCode {
	let report = runtime().block_on(doctor::run(path));
	print!("{report}");
	if report.passed() {
		ExitCode::SUCCESS
	} else {
		ExitCode::FAILURE
	}
}

fn load(path: &Path) -> Result<Config, String> {
	std::fs::read_to_string(path)
		.map_err(|e| format!("cannot read {}: {e}", path.display()))
		.and_then(|t| Config::parse(&t).map_err(|e| e.to_string()))
}

fn cmd_run(path: &Path) -> ExitCode {
	let config = match load(path) {
		Ok(c) => c,
		Err(e) => {
			println!("{e}");
			return ExitCode::FAILURE;
		}
	};
	let rt = runtime();
	// Listen for Ctrl-C and SIGTERM from the very start: a signal before the
	// first tick then stops the agent cleanly instead of killing it.
	let stop = {
		let _guard = rt.enter();
		shutdown()
	};
	// Production starts only on a clean bill of health.
	if config.mode == Mode::Production {
		let report = rt.block_on(doctor::run(path));
		if !report.passed() {
			print!("{report}");
			println!("Production mode needs a passing `merchant-agent doctor`. Not starting.");
			return ExitCode::FAILURE;
		}
	}
	if let Err(e) = logging::init(&config.logging.level, config.logging.format) {
		println!("cannot start logging: {e}");
		return ExitCode::FAILURE;
	}
	tracing::info!(mode = %config.mode, "config loaded");
	let digest = std::fs::read_to_string(path)
		.map(|t| runner::config_digest(&t))
		.unwrap_or_default();
	match rt.block_on(runner::run(config, &digest, stop)) {
		Ok(()) => ExitCode::SUCCESS,
		Err(e) => {
			tracing::error!("{e}");
			ExitCode::FAILURE
		}
	}
}

/// Ctrl-C, or SIGTERM from a container runtime or systemd.
/// Ctrl-C or SIGTERM. The handlers are installed when this is called, not
/// when the future is first polled, so call it inside the runtime, early.
fn shutdown() -> impl std::future::Future<Output = ()> {
	use tokio::signal::unix::{signal, SignalKind};
	let mut int = signal(SignalKind::interrupt()).expect("the SIGINT handler installs");
	let mut term = signal(SignalKind::terminate()).expect("the SIGTERM handler installs");
	async move {
		tokio::select! {
			_ = int.recv() => {}
			_ = term.recv() => {}
		}
	}
}

fn cmd_screening_hash(path: &Path) -> ExitCode {
	let config = match load(path) {
		Ok(c) => c,
		Err(e) => {
			println!("{e}");
			return ExitCode::FAILURE;
		}
	};
	let pepper =
		match runtime().block_on(Secrets::new(config.mode).resolve(&config.payee.tag_pepper)) {
			Ok(p) => p,
			Err(e) => {
				println!("the payee tag pepper: {e}");
				return ExitCode::FAILURE;
			}
		};
	let mut input = String::new();
	if let Err(e) = io::Read::read_to_string(&mut io::stdin(), &mut input) {
		println!("cannot read stdin: {e}");
		return ExitCode::FAILURE;
	}
	match screening::hash_list(&pepper, &input) {
		Ok(tags) => {
			println!("# merchant-agent deny-list: payee tags, one per line");
			for t in tags {
				println!("{t}");
			}
			ExitCode::SUCCESS
		}
		Err(e) => {
			println!("{e}");
			ExitCode::FAILURE
		}
	}
}

fn fail(msg: impl std::fmt::Display) -> ExitCode {
	println!("{msg}");
	ExitCode::FAILURE
}

fn control_store(config: &Config) -> Result<Store, String> {
	Store::open_control(&config.storage.data_dir).map_err(|e| e.to_string())
}

async fn client_for(config: &Config) -> Result<Client, String> {
	let key = Secrets::new(config.mode)
		.resolve(&config.aggregator.api_key)
		.await
		.map_err(|e| e.to_string())?;
	Client::from_config(config, key).map_err(|e| e.to_string())
}

/// The kill switch (invariant 11): stop new payouts locally first, then
/// withdraw the offers. Never touches a payout in flight.
fn cmd_pause(path: &Path, by: &str) -> ExitCode {
	let config = match load(path) {
		Ok(c) => c,
		Err(e) => return fail(e),
	};
	let store = match control_store(&config) {
		Ok(s) => s,
		Err(e) => return fail(e),
	};
	if let Err(e) = store.set_paused_control(true, by, false, None) {
		return fail(format!("could not pause: {e}"));
	}
	println!("Paused: no new payouts will start. Payouts already with the bank carry on.");
	let withdrawn = runtime().block_on(async {
		let client = client_for(&config).await?;
		client.pause_offers().await.map_err(|e| e.to_string())
	});
	match withdrawn {
		Ok(book) => {
			let _ = store.set_paused_control(true, by, false, Some(&book));
			println!("Your offers on RecoilPay are withdrawn, so no new trades arrive.");
			ExitCode::SUCCESS
		}
		Err(e) => {
			println!("Could not withdraw your offers on RecoilPay ({e}). New trades may still arrive, but the agent won't pay them while paused. Withdraw them in the portal.");
			ExitCode::FAILURE
		}
	}
}

fn cmd_resume(path: &Path, by: &str) -> ExitCode {
	let config = match load(path) {
		Ok(c) => c,
		Err(e) => return fail(e),
	};
	let store = match control_store(&config) {
		Ok(s) => s,
		Err(e) => return fail(e),
	};
	let (paused, paused_by, auto, book) = match store.control() {
		Ok(c) => c,
		Err(e) => return fail(e),
	};
	if !paused {
		println!("Payouts are not paused.");
		return ExitCode::SUCCESS;
	}
	if auto {
		return fail(format!(
			"The agent paused itself ({}). Resuming that needs an approver: use Resume in the console.",
			paused_by.as_deref().unwrap_or("automatically")
		));
	}
	let republished = runtime().block_on(async {
		match book.as_deref() {
			Some(b) if b != "[]" => client_for(&config)
				.await?
				.resume_offers(b)
				.await
				.map_err(|e| e.to_string()),
			_ => Ok(0),
		}
	});
	match republished {
		Ok(n) => {
			if let Err(e) = store.set_paused_control(false, by, false, Some("[]")) {
				return fail(format!("could not resume: {e}"));
			}
			println!("Resumed. {n} offer(s) republished.");
			if n == 0 {
				println!("Publish a new quote book in the portal so customers can find you.");
			}
			ExitCode::SUCCESS
		}
		Err(e) => fail(format!(
			"Could not republish your offers ({e}); still paused."
		)),
	}
}

fn cmd_status(path: &Path) -> ExitCode {
	let config = match load(path) {
		Ok(c) => c,
		Err(e) => return fail(e),
	};
	let store = match control_store(&config) {
		Ok(s) => s,
		Err(e) => return fail(e),
	};
	let (paused, by, auto, _) = store.control().unwrap_or_default();
	match store.lease() {
		Ok(Some((holder, _, until))) if until > chrono::Utc::now() => {
			println!("Agent: running ({holder})")
		}
		_ => println!("Agent: not running"),
	}
	if paused {
		println!(
			"Payouts: PAUSED by {}{}",
			by.as_deref().unwrap_or("unknown"),
			if auto {
				" (automatically; resuming needs an approver)"
			} else {
				""
			}
		);
	} else {
		println!("Payouts: running");
	}
	let trades = store.recent_trades(500).unwrap_or_default();
	let tasks = store.open_manual_tasks().unwrap_or_default();
	let queue: Vec<_> = trades
		.iter()
		.filter(|t| {
			matches!(t.state, AgentState::Held(_) | AgentState::NeedsPerson(_))
				|| tasks.iter().any(|k| k.trade_id == t.trade_id)
		})
		.collect();
	println!(
		"\nException queue: {}",
		if queue.is_empty() {
			"empty".to_string()
		} else {
			queue.len().to_string()
		}
	);
	for t in queue {
		let why = match &t.state {
			AgentState::Held(r) => r.message(),
			AgentState::NeedsPerson(r) => r.message(),
			_ => "Waiting for a person to pay this from the bank app.",
		};
		println!(
			"  {}  {}  {} to {}\n    {}",
			t.trade_id,
			t.state.label(),
			money::naira(t.fiat_minor),
			t.account_masked.as_deref().unwrap_or("(not yet known)"),
			why
		);
	}
	let active = trades.iter().filter(|t| !t.state.is_terminal()).count();
	println!("\nActive trades: {active}. Open the console for actions.");
	ExitCode::SUCCESS
}

fn cmd_users(path: &Path, command: UsersCommand) -> ExitCode {
	let config = match load(path) {
		Ok(c) => c,
		Err(e) => return fail(e),
	};
	let store = match control_store(&config) {
		Ok(s) => s,
		Err(e) => return fail(e),
	};
	match command {
		UsersCommand::List => {
			for u in store.users().unwrap_or_default() {
				let status = if u.disabled { "disabled" } else { "active" };
				println!("{:<24} {:<9} {status}", u.username, u.role);
			}
			ExitCode::SUCCESS
		}
		UsersCommand::Disable { username } | UsersCommand::Enable { username }
			if username.is_empty() =>
		{
			fail("a username is needed")
		}
		UsersCommand::Disable { username } => match store.set_user_disabled(&username, true, "cli")
		{
			Ok(true) => {
				println!("{username} disabled.");
				ExitCode::SUCCESS
			}
			Ok(false) => fail("no such user"),
			Err(e) => fail(e),
		},
		UsersCommand::Enable { username } => match store.set_user_disabled(&username, false, "cli")
		{
			Ok(true) => {
				println!("{username} enabled.");
				ExitCode::SUCCESS
			}
			Ok(false) => fail("no such user"),
			Err(e) => fail(e),
		},
		UsersCommand::Remove { username } => match store.remove_user(&username, "cli") {
			Ok(true) => {
				println!("{username} removed.");
				ExitCode::SUCCESS
			}
			Ok(false) => fail("no such user"),
			Err(e) => fail(e),
		},
		UsersCommand::Add {
			username,
			role,
			password_stdin,
			no_confirm,
		} => {
			let Some(role) = auth::Role::parse(&role) else {
				return fail("--role is viewer, operator, approver or admin");
			};
			if no_confirm && config.mode == Mode::Production {
				return fail("--no-confirm is for sandbox only");
			}
			let pepper = match runtime()
				.block_on(Secrets::new(config.mode).resolve(&config.payee.tag_pepper))
			{
				Ok(p) => p,
				Err(e) => return fail(format!("the payee tag pepper: {e}")),
			};
			let key = auth::TotpKey::from_pepper(&pepper);
			let password = if password_stdin {
				let mut line = String::new();
				if io::stdin().lock().read_line(&mut line).is_err() {
					return fail("could not read the password");
				}
				line.trim_end_matches(['\n', '\r']).to_string()
			} else {
				let first =
					rpassword::prompt_password("Password (12+ characters): ").unwrap_or_default();
				let again = rpassword::prompt_password("Again: ").unwrap_or_default();
				if first != again {
					return fail("the passwords differ");
				}
				first
			};
			let now = chrono::Utc::now();
			let enrolment = match auth::add_user(&store, &key, &username, role, &password, now) {
				Ok(e) => e,
				Err(e) => return fail(e),
			};
			println!("Added {username} as {role}.");
			println!("Add this to their authenticator app (shown once):");
			println!("  {}", enrolment.uri.as_str());
			println!("  or type the secret: {}", enrolment.secret_base32.as_str());
			if no_confirm {
				return ExitCode::SUCCESS;
			}
			for _ in 0..3 {
				print!("Enter the 6-digit code the app shows now: ");
				let _ = io::stdout().flush();
				let mut code = String::new();
				let _ = io::stdin().lock().read_line(&mut code);
				if auth::login(
					&store,
					&key,
					&username,
					&password,
					code.trim(),
					chrono::Utc::now(),
				)
				.is_ok()
				{
					println!("Confirmed. {username} can sign in to the console.");
					return ExitCode::SUCCESS;
				}
				println!("That code didn't match.");
			}
			let _ = store.remove_user(&username, "cli");
			fail(format!(
				"Not confirmed, so {username} was not added. Try again."
			))
		}
	}
}

fn cmd_alerts_test(path: &Path) -> ExitCode {
	let config = match load(path) {
		Ok(c) => c,
		Err(e) => return fail(e),
	};
	let rt = runtime();
	let alerter = match rt.block_on(runner::alerter(&config, &Secrets::new(config.mode))) {
		Ok(a) => a,
		Err(e) => return fail(e),
	};
	if alerter.channels().is_empty() {
		return fail("No alert channels are configured; alerts are only logged. Add an alerts section to the config.");
	}
	let test = merchant_agent_core::engine::Alert {
		trade_id: "ft-test".into(),
		state: AgentState::Held(merchant_agent_core::machine::HoldReason::Paused),
		urgent: false,
		account_masked: Some("******0000".into()),
		amount: money::Minor(100),
		detail: Some("This is a test alert from merchant-agent alerts test.".into()),
	};
	rt.block_on(alerter.deliver(&[test], chrono::Utc::now()));
	println!(
		"Sent a test alert to: {}. Check that it arrived.",
		alerter.channels().join(", ")
	);
	ExitCode::SUCCESS
}

fn cmd_audit(path: &Path, command: AuditCommand) -> ExitCode {
	if let AuditCommand::Verify { export: Some(file) } = &command {
		let text = match std::fs::read_to_string(file) {
			Ok(t) => t,
			Err(e) => return fail(format!("cannot read {}: {e}", file.display())),
		};
		let lines: Vec<String> = text
			.lines()
			.filter(|l| !l.trim().is_empty())
			.map(String::from)
			.collect();
		let problems = merchant_agent_core::audit::verify_export(&lines);
		if problems.is_empty() {
			println!(
				"OK: {} exported entries, every hash and link intact.",
				lines.len()
			);
			return ExitCode::SUCCESS;
		}
		for p in &problems {
			println!("PROBLEM: {p}");
		}
		return fail(format!(
			"{} problem(s): the export was changed after it was written.",
			problems.len()
		));
	}
	let config = match load(path) {
		Ok(c) => c,
		Err(e) => return fail(e),
	};
	let store = match control_store(&config) {
		Ok(s) => s,
		Err(e) => return fail(e),
	};
	match command {
		AuditCommand::Verify { .. } => match store.verify_audit() {
			Ok(v) if v.ok() => {
				println!(
					"OK: {} entries, chained and matching audit.jsonl ({} lines).",
					v.entries, v.in_file
				);
				ExitCode::SUCCESS
			}
			Ok(v) => {
				for p in &v.problems {
					println!("PROBLEM: {p}");
				}
				fail(format!(
					"{} problem(s). The audit log was changed after it was written: treat this as a security incident.",
					v.problems.len()
				))
			}
			Err(e) => fail(e),
		},
		AuditCommand::Export { from, to, out } => {
			let parse = |s: &str| {
				s.parse::<chrono::DateTime<chrono::Utc>>()
					.map_err(|_| format!("{s} is not an RFC 3339 time"))
			};
			let (from, to) = match (parse(&from), parse(&to)) {
				(Ok(f), Ok(t)) => (f, t),
				(Err(e), _) | (_, Err(e)) => return fail(e),
			};
			let lines = match store.export_audit(from, to) {
				Ok(l) => l,
				Err(e) => return fail(e),
			};
			let text = lines.iter().map(|l| format!("{l}\n")).collect::<String>();
			match out {
				Some(file) => match std::fs::write(&file, text) {
					Ok(()) => {
						println!("Wrote {} entries to {}.", lines.len(), file.display());
						ExitCode::SUCCESS
					}
					Err(e) => fail(format!("cannot write {}: {e}", file.display())),
				},
				None => {
					print!("{text}");
					ExitCode::SUCCESS
				}
			}
		}
	}
}

fn cmd_healthcheck(path: &Path, addr: Option<std::net::SocketAddr>) -> ExitCode {
	use std::io::Read;
	let addr = match addr {
		Some(a) => a,
		None => match load(path) {
			Ok(c) => c.server.health_bind,
			Err(e) => return fail(e),
		},
	};
	// The agent binds 0.0.0.0 in a container; ask it on loopback.
	let target = if addr.ip().is_unspecified() {
		std::net::SocketAddr::new(std::net::Ipv4Addr::LOCALHOST.into(), addr.port())
	} else {
		addr
	};
	let timeout = std::time::Duration::from_secs(3);
	let answer = (|| -> io::Result<String> {
		let mut s = std::net::TcpStream::connect_timeout(&target, timeout)?;
		s.set_read_timeout(Some(timeout))?;
		s.set_write_timeout(Some(timeout))?;
		write!(
			s,
			"GET /healthz HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n"
		)?;
		let mut out = String::new();
		s.take(4096).read_to_string(&mut out)?;
		Ok(out)
	})();
	match answer {
		Ok(r) if r.starts_with("HTTP/1.1 200") => {
			println!("healthy ({target})");
			ExitCode::SUCCESS
		}
		Ok(r) => fail(format!(
			"unhealthy: {target} answered {}",
			r.lines().next().unwrap_or("nothing")
		)),
		Err(e) => fail(format!("unhealthy: {target}: {e}")),
	}
}

fn cmd_adapter_test(
	path: &Path,
	accounts: merchant_agent_core::rail::conformance::Accounts,
) -> ExitCode {
	use merchant_agent_core::config::RailKind;
	use merchant_agent_core::rail::template::TemplateRail;
	let config = match load(path) {
		Ok(c) => c,
		Err(e) => return fail(e),
	};
	if config.rail.kind != RailKind::Template {
		return fail("adapter-test is for rail.kind template: your own payout adapter.");
	}
	if config.mode == Mode::Production {
		return fail(
			"adapter-test sends test payouts, so it refuses a production config. Point a sandbox config at your adapter's test environment.",
		);
	}
	let (Some(r), Some(url)) = (&config.rail.credentials, config.rail.url.clone()) else {
		return fail("rail.url and rail.credentials are needed");
	};
	let rt = runtime();
	let token = match rt.block_on(Secrets::new(config.mode).resolve(r)) {
		Ok(t) => t,
		Err(e) => return fail(format!("the adapter token: {e}")),
	};
	let rail = match rt.block_on(TemplateRail::connect(
		&url,
		token,
		std::time::Duration::from_secs(30),
	)) {
		Ok(r) => r,
		Err(e) => return fail(e),
	};
	println!(
		"Testing {} at {url} (protocol 1, says {}).\n",
		if rail.info().name.is_empty() {
			"the adapter"
		} else {
			&rail.info().name
		},
		if rail.info().idempotent_send {
			"idempotent"
		} else {
			"not idempotent"
		}
	);
	let report = rt.block_on(merchant_agent_core::rail::conformance::run(
		&rail, &accounts,
	));
	for c in &report.checks {
		use merchant_agent_core::doctor::Outcome;
		match &c.outcome {
			Outcome::Pass(n) => println!("PASS  {}  {n}", c.name),
			Outcome::Warn(n) => println!("WARN  {}  {n}", c.name),
			Outcome::Skip(n) => println!("SKIP  {}  {n}", c.name),
			Outcome::Fail { why, fix } => {
				println!("FAIL  {}  {why}", c.name);
				println!("        fix: {fix}");
			}
		}
	}
	if report.passed() {
		println!("\nNo failures: the agent can pay through this adapter.");
		ExitCode::SUCCESS
	} else {
		println!(
			"\n{} failure(s). The agent must not pay through this adapter until they pass.",
			report.failures()
		);
		ExitCode::FAILURE
	}
}
