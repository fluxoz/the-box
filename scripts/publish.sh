#!/usr/bin/env bash
# Build the thebox.build bundle and push it to a host.
#
#   scripts/publish.sh [--dry-run]
#
# Edit the two targets below for your host (see docs/publish.md). Defaults are
# placeholders and intentionally point nowhere real.
set -euo pipefail

# tmpfs/object-store target for the heavy netboot artifacts (rclone remote).
NETBOOT_TARGET="${BOX_NETBOOT_TARGET:-r2:thebox-build/netboot}"
# static web root for install.sh / index.html / SHA256SUMS (rsync destination).
WEBROOT_TARGET="${BOX_WEBROOT_TARGET:-deploy@thebox.build:/var/www/thebox.build/}"

DRY=""
[ "${1:-}" = "--dry-run" ] && DRY=1

say() { printf '\033[1;33m[publish]\033[0m %s\n' "$*"; }

say "building the site bundle ..."
site=$(nix build .#site --no-link --print-out-paths)
say "built: $site"
say "contents:"; ( cd "$site" && find . -type f | sort | sed 's/^/    /' )

run() { if [ -n "$DRY" ]; then echo "    DRY: $*"; else "$@"; fi; }

say "uploading heavy artifacts -> $NETBOOT_TARGET"
if command -v rclone >/dev/null 2>&1; then
  run rclone copy "$site/netboot" "$NETBOOT_TARGET"
else
  say "rclone not found — skipping object-store upload (see docs/publish.md)"
fi

say "uploading static files -> $WEBROOT_TARGET"
# Everything except the heavy netboot blobs (those went to object storage);
# netboot.ipxe is tiny and belongs at the web root too.
run rsync -a --delete --exclude 'netboot/bzImage' --exclude 'netboot/initrd' \
  "$site/" "$WEBROOT_TARGET"

say "done. Verify:  curl -fsSL https://thebox.build/install.sh | head"
