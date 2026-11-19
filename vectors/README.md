# Shared test vectors

Copies of the vectors in RecoilPay's own repository (`test-vectors/` in
`recoil-labs/recoil-pay-v2`), which is their source of truth. The RecoilPay
aggregator, the TypeScript kit and this agent are all tested against the same
bytes, so none of them can change a shared rule alone.

| File | Pins |
|---|---|
| `fiat-payee.json` | the bank-details envelope, the payee key and the commitment (`src/payee/`) |
| `fiat-lock.json` | the escrow lock id (`src/chain/lock_id.rs`) |
| `fiat-pricing.json` | how a cash-out's naira amount is computed (`src/policy/pricing.rs`) |
| `merchant-agent-api.json` | the shapes of the aggregator routes the agent calls (`src/aggregator/`) |
| `seller-names.json` | whether the bank's name for an account matches a seller's verified name (`src/rail/names.rs`) |

Never edit them here. To update, from a checkout of both repositories:

```bash
scripts/sync-from-monorepo.sh ../recoil-pay-v2    # copies, then run cargo test
```

CI runs `scripts/sync-from-monorepo.sh --check` daily and fails if the copies
differ. It needs the `RECOILPAY_REPO_TOKEN` secret: a fine-grained token with
read-only access to the contents of `recoil-labs/recoil-pay-v2`.
