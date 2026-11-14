# The RecoilPay Merchant Agent

The Merchant Agent pays your RecoilPay cash-outs for you. It runs on your own
machine, next to your bank account, and does what a person on your team would
do for each cash-out:

1. sees a customer's cash-out waiting for you on RecoilPay;
2. checks on the blockchain, through your own node provider, that the
   customer's USDC is locked in the escrow for you;
3. opens the customer's sealed bank details with your payee key;
4. checks the amount, your limits and the account holder's name;
5. pays them from your bank account;
6. tells RecoilPay the transfer reference.

When the customer confirms they were paid, the escrow releases the USDC to
your wallet. When anything looks wrong, the agent stops, holds the trade, and
alerts you. A person decides.

## What it never does

- **It never sends bank details to RecoilPay.** Customers seal their bank
  details to your payee key in their own browser. Only you can open them.
  The agent opens them in memory to pay, and keeps only the last four digits.
- **It never pays without an on-chain lock.** Before paying, the agent reads
  the escrow lock itself, from your RPC provider: it must exist, be open, pay
  your wallet, and hold the right amount. It doesn't take RecoilPay's word for
  it.
- **It never pays the same trade twice.** Every payout has a fixed key, written
  down before your bank is called. After a crash, a timeout or a lost answer,
  the agent asks your bank about that key. It doesn't guess.
- **It never goes over your limits.** Per trade, per day, per customer
  account, how many payouts an hour, and a minimum balance. Over a limit, it
  holds the trade for a person.
- **It never listens to the internet.** It makes outbound calls only. Its
  console and health port listen on your machine.

## How it fits together

```text
 RecoilPay ◀── HTTPS ──  Merchant Agent  ── HTTPS ──▶ your bank or payout API
 (trades, reports)        (your machine)    ── HTTPS ──▶ your RPC provider (the escrow lock)
                               │
                        console on 127.0.0.1:8740 · health on 127.0.0.1:8741
```

The agent keeps its state in one small database on one volume: the trades it
has seen, every payout it started, and a tamper-evident audit log of every
decision.

## Quick start: a sandbox agent

Sandbox mode uses Base Sepolia testnet and a **simulated bank: no real money
moves**. It takes about five minutes with Docker. Follow
[the deploy guide](../deploy/README.md#quick-start-a-sandbox-agent-with-docker-compose).
In short:

```bash
cd deploy                          # in your clone of this repo
cp .env.example .env              # your operator id, wallet, API key, payee key
docker compose build
mkdir -p config && docker compose run --rm -T init > config/merchant-agent.yaml
docker compose up -d
docker compose exec agent merchant-agent doctor
```

Then add yourself to the console and open <http://127.0.0.1:8740>:

```bash
docker compose exec agent merchant-agent users add YOUR_NAME --role admin
```

### Your first held trade

To see the agent hold a trade and a person clear it, make one that's over
a limit:

1. In `config/merchant-agent.yaml`, set `limits.per_trade_minor` to `"1000000"`
   (₦10,000), and keep `per_day_minor` and `per_payee_per_day_minor` at least
   that. Restart: `docker compose restart agent`.
2. In the RecoilPay app (sandbox), cash out a little more than ₦10,000 of
   test USDC to any test account, with your offer the one you're quoted.
3. In the console, **Exceptions** shows the trade: *Held for review*,
   *This payout is larger than your per-trade limit.* You also see it with
   `docker compose exec agent merchant-agent status`.
4. As an `approver` or `admin`, press **Approve and pay**. The agent checks
   the lock and everything else again, waives only that limit, pays (the
   simulated bank), and reports. On the **Trades** page the trade moves to
   *Paid, waiting for the customer*, then *Settled* once they confirm.

[A trade needs a person](runbooks/needs-a-person.md) explains every hold and
what to do about it.

## Going to production

Production pays real naira from your real bank account. Change these in the
config, then run `merchant-agent doctor` until it shows no failures:

| Setting | Sandbox | Production |
|---|---|---|
| `mode` | `sandbox` | `production` |
| `aggregator.url` | `https://api.recoilpay.com` | RecoilPay's production URL |
| secrets | environment variables | files (owned by the agent, mode 0400), or GCP or AWS Secret Manager |
| `chains` | Base Sepolia, a public RPC | your production chain, with your own RPC provider(s) |
| `rail.kind` | `simulated` | `paystack`, `manual`, or `template` (your own adapter) |
| `limits` | low | your real limits |

The rails:

- **`paystack`**: pays with Paystack Transfers. Needs a Paystack business that
  can send third-party transfers (a Registered Business), and OTP for
  transfers turned off, or every payout waits for a person.
- **`manual`**: a person pays from your bank app. The agent does everything
  else: it shows the bank details once, on request, and records the
  reference you type.
- **`template`**: your own bank or payout API, through a small adapter you
  run. See [adapters.md](adapters.md).

Production refuses environment-variable secrets, plain-HTTP connections, and
the simulated bank. Before turning it on, read [security.md](security.md)
with your security team.

### Paystack webhooks

The agent doesn't use webhooks. It accepts no inbound connections, and asks
Paystack about each transfer by its reference instead. If you also want
Paystack's webhooks for your own systems, receive them on your own gateway as
usual; the agent doesn't need them.

## Everyday operation

| You want to | Do |
|---|---|
| see what's happening | the console's dashboard, or `merchant-agent status` |
| stop all new payouts now | **Pause now** in the console, or `merchant-agent pause --by YOU` |
| start again | **Resume payouts**, or `merchant-agent resume --by YOU` |
| check the setup | `merchant-agent doctor` |
| check the audit log | `merchant-agent audit verify` |
| test your alerts | `merchant-agent alerts test` |

Console roles: a **viewer** sees; an **operator** can also pause, mark a
trade paid, and pay manual payouts; an **approver** can also approve held
payouts and resume after an automatic pause; an **admin** also manages users.

## Runbooks

One page per situation, each with what you'll see, what the agent already
did, what to do, and how to know it's fixed:

- [A trade needs a person](runbooks/needs-a-person.md)
- [The bank is down](runbooks/bank-down.md)
- [The float is low](runbooks/low-float.md)
- [RecoilPay can't be reached](runbooks/aggregator-unreachable.md)
- [The RPC providers disagree](runbooks/rpc-disagrees.md)
- [The kill switch fired](runbooks/kill-switch.md)
- [Rotating the API key](runbooks/rotate-api-key.md)
- [Rotating the payee key](runbooks/rotate-payee-key.md)
- [Restoring from a backup](runbooks/restore-from-backup.md)
- [Upgrading](runbooks/upgrading.md)

## Known limits of v1

- **Cash-outs only.** The agent pays naira for USDC. It doesn't handle
  purchases (naira in, crypto out).
- **EVM chains only.** No Solana.
- **One instance.** No high availability: one agent per operator, on one
  volume. If its machine dies, start it on another with the same volume.
- **No webhooks.** It polls RecoilPay and the bank.
- **Clock skew is checked when the agent starts** (doctor, which production
  runs first), not again while it runs.
- **The API key can do everything an operator can,** and RecoilPay can't
  replace it yet. There are no narrower keys for agents. Store it like a
  bank credential.
- **The payee key can't be changed on its own.** See
  [Rotating the payee key](runbooks/rotate-payee-key.md).
- **Amd64 images only** are published; on other machines, build the image.
