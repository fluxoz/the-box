#!/usr/bin/env bash
# The Box — multi-VM test lab.
#
# Two-phase, unprivileged (uses the setuid qemu-bridge-helper + libvirt's
# virbr0 for a shared L2 net with DHCP, so VMs discover each other over mDNS and
# every dashboard is reachable from the host):
#
#   provision N  boot the installer ISO per VM with a per-VM handoff on a
#                BOX-INSTALL seed disk; it wipes /dev/vda, installs Box OS,
#                powers off.
#   run N        boot the installed disks on virbr0 -> a live fleet.
#   status       list VMs, their DHCP IPs, and dashboard URLs.
#   down         power off all lab VMs.
#
# Requires: the libvirt "default" network active (virbr0). Run:
#   virsh -c qemu:///system net-start default
set -euo pipefail

REPO="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
WORK="${BOX_LAB_DIR:-/tmp/claude-1000/-home-murphy-dev-the-box/b43e58c8-3126-4f96-b9ee-b901e1fbac5d/scratchpad/lab}"
BRIDGE="${BOX_LAB_BRIDGE:-virbr0}"
HELPER=/run/wrappers/bin/qemu-bridge-helper
MEM="${BOX_LAB_MEM:-2048}"
CPU="${BOX_LAB_CPU:-2}"
mkdir -p "$WORK"

log() { echo "[lab] $*" >&2; }

ensure_key() {
  [ -f "$WORK/operator_key" ] || ssh-keygen -t ed25519 -N "" -C operator@lab -f "$WORK/operator_key" >/dev/null
}

resolve_iso() {
  local out
  out="$(nix build "$REPO#installer-iso" --no-link --print-out-paths 2>/dev/null)"
  ISO="$(echo "$out"/iso/*.iso)"
  [ -f "$ISO" ] || { log "installer ISO not found under $out"; exit 1; }
}

resolve_ovmf() {
  local out
  out="$(nix build nixpkgs#OVMF.fd --no-link --print-out-paths 2>/dev/null | tail -1)"
  OVMF_CODE="$(find "$out" -name 'OVMF_CODE*.fd' | head -1)"
  OVMF_VARS_SRC="$(find "$out" -name 'OVMF_VARS*.fd' | head -1)"
  [ -f "$OVMF_CODE" ] && [ -f "$OVMF_VARS_SRC" ] || { log "OVMF firmware not found"; exit 1; }
}

mac_for() { printf '52:54:00:b0:00:%02x' "$1"; }

gen_seed() {
  local i="$1" json="$WORK/box-$i-handoff.json" seed="$WORK/box-$i-seed.img"
  # Mint the enrollment code the way the Configurator would: user keeps the
  # code, only its hash goes in the orders. Saved so `status` can show it.
  local code hash
  code="$(head -c5 /dev/urandom | od -An -tx1 | tr -d ' \n')"
  hash="$(printf '%s' "$code" | sha256sum | cut -d' ' -f1)"
  echo "$code" > "$WORK/box-$i-paircode.txt"
  cat > "$json" <<EOF
{
  "erase_disk": true,
  "disk": "/dev/vda",
  "hostname": "box-$i",
  "ssh_authorized_keys": ["$(cat "$WORK/operator_key.pub")"],
  "enrollment_code_hash": "$hash",
  "finish": "poweroff"
}
EOF
  rm -f "$seed"; truncate -s 16M "$seed"
  nix shell nixpkgs#dosfstools nixpkgs#mtools -c bash -c \
    "mkfs.vfat -n BOX-INSTALL '$seed' >/dev/null && mcopy -i '$seed' '$json' ::/box-install.json"
}

vm_vars() { # per-VM writable UEFI NVRAM, created once
  local vars="$WORK/box-$1-vars.fd"
  if [ ! -f "$vars" ]; then
    cp "$OVMF_VARS_SRC" "$vars"
    chmod u+w "$vars" # store copy is read-only; qemu needs pflash writable
  fi
  echo "$vars"
}

provision_one() {
  local i="$1" disk="$WORK/box-$i.qcow2" seed="$WORK/box-$i-seed.img"
  gen_seed "$i"
  qemu-img create -f qcow2 "$disk" 12G >/dev/null
  local vars; vars="$(vm_vars "$i")"
  log "installing box-$i ..."
  qemu-system-x86_64 -enable-kvm -machine q35 -m "$MEM" -smp "$CPU" \
    -drive if=pflash,format=raw,readonly=on,file="$OVMF_CODE" \
    -drive if=pflash,format=raw,file="$vars" \
    -cdrom "$ISO" \
    -drive file="$disk",if=virtio,format=qcow2 \
    -drive file="$seed",if=virtio,format=raw \
    -boot d -no-reboot -display none \
    -serial "file:$WORK/box-$i-install.log"
  log "box-$i install finished (powered off)"
}

