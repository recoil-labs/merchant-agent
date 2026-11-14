# The RPC providers disagree

## What you see

- Trades *Held for review*: "The escrow lock on-chain does not match the
  trade", with an alert. The audit entry says the RPCs disagree about the
  lock.
- Only with `require_rpc_agreement: true` and two or more RPCs for a chain.

## What the agent already did

It asked every RPC endpoint about the lock. They gave different answers (one
or two blocks behind is allowed), so it held the trade. It never picks one
answer to trust.

## What to do

1. Check which endpoint is wrong: compare the latest block from each
   (`cast block-number --rpc-url <url>`), or check the provider's status page.
   `merchant-agent doctor` fails the RPC check if any endpoint is unreachable
   or on the wrong chain.
2. If one provider is behind or broken, fix it or take it out of
   `chains[].rpcs` until it recovers, and restart the agent. Keep at least
   two in production.
3. A lock hold can't be approved: the trade stays held, closes unpaid at its
   deadline, and the customer is refunded. Only trades that arrive after the
   fix are paid. Tell RecoilPay support the held trade ids, if you want to
   follow up.
4. If both providers agree and the lock still doesn't match the trade, that's
   not an RPC problem: tell RecoilPay. The trade's data and the chain
   disagree.

## How to know it's fixed

`doctor` passes the RPC check, the endpoints report nearly the same block,
and new trades reach *Checking* and then *Paying*.
