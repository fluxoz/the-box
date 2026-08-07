# 📦 The Box

Nix-powered, plug-and-play personal server platform. Turn almost any Linux
machine into a reliable host for sites, apps and agent workloads — with atomic
deploys, true rollbacks, and zero Nix knowledge required.

See [PLAN.md](PLAN.md) for the full product plan. This repository currently
implements the **deploy pipeline core**: declarative config → Nix generation
build → atomic switch → one-click rollback, plus the local web dashboard and
JSON API, with static sites as the first template.

## How it works

```
box.toml (desired state)
   │  boxd generates a flake + manifest        <data>/generation-src/
   ▼
nix build                                      → /nix/store/…-box-generation
   │  immutable output tree: manifest.json + services/<name>/www
   ▼
profile switch (atomic symlink swap)           <data>/profiles/box → box-N-link
   │
   ▼
boxd serves sites out of the *current* profile  http://…/sites/<name>/
```

- Every apply produces a numbered, immutable generation; nothing is mutated in
  place. Rollback atomically re-points the profile **and** restores the
  declarative config + sources that generation was built from.
- The generation manifest embeds a full snapshot of `box.toml`, so state
  travels with the artifact.
- On machines without Nix, a pure-Rust `local` backend provides the same
  generation/rollback semantics (also used by the test suite). `--backend auto`
  picks Nix when available.

## Running (dev)

```sh
nix develop                      # rustc, cargo, clippy, rust-analyzer…
cd boxd
cargo run -- --data-dir ../.dev-data serve
# dashboard at http://127.0.0.1:2693
```

Or without entering the shell:

```sh
nix shell nixpkgs#cargo nixpkgs#rustc nixpkgs#gcc -c cargo test
```

### CLI

```
boxd serve [--listen 127.0.0.1:2693]   run dashboard + API + site router
boxd apply                             build & activate a generation from box.toml
boxd status                            current generation + declared services
boxd generations                       list generations (* = current)
boxd rollback <n>                      atomic rollback
```

All commands take `--data-dir` (default `$BOXD_DATA_DIR` or
`~/.local/share/boxd`) and `--backend auto|nix|local`.

### JSON API (the surface the MCP tools will wrap)

```
GET    /api/v1/status
GET    /api/v1/services
POST   /api/v1/services                {"name","index_html"?,"source_path"?,"domain"?,"public"?}
DELETE /api/v1/services/{name}
GET    /api/v1/generations
POST   /api/v1/generations/{n}/rollback
```

Example:

```sh
curl -X POST localhost:2693/api/v1/services \
  -H 'content-type: application/json' \
  -d '{"name":"hello","index_html":"<h1>hi</h1>"}'
# → {"service":"hello","generation":1,"url":"/sites/hello/"}
```

## NixOS module

```nix
{
  inputs.the-box.url = "github:.../the_box";
  # …
  imports = [ the-box.nixosModules.default ];
  services.the-box.enable = true;      # dashboard on 127.0.0.1:2693
}
```

## Repository layout

```
boxd/                the daemon (Rust): CLI, dashboard, API, site router
  src/config.rs      declarative box.toml model
  src/nixgen.rs      config → machine-generated generation flake
  src/store/         generation profiles: nix + local builders, atomic switch
  src/ops.rs         deploy / apply / rollback / delete (shared by UI, API, CLI)
  src/web/           axum server: dashboard pages, /api/v1, /sites/<name>/
nix/module.nix       NixOS module (services.the-box.*)
flake.nix            package + devShell + nixosModules.default
```

## Roadmap toward MVP (from PLAN.md)

- [x] Atomic Nix apply + one-click rollback
- [x] Local web dashboard (first pass) + static-site template
- [x] JSON API groundwork for agents
- [ ] Installer script (curl-to-Box for existing Linux machines)
- [ ] More templates (notes API, photo library, …) with binary caches
- [ ] Cloudflare Tunnel one-token flow (BYO public exposure)
- [ ] Local MCP server wrapping the high-level ops
- [ ] Secrets handling, logs/metrics in dashboard
- [ ] GC-root registration for generation profiles; systemd-managed
      service templates beyond static sites
