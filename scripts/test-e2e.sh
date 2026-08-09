#!/usr/bin/env bash
# End-to-end integration tests for Box Backup + the cloud control plane.
#
# Unlike `cargo test` (pure logic, no I/O), this drives the REAL binaries —
# restic, rclone, boxd, box-cloud — through the flows a user actually hits:
#   1. local backend      : key -> backup -> the repo is ciphertext -> restore
#   2. managed tier (S3)   : box-cloud enroll -> backup lands in the scoped
#                            prefix over a real S3 endpoint (rclone serve s3)
#   3. auth / connect      : bad token -> 401; connect mints coordinator + key
#
# Run from the repo root inside `nix develop`. restic/rclone are pulled from
# nixpkgs if absent. box-cloud is built from its sibling checkout.
set -euo pipefail

# --- tools: re-exec with restic + rclone on PATH if they're missing ----------
if ! command -v restic >/dev/null || ! command -v rclone >/dev/null; then
  exec nix shell nixpkgs#restic nixpkgs#rclone -c "$0" "$@"
fi

REPO_ROOT="$(cd "$(dirname "$0")/.." && pwd)"
BOX_CLOUD_DIR="${BOX_CLOUD_DIR:-$REPO_ROOT/../box-cloud}"
WORK="$(mktemp -d)"
PIDS=()
PASS=0
FAIL=0

cleanup() {
  for p in "${PIDS[@]:-}"; do kill "$p" 2>/dev/null || true; done
  rm -rf "$WORK"
}
trap cleanup EXIT

ok()   { printf '  \033[32m✓\033[0m %s\n' "$1"; PASS=$((PASS + 1)); }
bad()  { printf '  \033[31m✗ %s\033[0m\n' "$1"; FAIL=$((FAIL + 1)); }
# assert_eq EXPECTED ACTUAL MSG
assert_eq() { [ "$1" = "$2" ] && ok "$3" || bad "$3 (want '$1', got '$2')"; }
# assert_contains HAYSTACK NEEDLE MSG
assert_contains() { case "$1" in *"$2"*) ok "$3" ;; *) bad "$3 (missing '$2')" ;; esac; }
# assert_absent DIR NEEDLE MSG — the plaintext sentinel must not appear on disk
assert_absent() { grep -rq "$2" "$1" 2>/dev/null && bad "$3 (plaintext '$2' found!)" || ok "$3"; }
section() { printf '\n\033[1m%s\033[0m\n' "$1"; }

# --- build --------------------------------------------------------------------
section "Building binaries"
( cd "$REPO_ROOT" && cargo build -q -p boxd )
BOXD="$REPO_ROOT/target/debug/boxd"
if [ -d "$BOX_CLOUD_DIR" ]; then
  ( cd "$BOX_CLOUD_DIR" && cargo build -q )
  BOX_CLOUD="$BOX_CLOUD_DIR/target/debug/box-cloud"
  ok "boxd + box-cloud built"
else
  BOX_CLOUD=""
  echo "  (box-cloud checkout not found at $BOX_CLOUD_DIR — skipping managed tier)"
fi

SENTINEL="PLAINTEXT-SENTINEL-$$-do-not-leak"

# =============================================================================
section "1. Local backend — ciphertext roundtrip"
# =============================================================================
DATA="$WORK/local/data"; REPO="$WORK/local/repo"; RESTORE="$WORK/local/restore"
mkdir -p "$DATA" "$REPO" "$RESTORE"
printf '%s\n' "$SENTINEL" > "$DATA/secret-note.txt"   # a file we expect to survive a roundtrip
cat > "$DATA/box.toml" <<EOF
[backup]
enabled = true
schedule = "daily"

[backup.backend]
kind = "local"
path = "$REPO"
EOF

"$BOXD" --data-dir "$DATA" backup init >/dev/null
"$BOXD" --data-dir "$DATA" backup run >/dev/null
SNAPS=$("$BOXD" --data-dir "$DATA" backup snapshots 2>/dev/null | grep -c . || true)
[ "$SNAPS" -ge 1 ] && ok "backup produced a snapshot" || bad "no snapshot after backup run"

# The whole point: the repo on disk is encrypted, our sentinel must not be in it.
assert_absent "$REPO" "$SENTINEL" "repo is ciphertext (sentinel not on disk)"

"$BOXD" --data-dir "$DATA" backup restore --target "$RESTORE" >/dev/null
if grep -rq "$SENTINEL" "$RESTORE" 2>/dev/null; then
  ok "restore recovered the original file"
else
  bad "restore did not recover the sentinel file"
fi

"$BOXD" --data-dir "$DATA" backup check >/dev/null 2>&1 && ok "restic check passed" || bad "restic check failed"

