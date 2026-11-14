#!/usr/bin/env bash
# Smoke test of the Kubernetes kit on a throwaway kind cluster: load the
# image, make a throwaway operator on a local aggregator, apply
# deploy/k8s with a generated config and Secret, wait for the pod to be
# Ready (the readiness probe is /readyz), and run `doctor` inside it.
#
#   deploy/test/k8s-smoke.sh                                   # stub aggregator
#   RECOILPAY_REPO=../recoil-pay-v2 deploy/test/k8s-smoke.sh  # the real one
#
# Needs docker, kind, kubectl, node, cast, curl, openssl, python3 (and cargo
# with RECOILPAY_REPO). Uses the
# image merchant-agent:smoke, which it builds.
# kind's network plugin enforces NetworkPolicy: the kit's policy allows
# only HTTPS to public addresses, so the test adds one more policy letting
# the pod reach the local test aggregator (policies add up).
set -euo pipefail

here="$(cd "$(dirname "$0")" && pwd)"
agent_dir="$here/../.."
port="${AGG_PORT:-3999}"
image="${MERCHANT_AGENT_IMAGE:-merchant-agent:smoke}"
cluster="merchant-agent-smoke"
work="$(mktemp -d "${TMPDIR:-/tmp}/merchant-agent-k8s.XXXXXX")"
started=$(date +%s)
agg_pid=""
ctx="kind-$cluster"
k() { kubectl --context "$ctx" -n merchant-agent "$@"; }

cleanup() {
  status=$?
  if [ $status -ne 0 ]; then
    k describe pod merchant-agent-0 2>/dev/null | tail -25 || true
    k logs merchant-agent-0 -c agent 2>/dev/null | tail -30 || true
  fi
  kind delete cluster --name "$cluster" >/dev/null 2>&1 || true
  docker rm -f "$cluster-relay" >/dev/null 2>&1 || true
  [ -n "$agg_pid" ] && kill "$agg_pid" 2>/dev/null || true
  exit $status
}
trap cleanup EXIT

# Always the current code (the build cache makes a rebuild quick).
docker build -q -t "$image" --build-arg GIT_COMMIT="$(git -C "$agent_dir" rev-parse --short=12 HEAD 2>/dev/null || echo unknown)" "$agent_dir" >/dev/null

echo "[1] a kind cluster"
kind create cluster --name "$cluster" --wait 120s >/dev/null
kind load docker-image "$image" --name "$cluster" >/dev/null
kind load docker-image busybox:1.37.0 --name "$cluster" >/dev/null 2>&1 || true

echo "[2] a local aggregator and a throwaway operator"
source "$here/aggregator.sh"
# Pods can't count on reaching the host by name; a relay on kind's docker
# network forwards to the aggregator on the host.
docker rm -f "$cluster-relay" >/dev/null 2>&1 || true
docker run -d --name "$cluster-relay" --network kind --add-host host.docker.internal:host-gateway \
  alpine/socat:1.8.0.3@sha256:beb4a68d9e4fe6b0f21ea774a0fde6c31f580dde6368939ed70100c5385b015e \
  "TCP-LISTEN:$port,fork,reuseaddr" "TCP:host.docker.internal:$port" >/dev/null
relay="$(docker inspect --format '{{(index .NetworkSettings.Networks "kind").IPAddress}}' "$cluster-relay")"
echo "    the aggregator, as pods see it: http://$relay:$port"

echo "[3] the README's steps: config, Secret, apply"
cp -R "$here/../k8s" "$work/k8s"
docker run --rm "$image" init --yes --stdout --public-health \
  --secrets-dir /run/merchant-agent/secrets \
  --solver-id "$solver" --wallet "$op_addr" \
  --aggregator-url "http://$relay:$port" \
  --data-dir /var/lib/merchant-agent >"$work/k8s/merchant-agent.yaml"
# The locally built image instead of the published one.
python3 - "$work/k8s/kustomization.yaml" "$image" <<'PY'
import sys
p, image = sys.argv[1], sys.argv[2]
name, tag = image.rsplit(":", 1)
s = open(p).read().replace("newName: ghcr.io/recoil-labs/merchant-agent", f"newName: {name}").replace("newTag: latest", f"newTag: {tag}")
open(p, "w").write(s)
PY
kubectl --context "$ctx" apply -f "$work/k8s/namespace.yaml" >/dev/null
k create secret generic merchant-agent-secrets \
  --from-literal=api-key="$api_key" \
  --from-literal=payee-key="$payee_key" \
  --from-literal=payee-tag-pepper="$(openssl rand -hex 32)" >/dev/null
kubectl --context "$ctx" apply -k "$work/k8s"
# Test only: the local aggregator is on a private address and port.
cat <<YAML | kubectl --context "$ctx" apply -f - >/dev/null
apiVersion: networking.k8s.io/v1
kind: NetworkPolicy
metadata: {name: smoke-test-aggregator, namespace: merchant-agent}
spec:
  podSelector: {matchLabels: {app: merchant-agent}}
  policyTypes: [Egress]
  egress:
    - to: [{ipBlock: {cidr: "$relay/32"}}]
      ports: [{protocol: TCP, port: $port}]
YAML

echo "[4] wait for Ready (the readiness probe is /readyz)"
k rollout status statefulset/merchant-agent --timeout=240s
k get pod merchant-agent-0 -o jsonpath='{.status.conditions[?(@.type=="Ready")].status}{"\n"}'

echo "[5] doctor, inside the pod"
k exec merchant-agent-0 -c agent -- merchant-agent doctor

echo "[6] the pod's guarantees"
k get pod merchant-agent-0 -o jsonpath='runAsUser={.spec.securityContext.runAsUser} readOnlyRootFilesystem={.spec.containers[0].securityContext.readOnlyRootFilesystem} drop={.spec.containers[0].securityContext.capabilities.drop}{"\n"}'
k exec merchant-agent-0 -c agent -- merchant-agent healthcheck

echo "OK: Ready on Kubernetes in $(( $(date +%s) - started ))s"
