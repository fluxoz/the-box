#!/usr/bin/env bash
# Push a closure's OWN paths to our binary cache, skipping everything the
# public caches already serve — so the next release downloads what it would
# otherwise rebuild, and fluxoz stays small (our bits, not a second copy of
# nixpkgs).
#
# Usage: push-closure.sh <store-path> [extra-public-cache-url ...]
#        PUSH_DRY_RUN=1 to classify and report without pushing.
#
# Speed matters here: the obvious implementation, `nix path-info --store
# <cache> <path>` once per path, is a serial HTTPS round trip per path and
# took 6m37s on one Pi image closure — longer than the build it exists to
# accelerate. Asking each cache for the .narinfo directly, with curl doing the
# parallelism in-process (connection reuse, no fork per path), classifies a
# ~9k-path system closure in about 9 seconds.
set -euo pipefail

root="${1:?usage: push-closure.sh <store-path> [cache-url ...]}"
shift || true
caches=(https://cache.nixos.org "$@")

# Never realise anything from here: a cache step that triggers a build can cost
# more than the build it was meant to save (asking for the Pi kernel's `dev`
# output, which no cache carries, rebuilds the entire kernel).
if [ ! -e "$root" ]; then
  echo "not in this store: $root — nothing to push (refusing to build it)"
  exit 0
fi

tmp=$(mktemp -d)
trap 'rm -rf "$tmp"' EXIT

nix-store -qR "$root" | sort -u > "$tmp/closure"
total=$(wc -l < "$tmp/closure")

# hash <TAB> path, so a .narinfo hit maps back to its store path.
awk '{ h=$0; sub(/^\/nix\/store\//,"",h); sub(/-.*$/,"",h); print h "\t" $0 }' \
  "$tmp/closure" > "$tmp/map"
cut -f1 "$tmp/map" | sort -u > "$tmp/todo"
: > "$tmp/upstream"

for cache in "${caches[@]}"; do
  [ -s "$tmp/todo" ] || break
  sed "s|^|${cache%/}/|; s|$|.narinfo|" "$tmp/todo" > "$tmp/urls"
  # One curl per batch; curl parallelizes internally and reuses connections.
  # --retry so a dropped connection under load doesn't misclassify a path as
  # ours (harmless — it would just be pushed again — but it bloats the cache).
  xargs -a "$tmp/urls" -n 500 \
    curl -sI --parallel --parallel-max 48 --max-time 120 \
         --retry 2 --retry-connrefused \
         -o /dev/null -w '%{http_code} %{url_effective}\n' 2>/dev/null \
    | awk '$1==200 { n=split($2,a,"/"); h=a[n]; sub(/\.narinfo$/,"",h); print h }' \
    >> "$tmp/upstream" || true
  sort -u "$tmp/upstream" -o "$tmp/upstream"
  # Only ask the next cache about paths still unaccounted for.
  comm -23 "$tmp/todo" "$tmp/upstream" > "$tmp/todo.next"
  mv "$tmp/todo.next" "$tmp/todo"
done

awk -v f="$tmp/upstream" '
  BEGIN { while ((getline l < f) > 0) seen[l] = 1 }
  !seen[$1] { print $2 }
' "$tmp/map" | sort -u > "$tmp/ours"

ours=$(wc -l < "$tmp/ours")
echo "closure $total paths: $((total - ours)) already public, $ours ours"

if [ -n "${PUSH_DRY_RUN:-}" ]; then
  echo "(dry run — not pushing)"
  exit 0
fi
if [ -z "${CACHIX_AUTH_TOKEN:-}" ]; then
  echo "CACHIX_AUTH_TOKEN not set — skipping push."
  exit 0
fi

cachix push fluxoz < "$tmp/ours"
