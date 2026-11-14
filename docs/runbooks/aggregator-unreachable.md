# RecoilPay can't be reached

## What you see

- `/readyz` answers 503 with `"aggregator": false`;
  `merchant_agent_ready 0`.
- `merchant_agent_heartbeat_failures_total` rising, and
  `merchant_agent_last_tick_timestamp_seconds` falling behind.
- Log lines saying RecoilPay timed out or refused.
- `merchant-agent doctor`: **FAIL Aggregator reachable and API key accepted**.

## What the agent already did

- It keeps running and keeps trying. It can't see new trades, and can't
  report payouts it made.
- A payout already sent stays recorded, with its bank reference. The agent
  reports it as soon as RecoilPay answers.
- It never pays a trade it can't see, so nothing is paid on stale
  information.

## What to do

1. Run `merchant-agent doctor`. Is it the network, or the key?
   - "refused the API key" or 401: [the API key](rotate-api-key.md) changed or
     was revoked.
   - timeouts or connection errors: check your outbound network and DNS,
     and RecoilPay's status. In Kubernetes, check the NetworkPolicy allows
     your `aggregator.url`.
2. If it lasts, tell your customers' support team: trades in progress can't
   be reported until it's back. Their deadline doesn't stop; RecoilPay
   handles trades that expire.

## How to know it's fixed

`/readyz` answers 200, `doctor` passes the aggregator checks, heartbeats
count up again, and payouts made during the outage show *Paid, waiting for
the customer*.
