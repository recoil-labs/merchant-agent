# Changelog

## Unreleased

- **Merchant-signed purchases** ([docs/purchases.md](docs/purchases.md)),
  off unless `purchases.enabled`. The agent signs each purchase's Permit2
  lock from your own float wallet, so RecoilPay never holds your sale USDC;
  its fill wallet only relays and pays gas. It signs only what your own
  first-seen purchase offer gives for the buyer's naira (pinned by
  `vectors/fiat-pricing.json` `onrampCases`), inside your rate band,
  purchase limits and float floor, for a buyer not on your deny list, with
  the chain, escrow and USDC from your config. The digest and nonce are
  pinned by `vectors/fiat-lock.json`. Refusals are recorded, audited and
  alerted. Signatures are stored before posting and re-posted after a
  crash, never signed twice.
- **New settings:** `purchases.enabled`, `purchases.signer` (its own
  secret, never the payee key), `purchases.max_per_trade_minor`,
  `purchases.max_per_day_minor`, `purchases.float_floor` (per chain),
  `purchases.poll_secs`, `purchases.max_deadline_secs`.
- **`doctor`** checks the float wallet key, that RecoilPay has it
  registered, and on each merchant-signed chain the Permit2 allowance and
  the balance against the floor.
- **The deny list** also takes buyer wallets (`0x…` lines).
- **The quote history records direction** (database migration 5; older
  quotes are cash-out quotes). A cash-out is priced only on cash-out terms,
  a purchase only on purchase terms.
- **The console** dashboard shows purchases signed and refused, and the
  median time to sign.

- **Licence.** The agent now ships under the RecoilPay Merchant Agent License
  ([`LICENSE`](LICENSE)), which replaces the bare "All rights reserved"
  notice. It is source-available, not open source: you may read, audit and
  build the code, run it with RecoilPay for your own merchant account,
  configure it, and write your own payout adapter. You may not modify,
  fork, redistribute it, or use it with any other platform.

## 0.2.0

**Required for merchants who require verified sellers.** RecoilPay now lets a
merchant take cash-outs only from sellers who passed an identity check, and
sends the seller's verified legal name on those trades (`payer_name`).
Version 0.1.0 ignores that name, so it can't hold a payout to an account in
someone else's name. Upgrade before turning on "Require verified sellers",
or before RecoilPay requires it for your account.

- **Verified seller names.** On a trade that carries the seller's verified
  name, the bank's name for the account must match it under RecoilPay's
  shared rule (`vectors/seller-names.json`: the same words in any order;
  titles dropped). Two new hold reasons:
  - `verified_name_mismatch`: the account isn't in the seller's verified
    name, or only partly matches it (a missing middle name, initials). A
    person can check and approve.
  - `verified_name_unavailable`: the trade needed a verified seller but
    RecoilPay couldn't show the name. Can't be approved.
  When your bank can't look account names up, the name the seller gave is
  compared instead.
- **New setting:** `policy.require_verified_name_match`, on unless set.
  Turning it off is for sandbox only: a production config with it off is
  refused.
- **Personal data:** the verified name is handled like the account holder's
  name: zeroed when dropped, never logged, alerted, audited or stored.
- Trades without a verified name are paid exactly as before.

## 0.1.0

First release.
