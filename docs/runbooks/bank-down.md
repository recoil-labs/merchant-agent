# The bank is down

## What you see

- `merchant_agent_bank_errors_total` rising in your metrics, by kind
  (`lookup`, `send`, `status`, `balance`).
- Log lines from the agent saying it couldn't reach the bank.
- Trades staying in *Checking* or *Paying*. After enough failed status
  checks (10 by default), a trade moves to *Needs a person* with an urgent
  alert.
- `merchant-agent doctor`: **FAIL Bank rail reachable and balance readable**.

## What the agent already did

- Before paying: it waits, and tries again on the next pass. Nothing is
  sent.
- After sending: it never sends again. It keeps asking the bank about the
  payout's key. If the bank still can't say, the trade goes to *Needs a
  person*.
- If a payout can't start before its deadline margin, it doesn't start, and
  the customer is refunded at the deadline.

## What to do

1. Check your bank's or payment provider's status page, and your account.
2. If it's a long outage, [pause](kill-switch.md) so no new trades arrive:
   `merchant-agent pause --by YOU`. Payouts already with the bank carry on.
3. Check your credentials haven't expired: `merchant-agent doctor`.
4. When the bank is back, clear any [trades that need a person](needs-a-person.md):
   check each in the bank before choosing.
5. Resume: `merchant-agent resume --by YOU`.

## How to know it's fixed

`doctor` passes the bank check, `merchant_agent_bank_errors_total` stops
rising, and new trades reach *Paid, waiting for the customer*.
