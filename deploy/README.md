# Deploying the Merchant Agent

The agent is one container that must run **all the time, as exactly one
instance, with one persistent volume**. Two kits are here:

- [`docker-compose.yml`](docker-compose.yml): one machine (a VM or a server).
  The simplest path, and the one this page starts with.
- [`k8s/`](k8s/): Kubernetes, as a single-replica `StatefulSet`.

Both run the same image: distroless (no shell), as UID 10001, with a read-only
root filesystem. The agent writes only to `/var/lib/merchant-agent`.

## Quick start: a sandbox agent with Docker Compose

Sandbox means Base Sepolia testnet and the simulated bank. **No real money
moves.** You need Docker with Compose, and about five minutes.

**1. Get your operator details from RecoilPay.** In the portal
([solver.recoilpay.com](https://solver.recoilpay.com)), register as an
operator. Note:

- your **operator id** (`solver-…`) and the **wallet** you registered with;
- your **API key**, shown once when you register;
- your **payee key**: Payouts → **Export payee key**. It opens your customers'
  sealed bank details; treat it like a password.

**2. Fill in `.env`.** From this directory (`deploy/`):

```bash
cp .env.example .env
```

Open `.env` and fill in those four values. For the last one, paste the output
of `openssl rand -hex 32`. `.env` holds secrets: never commit or share it.

**3. Get the image, then write the config.**

```bash
docker compose build        # a few minutes, once
mkdir -p config
docker compose run --rm -T init > config/merchant-agent.yaml
```

The agent writes the config itself, from your `.env`. Once releases are
published, `docker compose pull` fetches the image instead of building it.

**4. Start it.**

```bash
docker compose up -d
```

**5. Check it.**

```bash
docker compose ps                               # STATUS shows (healthy)
curl -s http://127.0.0.1:8741/readyz            # {"ready":true,...}
docker compose exec agent merchant-agent doctor # every check, in plain English
```

Ready means: the agent holds its lease, its database takes writes, RecoilPay
and the RPCs answer, and payouts aren't paused.

**6. Open the console.** Add yourself as a user, then browse to
<http://127.0.0.1:8740>:

```bash
docker compose exec agent merchant-agent users add YOUR_NAME --role admin
```

It asks for a password and shows an authenticator-app secret. The console
listens only on this machine. To reach it from another computer, use an SSH
tunnel (`ssh -L 8740:127.0.0.1:8740 you@server`); never publish the port.

### Everyday commands

```bash
docker compose logs -f agent                        # follow the log
docker compose exec agent merchant-agent status     # trades and the exception queue
docker compose exec agent merchant-agent pause --by YOUR_NAME   # the kill switch
docker compose exec agent merchant-agent resume --by YOUR_NAME
docker compose exec agent merchant-agent audit verify
docker compose down                                 # stop (the data volume stays)
```

The database and the audit log live in the `merchant-agent_data` volume.
**Back it up.** It is the record of every payout the agent made.

## Kubernetes

[`k8s/`](k8s/) is a kustomization: a `StatefulSet` with `replicas: 1` and a
1 GiB volume, a headless `Service` for the health port, and a
`NetworkPolicy`. The namespace enforces the `restricted` Pod Security level.

**1. Write the config**, reading secrets from files:

```bash
docker run --rm ghcr.io/recoil-labs/merchant-agent:latest init --yes --stdout \
  --bind-all --secrets-dir /run/merchant-agent/secrets \
  --solver-id solver-… --wallet 0x… --aggregator-url https://api.recoilpay.com \
  --data-dir /var/lib/merchant-agent > k8s/merchant-agent.yaml
```

**2. Create the Secret** with your three secrets:

```bash
kubectl create namespace merchant-agent
kubectl -n merchant-agent create secret generic merchant-agent-secrets \
  --from-literal=api-key='…' \
  --from-literal=payee-key='0x…' \
  --from-literal=payee-tag-pepper="$(openssl rand -hex 32)"
```

The agent reads a secret file only if its own user owns it and nobody else
can read it. Kubernetes mounts Secrets owned by root, so an init container
copies them into memory as UID 10001, mode 0400.

**3. Apply it, and check it.**

```bash
kubectl apply -k k8s
kubectl -n merchant-agent rollout status statefulset/merchant-agent
kubectl -n merchant-agent exec merchant-agent-0 -- merchant-agent doctor
kubectl -n merchant-agent port-forward pod/merchant-agent-0 8740   # the console
```

Prometheus finds `/metrics` on port 8741 through the pod's
`prometheus.io/*` annotations.

**About the NetworkPolicy.** It lets the agent out only to DNS and to HTTPS on
public addresses, and in only from a namespace named `monitoring`, on the
health port. Rename that namespace in `k8s/networkpolicy.yaml` if yours
differs. A NetworkPolicy matches addresses, not host names. To allow exactly
your aggregator, your RPCs and your bank (`api.paystack.co`), use your
network plugin's FQDN policies or an egress proxy. It takes effect only with
a network plugin that enforces NetworkPolicy.

## Why not Cloud Run, Lambda or other scale-to-zero platforms

Not in v1. The agent:

- polls RecoilPay and its bank all the time, so it must run when nobody is
  calling it;
- keeps its state in a local SQLite database with a lease and a file lock,
  which needs **one persistent volume**, never shared between instances;
- must never run as two copies at once.

Scale-to-zero platforms stop idle instances, start extra ones under load, and
give each a throwaway disk: all three break those rules. Use a VM with
Compose, or Kubernetes.

## Going to production

The sandbox config is a starting point. Before real money:

1. In the config: `mode: production`, RecoilPay's production `aggregator.url`,
   your production chain and RPCs, your real `limits`, and a real
   `rail.kind` (`paystack`, `manual`, or your own). `doctor` explains anything
   it refuses.
2. **Secrets.** Production refuses environment variables. Use files, or a
   cloud secret manager (`gcp-sm://`, `aws-sm://`; the image includes both):
   - With Compose, write each secret to a file, then
     `sudo chown 10001:10001 secrets/* && sudo chmod 0400 secrets/*`. Mount the
     directory read-only (`- ./secrets:/run/merchant-agent/secrets:ro`),
     remove `env_file`, and write the config with
     `--secrets-dir /run/merchant-agent/secrets`.
   - With Kubernetes, the kit already uses files.
3. Run `doctor` until it shows no failures, then start the agent.

## Upgrades

Change the image tag, then restart: `docker compose pull && docker compose up -d`,
or update the tag in `k8s/kustomization.yaml` and `kubectl apply -k k8s`.

On start, the agent migrates its database forward, if the new version needs
to. It first saves a copy beside it in the volume, named
`agent.db.backup-v<old schema>-<time>`. Migrations only go forward. To go
back, stop the agent, put the backup in place of `agent.db`, and run the
older image.

`merchant-agent version` prints the version, git commit, database schema and
config schema:

```bash
docker compose exec agent merchant-agent version
```

## Testing the kits

`test/smoke.sh` and `test/k8s-smoke.sh` build the image, register a
throwaway operator with a local aggregator, and follow these steps (Compose,
or a `kind` cluster). They pass when the agent is ready and `doctor` shows no
failures. CI runs both.
