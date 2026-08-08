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
WORK="/run/box-install"                        # tmpfs — orders never touch a disk

say() { printf '\033[1;33m[box]\033[0m %s\n' "$*" >&2; }
die() { printf '\033[1;31m[box] error:\033[0m %s\n' "$*" >&2; exit 1; }

[ "$(id -u)" = 0 ] || die "run as root: pipe into 'sudo sh'."
for c in kexec curl base64 cpio gzip; do
  command -v "$c" >/dev/null 2>&1 || die "$c not found (kexec needs kexec-tools; cpio/base64/gzip are usually present)."
done

mkdir -p "$WORK"
# Orders come from the Configurator: either encoded in the pasted command
# (BOX_ORDERS_B64 — the normal path, no file to move) or a local file.
if [ -n "${BOX_ORDERS_B64:-}" ]; then
  printf '%s' "$BOX_ORDERS_B64" | base64 -d > "$WORK/box-install.json" 2>/dev/null \
    || die "could not decode the orders embedded in the command (BOX_ORDERS_B64)."
  ORDERS="$WORK/box-install.json"
else
  ORDERS="${BOX_ORDERS:-box-install.json}"
  [ -f "$ORDERS" ] || die "no orders. Paste the command from the Configurator (it embeds them), or place box-install.json here."
fi
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

# Inject the orders into the kexec initrd so they ride into the installer in RAM
# and never touch a disk (nor depend on the disk layout). The kernel unpacks all
# concatenated initramfs archives, so /box-installer/box-install.json ends up in
# the installer's root filesystem, where box-install reads it directly.
mkdir -p "$WORK/inject/box-installer"
cp "$ORDERS" "$WORK/inject/box-installer/box-install.json"
( cd "$WORK/inject" && find . | cpio -o -H newc 2>/dev/null ) | gzip -9 > "$WORK/orders.cpio.gz"
cat "$WORK/initrd" "$WORK/orders.cpio.gz" > "$WORK/initrd.full"

# Derive the installer's kernel command line from the netboot script.
params=$(grep '^kernel' "$WORK/netboot.ipxe" \
  | sed -e 's/^kernel bzImage //' -e 's/ initrd=initrd//' -e 's/ *${cmdline}//')
[ -n "$params" ] || die "could not parse boot parameters from netboot.ipxe."

say "handing off to the Box installer via kexec — the machine will now wipe and install."
say "it will come back up at ${name:-box}.local in a few minutes."
kexec -l "$WORK/bzImage" --initrd="$WORK/initrd.full" --command-line="$params"
sync
kexec -e
