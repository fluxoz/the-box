#!/usr/bin/env bash
# The birth rehearsal: a blank machine takes the LIVE thebox.build one-liner
# and becomes a serving Box, with a stopwatch on every stage. This is the
# same walk a stranger's machine takes, run by CI nightly, and the timings it
# emits are the receipts the landing page shows. A rehearsal that fails means
# the funnel is broken for real people — that is the point of running it.
#
# Emits: $OUT/receipt.json  {verified_at, ok, stages: {…seconds}, release}
# Needs: qemu-system-x86_64, OVMF, genisoimage, curl, ssh-keygen, python3.
# KVM is used when present; plain TCG works, slower.
set -euo pipefail

WORK="${WORK:-$(mktemp -d)}"
OUT="${OUT:-$WORK}"
IMG_URL="${IMG_URL:-https://cloud.debian.org/images/cloud/bookworm/latest/debian-12-genericcloud-amd64.qcow2}"
SITE="${SITE:-https://thebox.build}"
mkdir -p "$WORK" "$OUT"
cd "$WORK"

say() { printf '[birth] %s\n' "$*"; }
now() { date +%s; }
T0=$(now)
declare -A STAGE

fail() {
  say "FAILED at $1"
  python3 - "$OUT/receipt.json" "$1" <<'PY'
import json, sys, time
json.dump({"verified_at": int(time.time()), "ok": False, "failed_at": sys.argv[2]},
          open(sys.argv[1], "w"))
PY
  exit 1
}

# ---- the blank machine ----------------------------------------------------
say "fetching the blank machine image"
curl -fsSL -o debian.qcow2 "$IMG_URL" || fail "image-download"
qemu-img resize debian.qcow2 16G >/dev/null
ssh-keygen -t ed25519 -N '' -f vmkey -q
cat > user-data <<EOF
#cloud-config
users:
  - name: op
    sudo: ALL=(ALL) NOPASSWD:ALL
    shell: /bin/bash
    ssh_authorized_keys:
      - $(cat vmkey.pub)
EOF
printf 'instance-id: birth\nlocal-hostname: sparepc\n' > meta-data
genisoimage -quiet -output seed.iso -volid cidata -joliet -rock user-data meta-data

OVMF_CODE=""
for c in /usr/share/OVMF/OVMF_CODE_4M.fd /usr/share/OVMF/OVMF_CODE.fd "${OVMF_DIR:-}/OVMF_CODE.fd"; do
  [ -f "$c" ] && OVMF_CODE=$c && break
done
[ -n "$OVMF_CODE" ] || fail "no-ovmf"
OVMF_VARS="${OVMF_CODE/CODE/VARS}"
cp "$OVMF_VARS" vars.fd && chmod +w vars.fd

# KVM only when actually usable; TCG otherwise (slower, still true).
ACCEL=""; [ -r /dev/kvm ] && [ -w /dev/kvm ] && ACCEL="-enable-kvm -cpu host"
# shellcheck disable=SC2086
qemu-system-x86_64 $ACCEL -m 4096 -smp 4 \
  -drive if=pflash,format=raw,readonly=on,file="$OVMF_CODE" \
  -drive if=pflash,format=raw,file=vars.fd \
  -drive file=debian.qcow2,if=virtio,format=qcow2 \
  -drive file=seed.iso,if=virtio,format=raw,readonly=on \
  -netdev user,id=n0,hostfwd=tcp:127.0.0.1:2222-:22,hostfwd=tcp:127.0.0.1:2695-:2693 \
  -device virtio-net-pci,netdev=n0 \
  -display none -serial "file:$WORK/serial.log" -daemonize -pidfile qemu.pid
trap 'kill "$(cat qemu.pid 2>/dev/null)" 2>/dev/null || true' EXIT
STAGE[boot_blank]=$(( $(now) - T0 )); T1=$(now)

