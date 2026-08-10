#!/usr/bin/env bash
# Live-convert a running Raspberry Pi OS into a Box, in place — no kexec, no
# separate image. Detects the Pi model, installs Nix, builds/pulls the matching
# Box system closure, installs it over the running root, rewrites /boot/firmware
# to NixOS's boot layout, and reboots. The Pi's own firmware boots the Box.
#
#   curl -fsSL https://thebox.build/pi-install.sh | sudo bash
#
# Override the source flake for local testing:  BOX_FLAKE=/path sudo -E bash convert.sh
set -euo pipefail
[ "$(id -u)" = 0 ] || { echo "run as root: sudo bash convert.sh"; exit 1; }

BOX_FLAKE="${BOX_FLAKE:-github:fluxoz/the-box}"
CACHE_SUB="https://nixos-raspberrypi.cachix.org"
CACHE_KEY="nixos-raspberrypi.cachix.org-1:4iMO9LXa8BqhU+Rpg6LQKiGa2lsNh/j2oiYLNOQ5sPI="

# --- 1. detect the model -----------------------------------------------------
model_str=$(tr -d '\0' < /proc/device-tree/model 2>/dev/null || true)
case "$model_str" in
  *"Raspberry Pi 5"*) MODEL=5 ;;
  *"Raspberry Pi 4"*) MODEL=4 ;;
  *"Raspberry Pi 3"*) MODEL=3 ;;
  *) echo "!! unsupported/undetected board: '$model_str' (need Pi 3/4/5)"; exit 1 ;;
esac
echo ">> $model_str  ->  convert-pi${MODEL}-system"

# Sanity: the convert config expects these labels (Raspberry Pi OS defaults).
findmnt -no SOURCE / | grep -q . || { echo "!! can't determine root device"; exit 1; }
if ! blkid -L rootfs >/dev/null 2>&1 || ! blkid -L bootfs >/dev/null 2>&1; then
  echo "!! expected partitions labelled 'rootfs' (ext4) and 'bootfs' (vfat)."
  echo "   This is a stock Raspberry Pi OS layout; relabel if yours differs."
  exit 1
fi

# --- 2. ensure Nix -----------------------------------------------------------
if ! command -v nix >/dev/null 2>&1; then
  echo ">> installing Nix ..."
  # Raspberry Pi OS has systemd, so let the installer wire up the daemon.
  curl -fsSL https://install.determinate.systems/nix | sh -s -- install --no-confirm
fi
# shellcheck disable=SC1091
. /nix/var/nix/profiles/default/etc/profile.d/nix-daemon.sh 2>/dev/null || true
export PATH="/nix/var/nix/profiles/default/bin:$PATH"
NIX_OPTS=(--extra-experimental-features "nix-command flakes"
          --option extra-substituters "$CACHE_SUB"
          --option extra-trusted-public-keys "$CACHE_KEY")

# --- 3. build/pull the Box system closure ------------------------------------
echo ">> building the Box system (vendor kernel etc. come from the cache) ..."
SYS=$(nix build --no-link --print-out-paths "${NIX_OPTS[@]}" \
  "${BOX_FLAKE}#packages.aarch64-linux.convert-pi${MODEL}-system")
echo ">> system: $SYS"

# --- 4. install in place (nixos-infect-style, Pi-aware) ----------------------
# Mark this a NixOS system and point the system profile at the new closure.
touch /etc/NIXOS
mkdir -p /nix/var/nix/profiles
nix-env -p /nix/var/nix/profiles/system --set "$SYS"

# switch-to-configuration `boot` installs the bootloader for NEXT boot only
# (doesn't tear down the running Raspbian userland), and NIXOS_INSTALL_BOOTLOADER
# makes the nvmd kernelboot builder (re)write /boot/firmware for the Pi firmware.
echo ">> writing the NixOS boot generation to /boot/firmware ..."
NIXOS_INSTALL_BOOTLOADER=1 "$SYS/bin/switch-to-configuration" boot

echo ">> Converted. Rebooting into the Box — it will come up as box.local (:2693)."
sync
systemctl reboot || reboot
