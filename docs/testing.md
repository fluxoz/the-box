# Testing the backup + cloud stack

Three layers, cheapest first. The first two run in CI with no network or
external binaries; the third is a real integration run you invoke by hand.

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

## 3. Full-tunnel / on-Box — VM

Two things need a real machine and can't run in the sandbox, same tier as the
install tests:

- **Box Connect tunnel reachability** — needs `tailscaled` + a Headscale
  coordinator actually running; the control-plane provisioning (mint coordinator
  + key) is covered in layers 1–2, but the WireGuard tunnel coming up and
  carrying traffic is a VM test.
- **The systemd timer path** — `boxd-backup` firing `backup run --if-due` on the
  hourly heartbeat, as wired in `nix/module.nix`.
