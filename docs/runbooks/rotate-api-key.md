# Rotating the API key

Rotate it on a schedule, when someone who had it leaves, or at once if you
think it leaked. It can do everything your operator account can.

> **Not possible yet.** RecoilPay can't replace an operator's API key today:
> the portal's rotate button makes a new key but doesn't save it, so the new
> key is refused and the **old key keeps working**. Re-registering keeps the
> old key too. Until RecoilPay fixes this, if your key may have leaked:
> pause (`merchant-agent pause --by YOU`), tell RecoilPay at once, and follow
> the steps below once they've issued you a working new key. The steps are
> what to do when rotation works.

## What you see

Nothing, until you rotate. RecoilPay invalidates the old key **the moment**
a new one is made. From then until the agent has the new key, the agent gets
"refused the API key" errors and `/readyz` answers 503.

## What the agent already did

Nothing: rotation is yours to do. When it starts with a different key, it
records `secret_rotated` (`api_key`) in the audit log. It stores a keyed
fingerprint, never the key.

## What to do

1. Pause, so no new trades arrive while the key changes:
   `merchant-agent pause --by YOU`.
2. Check nothing is half-done: `merchant-agent status` shows no trade in
   *Paying*. Wait a moment if one is.
3. In the RecoilPay portal: **Settings** → rotate the API key. Copy the new
   key. It's shown once.
4. Put it where the agent reads it (`aggregator.api_key` in your config):
   - a file: write it, then `chown 10001:10001` (or the agent's user) and
     `chmod 0400`;
   - GCP or AWS Secret Manager: add a new version;
   - sandbox `.env`: replace `MERCHANT_AGENT_API_KEY`.
5. Restart the agent: `docker compose up -d` (it re-reads `.env`), or
   `docker compose restart agent`, or
   `kubectl -n merchant-agent rollout restart statefulset/merchant-agent`.
   In Kubernetes, update the Secret first:
   `kubectl -n merchant-agent create secret generic merchant-agent-secrets --from-literal=api-key=… --from-literal=payee-key=… --from-literal=payee-tag-pepper=… --dry-run=client -o yaml | kubectl apply -f -`.
6. Run `merchant-agent doctor`, then resume: **Resume payouts**, or
   `merchant-agent resume --by YOU`.

If the old key leaked: also check the audit log and RecoilPay's trade
history for anything you didn't do, such as a report you didn't make, offers
you didn't publish, or a changed wallet.

## How to know it's fixed

`doctor` passes **Aggregator reachable and API key accepted**, `/readyz`
answers 200, and **Audit** shows `secret_rotated` with detail `api_key`.
