#!/bin/sh
# The Box — serve the installer over the network, for provisioning a fleet.
#
#   curl -fsSL https://thebox.build/pxe.sh | sudo BOX_ORDERS_B64='<paste>' sh
#
# Run this on any Linux machine on the LAN (it is NOT wiped — it becomes the
# depot). It fetches the Box installer once, then serves it to every machine
# you network-boot: pick "network boot" in each machine's boot menu (or put
# PXE first on blank machines) and each one wipes itself and becomes a Box
# carrying these orders. With the default "auto" hostname each machine names
# itself, so twenty machines come up as twenty distinct Boxes, all claimable
# with the one pairing code in your orders.
#
# It answers ONLY network-boot requests (proxy DHCP): your router keeps
# handing out addresses, nothing on the LAN is reconfigured, and machines
# that boot from their own disks are untouched. Stop it with Ctrl-C when the
# fleet is up.
set -eu

BASE="${BOX_BASE:-https://thebox.build}"
# Where the heavy netboot artifacts live: stamped at publish time (a GitHub
# Release), falling back to $BASE/netboot for a dev/all-in-one host.
NETBOOT_BASE="${BOX_NETBOOT_BASE:-@NETBOOT_BASE@}"
case "$NETBOOT_BASE" in *@*) NETBOOT_BASE="$BASE/netboot" ;; esac
WORK="${BOX_PXE_DIR:-/var/tmp/box-pxe}"
HTTP_PORT="${BOX_PXE_PORT:-2698}"

# Pinned checksums of everything this script serves to machines that will
# EXECUTE it, stamped in when thebox.build publishes this script (placeholders
# in the repo skip verification). The served script arrives over TLS, so a
# compromised artifact mirror cannot hand your fleet a different installer.
BZIMAGE_SHA256="@BZIMAGE_SHA256@"
INITRD_SHA256="@INITRD_SHA256@"
IPXE_SHA256="@IPXE_SHA256@"
IPXE_EFI_SHA256="@IPXE_EFI_SHA256@"
UNDIONLY_SHA256="@UNDIONLY_SHA256@"

say() { printf '\033[1;33m[box-pxe]\033[0m %s\n' "$*" >&2; }
die() { printf '\033[1;31m[box-pxe] error:\033[0m %s\n' "$*" >&2; exit 1; }

verify_sha() { # <file> <expected-hash>
  case "$2" in *@*) return 0 ;; esac
  got=$(sha256sum "$1" | cut -d' ' -f1) || die "sha256sum failed for $1."
  [ "$got" = "$2" ] || die "checksum mismatch on $(basename "$1") — refusing to serve a possibly-tampered installer."
}

[ "$(id -u)" = 0 ] || die "run as root: pipe into 'sudo sh' (the PXE responder needs ports 67/69)."

# ---- the orders every machine will carry -----------------------------------
[ -n "${BOX_ORDERS_B64:-}" ] \
  || die "no orders. Build them at $BASE/configurator/ — the fleet panel emits this command with the orders embedded."
ORDERS=/dev/shm/box-pxe-orders.json
printf '%s' "$BOX_ORDERS_B64" | base64 -d > "$ORDERS" 2>/dev/null \
  || die "could not decode the orders embedded in the command (BOX_ORDERS_B64)."
grep -q '"erase_disk"[[:space:]]*:[[:space:]]*true' "$ORDERS" \
  || die "orders do not consent to erase_disk:true — refusing to serve an installer that wipes machines."
# Same ceiling as the takeover: the orders ride each client's kernel command
# line (~2048 bytes on x86-64, ~230 already used by the installer's params).
orders_b64_probe=$(base64 -w0 "$ORDERS")
[ "${#orders_b64_probe}" -le 1750 ] \
  || die "your orders are too large for the netboot kernel command line (~2KB). \
Flash the image from https://thebox.build and boot each machine from the same stick, or trim SSH keys."

# ---- tools -----------------------------------------------------------------
ensure_dnsmasq() {
  command -v dnsmasq >/dev/null 2>&1 && return 0
  say "dnsmasq not found — installing it ..."
  if   command -v apt-get >/dev/null 2>&1; then DEBIAN_FRONTEND=noninteractive apt-get update -qq && DEBIAN_FRONTEND=noninteractive apt-get install -y -qq dnsmasq
  elif command -v dnf     >/dev/null 2>&1; then dnf install -y -q dnsmasq
  elif command -v yum     >/dev/null 2>&1; then yum install -y -q dnsmasq
  elif command -v zypper  >/dev/null 2>&1; then zypper -n install dnsmasq
  elif command -v pacman  >/dev/null 2>&1; then pacman -Sy --noconfirm dnsmasq
  elif command -v apk     >/dev/null 2>&1; then apk add dnsmasq
  else return 1; fi
  # Distros that auto-start dnsmasq as a DNS service would hold the ports this
  # script is about to bind; ours runs in the foreground from a private config.
  systemctl stop dnsmasq 2>/dev/null || true
  systemctl disable dnsmasq 2>/dev/null || true
  command -v dnsmasq >/dev/null 2>&1
}
ensure_dnsmasq || die "could not find or install dnsmasq. Install it and re-run."
command -v python3 >/dev/null 2>&1 || die "python3 not found — it serves the installer over HTTP. Install it and re-run."
for c in curl base64 sha256sum ip; do
  command -v "$c" >/dev/null 2>&1 || die "$c not found — it is usually preinstalled."
done

