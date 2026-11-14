#!/usr/bin/env bash
# Smoke test of the image and the compose kit, as the README runs them:
# build the image, make a throwaway operator on a local aggregator,
# write the config with the `init` service, `docker compose up`, wait for
# the container to be healthy and /readyz to pass, then run `doctor` inside.
#
#   deploy/test/smoke.sh                                   # against the stub aggregator
#   RECOILPAY_REPO=../recoil-pay-v2 deploy/test/smoke.sh  # against the real one
#
# Needs docker (with compose), node, cast (foundry), curl, openssl, python3;
# cargo too with RECOILPAY_REPO.
# Everything it makes is a throwaway; no secrets are read.
set -euo pipefail

here="$(cd "$(dirname "$0")" && pwd)"
agent_dir="$here/../.."
port="${AGG_PORT:-3999}"
image="${MERCHANT_AGENT_IMAGE:-merchant-agent:smoke}"
work="$(mktemp -d "${TMPDIR:-/tmp}/merchant-agent-smoke.XXXXXX")"
started=$(date +%s)
agg_pid=""

cleanup() {
  status=$?
  if [ -f "$work/docker-compose.yml" ]; then
    if [ $status -ne 0 ]; then
      echo "--- agent logs ---"
      (cd "$work" && docker compose logs --no-color agent 2>/dev/null | tail -40) || true
    fi
    (cd "$work" && docker compose down -v --remove-orphans >/dev/null 2>&1) || true
  fi
  [ -n "$agg_pid" ] && kill "$agg_pid" 2>/dev/null || true
  [ $status -ne 0 ] && echo "aggregator log: $work/aggregator.log"
  exit $status
}
trap cleanup EXIT

echo "[1] build the image"
docker build -q -t "$image" --build-arg GIT_COMMIT="$(git -C "$agent_dir" rev-parse --short=12 HEAD 2>/dev/null || echo unknown)" "$agent_dir" >/dev/null
docker run --rm "$image" version

echo "[2] a local aggregator and a throwaway operator"
source "$here/aggregator.sh"

echo "[3] the README's steps: .env, init, compose up"
cp "$here/../docker-compose.yml" "$work/"
# The aggregator runs on the host; containers reach it as host.docker.internal.
cat >"$work/docker-compose.override.yml" <<YAML
services:
  agent:
    extra_hosts: ["host.docker.internal:host-gateway"]
YAML
cat >"$work/.env" <<ENV
MERCHANT_AGENT_IMAGE=$image
MERCHANT_AGENT_SOLVER_ID=$solver
MERCHANT_AGENT_WALLET=$op_addr
MERCHANT_AGENT_API_KEY=$api_key
MERCHANT_AGENT_AGGREGATOR_URL=http://host.docker.internal:$port
MERCHANT_AGENT_PAYEE_KEY=$payee_key
MERCHANT_AGENT_PAYEE_TAG_PEPPER=$(openssl rand -hex 32)
ENV
cd "$work"
mkdir -p config
docker compose run --rm -T init >config/merchant-agent.yaml
docker compose up -d --no-build agent

echo "[4] wait for healthy and ready"
for _ in $(seq 1 90); do
  health="$(docker inspect --format '{{.State.Health.Status}}' "$(docker compose ps -q agent)" 2>/dev/null || echo starting)"
  ready="$(curl -s -o /dev/null -w '%{http_code}' http://127.0.0.1:8741/readyz || true)"
  [ "$health" = healthy ] && [ "$ready" = 200 ] && break
  sleep 2
done
echo "    container: $health, /readyz: $ready"
curl -s http://127.0.0.1:8741/readyz; echo
[ "$health" = healthy ] && [ "$ready" = 200 ] || { echo "not ready"; exit 1; }

echo "[5] doctor, inside the container"
docker compose exec -T agent merchant-agent doctor

echo "[6] the container's guarantees"
cid="$(docker compose ps -q agent)"
docker inspect --format 'user={{.Config.User}} read_only={{.HostConfig.ReadonlyRootfs}} cap_drop={{.HostConfig.CapDrop}}' "$cid"
if docker compose exec -T agent /bin/sh -c true >/dev/null 2>&1; then echo "the image has a shell"; exit 1; fi
echo "    no shell in the image"

echo "OK: a ready sandbox agent in $(( $(date +%s) - started ))s (image build included)"
