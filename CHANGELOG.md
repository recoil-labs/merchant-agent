# Changelog

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
