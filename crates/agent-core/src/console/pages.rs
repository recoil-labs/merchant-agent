//! The console's pages. maud escapes everything it renders, so nothing a
//! trade or a person typed can become markup.
//!
//! The words follow the experience principles: states in plain English,
//! every exception says what happened, and the actions sit next to it.

use chrono::{DateTime, Utc};
use maud::{html, Markup, DOCTYPE};

use super::auth::{Role, Session};
use super::{Console, Snapshot};
use crate::engine::Revealed;
use crate::machine::{AgentState, PersonReason};
use crate::money::{naira, Minor};
use crate::store::{ManualTask, TradeRecord, UserRow};

pub const CSS: &str = r#"
:root { --bg:#0b0812; --panel:#16111f; --line:#2a2238; --text:#ece8f5; --muted:#a9a1bd; --accent:#7c4be4; --warn:#f2b84b; --bad:#ff7a7a; --good:#5fd39a; }
* { box-sizing:border-box; }
body { margin:0; font:15px/1.5 system-ui,-apple-system,"Segoe UI",sans-serif; background:var(--bg); color:var(--text); }
header { display:flex; gap:16px; align-items:center; padding:12px 20px; border-bottom:1px solid var(--line); flex-wrap:wrap; }
header b { margin-right:12px; }
a { color:#9d7bf0; }
header a { color:var(--muted); text-decoration:none; }
header a.on, header a:hover { color:var(--text); }
header form { margin-left:auto; }
main { max-width:1100px; margin:0 auto; padding:20px; }
h1 { font-size:22px; margin:0 0 16px; }
h2 { font-size:16px; margin:24px 0 8px; color:var(--muted); font-weight:600; }
.grid { display:grid; grid-template-columns:repeat(auto-fit,minmax(220px,1fr)); gap:12px; }
.card { background:var(--panel); border:1px solid var(--line); border-radius:10px; padding:14px 16px; }
.card .big { font-size:24px; font-weight:650; }
.muted { color:var(--muted); }
.flash { background:#1d1630; border:1px solid var(--accent); border-radius:8px; padding:10px 14px; margin-bottom:16px; }
.paused { border-color:var(--warn); }
.urgent { border-color:var(--bad); }
table { width:100%; border-collapse:collapse; }
th, td { text-align:left; padding:8px 6px; border-bottom:1px solid var(--line); vertical-align:top; }
th { color:var(--muted); font-weight:600; font-size:13px; }
code { font-size:13px; }
button { background:var(--accent); color:#fff; border:0; border-radius:7px; padding:7px 12px; font-weight:600; cursor:pointer; }
button.quiet { background:transparent; border:1px solid var(--line); color:var(--text); }
button.danger { background:#a33b3b; }
input { background:#0f0b17; color:var(--text); border:1px solid var(--line); border-radius:7px; padding:7px 10px; }
.actions { display:flex; gap:8px; flex-wrap:wrap; margin-top:10px; }
.actions form { display:flex; gap:6px; }
.deadline.late { color:var(--bad); }
.login { max-width:360px; margin:10vh auto; }
.login input { width:100%; margin:4px 0 12px; }
.reveal dd { font-size:20px; font-weight:650; margin:0 0 10px; }
"#;

pub const JS: &str = r#"
(function () {
  function tick() {
    document.querySelectorAll('[data-deadline]').forEach(function (el) {
      var left = Math.floor((Date.parse(el.dataset.deadline) - Date.now()) / 1000);
      if (left <= 0) { el.textContent = 'past the deadline'; el.classList.add('late'); return; }
      var m = Math.floor(left / 60), s = ('0' + (left % 60)).slice(-2);
      el.textContent = m + ':' + s + ' left';
      if (left < 180) el.classList.add('late');
    });
  }
  tick(); setInterval(tick, 1000);
  document.querySelectorAll('[data-hide-after]').forEach(function (el) {
    var secs = parseInt(el.dataset.hideAfter, 10);
    var counter = el.querySelector('[data-countdown]');
    var timer = setInterval(function () {
      secs -= 1;
      if (counter) counter.textContent = secs;
      if (secs <= 0) {
        clearInterval(timer);
        el.innerHTML = '<p>Hidden. If you still need the details, reveal them again from the exception queue. Each reveal is recorded.</p>';
      }
    }, 1000);
  });
  if (document.body.dataset.refresh) {
    setTimeout(function () { location.reload(); }, parseInt(document.body.dataset.refresh, 10) * 1000);
  }
})();
"#;

fn page(
	title: &str,
	session: Option<&Session>,
	current: &str,
	flash: Option<&str>,
	refresh: bool,
	body: Markup,
) -> Markup {
	html! {
		(DOCTYPE)
		html lang="en" {
			head {
				meta charset="utf-8";
				meta name="viewport" content="width=device-width, initial-scale=1";
				title { (title) " · RecoilPay Merchant Agent" }
				link rel="stylesheet" href="/static/console.css";
			}
			body data-refresh=[refresh.then_some("15")] {
				@if let Some(s) = session {
					header {
						b { "Merchant Agent" }
						@for (href, label) in [("/", "Dashboard"), ("/exceptions", "Exceptions"), ("/trades", "Trades"), ("/audit", "Decisions"), ("/settings", "Settings")] {
							a href=(href) class=[(current == href).then_some("on")] { (label) }
						}
						@if s.role >= Role::Admin {
							a href="/users" class=[(current == "/users").then_some("on")] { "Users" }
						}
						form method="post" action="/logout" {
							input type="hidden" name="csrf" value=(s.csrf);
							span.muted { (s.username) " (" (s.role) ") " }
							button.quiet type="submit" { "Sign out" }
						}
					}
				}
				main {
					@if let Some(f) = flash { div.flash { (f) } }
					(body)
				}
				script src="/static/console.js" {}
			}
		}
	}
}

pub fn message(title: &str, text: &str) -> Markup {
	page(
		title,
		None,
		"",
		None,
		false,
		html! { div.card.login { h1 { (title) } p { (text) } p { a href="/" { "Back" } } } },
	)
}

pub fn login(flash: Option<&str>) -> Markup {
	page(
		"Sign in",
		None,
		"",
		None,
		false,
		html! {
			div.login {
				h1 { "Sign in" }
				@if let Some(f) = flash { div.flash { (f) } }
				form.card method="post" action="/login" {
					label { "Username" input name="username" autocomplete="username" required; }
					label { "Password" input type="password" name="password" autocomplete="current-password" required; }
					label { "Code from your authenticator app" input name="code" inputmode="numeric" autocomplete="one-time-code" pattern="[0-9]{6}" required; }
					button type="submit" { "Sign in" }
				}
				p.muted { "No account? An admin adds people with " code { "merchant-agent users add" } "." }
			}
		},
	)
}

fn needs_attention(t: &TradeRecord, tasks: &[ManualTask]) -> bool {
	matches!(t.state, AgentState::Held(_) | AgentState::NeedsPerson(_))
		|| tasks.iter().any(|task| task.trade_id == t.trade_id)
}

fn deadline(t: &TradeRecord) -> Markup {
	let running = matches!(
		t.state,
		AgentState::Detected
			| AgentState::LockVerified
			| AgentState::ChecksPassed
			| AgentState::PayoutIntentRecorded
			| AgentState::Paying
			| AgentState::Held(_)
	);
	html! {
		@if running {
			span.deadline data-deadline=(t.deadline_at.to_rfc3339()) { (t.deadline_at.format("%H:%M:%S UTC")) }
		} @else { span.muted { "—" } }
	}
}

fn reason(t: &TradeRecord) -> Option<&'static str> {
	match &t.state {
		AgentState::Held(r) => Some(r.message()),
		AgentState::NeedsPerson(r) => Some(r.message()),
		_ => None,
	}
}

pub fn dashboard(
	c: &Console,
	s: &Session,
	snap: &Snapshot,
	flash: Option<&str>,
	now: DateTime<Utc>,
) -> Markup {
	let attention = snap
		.trades
		.iter()
		.filter(|t| needs_attention(t, &snap.open_tasks))
		.count();
	let active = snap
		.trades
		.iter()
		.filter(|t| !t.state.is_terminal() && t.state != AgentState::Expired)
		.count();
	let since = snap.last_tick.map(|t| (now - t).num_seconds());
	page(
		"Dashboard",
		Some(s),
		"/",
		flash,
		true,
		html! {
			h1 { "Dashboard" }
			div.grid {
				div.card.paused[snap.paused] {
					div.muted { "Payouts" }
					@if snap.paused {
						div.big { "Paused" }
						p.muted {
							"By " (snap.paused_by.as_deref().unwrap_or("unknown"))
							@if snap.paused_auto { ", automatically. Resuming needs an approver." }
						}
						form method="post" action="/resume" {
							input type="hidden" name="csrf" value=(s.csrf);
							button type="submit" { "Resume payouts" }
						}
					} @else {
						div.big { "Running" }
						form method="post" action="/pause" {
							input type="hidden" name="csrf" value=(s.csrf);
							button.danger type="submit" { "Pause now" }
						}
						p.muted { "Pausing stops new payouts and withdraws your offers. Payouts already with the bank carry on." }
					}
				}
				div.card.urgent[attention > 0] {
					div.muted { "Need you" }
					div.big { (attention) }
					a href="/exceptions" { "Open the exception queue" }
				}
				div.card {
					div.muted { "Payout account balance" }
					div.big { (snap.balance.map(naira).unwrap_or_else(|| "Not readable on this rail".into())) }
					@if let Some(l) = &c.limits { p.muted { "Never below " (naira(l.min_float)) } }
				}
				div.card {
					div.muted { "Paid today (Lagos time)" }
					div.big { (naira(snap.today)) }
					@if let Some(l) = &c.limits {
						p.muted { "of " (naira(l.per_day)) " daily limit" }
					}
				}
				div.card {
					div.muted { "Agent" }
					div.big { (active) " active" }
					p.muted {
						@match since {
							Some(secs) => { "Last pass " (secs) "s ago" },
							None => { "No pass yet" },
						}
						@if let Some(h) = &snap.lease_holder { br; "Lease: " code { (h) } }
					}
				}
			}
		},
	)
}

fn trade_table(trades: &[&TradeRecord]) -> Markup {
	html! {
		table {
			tr { th { "Trade" } th { "State" } th { "Naira" } th { "Account" } th { "Deadline" } }
			@for t in trades {
				tr {
					td { code { (t.trade_id) } }
					td { (t.state.label()) @if let Some(r) = reason(t) { br; span.muted { (r) } } }
					td { (naira(t.fiat_minor)) }
					td { code { (t.account_masked.as_deref().unwrap_or("—")) } }
					td { (deadline(t)) }
				}
			}
		}
	}
}

pub fn trades(_c: &Console, s: &Session, snap: &Snapshot, flash: Option<&str>) -> Markup {
	let all: Vec<&TradeRecord> = snap.trades.iter().collect();
	page(
		"Trades",
		Some(s),
		"/trades",
		flash,
		true,
		html! {
			h1 { "Trades" }
			@if all.is_empty() { p.muted { "No trades yet." } } @else { (trade_table(&all)) }
		},
	)
}

fn action(
	s: &Session,
	trade_id: &str,
	act: &str,
	label: &str,
	with_reference: bool,
	class: &str,
) -> Markup {
	html! {
		form method="post" action=(format!("/trades/{trade_id}/{act}")) {
			input type="hidden" name="csrf" value=(s.csrf);
			@if with_reference {
				input name="reference" placeholder="Bank reference" maxlength="64" required;
			}
			button class=(class) type="submit" { (label) }
		}
	}
}

pub fn exceptions(c: &Console, s: &Session, snap: &Snapshot, flash: Option<&str>) -> Markup {
	let items: Vec<&TradeRecord> = snap
		.trades
		.iter()
		.filter(|t| needs_attention(t, &snap.open_tasks))
		.collect();
	let operator = s.role >= Role::Operator;
	let approver = s.role >= Role::Approver;
	page(
		"Exceptions",
		Some(s),
		"/exceptions",
		flash,
		false,
		html! {
			h1 { "Exception queue" }
			@if items.is_empty() { p.muted { "Nothing needs you. Trades the agent can't settle on its own appear here." } }
			@for t in &items {
				@let task = snap.open_tasks.iter().find(|task| task.trade_id == t.trade_id);
				@let urgent = matches!(t.state, AgentState::NeedsPerson(_));
				div.card.urgent[urgent] style="" {
					div { b { (t.state.label()) } " · " code { (t.trade_id) } " · " (deadline(t)) }
					p {
						@if let Some(r) = reason(t) { (r) " " }
						@if task.is_some() { "Waiting for a person to pay this from the bank app." }
					}
					p.muted { (naira(t.fiat_minor)) " to " code { (t.account_masked.as_deref().unwrap_or("—")) } }
					div.actions {
						@if task.is_some() && operator {
							(action(s, &t.trade_id, "reveal", "Reveal bank details", false, ""))
							(action(s, &t.trade_id, "manual-paid", "I paid it", true, ""))
							(action(s, &t.trade_id, "manual-failed", "I couldn't pay it", false, "quiet"))
						}
						@match &t.state {
							AgentState::Held(r) => {
								@if r.approvable() && approver {
									@let two = c.two_person.enabled && c.two_person.approve_over.is_none_or(|o| t.fiat_minor > o);
									(action(s, &t.trade_id, "approve", if two { "Approve (needs a second approver)" } else { "Approve and pay" }, false, ""))
								}
								@if operator { (action(s, &t.trade_id, "mark-paid", "Paid outside the agent", true, "quiet")) }
							},
							AgentState::NeedsPerson(PersonReason::BankFailed | PersonReason::BankUnknown) => {
								@if approver { (action(s, &t.trade_id, "repay", "Pay again (new transfer)", false, "")) }
								@if operator {
									(action(s, &t.trade_id, "mark-paid", "It did arrive", true, "quiet"))
									(action(s, &t.trade_id, "nothing-sent", "Nothing was sent", false, "quiet"))
								}
							},
							AgentState::NeedsPerson(_) => {
								@if operator { (action(s, &t.trade_id, "mark-paid", "Paid outside the agent", true, "quiet")) }
							},
							_ => {},
						}
						@if !operator { span.muted { "Your role can only view." } }
					}
				}
			}
		},
	)
}

pub fn reveal(s: &Session, shown: &Revealed) -> Markup {
	page(
		"Bank details",
		Some(s),
		"/exceptions",
		None,
		false,
		html! {
			h1 { "Bank details for " code { (shown.trade_id) } }
			div.card.reveal data-hide-after="60" {
				p.muted { "This reveal is recorded. The details hide in " span data-countdown { "60" } " seconds." }
				dl {
					dt.muted { "Amount" } dd { (naira(shown.amount)) }
					dt.muted { "Bank code" } dd { (shown.opened.identity.bank_code) }
					dt.muted { "Account number" } dd { (shown.opened.identity.account.expose()) }
					dt.muted { "Account name" } dd { (shown.opened.name.expose()) }
				}
				p { "Pay exactly this amount from your bank app, then enter the bank's reference in the exception queue." }
			}
			p { a href="/exceptions" { "Back to the exception queue" } }
		},
	)
}

pub fn settings(c: &Console, s: &Session) -> Markup {
	page(
		"Settings",
		Some(s),
		"/settings",
		None,
		false,
		html! {
			h1 { "Settings" }
			p.muted { "Read-only. Change them in the config file and restart the agent." }
			table {
				@for (k, v) in &c.settings { tr { th { (k) } td { (v) } } }
				@if let Some(l) = &c.limits {
					tr { th { "Per trade" } td { (naira(l.per_trade)) } }
					tr { th { "Per day" } td { (naira(l.per_day)) } }
					tr { th { "Per payee per day" } td { (naira(l.per_payee_per_day)) } }
					tr { th { "Velocity" } td { (l.velocity_max_payouts) " payouts per " (l.velocity_window_secs) "s" } }
					tr { th { "Minimum float" } td { (naira(l.min_float)) } }
				}
				tr { th { "Two-person approvals" } td {
					@if c.two_person.enabled { "On, above " (c.two_person.approve_over.map(naira).unwrap_or_else(|| naira(Minor(0)))) } @else { "Off" }
				} }
			}
		},
	)
}

pub fn audit(s: &Session, rows: &[(String, String, String, Option<String>)]) -> Markup {
	page(
		"Decisions",
		Some(s),
		"/audit",
		None,
		false,
		html! {
			h1 { "Decisions people made" }
			p.muted { "Approvals and manual payments. The full tamper-evident audit log arrives with the next release." }
			table {
				tr { th { "When" } th { "Trade" } th { "What" } th { "Outcome" } }
				@for (when, trade, what, outcome) in rows {
					tr { td { (when) } td { code { (trade) } } td { (what) } td { (outcome.as_deref().unwrap_or("pending")) } }
				}
			}
		},
	)
}

pub fn users(s: &Session, list: &[UserRow], flash: Option<&str>) -> Markup {
	page(
		"Users",
		Some(s),
		"/users",
		flash,
		false,
		html! {
			h1 { "Users" }
			p.muted { "Add people with " code { "merchant-agent users add <name> --role <viewer|operator|approver|admin>" } " on the agent's host. Each sets a password and an authenticator app." }
			table {
				tr { th { "User" } th { "Role" } th { "Status" } th {} }
				@for u in list {
					tr {
						td { (u.username) } td { (u.role) }
						td { @if u.disabled { "Disabled" } @else if u.locked_until.is_some() { "Locked after failed sign-ins" } @else { "Active" } }
						td {
							@if u.username != s.username {
								form method="post" action=(format!("/users/{}/{}", u.username, if u.disabled { "enable" } else { "disable" })) {
									input type="hidden" name="csrf" value=(s.csrf);
									button.quiet type="submit" { @if u.disabled { "Enable" } @else { "Disable" } }
								}
							}
						}
					}
				}
			}
		},
	)
}
