# The float is low

## What you see

- An urgent alert: *Held for review*, "Your payout account would drop below
  its minimum balance. New offers are paused until you top it up."
- The console's dashboard says payouts are paused by `agent`, "automatically.
  Resuming needs an approver."
- `merchant_agent_float_kobo` near `limits.min_float_minor`.

## What the agent already did

- It held the trade that would have taken the balance below
  `limits.min_float_minor`, and paid nothing.
- It paused itself and withdrew your offers from RecoilPay, so no new trades
  arrive. Payouts already with the bank carry on.

## What to do

1. Top up your payout account.
2. Check the agent sees it: `merchant-agent doctor` shows the balance on the
   bank check.
3. Resume from the console (**Resume payouts**). An automatic pause needs an
   **approver**; `merchant-agent resume` refuses it on purpose. Resuming
   republishes your offers.
4. Approve the held trade (**Approve and pay**), if it's still in time.

If you keep hitting the minimum, raise your balance or lower your limits.

## How to know it's fixed

The dashboard says payouts are running, your offers are back on RecoilPay,
and the held trade is paid or has expired.
