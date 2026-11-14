# Runbooks

One page per situation: what you see, what the agent already did, what to do,
and how to know it's fixed.

| Situation | Page |
|---|---|
| A trade is held, or needs a person | [needs-a-person.md](needs-a-person.md) |
| The bank or payout API is down | [bank-down.md](bank-down.md) |
| The payout account is running low | [low-float.md](low-float.md) |
| RecoilPay can't be reached | [aggregator-unreachable.md](aggregator-unreachable.md) |
| RPC providers give different answers | [rpc-disagrees.md](rpc-disagrees.md) |
| Payouts are paused | [kill-switch.md](kill-switch.md) |
| Changing the API key | [rotate-api-key.md](rotate-api-key.md) |
| The payee key may have leaked | [rotate-payee-key.md](rotate-payee-key.md) |
| The data volume was lost | [restore-from-backup.md](restore-from-backup.md) |
| A new version is out | [upgrading.md](upgrading.md) |

`merchant-agent doctor` catches the misconfigurations these pages mention: a
refused API key, a payee key that doesn't match RecoilPay's, an escrow or
USDC address that doesn't match the registry, an unreachable RPC or one on
the wrong chain, an unreadable secret or one with loose permissions, an
unreachable bank or adapter, a database newer than the agent, a clock that
disagrees with RecoilPay's, and production running on test settings.
