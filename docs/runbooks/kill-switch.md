# The kill switch fired

## What you see

- The console's dashboard says payouts are **paused**, by whom, and whether
  it was automatic.
- `merchant-agent status` shows `Payouts: PAUSED by …`.
- `/readyz` answers 503 with `"running": false`; `merchant_agent_paused 1`.
- Your offers are gone from RecoilPay.

## What the agent already did

- It stopped starting new payouts. Trades that arrive are held as *Payouts
  are paused*.
- It withdrew your offers from RecoilPay, so no new trades arrive. It kept a
  copy to republish later.
- A payout already with the bank carries on, and the agent still follows it
  up and reports it.

Someone pressed **Pause now** or ran `merchant-agent pause`, or the agent
paused itself because [the float is low](low-float.md).

## What to do

1. Find out why: who paused it (the dashboard, and **Audit** shows
   `paused` with who).
2. Fix the cause.
3. Resume:
   - a pause by a person: **Resume payouts** (operator), or
     `merchant-agent resume --by YOU`;
   - an automatic pause: **Resume payouts** in the console, as an
     **approver**. The command line refuses it, on purpose.
4. Resuming republishes your offers. If none of them are still valid (they
   expired while paused), publish a new offer book in the RecoilPay portal.
5. Trades held while paused can be approved (see
   [A trade needs a person](needs-a-person.md)).

To pause in an emergency, any of these works, even if the agent isn't
running:

```bash
merchant-agent pause --by YOUR_NAME
docker compose exec agent merchant-agent pause --by YOUR_NAME
kubectl -n merchant-agent exec merchant-agent-0 -- merchant-agent pause --by YOUR_NAME
```

## How to know it's fixed

The dashboard says payouts are running, `/readyz` answers 200, your offers
are back on RecoilPay, and `status` shows `Payouts: running`.
