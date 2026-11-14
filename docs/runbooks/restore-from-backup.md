# Restoring from a backup

## What to back up

The agent's data volume: `agent.db` (and its `-wal` file), `audit.jsonl`,
and any `agent.db.backup-*` files. Take backups with the agent **stopped**,
or as an atomic snapshot of the whole volume (a cloud disk snapshot).

With Docker Compose:

```bash
docker compose stop agent
docker run --rm -v merchant-agent_data:/data -v "$PWD":/backup busybox \
  tar czf /backup/merchant-agent-$(date +%F).tgz -C /data .
docker compose start agent
```

In Kubernetes, snapshot the `data-merchant-agent-0` volume with your storage's
`VolumeSnapshot`, or scale to 0 and copy it.

## What you see

You're restoring because the volume was lost or damaged: the agent won't
start ("the database …"), or `merchant-agent audit verify` fails.

## What the agent already did

Nothing: restoring is yours to do. Two things matter.

- **A backup forgets what happened after it.** The agent won't know about
  payouts it made after the backup was taken.
- Through Paystack, or an adapter that says it's idempotent, that's safe: a
  forgotten trade is paid under the same key, and the bank returns the payout
  it already made. Through the **manual** rail, or an adapter that isn't
  idempotent, it is **not** safe: check before resuming (step 4). (In
  sandbox, the simulated bank's ledger lives on the same volume, so it
  forgets too; its money is pretend.)

## What to do

1. Stop the agent: `docker compose stop agent`, or scale the StatefulSet to 0.
2. Put the backup in place:
   ```bash
   docker run --rm -v merchant-agent_data:/data -v "$PWD":/backup busybox \
     sh -c 'rm -rf /data/* && tar xzf /backup/merchant-agent-YYYY-MM-DD.tgz -C /data'
   ```
3. **Pause before it starts**: `docker compose run --rm agent pause --by YOU`.
   (`pause` works with the agent stopped.)
4. Start it, then check:
   - `merchant-agent audit verify` passes;
   - `merchant-agent status` and **Trades**: for every trade RecoilPay shows
     as awaiting your payment, look in your bank for a transfer under the
     key `rpma-<trade id>` made after the backup. Mark any you find as paid
     (**Paid outside the agent**, with the bank's reference) before resuming.
5. Resume: `merchant-agent resume --by YOU`.

## How to know it's fixed

The agent is ready, `audit verify` passes, every trade awaiting payment has
been checked against the bank, and new trades are paid once.
