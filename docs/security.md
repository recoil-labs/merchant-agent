# Merchant Agent: security

For your security team. It says what the agent protects, from whom, how, and
where the limits are. Every rule below has tests named after it
(`invariant_<n>_…` in the source); the list is at the end.

## What it protects

- **Your money.** The agent can send naira from your payout account. The
  main risks are paying twice, paying someone who isn't owed, paying for a
  trade whose crypto isn't really locked for you, and paying too much.
- **Your customers' bank details.** Account numbers and names, which the
  agent must open to pay.
- **Your credentials.** Your RecoilPay API key, your payee key, your bank or
  adapter credentials.

## Who it defends against

| Threat | What the agent does |
|---|---|
| **RecoilPay is wrong or compromised**, or someone sits between you and it | It checks every money fact itself: the escrow lock on-chain through your own RPC, your wallet as the counterparty, the escrow and token addresses pinned in your config, and the naira amount recomputed from the rate you published. It never pays on RecoilPay's word alone. |
| **A customer tampers with their bank details** | It opens the sealed details and checks them against the commitment the trade carries. If they don't match, it holds the trade. |
| **The account isn't the customer's** | It looks the account up at your bank and compares the name with the sealed one. If it differs, it holds the trade. |
| **A crash, a timeout, or a lost answer from your bank** | It writes down its intent to pay, under a fixed key, before calling the bank. Afterwards it asks the bank about that key, and never resends blind. |
| **A bug, a flood of trades, or a stolen API key** | Your local limits always apply: per trade, per day, per customer account, how many an hour, and a minimum balance. Over a limit, it holds the trade. |
| **A second copy of the agent** | One agent per operator: a file lock, and a lease in the database with a fencing token. A second copy refuses to pay. |
| **Someone on the network** | The agent listens only on your machine (127.0.0.1) unless you say otherwise. Every outbound call is HTTPS with certificates checked, and it never follows redirects. |
| **Someone editing the records** | Every decision goes into a hash-chained audit log. `merchant-agent audit verify` finds any edited or deleted entry. |
| **A misconfiguration** | It fails closed: anything it doesn't recognise means no payout, and an alert. `merchant-agent doctor` checks the whole setup. |

It does **not** defend against someone with root on the machine it runs on:
they can read its memory and its secrets. Treat that machine like the one
that holds your bank credentials.

## The rules it keeps

1. **Never pay twice.** Every payout's key is fixed (`rpma-` plus the trade
   id), and the intent to pay is written to the database before the bank is
   called. After a crash at any point, a restart never sends a second
   payment for the same trade.
2. **Never retry an unclear payout blind.** If the bank times out, answers
   with an error, or says something the agent can't read, it asks the bank
   about the key. If the bank still can't say, the trade goes to *Needs a
   person*, and nothing is sent again automatically.
3. **Don't trust RecoilPay for money facts.** Before paying, the agent checks
   for itself that:
   - the escrow lock exists, at the confirmation depth you set, and is still
     open;
   - it was funded by the trade's user, pays **your** wallet (from your
     config), holds the chain's USDC (from your config), and holds the
     trade's amount;
   - the escrow and USDC addresses match RecoilPay's registry (checked by
     `doctor`);
   - the naira amount equals the locked USDC times the rate you published,
     minus your fees, to the kobo. The agent keeps its own record of every
     offer you published, and the first terms it saw win.
4. **Pay only the committed account.** It opens the sealed details, checks
   them against the trade's commitment, and checks the account name at your
   bank. A mismatch is held.
5. **Local limits always apply**, whatever RecoilPay says. Over a limit, it
   holds, never pays.
6. **Don't start a payout that could land after the deadline.** If less than
   `policy.start_margin_secs` is left, it doesn't start, and alerts you. A
   late payment could be paid by you *and* refunded to the customer.
7. **Bank details never leave memory.** They are opened, used and wiped. They
   never reach the logs, metrics, alerts, the audit log or the database. The
   database keeps the commitment, the last four digits, and a keyed hash of
   the account (the payee tag, keyed with your pepper) for the per-account
   limit and your screening list. A test runs the whole flow and searches
   every log, alert, audit entry and database row for the test account
   number and name.
