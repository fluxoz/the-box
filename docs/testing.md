# Testing the backup + cloud stack

Three layers, cheapest first.

`.github/workflows/test.yml` runs layer 1 (the whole workspace, plus clippy) and
the NixOS VM checks on every push and pull request. Layer 2 needs real network
endpoints, so you invoke it by hand.

Two things worth knowing about how these can lie to you, both since fixed:
a test that shells out to a missing binary and returns early reports success
having checked nothing (the secrets tests did this in the Nix build, where
`age` was absent), and a test that constructs the system by hand does not
exercise the path a user takes (every container test wired the NixOS module
directly, so deploying a container through boxd was broken for a long time
without a single failing test).

## 1. Pure-logic unit tests — `cargo test`

Fast, hermetic, no I/O. They pin the decisions that are easy to get subtly
wrong and impossible to notice at runtime:

- **`boxd` — `backup.rs`**: restic repository-URL construction for every backend
  (the plain-HTTP-S3 scheme-preservation regression lives here), the
  scheduled-backup due-check, and manifest-derived backup path selection.
- **`box-cloud` — `main.rs`**: the full HTTP contract via `tower`'s `oneshot`
  (no socket) — one-time enrollment, the Bearer auth gate, the billing gate
  (402), idempotent + account-scoped provisioning, and connect-key minting.

```sh
nix develop -c cargo test -p boxd
cd ../box-cloud && nix develop ../the_box -c cargo test
```

## 2. End-to-end integration — `scripts/test-e2e.sh`

Drives the **real** binaries — `restic`, `rclone`, `boxd`, `box-cloud` — through
the flows a user actually hits, asserting on ciphertext-on-disk, a scoped S3
roundtrip over a live endpoint, managed enrollment, and connect provisioning.
Pulls restic/rclone from nixpkgs if absent; builds `box-cloud` from its sibling
checkout (`$BOX_CLOUD_DIR`, default `../box-cloud`).

```sh
nix develop -c bash scripts/test-e2e.sh
```

The load-bearing assertion in every backup case is the same: a known plaintext
sentinel written before the backup must **not** appear anywhere in the repo on
disk — proof the encryption is actually happening, not just claimed.

## 3. NixOS VM checks — `nix build .#checks.x86_64-linux.<name>`

Real Boxes booted in QEMU. `nix flake show` lists them; the one that matters
most for services is **`deploy-through-boxd`**, which deploys a container the
way a user does (through the API) and asserts that what boxd reports back is
true. It runs with no network on purpose: a Box has to be able to build a
generation without reaching out to anything.

## 4. Full-tunnel / on-Box — a real machine

Two things need a real machine and can't run in the sandbox, same tier as the
install tests:

- **Box Connect tunnel reachability** — needs `tailscaled` + a Headscale
  coordinator actually running; the control-plane provisioning (mint coordinator
  + key) is covered in layers 1–2, but the WireGuard tunnel coming up and
  carrying traffic is a VM test.
- **The systemd timer path** — `boxd-backup` firing `backup run --if-due` on the
  hourly heartbeat, as wired in `nix/module.nix`.
