# A trade needs a person

## What you see

- An alert: *Held for review* or *Needs a person* (urgent), with the trade
  id, the amount, the last four digits, what happened and what to do.
- The trade in the console's **Exceptions** page, and under "Exception
  queue" in `merchant-agent status`.

## What the agent already did

It stopped. It will not move money for this trade on its own. Everything it
decided is in the audit log (console → **Audit**).

There are two kinds:

- **Held for review**: the agent didn't pay, because a check failed. No money
  moved.
- **Needs a person**: something happened after the bank was asked, and only a
  person, looking at the bank, can say what's true.

## What to do

Read the reason on the card. Then:

### Held: a limit, the rate, the name, the deadline, screening, the float, a bank refusal

| Reason | Check | Then |
|---|---|---|
| Over the per-trade, daily or per-payee limit, or too many too fast | Is this payout expected? | **Approve and pay** (approver). Only that limit is waived; every other check runs again. |
| The offer's rate is too far from your reference rate | Is your reference rate in the config up to date? | Fix `pricing.reference_rates`, or approve. |
| The trade names an offer the agent never saw | Did you publish it from somewhere else? | Approve only if you did publish it. |
| The naira amount doesn't match | Usually a sign of tampering, or an offer changed under the trade | Don't approve unless you can explain it. |
| The account name at the bank doesn't match | Compare the names (the card shows the account's last four digits) | Approve if it's plainly the same person, otherwise leave it. |
| The account isn't in the seller's verified name, or only partly matches it | This offer pays only verified sellers, and only to an account in their own name. Look the account up in your bank and compare it with the seller's verified name on the RecoilPay portal's payout desk (the agent never stores or shows it) | Approve only if it's plainly the seller's own account (a missing middle name, initials). Otherwise leave it: it expires, and the seller is refunded. |
| The payee matched your screening list | Your compliance process | Approve, or leave it. |
| Too close to the deadline | Is there still time to pay safely? | Usually leave it: it expires, and the customer is refunded. |
| Your payout account would drop below its minimum | [The float is low](low-float.md) | Top up, then approve. |
| Your bank refused the transfer | The refusal is on the card | Fix the cause, then approve: a new transfer is made under a new key. |
| Payouts are paused | Why was it paused? | [Resume](kill-switch.md) first, then approve: a held trade isn't picked up again on its own. |

With two-person approval on, the first approver's press records the approval
and the button asks for a **different** approver. The same person pressing
again does nothing.

### Held: the lock, the payee, the trade changed, the account doesn't exist

The lock doesn't match, the lock was already closed, the bank details can't be
opened or don't match their commitment, the trade changed after the agent
saw it, your bank says the account doesn't exist, or the trade needed a verified
seller but RecoilPay couldn't show the seller's verified name. **These can't be
approved:** the customer's money isn't there for you, or the destination is
wrong. Leave the trade. It expires, and RecoilPay refunds the customer. If
the lock doesn't match, or the verified name was unavailable, tell RecoilPay:
the trade's data and the chain disagree, or RecoilPay's identity records
couldn't be read.

### Needs a person: the bank can't say, or said it failed

1. Look the transfer up in your bank, by the amount, the last four digits and
   the time, or by the key `rpma-<trade id>` if your bank shows it.
2. Then, in the console:
   - it arrived: **It did arrive**, with the bank's reference. The agent
     reports it to RecoilPay.
   - nothing left your account: **Nothing was sent**. The trade is closed
     unpaid, and the customer is refunded at the deadline.
   - nothing left, and you still want to pay: **Pay again (new transfer)**
     (approver). It sends a new transfer under a new key, so confirm in the
     bank first that the old one didn't go.

### Needs a person: anything else

| Reason | What to do |
|---|---|
| The transfer failed or vanished after it was reported | Check the bank. If the customer wasn't paid, pay them and tell RecoilPay support. |
| You paid, but RecoilPay refunded their crypto | Recover the naira from the customer directly; RecoilPay support can help. |
| RecoilPay wouldn't accept the payment report | Check the trade on RecoilPay; contact support with the trade id. |
| Someone else reported this trade as paid | Someone used your API key or the portal: find out who before anything else. |
| The customer confirmed before the agent paid | Nothing was sent. Check with RecoilPay support. |

If you paid a held trade yourself, outside the agent: **Paid outside the
agent** (operator), with your bank's reference.

## How to know it's fixed

The card leaves **Exceptions**, and the trade on **Trades** shows *Paid,
waiting for the customer* or *Settled*, or *Missed, not paid* if you let it
go. `merchant-agent audit verify` still passes, and the audit shows who
decided what.
