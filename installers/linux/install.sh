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

BASE="${BOX_BASE:-https://thebox.build}"       # where this script + landing live
# Where the heavy netboot artifacts live. Stamped in at publish time (e.g. a
# GitHub Release), so thebox.build itself only serves the small script. Falls
# back to $BASE/netboot when unstamped (a single all-in-one host).
NETBOOT_BASE="${BOX_NETBOOT_BASE:-@NETBOOT_BASE@}"
case "$NETBOOT_BASE" in *@*) NETBOOT_BASE="$BASE/netboot" ;; esac
# Stage the (large) netboot artifacts on DISK, not tmpfs: kexec reserves memory
# to load the ~1.4 GB initrd, so a tmpfs copy on top of that OOMs small boxes
# (a 2-4 GB VPS). This disk is about to be wiped anyway; the secret orders still
# never touch it (they stay in RAM and ride the kexec command line).
WORK="${BOX_WORK:-/var/tmp/box-install}"
ORDERS_RAM="/dev/shm/box-orders.json"          # tiny, RAM-only

# Expected checksums of the netboot artifacts, stamped in when thebox.build
# publishes this script (see the `site` build). Left as placeholders in the
# repo, where verification is skipped. The published script carries the real
# hashes and is served over TLS, so a compromised mirror cannot feed you a
# different kernel/initrd to kexec into — this script WIPES DISKS.
BZIMAGE_SHA256="@BZIMAGE_SHA256@"
INITRD_SHA256="@INITRD_SHA256@"

say() { printf '\033[1;33m[box]\033[0m %s\n' "$*" >&2; }
die() { printf '\033[1;31m[box] error:\033[0m %s\n' "$*" >&2; exit 1; }

verify_sha() { # <file> <expected-hash>
  case "$2" in *@*) return 0 ;; esac  # unsubstituted placeholder (dev/local BASE) — skip
  got=$(sha256sum "$1" | cut -d' ' -f1) || die "sha256sum failed for $1."
  [ "$got" = "$2" ] || die "checksum mismatch on $(basename "$1") — refusing to boot a possibly-tampered image."
}

[ "$(id -u)" = 0 ] || die "run as root: pipe into 'sudo sh'."

# kexec is the one thing stock distros usually lack — install it for them.
ensure_kexec() {
  command -v kexec >/dev/null 2>&1 && return 0
  say "kexec not found — installing kexec-tools ..."
  if   command -v apt-get >/dev/null 2>&1; then DEBIAN_FRONTEND=noninteractive apt-get update -qq && DEBIAN_FRONTEND=noninteractive apt-get install -y -qq kexec-tools
  elif command -v dnf     >/dev/null 2>&1; then dnf install -y -q kexec-tools
  elif command -v yum     >/dev/null 2>&1; then yum install -y -q kexec-tools
  elif command -v zypper  >/dev/null 2>&1; then zypper -n install kexec-tools
  elif command -v pacman  >/dev/null 2>&1; then pacman -Sy --noconfirm kexec-tools
  elif command -v apk     >/dev/null 2>&1; then apk add kexec-tools
  else return 1; fi
  command -v kexec >/dev/null 2>&1
}
ensure_kexec || die "could not find or install kexec (needs kexec-tools). Install it and re-run."
for c in curl base64 sha256sum; do
  command -v "$c" >/dev/null 2>&1 || die "$c not found — it is usually preinstalled."
done

mkdir -p "$WORK"
# Orders come from the Configurator: either encoded in the pasted command
# (BOX_ORDERS_B64 — the normal path, no file to move) or a local file. They hold
# secrets, so keep them in RAM (never on the disk we stage artifacts to).
if [ -n "${BOX_ORDERS_B64:-}" ]; then
  printf '%s' "$BOX_ORDERS_B64" | base64 -d > "$ORDERS_RAM" 2>/dev/null \
    || die "could not decode the orders embedded in the command (BOX_ORDERS_B64)."
  ORDERS="$ORDERS_RAM"
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
say "fetching the Box installer (kernel + initrd) from $NETBOOT_BASE ..."
curl -fsSL "$NETBOOT_BASE/bzImage"      -o "$WORK/bzImage"      || die "could not fetch the kernel from $NETBOOT_BASE."
curl -fsSL "$NETBOOT_BASE/initrd"       -o "$WORK/initrd"       || die "could not fetch the initrd."
curl -fsSL "$NETBOOT_BASE/netboot.ipxe" -o "$WORK/netboot.ipxe" || die "could not fetch boot parameters."

# Verify the kernel + initrd against the hashes baked into this script before we
# ever kexec into them. (netboot.ipxe only carries boot params, checked below.)
verify_sha "$WORK/bzImage" "$BZIMAGE_SHA256"
verify_sha "$WORK/initrd"  "$INITRD_SHA256"

# Inject the orders into the kexec initrd so they ride into the installer in RAM
# and never touch a disk (nor depend on the disk layout). The kernel unpacks all
# concatenated initramfs archives, so /box-installer/box-install.json ends up in
# the installer's root filesystem, where box-install reads it directly.
# Pass the orders on the kernel command line (base64). Unlike files injected
# into the initramfs — which NixOS discards when it switches root into the
# installer — the command line survives to stage 2, and it still touches no
# disk. Orders are small; if yours ever exceed the kernel command-line limit,
# use the USB path.
orders_b64=$(base64 -w0 "$ORDERS")

params=$(grep '^kernel' "$WORK/netboot.ipxe" \
  | sed -e 's/^kernel bzImage //' -e 's/ initrd=initrd//' -e 's/ *${cmdline}//')
[ -n "$params" ] || die "could not parse boot parameters from netboot.ipxe."

say "handing off to the Box installer via kexec — the machine will now wipe and install."
say "it will come back up at ${name:-box}.local in a few minutes."
kexec -l "$WORK/bzImage" --initrd="$WORK/initrd" --command-line="$params box.install-b64=$orders_b64"
# kexec has loaded both into reserved memory — free the staging copies so the
# peak footprint is just that reservation (helps small boxes clear the jump).
rm -f "$WORK/bzImage" "$WORK/initrd"
sync
kexec -e