run_one() {
  local i="$1" disk="$WORK/box-$i.qcow2" vars pid
  [ -f "$disk" ] || { log "box-$i not provisioned"; return 1; }
  vars="$(vm_vars "$i")"
  qemu-system-x86_64 -enable-kvm -machine q35 -m "$MEM" -smp "$CPU" \
    -drive if=pflash,format=raw,readonly=on,file="$OVMF_CODE" \
    -drive if=pflash,format=raw,file="$vars" \
    -drive file="$disk",if=virtio,format=qcow2 \
    -netdev bridge,id=lan,br="$BRIDGE",helper="$HELPER" \
    -device virtio-net-pci,netdev=lan,mac="$(mac_for "$i")" \
    -display none -serial "file:$WORK/box-$i-run.log" \
    -pidfile "$WORK/box-$i.pid" -daemonize
  log "box-$i running (pid $(cat "$WORK/box-$i.pid"))"
}

cmd_provision() {
  local n="$1"; ensure_key; resolve_iso; resolve_ovmf
  for i in $(seq 1 "$n"); do provision_one "$i"; done
}

cmd_run() {
  local n="$1"; ensure_key; resolve_ovmf
  for i in $(seq 1 "$n"); do run_one "$i"; done
  log "fleet up. 'lab.sh status' for IPs + dashboard URLs."
}

ip_of() {
  local mac; mac="$(mac_for "$1")"
  virsh -c qemu:///system net-dhcp-leases "${BOX_LAB_NET:-default}" 2>/dev/null \
    | awk -v m="$mac" 'tolower($3)==m {print $5}' | cut -d/ -f1 | head -1
}

ssh_to() {
  local ip; ip="$(ip_of "$1")"; [ -n "$ip" ] || { log "box-$1 has no lease"; return 1; }
  ssh -i "$WORK/operator_key" -o StrictHostKeyChecking=no \
    -o UserKnownHostsFile=/dev/null -o ConnectTimeout=8 "root@$ip" "${@:2}"
}

cmd_status() {
  printf '%-6s  %-15s  %-26s  %s\n' box ip dashboard "pair code"
  for pidf in "$WORK"/box-*.pid; do
    [ -f "$pidf" ] || continue
    local i ip code; i="$(basename "$pidf" .pid | sed 's/box-//')"; ip="$(ip_of "$i")"
    code="$(cat "$WORK/box-$i-paircode.txt" 2>/dev/null)"
    printf 'box-%-2s  %-15s  %-26s  %s\n' "$i" "${ip:-<no lease>}" \
      "${ip:+http://$ip:2693}" "${code:-<none>}"
  done
  echo
  echo "To pair a browser (the real user flow — no SSH): open the dashboard URL,"
  echo "go to /pair, and enter that box's pair code (from the recovery kit)."
  echo
  echo "Shortcuts if you want them: 'lab.sh ssh <i>' for a shell; or tunnel in for"
  echo "instant trusted-local access (no pairing):"
  echo "  ssh -i $WORK/operator_key -L 2693:localhost:2693 root@<ip>  # open http://localhost:2693"
}

# One-time pairing code for box i — exactly what a user runs over SSH.
cmd_enroll() { ssh_to "$1" boxd auth enroll; }
cmd_ssh()    { ssh_to "$1"; }                     # shell on box i (operator key)

cmd_down() {
  for pidf in "$WORK"/box-*.pid; do
    [ -f "$pidf" ] || continue
    kill "$(cat "$pidf")" 2>/dev/null && log "stopped $(basename "$pidf" .pid)"
    rm -f "$pidf"
  done
}

case "${1:-}" in
  provision) cmd_provision "${2:-1}" ;;
  run)       cmd_run "${2:-1}" ;;
  up)        cmd_provision "${2:-1}"; cmd_run "${2:-1}" ;;
  status)    cmd_status ;;
  enroll)    cmd_enroll "${2:?box index}" ;;
  ssh)       cmd_ssh "${2:?box index}" ;;
  down)      cmd_down ;;
  *) echo "usage: lab.sh {provision|run|up|status|enroll|ssh|down} [N|i]" >&2; exit 1 ;;
esac
