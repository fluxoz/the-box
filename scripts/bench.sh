#!/usr/bin/env bash
# The bench: one pass over a machine before it ships to a customer.
#
# Run from the bench workstation against a freshly installed Box on the LAN
# (boot the unit from the installer USB with its orders first). The pass:
#
#   1. wait for the fresh unit's health
#   2. redeem its enrollment code for a bench session
#   3. deploy the tier's rated model (llama.cpp Vulkan) and wait for it
#   4. benchmark the exact unit: best-of-3 tokens/sec through /v1
#   5. PASS/FAIL against the rating; a FAIL never ships
#   6. mint the customer's key material and write the USB drop-ins
#
# The session token this script mints IS the customer's initial login,
# labeled so, and their first job is rotating it (console -> Devices ->
# revoke). After that nobody, including us, holds a key to their machine.
#
# Usage: bench.sh --host 192.168.1.77 --code <enrollment-code> --tier 128 \
#          [--rating 30] [--serial SN123] [--out ./bench-SN123]
# Tiers: 128 (gpt-oss-120b, rating 30), 64 (gpt-oss-20b, rating 40),
#        starter (no AI rating; server checks only).
set -euo pipefail

HOST="" CODE="" TIER="" RATING="" SERIAL="unit" OUT=""
while [ $# -gt 0 ]; do case "$1" in
  --host) HOST=$2; shift 2;; --code) CODE=$2; shift 2;;
  --tier) TIER=$2; shift 2;; --rating) RATING=$2; shift 2;;
  --serial) SERIAL=$2; shift 2;; --out) OUT=$2; shift 2;;
  *) echo "unknown arg $1" >&2; exit 1;;
esac; done
[ -n "$HOST" ] && [ -n "$CODE" ] && [ -n "$TIER" ] || {
  echo "need --host, --code, --tier (128|64|starter)" >&2; exit 1; }
case "$TIER" in
  128)     MODEL="ggml-org/gpt-oss-120b-GGUF"; MODEL_NAME="gpt-oss-120b"; RATING=${RATING:-30};;
  64)      MODEL="ggml-org/gpt-oss-20b-GGUF";  MODEL_NAME="gpt-oss-20b";  RATING=${RATING:-40};;
  starter) MODEL=""; MODEL_NAME="none"; RATING=0;;
  *) echo "tier must be 128|64|starter" >&2; exit 1;;
esac
OUT="${OUT:-./bench-$SERIAL}"; mkdir -p "$OUT"
BASE="http://$HOST:2693"
say() { printf '[bench] %s\n' "$*" | tee -a "$OUT/bench.log"; }

# 1. the fresh unit answers
say "waiting for $HOST"
for _ in $(seq 1 60); do
  curl -fsS -m 4 "$BASE/api/v1/health" >/dev/null 2>&1 && break; sleep 5
done
HEALTH=$(curl -fsS -m 5 "$BASE/api/v1/health")
say "health: $HEALTH"

# 2. bench session
TOKEN=$(curl -fsS -m 10 -X POST "$BASE/pair/redeem" \
  -H 'accept: application/json' -H 'content-type: application/json' \
  -d "{\"code\":\"$CODE\",\"label\":\"bench (rotate me on first login)\"}" \
  | python3 -c 'import json,sys; print(json.load(sys.stdin)["token"])')
say "bench session established"
mcp() { # tool json-args
  curl -fsS -m "${MCP_TIMEOUT:-120}" -X POST "$BASE/mcp" \
    -H "Authorization: Bearer $TOKEN" -H 'Content-Type: application/json' \
    -d "{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"tools/call\",\"params\":{\"name\":\"$1\",\"arguments\":$2}}"
}

