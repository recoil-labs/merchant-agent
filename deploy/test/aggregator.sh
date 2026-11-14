# Sourced by smoke.sh and k8s-smoke.sh: start an aggregator on $port and
# make a throwaway operator. Sets agg_pid, solver, api_key, op_addr,
# payee_key and payee_pub. Expects $work and $port.
#
# By default it starts the stub (stub-aggregator.mjs), which needs only
# node. With RECOILPAY_REPO pointing at a checkout of recoil-pay-v2, it
# builds and starts the real aggregator (in-memory) and registers the
# operator with it, as RecoilPay's own CI does.

op_key="0x$(openssl rand -hex 32)"
op_addr="$(cast wallet address --private-key "$op_key")"
payee_key="0x$(openssl rand -hex 32)"
payee_pub="0x04$(cast wallet public-key --raw-private-key "$payee_key" | sed 's/^0x//')"

wait_for_aggregator() {
  for _ in $(seq 1 60); do curl -sf "http://127.0.0.1:$port/health" >/dev/null && return 0; sleep 1; done
  echo "the aggregator did not start; see $work/aggregator.log" >&2
  return 1
}

if [ -z "${RECOILPAY_REPO:-}" ]; then
  solver="solver-$(echo "${op_addr#0x}" | tr A-F a-f)"
  api_key="$(openssl rand -hex 24)"
  STUB_PORT="$port" STUB_SOLVER_ID="$solver" STUB_API_KEY="$api_key" STUB_PAYEE_PUB="$payee_pub" \
    node "$here/stub-aggregator.mjs" >"$work/aggregator.log" 2>&1 &
  agg_pid=$!
  wait_for_aggregator
  echo "    stub aggregator, operator $solver"
else
  agg_dir="$RECOILPAY_REPO/recoil-core/aggregator"
  if [ -z "${AGG_BIN:-}" ]; then
    (cd "$agg_dir" && cargo build --quiet --bin oif-aggregator)
    AGG_BIN="$agg_dir/target/debug/oif-aggregator"
  fi
  (
    cd "$agg_dir"
    export PORT="$port" HOST=0.0.0.0 ALLOW_LOOPBACK_SOLVERS=1
    export INTEGRITY_SECRET="$(openssl rand -hex 32)"
    export FILL_WALLET_ENCRYPTION_KEY="$(openssl rand -base64 32)"
    export ADMIN_API_KEYS="$(openssl rand -hex 24)"
    unset DATABASE_URL
    exec "$AGG_BIN"
  ) >"$work/aggregator.log" 2>&1 &
  agg_pid=$!
  wait_for_aggregator
  msg="$(curl -sf "http://127.0.0.1:$port/api/v1/solver/register/message" | python3 -c 'import sys,json;print(json.load(sys.stdin)["data"]["message"])')"
  sig="$(cast wallet sign --private-key "$op_key" "$msg")"
  reg="$(python3 -c 'import json,sys;print(json.dumps({"address":sys.argv[1],"message":sys.argv[2],"signature":sys.argv[3],"endpoint":"http://127.0.0.1:4099"}))' "$op_addr" "$msg" "$sig" \
    | curl -sf -X POST -H 'content-type: application/json' --data @- "http://127.0.0.1:$port/solver-api/account/register")"
  solver="$(echo "$reg" | python3 -c 'import json,sys;print(json.load(sys.stdin)["solverId"])')"
  api_key="$(echo "$reg" | python3 -c 'import json,sys;print(json.load(sys.stdin)["apiKey"])')"
  curl -sf -o /dev/null -X PUT -H "x-api-key: $api_key" -H 'content-type: application/json' \
    --data "{\"publicKey\":\"$payee_pub\"}" "http://127.0.0.1:$port/solver-api/operators/$solver/fiat-payee-key"
  echo "    real aggregator, operator $solver"
fi
