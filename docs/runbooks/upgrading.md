# Upgrading

## What you see

A new release: a new image tag (`ghcr.io/recoil-labs/merchant-agent:<version>`)
with release notes.

## What the agent already did

Nothing until you upgrade. When the new version starts:

- if its database layout changed, it **first saves a copy** beside the
  database, `agent.db.backup-v<old schema>-<time>`, then migrates forward;
- migrations only go forward: an older version refuses a newer database
  ("the database is from a newer agent").

## What to do

1. Read the release notes for config changes.
2. Check nothing is half-done: `merchant-agent status` shows no trade in
   *Paying*. Pause if you want a quiet moment:
   `merchant-agent pause --by YOU`.
3. Upgrade:
   - Compose: set `MERCHANT_AGENT_IMAGE=ghcr.io/recoil-labs/merchant-agent:<version>`
     in `.env`, then `docker compose pull && docker compose up -d`.
   - Kubernetes: set `newTag` in `k8s/kustomization.yaml`, then
     `kubectl apply -k k8s`.
4. Check: `merchant-agent version` (the version, git commit, schema), then
   `merchant-agent doctor`.
5. Resume if you paused.

Verify a release image's signature before using it, then deploy the exact
image you verified, by its digest:

```bash
# 1. The digest the version tag points at now.
digest=$(docker buildx imagetools inspect ghcr.io/recoil-labs/merchant-agent:<version> \
  --format '{{json .Manifest.Digest}}' | tr -d '"')

# 2. Signed by this repository's release workflow, for exactly that tag, on
#    a tag push. Anything else (another workflow, a branch, a pull request)
#    fails.
cosign verify "ghcr.io/recoil-labs/merchant-agent@$digest" \
  --certificate-identity "https://github.com/recoil-labs/merchant-agent/.github/workflows/ci.yml@refs/tags/v<version>" \
  --certificate-oidc-issuer https://token.actions.githubusercontent.com \
  --certificate-github-workflow-trigger push
```

Then use `ghcr.io/recoil-labs/merchant-agent@$digest` (not the tag) as
`MERCHANT_AGENT_IMAGE`, or as `digest:` in `k8s/kustomization.yaml`, so what
runs is what you verified.

### Going back

1. Stop the agent.
2. If the upgrade migrated the database, restore the copy it made, in the
   data volume:
   ```bash
   docker run --rm -v merchant-agent_data:/data busybox sh -c \
     'cd /data && rm -f agent.db-wal agent.db-shm && cp agent.db.backup-vN-TIME agent.db && chown 10001:10001 agent.db'
   ```
   Remove the `-wal` and `-shm` files first, as above: they belong to the
   newer database.
3. Start the older image. Anything the newer version did after the upgrade
   is not in the copy: follow step 4 of
   [Restoring from a backup](restore-from-backup.md).

## How to know it's fixed

`merchant-agent version` shows the new version, `doctor` has no failures,
and `/readyz` answers 200.