8. **Secrets come from a secret store, never from the config.** The config
   holds references (`file:///…`, `gcp-sm://…`, `aws-sm://…`, or `env://…`
   in sandbox only). A secret file must be owned by the agent's user and
   readable by nobody else, or the agent refuses to start. Production refuses
   `env://`.
9. **Outbound only.** No inbound port except the console and health port,
   both on 127.0.0.1 by default. Binding anywhere else needs
   `server.bind_public: true`. Production refuses plain-HTTP connections.
10. **One agent at a time.** A file lock on the data directory, and a lease
    with a rising fencing token, which every write checks.
11. **The kill switch always works.** `merchant-agent pause` or the
    console's **Pause now** stops new payouts at once, and withdraws your
    offers from RecoilPay (it replaces your offer book with an empty one) so
    no new trades arrive. A payout already with the bank carries on, and the
    agent still follows it up. The agent also pauses itself when your float
    runs low.
12. **A tamper-evident audit.** Every decision is one entry, chained to the
    one before it by its hash, in the database and in `audit.jsonl`. `audit
    verify` checks both chains and that they agree.
13. **Fail closed.** An unknown trade field, state or envelope version, RPC
    providers that disagree, a database error: each means no payout, and an
    alert. Clock skew is checked by `doctor`, which production runs before
    starting; it isn't re-checked while the agent runs.
14. **Supply chain.** Pinned dependencies (`Cargo.lock`), `cargo deny` and
    `cargo audit` in CI (each accepted advisory is written down, with its
    reason, in `deny.toml`), well-known audited crates for all cryptography
    (k256, aes-gcm, hkdf, sha2, sha3, argon2, zeroize), and no hand-written
    cryptography. Release images are signed with cosign and carry an SBOM.
15. **The API key.** RecoilPay doesn't yet have narrower keys for agents, so
    the agent's key can do everything an operator key can: rotate keys,
    change settings. Store it like a bank credential.

## What it stores

All in one SQLite database and one audit file, on its one volume
(`/var/lib/merchant-agent` in the image):

| Data | Kept | Never kept |
|---|---|---|
| Trades | id, chain, amounts, rate, the user's wallet address, the payee commitment, the payee tag, the last 4 digits, state and reason, bank reference, deadline | account number, name, bank details |
| Payouts | key, amount, the day, status, bank reference | |
| Offers seen | the terms of every offer you published, as first seen | |
| Approvals | who asked, who approved, when | |
| Manual payouts | amount, status, reference, who revealed the details and when | the details themselves |
| Console users | name, role, an argon2id password hash, the authenticator secret (encrypted with a key derived from your pepper) | passwords |
| Control | paused or not, by whom, your last offer book | |
| Audit log | every decision: when, who, what, which trade, a short detail | personal data |
| Sandbox bank (sandbox only) | simulated payments, last 4 digits | |

Back the volume up: it is the record of every payout. Before a database
migration the agent saves a copy beside it automatically.

## Network

Outbound only, all HTTPS:

| To | Why |
|---|---|
| `aggregator.url` (RecoilPay) | trades, offers, reports, heartbeats |
| each `chains[].rpcs` URL | reading the escrow lock |
| `api.paystack.co`, or your adapter's `rail.url` | lookups, payouts, status, balance |
| your alert channels | a webhook URL, `events.pagerduty.com`, or your SMTP server |
| GCP or AWS Secret Manager, and the instance metadata server | only with `gcp-sm://` or `aws-sm://` secrets |

Inbound: the console (8740) and health port (8741), on 127.0.0.1 unless you
set `bind_public`. In a container they listen inside it, and the compose file
publishes them on the host's 127.0.0.1 only. Don't expose the console to the
internet: reach it through an SSH tunnel or `kubectl port-forward`.

The console has: passwords hashed with argon2id, a one-time code from an
authenticator app on every sign-in, five failures locking the account for 15
minutes, 15-minute idle sessions, CSRF tokens on every form, and four roles.
Cookies are `Secure`: some browsers (Safari) won't keep them over plain
`http://127.0.0.1`. If sign-in doesn't stick, use Chrome or Firefox, or put
TLS in front.

