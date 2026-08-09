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
2. **Enable Pages**: repo → Settings → Pages → *Source: GitHub Actions*.
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
   - optional `CNAME` host `www` → `<you>.github.io`.
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

## Releases / update channel

Cutting a platform release (what `boxd channel` pulls) is separate from serving
the installer bundle: tag the flake so `nixosConfigurations`/the `the-box` input
resolve to the tag, and (optionally) publish the `site` bundle for that tag to
the mirror. Wiring `channel.rs` to the published release feed is the next
increment.
