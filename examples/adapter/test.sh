#!/usr/bin/env bash
# Runs `merchant-agent adapter-test` against this example adapter: the
# conformance suite every payout adapter must pass. Copy it for your own
# adapter: start yours in test mode instead of server.mjs.
#
#   examples/adapter/test.sh   (needs node 20+, cargo)
set -euo pipefail
here="$(cd "$(dirname "$0")" && pwd)"
agent_dir="$here/../.."
work="$(mktemp -d "${TMPDIR:-/tmp}/adapter-test.XXXXXX")"
port="${ADAPTER_PORT:-8750}"
pid=""
trap '[ -n "$pid" ] && kill "$pid" 2>/dev/null || true' EXIT

(cd "$agent_dir" && cargo build --quiet --bin merchant-agent)
agent="$agent_dir/target/debug/merchant-agent"

export ADAPTER_TOKEN="$(openssl rand -hex 32)"
PORT="$port" ADAPTER_LEDGER="$work/ledger.json" node "$here/server.mjs" >"$work/adapter.log" 2>&1 &
pid=$!
for _ in $(seq 1 30); do curl -s -o /dev/null "http://127.0.0.1:$port/v1/info" && break; sleep 0.2; done

# A sandbox config whose rail is this adapter.
"$agent" init --yes --stdout --solver-id solver-1a2b3c4d5e6f \
  --wallet 0x1111111111111111111111111111111111111111 --data-dir "$work/data" \
  | python3 -c '
import sys
s = sys.stdin.read()
old = "  kind: simulated\n"
assert old in s
sys.stdout.write(s.replace(old, "  kind: template\n  url: \"http://127.0.0.1:'"$port"'\"\n  credentials: env://ADAPTER_TOKEN\n", 1))
' >"$work/merchant-agent.yaml"

"$agent" --config "$work/merchant-agent.yaml" adapter-test --settle-secs 10
