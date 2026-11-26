# Merchant-signed purchases

From v0.3. Off unless you turn it on.

When a buyer purchases USDC from your offer, the USDC has to be locked in
the escrow before the buyer is shown where to pay. By default RecoilPay locks
it from your **fill wallet**, whose key RecoilPay holds. With merchant-signed
purchases, the agent locks it from **your own float wallet** instead: for each
purchase it signs a Permit2 lock for exactly that trade, and RecoilPay's fill
wallet only relays it and pays the gas. RecoilPay never holds your sale USDC.

Licensed merchants are offered this mode only.

## What the agent checks before it signs

RecoilPay asks; the agent decides. It signs a purchase only when all of these
hold, and refuses it otherwise:

- **The amount is what your own offer gives.** The agent prices the buyer's
  naira on the terms it recorded when it first saw your purchase offer, the
  same way RecoilPay does (`vectors/fiat-pricing.json`, `onrampCases`). One
  base unit more is refused. A quote it never saw is refused.
- **The rate is inside your reference band** (`pricing.reference_rates`,
  `policy.max_rate_deviation_bps`), as for cash-outs.
- **Your limits:** `purchases.max_per_trade_minor` and
  `purchases.max_per_day_minor` (naira, Lagos day).
- **Your float stays above its floor** (`purchases.float_floor`, per chain)
  and Permit2 may move it.
- **The buyer is not on your deny list.** Add buyer wallets (`0x…`) to the
  same file as payee tags (`screening.deny_list`).
- **Everything that decides where USDC goes is pinned in your config:** the
  chain, the escrow (Permit2's spender) and the USDC. The nonce is the
  trade's own, so a signature can fund only its own trade, and the deadline
  is the end of the signing window, never more than
  `purchases.max_deadline_secs` away.

The agent builds the lock from those values and signs that. RecoilPay's typed
data must say exactly the same, field by field.

A refusal is recorded with its reason, written to the audit log
(`purchase_refused`) and alerted. Nothing is signed, so none of your USDC
moves; the buyer is told you didn't confirm.

## Turning it on

1. **Make a float wallet** with its own key: never the payee key, never your
   fill wallet. An ordinary EOA (MetaMask, a hardware wallet, a Circle
   developer-controlled EOA wallet). Smart-contract wallets are not supported
   yet.
2. **Fund it** with USDC on each chain, and **approve Permit2** to spend that
   USDC once, from the float wallet: spender
   `0x000000000022D473030F116dDEE9F6B43aC78BA3`.
3. **Register it** in the portal (Fiat offers → Purchases): pick
   Merchant-signed for the chain and sign the registration message with the
   float wallet. Or `PUT /solver-api/operators/{id}/purchase-funding`.
4. **Configure the agent:**

   ```yaml
   purchases:
     enabled: true
     signer: gcp-sm://projects/acme/secrets/float-wallet-key/versions/1   # its own secret
     max_per_trade_minor: "50000000"     # ₦500,000
     max_per_day_minor: "500000000"      # ₦5,000,000
     float_floor:
       "eip155:8453": "100000000"        # never below 100 USDC
     # poll_secs: 2                      # how often to ask RecoilPay
     # max_deadline_secs: 300
   ```

5. **Run `merchant-agent doctor`.** It checks the key, that RecoilPay has the
   same float wallet registered, and on each chain that Permit2 is approved
   and the balance is above the floor.

The agent records your purchase offers only while purchases are enabled, so
enable it before publishing them, or restart it after.

## Timing

RecoilPay gives you a signing window, 3 minutes by default. The agent asks
every 2 seconds and signs within a few, so the window is slack for a slow
poll, an RPC error or a restart. A purchase still unsigned at the end of it
fails: nothing moved. Three in a row pause your purchases on that chain until
you save your purchase funding again in the portal.

## If the agent crashes

A signature is stored before it is posted, and posted again when the agent
starts. A purchase is never signed twice, and the nonce is the trade's own,
so posting the same signature again can lock nothing more.

## In the console

The dashboard shows purchases signed and refused, and the median time to
sign. `merchant-agent audit export` lists each one.
