#!/bin/sh
# The Box — convert a running Linux machine into a Box.
#
#   curl -fsSL https://thebox.build/install.sh | sudo sh
#
# with your orders (box-install.json, generated in the Configurator) in the
# current directory, or:
#
#   curl -fsSL https://thebox.build/install.sh | sudo BOX_ORDERS=/path/box-install.json sh
#
# DESTRUCTIVE: this ERASES every disk and installs Box OS. The same blow-away
# model as the USB/Windows paths. It reuses the installer's partition scan — your
# orders are copied to RAM before anything is wiped, and never leave this machine.
#
# Mechanism (like nixos-anywhere / nixos-infect): fetch the Box netboot
# kernel+initrd, stage the orders where the installer scans for them, and kexec
# straight into the installer — no USB, no reboot menu.
set -eu

BASE="${BOX_BASE:-https://thebox.build}"       # where the netboot artifacts live
ORDERS="${BOX_ORDERS:-box-install.json}"       # your orders; secrets stay local
WORK="/run/box-install"

say() { printf '\033[1;33m[box]\033[0m %s\n' "$*" >&2; }
die() { printf '\033[1;31m[box] error:\033[0m %s\n' "$*" >&2; exit 1; }

[ "$(id -u)" = 0 ] || die "run as root: pipe into 'sudo sh'."
command -v kexec >/dev/null 2>&1 || die "kexec not found — install kexec-tools (e.g. 'apt install kexec-tools')."
command -v curl  >/dev/null 2>&1 || die "curl not found."
[ -f "$ORDERS" ] || die "orders file '$ORDERS' not found. Generate box-install.json in the Configurator and place it here (or set BOX_ORDERS)."
grep -q '"erase_disk"[[:space:]]*:[[:space:]]*true' "$ORDERS" \
  || die "orders do not consent to erase_disk:true — refusing to touch any disk."

name=$(sed -n 's/.*"hostname"[[:space:]]*:[[:space:]]*"\([^"]*\)".*/\1/p' "$ORDERS" | head -1)

cat >&2 <<EOF

  ============================================================
   THE BOX — CONVERT THIS MACHINE  (erases everything)
   Every disk on this machine will be wiped and replaced with
   Box OS. All data is lost. It becomes: ${name:-box}
  ============================================================
EOF
if [ "${BOX_YES:-}" != "1" ]; then
  printf '  Type ERASE to proceed: ' >&2
  read -r ans
  [ "$ans" = "ERASE" ] || die "aborted — nothing was changed."
fi

mkdir -p "$WORK"
say "fetching the Box installer (kernel + initrd) from $BASE ..."
curl -fsSL "$BASE/netboot/bzImage"      -o "$WORK/bzImage"      || die "could not fetch the kernel from $BASE/netboot/."
curl -fsSL "$BASE/netboot/initrd"       -o "$WORK/initrd"       || die "could not fetch the initrd."
curl -fsSL "$BASE/netboot/netboot.ipxe" -o "$WORK/netboot.ipxe" || die "could not fetch boot parameters."

# Stage the orders where the installer's partition scan finds them — on a live
# filesystem, so the installer copies them to RAM before wiping anything (the
# exact trick the Windows takeover uses).
mkdir -p /box-installer
cp "$ORDERS" /box-installer/box-install.json
sync

# Derive the installer's kernel command line from the netboot script (init=...,
# console, etc.) and add the scan flag so it finds the staged orders.
params=$(grep '^kernel' "$WORK/netboot.ipxe" \
  | sed -e 's/^kernel bzImage //' -e 's/ initrd=initrd//' -e 's/ *${cmdline}//')
[ -n "$params" ] || die "could not parse boot parameters from netboot.ipxe."

say "handing off to the Box installer via kexec — the machine will now wipe and install."
say "it will come back up at ${name:-box}.local in a few minutes."
kexec -l "$WORK/bzImage" --initrd="$WORK/initrd" --command-line="$params box.install-scan=1"
sync
kexec -e
