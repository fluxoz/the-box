# Fleeting Boxes together over a mesh

mDNS discovery (`_thebox._tcp`) is zero-config but L2-only — it stops at the
subnet. To run a fleet that spans buildings, sites, or NATs, join every Box to
one **Headscale/Tailscale tailnet** (Box Connect). Discovery then works over the
mesh: boxd reads the fleet from `tailscale status`, filtering to the
`tag:box`-tagged nodes, and federates coarse health over the tailnet exactly as
it does on the LAN. LAN and mesh planes merge; a Box reachable on both shows once.

## The trust model still holds

Being on one tailnet is **network reachability, not trust**. The operator-centric
model (boxes trust *you*, not each other) is enforced at two layers:

- **boxd** — pairing + session tokens still gate every management action,
  regardless of who can reach the port.
- **Headscale ACL** — a *shaped* mesh, not a flat one. The shipped policy grants
  operator devices (`tag:admin`) full access to every Box, but limits Box↔Box to
  the coarse-health/API port (2693) only — which is all peer-federated read needs.
  A compromised Box can read a neighbor's already-coarse health and nothing more.

Full mesh (every Box → every Box, all ports) is a one-line opt-in in the policy;
it is deliberately not the default.

## Paid tier — managed coordinator

The control plane runs Headscale + DERP relays. Each Box:

```sh
boxd cloud connect            # provisions a tagged, ephemeral key + joins
```

Keys are **ephemeral** (a dead Box self-reaps off the tailnet, so the fleet list
never fills with ghosts) and **tagged** `tag:box` (one policy governs the fleet).
Apply the fleet policy to your Headscale once:

```sh
box-cloud fleet-policy | headscale policy set -f -
```

## Free tier — self-hosted coordinator

Run your own Headscale, then join each Box with your own tagged key:

```sh
# on the coordinator
headscale users create fleet
headscale preauthkeys create --user fleet --ephemeral --tags tag:box

# on each Box
boxd connect enroll --server https://headscale.example --authkey <key>
```

Use the same ACL shape as the managed policy (operator → Box full; Box → Box on
2693 only) — it's plain config, reproduced in `box-cloud/policy/headscale-acl.hujson`.

## Verifying

`scripts/test-e2e.sh` covers the key-minting and policy side. The tunnel actually
carrying fleet traffic is a VM test (needs `tailscaled` + a running Headscale) —
see `docs/testing.md`.
