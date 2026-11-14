# Rotating the payee key

## Read this first: it can't change on its own

Your payee key isn't stored anywhere you can replace it. The RecoilPay portal
**derives** it from a signature by your operator wallet, for your operator
id, every time. Same wallet, same operator, same key. So there is no
"rotate" button, and that's deliberate: you can always recover the key from
your wallet.

The key opens your customers' sealed bank details. Anyone who has it can read
the bank details of every trade sealed to you, past and future, if they can
also get the sealed data.

## What you see

Nothing on its own. You're here because the key may have leaked: a copy left
on a laptop, an old secret file, a person who had it leaving.

## What the agent already did

Nothing it can do: the agent only reads the key. If the key it reads ever
differs from the one registered with RecoilPay, it refuses to start, and
`doctor` fails **Payee key matches the key registered with RecoilPay**.

## What to do

**If it's routine**, with no reason to think it leaked: nothing to rotate.
Check who can read the secret, and tighten that.

**If it may have leaked**, move to a new payee key, which means a new
operator:

1. Pause now: `merchant-agent pause --by YOU`. No new customer seals details
   to the old key.
2. Let the trades in progress finish. `merchant-agent status` shows none in
   *Checking* or *Paying*, and nothing left in **Exceptions**.
3. Register a new operator in the portal **with a new wallet**: a new
   operator id, a new API key, and, from **Payouts**, a new payee key
   (**Export payee key**).
4. Move your bond and your offers to the new operator.
5. Set the agent up for the new operator: a new config (`operator.solver_id`,
   `operator.wallet`) and new secrets, on a **new, empty data volume**. The
   old volume belongs to the old operator: keep it as a record, don't reuse
   it.
6. Run `merchant-agent doctor`, start the agent, and resume.
7. Retire the old operator: rotate its API key and don't store the new one,
   withdraw its offers, and recover its bond when RecoilPay allows.

Bank details sealed to the old key before step 1 can still be read by
whoever has it: treat that as a data incident under your own policy.

A way to replace the payee key while keeping your operator is a RecoilPay
platform change, not built yet.

## How to know it's fixed

`doctor` passes on the new operator, the old operator has no offers, and new
trades come to the new operator id.