# =============================================================================
section "2. Managed tier — enroll + scoped S3 backup"
# =============================================================================
if [ -z "$BOX_CLOUD" ]; then
  echo "  (skipped — no box-cloud)"
else
  S3_ROOT="$WORK/s3"; BUCKET="box-backups"
  mkdir -p "$S3_ROOT/$BUCKET"
  AK="cloudkey"; SK="cloudsecretcloudsecret"
  # A real S3 endpoint backed by a local dir.
  rclone serve s3 "$S3_ROOT" --addr 127.0.0.1:9055 --auth-key "$AK,$SK" \
    --force-path-style >/dev/null 2>&1 &
  PIDS+=($!)
  # The control plane, handing out that endpoint's scoped creds.
  # All accounts must be created BEFORE serve starts: serve holds the store in
  # memory and persists on write, so a create-account by a separate process
  # after startup is invisible (and would be clobbered on the next write).
  CDATA="$WORK/box-cloud-data.json"
  TOKEN="$("$BOX_CLOUD" --data "$CDATA" create-account --id alice 2>/dev/null | tail -1)"
  T2="$("$BOX_CLOUD" --data "$CDATA" create-account --id bob 2>/dev/null | tail -1)"
  "$BOX_CLOUD" --data "$CDATA" serve --listen 127.0.0.1:8795 \
    --s3-endpoint "http://127.0.0.1:9055" --s3-bucket "$BUCKET" \
    --s3-key "$AK" --s3-secret "$SK" --s3-root "$S3_ROOT/$BUCKET" \
    --connect-login-server "https://connect.thebox.build" >/dev/null 2>&1 &
  PIDS+=($!)
  sleep 2

  MDATA="$WORK/managed/data"; mkdir -p "$MDATA"
  printf '%s\n' "$SENTINEL" > "$MDATA/secret-note.txt"
  # One command: enroll -> provision scoped S3 creds -> point Box Backup at them.
  "$BOXD" --data-dir "$MDATA" cloud enroll --server "http://127.0.0.1:8795" --token "$TOKEN" >/dev/null
  "$BOXD" --data-dir "$MDATA" backup run >/dev/null

  # Backup landed under the account-scoped prefix, over the wire, encrypted.
  if [ -d "$S3_ROOT/$BUCKET/acct-alice" ] && [ -n "$(ls -A "$S3_ROOT/$BUCKET/acct-alice" 2>/dev/null)" ]; then
    ok "backup landed in the scoped prefix (acct-alice/)"
  else
    bad "no data under the scoped prefix acct-alice/"
  fi
  assert_absent "$S3_ROOT/$BUCKET/acct-alice" "$SENTINEL" "managed backup is ciphertext"

  USAGE="$("$BOXD" --data-dir "$MDATA" cloud status 2>/dev/null || true)"
  assert_contains "$USAGE" "stored" "cloud status reports metered usage"
fi

# =============================================================================
section "3. Auth + Box Connect provisioning"
# =============================================================================
if [ -z "$BOX_CLOUD" ]; then
  echo "  (skipped — no box-cloud)"
else
  # Enroll bob (account created before serve, above) to get an api_token.
  API2="$(curl -fsS -X POST http://127.0.0.1:8795/v1/enroll -H 'content-type: application/json' \
    -d "{\"enroll_token\":\"$T2\"}" | python3 -c 'import sys,json;print(json.load(sys.stdin)["api_token"])')"

  CODE=$(curl -s -o /dev/null -w '%{http_code}' -X POST \
    http://127.0.0.1:8795/v1/connect/provision -H "authorization: Bearer badtoken")
  assert_eq "401" "$CODE" "connect provision rejects a bad token"

  CONN="$(curl -fsS -X POST http://127.0.0.1:8795/v1/connect/provision \
    -H "authorization: Bearer $API2")"
  assert_contains "$CONN" "connect.thebox.build" "connect provision returns the coordinator"
  assert_contains "$CONN" "mock-authkey-bob" "connect provision returns an account-scoped key"
  # Fleet: the minted key is tagged (one ACL policy governs the whole fleet).
  assert_contains "$CONN" "tag:box" "connect key carries the fleet tag"

  # The fleet ACL policy prints and parses (HuJSON -> strip // -> JSON).
  POLICY="$("$BOX_CLOUD" --data "$CDATA" fleet-policy | grep -v '^\s*//')"
  echo "$POLICY" | python3 -c 'import sys,json; d=json.load(sys.stdin); assert d["tagOwners"]["tag:box"]' \
    && ok "fleet-policy emits valid JSON owning tag:box" \
    || bad "fleet-policy did not emit valid tag:box policy"
fi

# --- summary -----------------------------------------------------------------
printf '\n\033[1mResult: %d passed, %d failed\033[0m\n' "$PASS" "$FAIL"
[ "$FAIL" -eq 0 ]
