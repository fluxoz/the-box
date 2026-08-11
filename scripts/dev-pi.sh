#!/usr/bin/env bash
# Put the working tree on a Pi and run it, without cutting a release.
#
# The release path (tag -> CI -> channel -> `boxd channel update`) takes about
# ten minutes and exists to ship a whole platform closure safely. That is the
# wrong tool for "does this button look right". This builds boxd natively on the
# Pi from the current source and runs it against the Box's real data dir, so the
# loop is a rebuild rather than a release.
#
#   scripts/dev-pi.sh [user@host]      push, build, run the dev binary
#   scripts/dev-pi.sh --restore [t]    stop it and hand back to the packaged boxd
#
# The Box's own boxd is stopped while the dev binary holds the port, and comes
# back untouched on --restore: nothing here modifies the installed system.
# shellcheck disable=SC2029  # $REMOTE_DIR is ours and expands here on purpose
set -euo pipefail

TARGET="${2:-${BOX_DEV_TARGET:-murphy@192.168.1.58}}"
REMOTE_DIR="boxd-dev"

if [ "${1:-}" = "--restore" ]; then
  ssh "$TARGET" 'sudo systemctl stop boxd-dev 2>/dev/null || true; sudo systemctl start boxd'
  echo "restored: the packaged boxd is running again"
  exit 0
fi
TARGET="${1:-$TARGET}"

echo "==> syncing source to $TARGET:$REMOTE_DIR"
# Source only. target/ is x86 here and would be useless (and huge) there.
rsync -a --delete --info=stats1 \
  --exclude 'target/' --exclude '.git/' --exclude '*.iso' \
  --exclude '.dev-data/' --exclude 'result*' \
  ./ "$TARGET:$REMOTE_DIR/"

echo "==> building on the Pi (first run compiles the toolchain closure; later runs are incremental)"
# Keep cargo's target/ OUT of the flake directory. `nix develop` on a
# non-git tree copies the whole directory into the store, so a target/ left
# inside would mean a multi-GB store path on every single build.
# devShells.build is the lean shell: a Rust toolchain and the binaries boxd
# shells out to. Deliberately NOT the default shell, which carries chromium and
# has no business on a Raspberry Pi.
ssh "$TARGET" "cd $REMOTE_DIR && CARGO_TARGET_DIR=\$HOME/boxd-dev-target \
  nix develop .#build -c cargo build --release -p boxd"

echo "==> swapping the running console for the dev build"
ssh "$TARGET" "
  sudo systemctl stop boxd 2>/dev/null || true
  sudo systemctl stop boxd-dev 2>/dev/null || true
  sudo systemd-run --unit=boxd-dev --collect \
    --setenv=BOX_CATALOG_DIR=\$HOME/$REMOTE_DIR/catalog \
    \$HOME/boxd-dev-target/release/boxd \
      --data-dir /var/lib/boxd --backend nix serve --listen 0.0.0.0:2693
  sleep 1
  systemctl is-active boxd-dev"

host="${TARGET#*@}"
echo
echo "running your working tree at http://$host:2693"
echo "put it back with: scripts/dev-pi.sh --restore $TARGET"