SSH="ssh -i vmkey -p 2222 -o StrictHostKeyChecking=no -o UserKnownHostsFile=/dev/null -o ConnectTimeout=3 -o BatchMode=yes"
# shellcheck disable=SC2086
for _ in $(seq 1 120); do $SSH op@127.0.0.1 true 2>/dev/null && break; sleep 5; done
# shellcheck disable=SC2086
$SSH op@127.0.0.1 true 2>/dev/null || fail "blank-machine-ssh"
say "blank machine up ($(( $(now) - T1 ))s)"

# ---- the takeover, exactly as documented ----------------------------------
CODE=$(head -c5 /dev/urandom | od -An -tx1 | tr -d ' \n')
HASH=$(printf %s "$CODE" | sha256sum | cut -d' ' -f1)
KEY=$(cat vmkey.pub)
B64=$(printf '{ "erase_disk": true, "hostname": "auto", "ssh_authorized_keys": ["%s"], "enrollment_code_hash": "%s" }' "$KEY" "$HASH" | base64 -w0)
say "running the live one-liner"
# shellcheck disable=SC2086
timeout 900 $SSH op@127.0.0.1 "curl -fsSL $SITE/install.sh | sudo BOX_ORDERS_B64=$B64 BOX_YES=1 sh" >/dev/null 2>&1 || true
STAGE[takeover]=$(( $(now) - T1 )); T1=$(now)

# ---- the newborn ----------------------------------------------------------
HEALTH=""
for _ in $(seq 1 180); do
  HEALTH=$(curl -fsS -m 4 http://127.0.0.1:2695/api/v1/health 2>/dev/null || true)
  printf '%s' "$HEALTH" | grep -q '"health"' && break
  sleep 10
done
printf '%s' "$HEALTH" | grep -q '"health"' || fail "newborn-never-answered"
RELEASE=$(printf '%s' "$HEALTH" | sed -n 's/.*"version":"\([^"]*\)".*/\1/p')
STAGE[install_to_health]=$(( $(now) - T1 )); T1=$(now)
say "newborn Box up on $RELEASE"

# ---- pair and deploy, like a stranger's agent -----------------------------
TOKEN=$(curl -fsS -m 10 -X POST http://127.0.0.1:2695/pair/redeem \
  -H 'accept: application/json' -H 'content-type: application/json' \
  -d "{\"code\":\"$CODE\",\"label\":\"birth-rehearsal\"}" | python3 -c 'import json,sys; print(json.load(sys.stdin)["token"])') || fail "pairing"
curl -fsS -m 180 -X POST http://127.0.0.1:2695/mcp \
  -H "Authorization: Bearer $TOKEN" -H 'Content-Type: application/json' \
  -d '{"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"name":"deploy_static_site","arguments":{"name":"hello","index_html":"<h1>verified tonight</h1>"}}}' \
  | grep -q '"isError": *false' || fail "deploy"
curl -fsS -m 10 http://127.0.0.1:2695/sites/hello/ | grep -q "verified tonight" || fail "serving"
STAGE[pair_and_deploy]=$(( $(now) - T1 ))
TOTAL=$(( $(now) - T0 ))
say "REHEARSAL COMPLETE in ${TOTAL}s"

python3 - "$OUT/receipt.json" "$RELEASE" "$TOTAL" \
  "${STAGE[boot_blank]}" "${STAGE[takeover]}" "${STAGE[install_to_health]}" "${STAGE[pair_and_deploy]}" <<'PY'
import json, sys, time
json.dump({
    "verified_at": int(time.time()),
    "ok": True,
    "release": sys.argv[2],
    "total_seconds": int(sys.argv[3]),
    "stages": {
        "boot_blank_machine": int(sys.argv[4]),
        "takeover_kexec": int(sys.argv[5]),
        "install_to_first_health": int(sys.argv[6]),
        "pair_and_first_deploy": int(sys.argv[7]),
    },
}, open(sys.argv[1], "w"), indent=2)
PY
say "receipt written to $OUT/receipt.json"
