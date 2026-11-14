# Writing a payout adapter

If you pay through your own bank API or payout system rather than Paystack,
you connect it to the agent with an **adapter**: a small HTTPS service you
run, in any language, that turns four simple calls into calls to your bank.
The agent uses it with `rail.kind: template`.

```text
Merchant Agent ──HTTPS──▶ your adapter ──▶ your bank or payout API
```

Start from the example in [`examples/adapter/server.mjs`](../examples/adapter/server.mjs):
plain Node, no dependencies. It already speaks the protocol and keeps the
rules below. You replace four functions, marked **YOUR BANK**, with calls to
your bank. Then you run the conformance suite against it until it passes.

## The one rule

**A payout key is paid at most once, ever.** The agent gives every payout a
key (`rpma-ft-…`). It may send the same key again: after a crash, after a
timeout, or twice at once. Your adapter must pay it once and give back the
first answer every time. If your bank takes an idempotency key or a
reference, pass the agent's key to it.

Everything else follows from that rule, and the conformance suite checks it.

## The protocol (version 1)

Every request carries `Authorization: Bearer <token>`, the token in the
agent's `rail.credentials`. Answer anything without it with `401`. Bodies are
JSON. Amounts are kobo, written as strings of digits (`"1490500"` is
₦14,905.00).

**Only an HTTP 200 with a `result` the protocol knows counts as an answer.**
Anything else (a timeout, a 4xx, a 5xx, a body that isn't the protocol)
tells the agent "I couldn't ask". It then asks again by key. It never assumes
nothing was sent. So **a refusal must be an explicit `rejected`**, never an
error status.

### `GET /v1/info`

```json
{"protocol": 1, "name": "Acme payouts", "idempotent_send": true}
```

`idempotent_send: true` promises the rule above. With `false`, the agent
never sends a key twice, and any lost answer goes to a person.

### `POST /v1/lookup`: who holds this account?

```json
{"bank_code": "058", "account_number": "0123456789"}
```

| Answer | Meaning |
|---|---|
| `{"result": "found", "account_name": "ADA OBI"}` | the bank's name for the account |
| `{"result": "not_found"}` | no such account |
| `{"result": "unsupported"}` | you can't look accounts up; the agent then can't check names |

### `POST /v1/payouts`: pay

```json
{
  "key": "rpma-ft-7c1e9a2b4d6f48e0a1b2c3d4e5f60718",
  "trade_id": "ft-7c1e9a2b4d6f48e0a1b2c3d4e5f60718",
  "amount_minor": "1490500",
  "currency": "NGN",
  "bank_code": "058",
  "account_number": "0123456789",
  "account_name": "Ada Obi",
  "narration": "RecoilPay cash-out"
}
```

| Answer | Meaning |
|---|---|
| `{"result": "accepted", "reference": "TRF-123"}` | the bank has it; `reference` is what your bank calls this transfer |
| `{"result": "rejected", "reason": "insufficient funds"}` | refused, and **no money moved** |
| `{"result": "unknown"}` | you can't tell whether it was sent |

The same key again must return the same `accepted` (same reference) or the
same `rejected`. Never send it to the bank twice.

**Write the key down before you call your bank**, as the example does. If
your process dies mid-call, you then know you may have sent it. Answer that
key `unknown` until a person has checked the bank.

The reference is reported to RecoilPay and the customer sees it: up to 100
printable characters, no spaces.

### `GET /v1/payouts/{key}`: what happened?

| Answer | Meaning |
|---|---|
| `{"result": "succeeded", "reference": "TRF-123"}` | paid; the same reference as `accepted` |
| `{"result": "pending"}` | accepted, still moving |
| `{"result": "failed", "reason": "…"}` | accepted, then failed, and the money came back |
| `{"result": "not_found"}` | you have never seen this key |
| `{"result": "unknown"}` | you can't tell |

`not_found` is a promise that the key was never paid. The agent may then send
it again, if you said `idempotent_send: true`.

### `GET /v1/balance`

```json
{"currency": "NGN", "available_minor": "123456789"}
```

Use `"available_minor": null` if you can't tell. The agent then can't keep
your minimum float, and says so.

### Personal data

The lookup and the payout carry an account number and a name, and the payout
carries an amount. Don't log the account number or name; log the key, the
amount and the result, as the example does. Don't echo them back in a
`reason`. The agent masks long digit runs in reasons anyway.

## Connecting the agent

```yaml
rail:
  kind: template
  url: "https://payouts.internal:8443"     # http:// only in sandbox
  credentials: file:///run/merchant-agent/secrets/adapter-token
```

Make the token long and random (`openssl rand -hex 32`), and give the same
value to your adapter. `merchant-agent doctor` checks the agent can reach the
adapter, that it speaks protocol 1, whether it says it's idempotent, and that
the balance reads.

## The conformance suite

`merchant-agent adapter-test` runs the suite against your adapter. **It sends
real test payouts** (₦1 each by default), so it refuses a production config.
Point a sandbox config at your adapter's test environment.

Your test environment must have:

- a test account that resolves and takes payouts: by default bank `058`,
  account `0000000001`;
- an account that doesn't exist: by default `0000000009`;
- a balance it can show.

Different accounts can be given with `--bank-code`, `--account` and
`--missing-account`. To try the example:

```bash
examples/adapter/test.sh
```

### What the conformance suite checks

| Check | Why the agent needs it |
|---|---|
| Balance is readable | the minimum-float rule |
| Lookup finds the test account | the name check |
| Lookup says a missing account is not found | holding payouts to accounts that don't exist |
| A payout is accepted, with a reference | paying at all |
| Its status settles by key, with the same reference | following a payout up after a crash |
| The same key again pays nothing more | **the rule**: resending after a lost answer |
| Two sends at once under one key pay once | **the rule**, under a race |
| A send the agent gave up on is resolved by key | a timeout: the key must say whether it arrived, and a resend must not pay twice |
| An unknown key is not found | telling "never sent" from "lost" |
| A payout to a missing account is refused | an explicit `rejected`, not an error |
| A payout larger than the balance is refused | the same, and no money moved |
| The balance moved by exactly what was paid | the ledger shows every payout once, and nothing for refusals |

Every check must pass, or be skipped for a reason you accept (for example,
no balance). **Don't connect an adapter to production money until it passes.**
Run the suite again after every change to your adapter.

The same suite runs in the agent's own tests against its simulated bank,
which can lose requests, fail after paying, deliver twice, stay pending, and
go down, and against the template rail in front of that bank. It also runs
against a deliberately careless adapter that pays a repeated key twice; the
suite must, and does, fail it. The agent's crash and fault drills run end to
end through the template rail too, and never pay twice.

## Running your adapter

- Run it beside the agent, on a private network: the agent is its only
  client. Serve HTTPS (the example takes `TLS_CERT` and `TLS_KEY`), or put
  it behind your TLS proxy.
- Keep its record of payouts (the example's `adapter-ledger.json`) on durable
  storage, and back it up with the agent's volume. It is what keeps the rule
  across restarts.
- One instance, like the agent, unless its record is in a shared database
  with a unique key on the payout key.
