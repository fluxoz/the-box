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

## The wired path: GitHub Releases + GitHub Pages (free, no org)

`.github/workflows/publish.yml` does this on a `v*` tag push:

- **Heavy artifacts** (`bzImage`, `initrd`, `netboot.ipxe`) → a **GitHub
  Release** on this repo (free, CDN-backed, handles the 1.4 GB initrd).
- **`install.sh` + `index.html`** → **GitHub Pages** at `thebox.build`. The
  published `install.sh` has `@NETBOOT_BASE@` rewritten to the Release download
  URL, so `thebox.build` only serves the tiny script + landing and the kernel/
  initrd come straight from the Release. The stamped hashes keep that split
  safe (see the trust model above).

Everything is on your **personal** account — the workflow uses
`${{ github.repository }}`, so no organization is needed.

### One-time setup (your hands)

1. **Push this repo to GitHub** (personal account is fine), e.g.
   `git remote add origin git@github.com:<you>/the-box.git && git push -u origin main`.
2. **Enable Pages**: repo → Settings → Pages → *Source: GitHub Actions*. Then
   Settings → Environments → `github-pages` → *Deployment branches and tags* →
   set *No restriction* or add a **Tag** rule `v*` — the auto-created
   environment only allows the default branch by default, so a tag-triggered
   deploy otherwise fails with "not allowed to deploy ... environment
   protection rules".
3. **Set the custom domain**: repo → Settings → Pages → *Custom domain* →
   `thebox.build` (the workflow also writes a `CNAME` file). Leave *Enforce
   HTTPS* on once DNS resolves.
4. **Verify the domain** (so only you can point Pages at it): GitHub → your
   account → Settings → Pages → *Verified domains* → add `thebox.build`; it
   gives you a `TXT` record to add at Namecheap.
5. **Namecheap DNS** (Domain → *Advanced DNS*):
   - the domain-verification `TXT` from step 4 (host `_github-pages-challenge-<you>`);
   - **apex A records** (host `@`) to GitHub Pages:
     `185.199.108.153`, `185.199.109.153`, `185.199.110.153`, `185.199.111.153`;
   - **`CNAME` host `www` → `<you>.github.io`** — required, not optional: with
     an apex custom domain GitHub Pages checks the `www` "alternate name" too,
     and errors ("alternate name isn't configured right") until it resolves.
   Remove Namecheap's default parking/redirect records.
6. **Release**: `git tag v0.1.0 && git push --tags`. The workflow builds,
   creates the Release, and deploys Pages.

### Verify it's live

```sh
curl -fsSL https://thebox.build/install.sh | head        # script, real hashes stamped
curl -fsSL https://thebox.build/install.sh | grep NETBOOT_BASE   # points at the Release
# smoke-test on a throwaway VPS:
curl -fsSL https://thebox.build/install.sh | sudo BOX_ORDERS_B64=<...> sh
```

## Alternatives (later, if you outgrow the free tier)

- **Cloudflare + R2:** move `netboot/` into R2 behind the CDN when download
  traffic warrants it; point `BOX_NETBOOT_BASE` / the stamped URL at R2.
- **Sovereign VPS:** nginx serving the whole `nix build .#site` result. Use
  `scripts/publish.sh` (edit its rsync target). Serve `install.sh` as
  `text/x-shellscript`/`text/plain`, not `application/octet-stream`.
- **A Box hosts it:** a Box behind a cloudflared tunnel serving the bundle —
  best once the initrd is trimmed.

The bundle is a plain directory and `install.sh` takes `BOX_BASE` /
`BOX_NETBOOT_BASE` overrides, so switching hosts is a target change, never a
rewrite.

## Binary cache — trim the initrd (optional but recommended)

By default the netboot installer **embeds the whole Box OS closure**, which
makes the initrd ~1.4 GB. `kexec` has to place that in RAM, so the `curl | sh`
path needs a box with **≥ 8 GB** — a 4 GB VPS OOM-kills it. Standing up a binary
cache lets the installer **fetch the closure at install time** instead, dropping
the initrd to ~540 MB (works on small boxes) and speeding installs and channel
updates.

It auto-activates once a real cache key is present; until then everything stays
fat/embedded and works offline. To turn it on:

1. Create a **free cachix cache** (public, for open source) named `fluxoz` at
   <https://app.cachix.org>. It shows a **public key** like
   `fluxoz.cachix.org-1:AbC…=`.
2. Paste that key into `flake.nix` → `boxCache.trustedPublicKeys` (replacing the
   `REPLACE_WITH_CACHIX_PUBLIC_KEY` placeholder). This flips `cacheReady`, so the
   installer trims and installed boxes gain the update substituter.
3. Add the cache's **auth token** as a repo secret: GitHub → Settings → Secrets
   and variables → Actions → `CACHIX_AUTH_TOKEN` (from cachix → cache → Settings).
4. Commit + tag a release. CI pushes the Box OS closure to the cache and ships
   the trimmed installer.

(If the cache name isn't `fluxoz`, update it in both `flake.nix` and the
`cachix push` step in `.github/workflows/publish.yml`.)

**Cache size:** CI pushes only the paths `cache.nixos.org` doesn't already have
— i.e. *our* paths (boxd, the box-os config), **~85 MB uncompressed / ~50 MB
stored per release** (measured: 55 of the closure's 654 paths). The nixpkgs
bulk (~1.3 GB) stays on cache.nixos.org, which the installer also uses. A free
5 GB cachix cache holds ~100 releases (more, with cross-release dedup).

The deeper win: with the closure on a cache, hosting the netboot artifacts gets
cheap and even "a Box hosts thebox.build" becomes realistic.

## Releases / update channel

Cutting a platform release (what `boxd channel` pulls) is separate from serving
the installer bundle: tag the flake so `nixosConfigurations`/the `the-box` input
resolve to the tag, and (optionally) publish the `site` bundle for that tag to
the mirror. Wiring `channel.rs` to the published release feed is the next
increment.
