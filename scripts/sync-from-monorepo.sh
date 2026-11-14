#!/usr/bin/env bash
# Keep this repo's copies in step with RecoilPay's (recoil-labs/recoil-pay-v2):
#   vectors/*.json            ← test-vectors/*.json (the source of truth)
#   crates/agent-core/tests/fixtures/{AttestedEscrow,MockToken}.json
#                             ← attested-escrow, built with forge (ABI and
#                               creation bytecode only)
#
#   scripts/sync-from-monorepo.sh ../recoil-pay-v2           # copy
#   scripts/sync-from-monorepo.sh --check ../recoil-pay-v2   # fail on any difference (CI)
#
# --check compares the vectors only (CI checks out test-vectors/ and nothing
# else). A changed vector means the aggregator, the TypeScript kit or this
# agent changed a shared rule: copy, run `cargo test`, and fix whichever
# side is wrong before merging.
set -euo pipefail

check=0
if [ "${1:-}" = "--check" ]; then check=1; shift; fi
repo="${1:?usage: $0 [--check] path/to/recoil-pay-v2}"
here="$(cd "$(dirname "$0")/.." && pwd)"
vectors=(fiat-payee fiat-lock fiat-pricing merchant-agent-api)

[ -d "$repo/test-vectors" ] || { echo "$repo has no test-vectors/" >&2; exit 1; }

if [ $check = 1 ]; then
  status=0
  for v in "${vectors[@]}"; do
    if ! cmp -s "$repo/test-vectors/$v.json" "$here/vectors/$v.json"; then
      echo "vectors/$v.json differs from recoil-pay-v2 test-vectors/$v.json"
      status=1
    fi
  done
  [ $status = 0 ] && echo "vectors match recoil-pay-v2 ($(git -C "$repo" rev-parse --short=12 HEAD 2>/dev/null || echo '?'))"
  exit $status
fi

for v in "${vectors[@]}"; do cp "$repo/test-vectors/$v.json" "$here/vectors/$v.json"; done
echo "copied ${#vectors[@]} vectors"

if command -v forge >/dev/null; then
  (cd "$repo/attested-escrow" && forge build >/dev/null)
  for pair in AttestedEscrow.sol/AttestedEscrow Tokens.sol/MockToken; do
    name="${pair#*/}"
    python3 - "$repo/attested-escrow/out/$pair.json" "$name" \
      "$here/crates/agent-core/tests/fixtures/$name.json" <<'PY'
import json, sys
src, name, dst = sys.argv[1:]
d = json.load(open(src))
about = (f"{name}: ABI and creation bytecode only, compiled from recoil-labs/recoil-pay-v2 "
         "attested-escrow/ for the on-chain lock tests. Regenerate with scripts/sync-from-monorepo.sh.")
json.dump({"_about": about, "abi": d["abi"], "bytecode": {"object": d["bytecode"]["object"]}},
          open(dst, "w"), indent=1)
PY
  done
  echo "rebuilt the escrow fixtures"
else
  echo "forge is not installed: the escrow fixtures were not refreshed" >&2
fi
echo "recoil-pay-v2 at $(git -C "$repo" rev-parse --short=12 HEAD 2>/dev/null || echo '?'). Now run cargo test."