TOKS="n/a"; VERDICT="PASS"
if [ -n "$MODEL" ]; then
  # 3. the rated engine, exact model this tier is sold on
  say "deploying llama.cpp with $MODEL_NAME"
  MCP_TIMEOUT=300 mcp deploy "{\"name\":\"llamacpp\",\"template\":\"llamacpp\",\"params\":{\"cmd\":[\"-hf\",\"$MODEL\",\"--host\",\"0.0.0.0\",\"--port\",\"8080\",\"--ctx-size\",\"16384\",\"--jinja\"]}}" \
    | grep -q '"isError": *false' || { say "deploy FAILED"; exit 1; }
  KEY=$(mcp ai_key_create '{"label":"owner"}' \
    | python3 -c 'import json,sys; t=json.load(sys.stdin)["result"]["content"][0]["text"]; print(json.loads(t)["key"])')

  # the model download is measured in tens of GB; poll long and honestly
  say "waiting for the model (large download; this is the slow part)"
  for _ in $(seq 1 360); do
    curl -fsS -m 5 -H "Authorization: Bearer $KEY" "$BASE/v1/models" 2>/dev/null \
      | grep -q '"id"' && break
    sleep 20
  done
  curl -fsS -m 5 -H "Authorization: Bearer $KEY" "$BASE/v1/models" | grep -q '"id"' \
    || { say "model never came up"; exit 1; }

  # 4. best-of-3 generation throughput, measured like a customer would see it
  say "benchmarking"
  BEST=0
  for run in 1 2 3; do
    T0=$(date +%s.%N)
    RESP=$(curl -fsS -m 600 -X POST "$BASE/v1/chat/completions" \
      -H "Authorization: Bearer $KEY" -H 'content-type: application/json' \
      -d '{"model":"default","max_tokens":256,"messages":[{"role":"user","content":"Write a detailed page about the history of home servers."}]}')
    T1=$(date +%s.%N)
    N=$(printf '%s' "$RESP" | python3 -c 'import json,sys; print(json.load(sys.stdin)["usage"]["completion_tokens"])')
    R=$(python3 -c "print(round($N/($T1-$T0),1))")
    say "run $run: $N tokens, $R tok/s"
    BEST=$(python3 -c "print(max($BEST,$R))")
  done
  TOKS=$BEST
  VERDICT=$(python3 -c "print('PASS' if $BEST >= $RATING else 'FAIL')")
else
  KEY=$(mcp ai_key_create '{"label":"owner"}' \
    | python3 -c 'import json,sys; t=json.load(sys.stdin)["result"]["content"][0]["text"]; print(json.loads(t)["key"])')
fi

# 5+6. the paper that rides in the box
python3 - "$OUT/receipt.json" "$SERIAL" "$TIER" "$MODEL_NAME" "$TOKS" "$RATING" "$VERDICT" <<'PY'
import json, sys, time
json.dump({"benched_at": int(time.time()), "serial": sys.argv[2], "tier": sys.argv[3],
           "model": sys.argv[4], "tokens_per_second": sys.argv[5],
           "rating": int(sys.argv[6]), "verdict": sys.argv[7]},
          open(sys.argv[1], "w"), indent=2)
PY
cat > "$OUT/customer-keys.txt" <<EOF
YOUR BOX
========
On your network, the console lives at:  http://box.local:2693

Initial login token (rotate this first):
  $TOKEN

Your AI endpoint key (any OpenAI-compatible app: base URL http://box.local:2693/v1):
  $KEY

FIRST LOGIN, TWO MINUTES:
  1. Open the console, paste the login token.
  2. Devices -> revoke "bench (rotate me on first login)" after adding
     your own device. After that, nobody but you holds a key,
     including us.
  3. AI keys can be re-minted there too, any time.

Benched: $MODEL_NAME at $TOKS tok/s (rating $RATING, $VERDICT)
EOF
say "verdict: $VERDICT ($MODEL_NAME at $TOKS tok/s vs rating $RATING)"
say "wrote $OUT/receipt.json and $OUT/customer-keys.txt"
[ "$VERDICT" = "PASS" ] || { say "A FAIL NEVER SHIPS."; exit 1; }