# ---- this machine's place on the LAN ---------------------------------------
IFACE="${BOX_PXE_IFACE:-$(ip -4 route show default 2>/dev/null | sed -n 's/.* dev \([^ ]*\).*/\1/p' | head -n1)}"
[ -n "$IFACE" ] || die "could not find a network interface with a default route (set BOX_PXE_IFACE)."
addr=$(ip -4 -o addr show dev "$IFACE" scope global 2>/dev/null | sed -n 's/.* inet \([0-9./]*\).*/\1/p' | head -n1)
[ -n "$addr" ] || die "no IPv4 address on $IFACE."
SRV_IP=${addr%%/*}
plen=${addr##*/}
# The subnet address, for dnsmasq's proxy range. POSIX arithmetic only.
IFS=. read -r o1 o2 o3 o4 <<EOF
$SRV_IP
EOF
n=$(( (o1 << 24) | (o2 << 16) | (o3 << 8) | o4 ))
mask=$(( plen == 0 ? 0 : (4294967295 << (32 - plen)) & 4294967295 ))
net=$(( n & mask ))
SUBNET="$(( (net >> 24) & 255 )).$(( (net >> 16) & 255 )).$(( (net >> 8) & 255 )).$(( net & 255 ))"

# ---- fetch + verify the installer ------------------------------------------
mkdir -p "$WORK"
say "fetching the Box installer from $NETBOOT_BASE (served once, to every machine) ..."
for f in bzImage initrd netboot.ipxe ipxe.efi undionly.kpxe; do
  curl -fsSL "$NETBOOT_BASE/$f" -o "$WORK/$f" || die "could not fetch $f from $NETBOOT_BASE."
done
verify_sha "$WORK/bzImage"       "$BZIMAGE_SHA256"
verify_sha "$WORK/initrd"        "$INITRD_SHA256"
verify_sha "$WORK/netboot.ipxe"  "$IPXE_SHA256"
verify_sha "$WORK/ipxe.efi"      "$IPXE_EFI_SHA256"
verify_sha "$WORK/undionly.kpxe" "$UNDIONLY_SHA256"

# ---- the boot script machines run ------------------------------------------
# The published netboot.ipxe already carries the exact kernel parameters this
# initrd expects and a ${cmdline} hook for extra arguments. Rewrite it to
# fetch from this depot over HTTP and to carry the orders on the kernel
# command line (where the installer reads them; they never touch the client's
# disk). `initrd=initrd` stays: iPXE names the fetched image by its URL
# basename, and EFI boot finds it by that name.
b64=$(base64 -w0 "$ORDERS")
grep -q '^kernel bzImage ' "$WORK/netboot.ipxe" || die "unexpected netboot.ipxe format."
sed -e "s|^kernel bzImage |kernel http://$SRV_IP:$HTTP_PORT/bzImage |" \
    -e "s|\${cmdline}|box.install-b64=$b64|" \
    -e "s|^initrd initrd$|initrd http://$SRV_IP:$HTTP_PORT/initrd|" \
    "$WORK/netboot.ipxe" > "$WORK/boot.ipxe"

# ---- proxy-DHCP + TFTP + HTTP ----------------------------------------------
# Proxy mode: the LAN's real DHCP server keeps assigning addresses; this only
# ADDS boot information for clients that ask to netboot. Firmware PXE (BIOS or
# UEFI) is chainloaded into iPXE over TFTP; iPXE (option 175) gets boot.ipxe
# and fetches the kernel+initrd over HTTP, which is what makes a half-gigabyte
# installer arrive in seconds instead of TFTP minutes.
cat > "$WORK/dnsmasq.conf" <<EOF
port=0
interface=$IFACE
bind-interfaces
dhcp-range=$SUBNET,proxy
dhcp-match=set:ipxe,175
pxe-service=tag:!ipxe,x86PC,"The Box installer",undionly.kpxe
pxe-service=tag:!ipxe,BC_EFI,"The Box installer",ipxe.efi
pxe-service=tag:!ipxe,X86-64_EFI,"The Box installer",ipxe.efi
pxe-service=tag:ipxe,x86PC,"The Box installer",boot.ipxe
pxe-service=tag:ipxe,BC_EFI,"The Box installer",boot.ipxe
pxe-service=tag:ipxe,X86-64_EFI,"The Box installer",boot.ipxe
enable-tftp
tftp-root=$WORK
log-dhcp
EOF

python3 -m http.server "$HTTP_PORT" --bind "$SRV_IP" --directory "$WORK" >/dev/null 2>&1 &
http_pid=$!
trap 'kill "$http_pid" 2>/dev/null || true' EXIT INT TERM
sleep 1
kill -0 "$http_pid" 2>/dev/null || die "could not serve HTTP on $SRV_IP:$HTTP_PORT (set BOX_PXE_PORT to a free port)."

name=$(sed -n 's/.*"hostname"[[:space:]]*:[[:space:]]*"\([^"]*\)".*/\1/p' "$ORDERS" | head -1)
cat >&2 <<EOF

  ============================================================
   THE BOX — PXE DEPOT UP ON $IFACE ($SRV_IP)
   Every machine on this network that you NETWORK-BOOT will be
   ERASED and become a Box$( [ "$name" = auto ] && printf ' (each names itself)' || printf ' named %s' "${name:-box}" ).
   Machines booting from their own disks are untouched.
   Press Ctrl-C to stop serving when your fleet is up.
  ============================================================
EOF
# In the foreground (not exec): the trap above must outlive dnsmasq to reap
# the HTTP server when the operator stops the depot.
dnsmasq --no-daemon --conf-file="$WORK/dnsmasq.conf"