## Secrets

| Secret | What it can do | How to rotate it |
|---|---|---|
| RecoilPay API key (`aggregator.api_key`) | everything your operator account can | Not possible yet: RecoilPay can't replace an operator key today. See [Rotating the API key](runbooks/rotate-api-key.md). |
| Payee key (`payee.private_key`) | open your customers' sealed bank details | [Rotating the payee key](runbooks/rotate-payee-key.md): it can't change on its own |
| Payee tag pepper (`payee.tag_pepper`) | keys the payee tags and the console's authenticator secrets | Changing it resets per-account limits for the day, breaks your screening list until you rebuild it (`merchant-agent screening hash`), and means every console user must be added again. Change it only with a new data volume, or on a quiet day. |
| Bank or adapter credentials (`rail.credentials`) | send money | Create a new one with your bank or adapter, put it in the secret store, restart the agent, then revoke the old one. |
| Alert channel URLs and keys | post alerts | Replace and restart; check with `merchant-agent alerts test`. |

When a secret changes between restarts, the agent records `secret_rotated`,
with the secret's name, in the audit log. It compares keyed fingerprints and
never stores the secret itself.

## Tests for each rule

`cargo test` runs them all. Names begin with `invariant_<n>_`:

- 1: `a_crash_at_any_step_never_pays_twice`, `dropping_the_runtime_mid_payout_never_pays_twice`, `any_mix_of_faults_never_pays_twice_and_the_books_balance`, `the_intent_and_its_key_are_written_before_any_send`, `never_two_payouts_for_one_trade`
- 2: `a_5xx_after_paying_is_asked_about_not_resent`, `a_send_that_fails_after_paying_is_asked_about_not_resent`, `a_bank_that_cant_say_goes_to_a_person_and_nothing_is_resent`, `an_unknown_status_goes_to_a_person_and_nothing_is_resent`
- 3: `each_single_field_mismatch_is_caught`, `a_lock_paying_someone_else_is_caught`, `a_released_lock_is_closed`, `a_lock_not_yet_at_depth_waits`, `an_rpc_on_another_chain_is_refused`, `doctor_fails_when_the_registry_lists_another_escrow`, `one_kobo_more_or_less_than_the_recomputation_is_held`, `first_seen_terms_win_over_a_rewritten_quote`, and more
- 4: `a_payee_that_does_not_match_the_commitment_is_held`, `any_single_bit_flip_is_refused`, `a_wrong_account_name_holds_the_trade`
- 5: one boundary test per limit, and `concurrent_intents_never_slip_under_the_daily_limit_together`
- 6: `a_trade_too_close_to_its_deadline_is_held_not_paid`, `the_deadline_margin_boundary`
- 7: `no_account_number_or_name_in_the_database_logs_or_alerts`, `a_webhook_alert_carries_no_personal_data`, `open_errors_never_carry_personal_data`
- 8: `production_refuses_env_secrets`, `a_world_readable_secret_file_is_refused`, `a_secret_file_owned_by_someone_else_is_refused`, `secrets_never_appear_in_debug_output`
- 9: `a_production_client_refuses_plain_http`, `production_refuses_a_plain_http_rpc`, `redirects_are_never_followed`
- 10: `a_second_process_on_the_same_directory_is_refused`, `two_hosts_on_one_database_only_one_acts`, `the_lease_has_one_holder_and_a_rising_fencing_token`
- 11: `pausing_from_the_console_withdraws_the_offers`, `a_paused_agent_holds_new_payouts`, `pause_status_and_resume_from_the_command_line`
- 12: `editing_any_line_of_the_file_is_detected`, `deleting_a_line_of_the_file_is_detected`, `editing_or_deleting_a_database_row_is_detected`
- 13: `an_unknown_field_is_refused`, `an_unknown_state_is_refused`, `malformed_amounts_are_refused`
- 14 and 15 are CI checks and the advice above.

RecoilPay's end-to-end run, against the real aggregator on a fork of Base
Sepolia (it lives in RecoilPay's own repository, with the aggregator), also
puts a lying proxy between the agent and RecoilPay. It inflates an amount,
and invents a trade whose lock pays someone else; the agent holds both.
