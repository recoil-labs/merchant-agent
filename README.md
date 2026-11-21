# RecoilPay Merchant Agent

The service a licensed fiat merchant runs on their own infrastructure to pay
RecoilPay naira cash-outs automatically, through their own bank. RecoilPay
never sees bank details and never holds the merchant's bank credentials.

- **What it does, and how to run it:** [`docs/README.md`](docs/README.md)
- **For your security team:** [`docs/security.md`](docs/security.md) and the
  [design note](docs/design.md)
- **Deploying it** (Docker Compose or Kubernetes): [`deploy/README.md`](deploy/README.md)
- **Connecting your own bank or payout API:** [`docs/adapters.md`](docs/adapters.md)
- **When something needs a person:** [`docs/runbooks/`](docs/runbooks/)

Release images are signed and published to
`ghcr.io/recoil-labs/merchant-agent`; see
[upgrading](docs/runbooks/upgrading.md) for how to verify one.

## Developing

```bash
cargo test --all-features                   # unit, crash, policy and console tests
MERCHANT_AGENT_REQUIRE_ANVIL=1 cargo test -p merchant-agent-core --test chain   # needs Foundry's anvil
examples/adapter/test.sh                    # the example adapter passes the conformance suite
deploy/test/smoke.sh                        # build the image and run it with Docker Compose
```

`vectors/` holds copies of RecoilPay's shared test vectors, which pin the
agent to the RecoilPay aggregator and TypeScript kit byte for byte. Don't edit
them here; see [`vectors/README.md`](vectors/README.md).

## Licence

The Merchant Agent is **source-available, not open source.** The code is
public so you and your security team can check exactly what runs on your
infrastructure. You may:

- **read and audit** the code, and build and test it;
- **run it**, including in production, but only to connect to RecoilPay as a
  merchant on your own RecoilPay account;
- **configure it**, and write your own payout adapter against
  [the adapter protocol](docs/adapters.md), starting from
  [`examples/adapter`](examples/adapter) if you like.

You may not modify or fork it, share or sell copies, use it with any other
platform, or run it for other merchants. Your rights end when your RecoilPay
account ends.

This is a summary. The [`LICENSE`](LICENSE) file is what governs. For any other
use, contact Recoil Labs.

Copyright © 2026 Recoil Labs. All rights reserved.
