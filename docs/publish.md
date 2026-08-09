# Publishing thebox.build

Everything a machine fetches during install lives in one reproducible bundle:

```
nix build .#site
# -> result/  with:
#    install.sh          curl|sh installer, real artifact hashes stamped in
#    install.ps1         Windows one-liner
#    index.html          landing page
#    mac.txt             the macOS note
#    netboot/bzImage     installer kernel  (~13 MB)
#    netboot/initrd      installer initrd  (~1.4 GB)
#    netboot/netboot.ipxe
#    SHA256SUMS          checksums over every served file
```

`thebox.build` just needs to serve that directory at its web root over TLS.

## Trust model

`install.sh` **wipes disks**, so it must not blindly `kexec` whatever it
downloads. The chain:

1. The user fetches `install.sh` from `https://thebox.build` — trusted via TLS.
2. The published `install.sh` has the **exact SHA-256 of `bzImage` and `initrd`
   stamped into it** at build time (the `site` derivation substitutes them).
3. Before `kexec`, `install.sh` re-hashes what it downloaded and aborts on any
   mismatch.

So the big artifacts can live on a **mirror or CDN on a different origin** and a
compromise there still can't feed a tampered kernel — the hashes come from the
TLS-protected script. (A future hardening signs `SHA256SUMS` with an offline
key to also cover a compromise of `thebox.build` itself; not required for v1.)

## Recommended hosting

- **Origin + CDN:** Cloudflare in front of the bundle for TLS + edge caching.
  The 1.4 GB `initrd` wants object storage — **Cloudflare R2** (or S3) behind
  the CDN — with `install.sh` / `index.html` served from the same host.
- **Mirror:** publish `netboot/` + `install.sh` to **GitHub Releases** on
  `coyote-technology/the-box`. Run the installer against it with
  `BOX_BASE=https://github.com/.../releases/download/<tag>`; the stamped hashes
  make the mirror safe.
- **Alternative (sovereign):** a small VPS running nginx serving `result/`, or
  a Box itself behind a cloudflared tunnel. Same bundle, no code change.

Any host works — the bundle is a plain directory. Only the trust model above is
load-bearing.

## Manual publish (until CI is wired)

```sh
site=$(nix build .#site --no-link --print-out-paths)

# object storage for the heavy artifacts
rclone copy "$site/netboot" r2:thebox-build/netboot

# static files at the web root (rsync to a VPS, or Cloudflare Pages, or...)
rsync -a --delete \
  --exclude netboot \
  "$site/" deploy@thebox.build:/var/www/thebox.build/
rsync -a "$site/netboot/netboot.ipxe" deploy@thebox.build:/var/www/thebox.build/netboot/
```

`scripts/publish.sh <target>` wraps this; edit its rsync/rclone targets for your
host.

## DNS / one-time setup (your hands)

1. Point `thebox.build` A/AAAA (or Cloudflare proxied CNAME) at the origin.
2. TLS: Cloudflare, or Let's Encrypt on the VPS.
3. Ensure `Content-Type` for `install.sh` is `text/x-shellscript` or
   `text/plain` (not `application/octet-stream`) so `curl | sh` streams cleanly.
4. Verify: `curl -fsSL https://thebox.build/install.sh | head` shows the script
   with real hashes, and `curl -fsSLI https://thebox.build/netboot/initrd`
   returns 200 with the full length.

## Releases / update channel

Cutting a platform release (what `boxd channel` pulls) is separate from serving
the installer bundle: tag the flake so `nixosConfigurations`/the `the-box` input
resolve to the tag, and (optionally) publish the `site` bundle for that tag to
the mirror. Wiring `channel.rs` to the published release feed is the next
increment.
