# Installing Box OS

Box OS is the dedicated appliance: the machine runs nothing but The Box, and
the **entire operating system is a Nix generation** — kernel, services and
boxd upgrade and roll back atomically together.

Every install path is driven by the same two artifacts:

- **The installer image** — a NixOS system that boots entirely into RAM,
  carries the full Box OS closure (installs 100% offline), wipes the chosen
  disk with [disko](https://github.com/nix-community/disko) and reboots into
  Box OS. Built as an ISO (`nix build .#installer-iso`), as PXE netboot
  artifacts (`nix build .#installer-netboot`), or as a Windows staging payload
  (`nix build .#installer-windows`).
- **The handoff file** — `box-install.json`, the single unit of consent and
  configuration. No handoff, no disk touched, ever.

## The handoff file (agent-first by design)

The handoff is what makes installs automatable: a human clicks through a
staging UI exactly once, or an agent writes a JSON file. Same file either way.

```json
{
  "erase_disk": true,
  "disk": "auto",
  "hostname": "auto",
  "wifi": { "ssid": "HomeNet", "password": "..." },
  "static_ip": { "address": "192.168.1.50/24", "gateway": "192.168.1.1", "dns": ["1.1.1.1"] },
  "ssh_authorized_keys": ["ssh-ed25519 AAAA... agent@controller"],
  "cloudflare_tunnel_token": "eyJh...",
  "min_disk_gb": 8,
  "force": false,
  "finish": "reboot"
}
```

| Field | Default | Meaning |
|---|---|---|
| `erase_disk` | — | **Required, must be `true`.** The consent bit; the installer refuses to run without it. |
| `disk` | `"auto"` | Target disk. `"auto"` or an explicit path (prefer `/dev/disk/by-id/...`). |
| `hostname` | `"box"` | mDNS name. `"auto"` derives `box-<6 chars of machine-id>` — use for batch installs so machines don't collide on `box.local`. |
| `wifi` | none | Materialized as a NetworkManager profile on first boot. Ethernet needs nothing. |
| `static_ip` | DHCP | Pins the LAN address at first boot: `address` (IPv4/prefix, required), `gateway`, `dns` (list; defaults to the gateway). Applies to Wi-Fi when `wifi` is set, otherwise to wired. |
| `ssh_authorized_keys` | `[]` | Keys land in `/etc/box/authorized_keys`; password auth is disabled everywhere. This is how agents keep managing the Box after install. |
| `cloudflare_tunnel_token` | none | Seeds boxd's secret store on first boot: the Box comes up already publicly reachable. |
| `min_disk_gb` | `8` | Auto-selection ignores smaller disks. |
| `force` | `false` | Reinstall over an existing Box OS (otherwise the installer refuses, which also makes leaving the USB stick in harmless). |
| `finish` | `"reboot"` | `reboot` \| `poweroff` \| `none`. |

The handoff is copied to `/etc/box/install-config.json` on the installed
system and applied by `box-firstboot` on every boot — the Box OS closure
itself is generic, which is why one image serves every machine.

### How the installer finds the handoff

1. `box.install-url=<http url>` on the kernel command line (PXE/batch path)
2. A filesystem labeled `BOX-INSTALL` containing `box-install.json` (USB path
   — can be the installer stick itself or a second stick)
3. With `box.install-scan` on the command line: any partition containing
   `box-installer/box-install.json` (staged-from-Windows path)

The handoff is copied to RAM before any disk is touched — it may live on the
disk being erased.

## Disk selection and layout (disko)

**Selection policy** — `"disk": "auto"` picks the *largest internal
(non-removable) disk* of at least `min_disk_gb`. USB media are never
candidates. For machines with several disks where largest-wins is not what
you want, set `disk` explicitly; agents should use `/dev/disk/by-id/...`
paths, which are stable across boots and hardware re-enumeration.

**Layout** — `nix/disko-template.nix`, parameterized only by device:
GPT → 1G ESP (vfat, label `BOX-ESP`) → rest ext4 (label `box-root`).
Box OS mounts by label, so the same system closure works on any disk. The
template is deliberately boring for the MVP; variants (swap, LUKS, btrfs/ZFS,
mirrored disks) are additional templates selected by a future handoff field,
not runtime cleverness.

**Safety rules**, in order: no handoff → nothing happens; `erase_disk` not
`true` → nothing happens; target already carries Box OS and `force` is not
set → nothing happens. The one irreversible step is disko's wipe, and it runs
only after all three gates pass.

## Install paths

### USB / virtual media (any machine, the recovery path)

```sh
nix build .#installer-iso
# write result/iso/*.iso to a USB stick, add box-install.json to a volume
# labeled BOX-INSTALL, boot the target machine from USB
```

### From a running Windows install (the "old Windows laptop" path)

`nix build .#installer-windows` produces `stage.ps1` + payload
(`grubx64.efi`, `bzImage`, `initrd`, `box-marker`). Put them and a
`box-install.json` in one directory and run `stage.ps1` as Administrator.

It stages the payload on `C:\box-installer` (the ESP is too small for the
initrd; GRUB reads NTFS), puts GRUB on the ESP, and registers a **one-shot
firmware boot entry** — the same stage-and-reboot pattern Windows uses for
its own upgrades. If the installer never runs (e.g. Secure Boot blocked it),
the next boot falls back into Windows untouched; Secure Boot must currently
be disabled in firmware first. UEFI only; legacy-BIOS machines use the USB
path.

Two edge cases worth knowing: if Windows has **pending updates**, its own
update-install reboot can win the first reboot ahead of our one-shot entry —
re-run `stage.ps1` once updates settle. And **Secure Boot** must be off until
we ship a signed shim (the staged GRUB is unsigned). Both are detected and
called out by the stager before it does anything destructive.

### PXE / batch (fleets)

```sh
nix build .#installer-netboot   # bzImage, initrd, netboot.ipxe
```

Serve kernel + initrd from any PXE/iPXE setup and put
`box.install-url=http://<server>/box-install.json` on the kernel command
line. Every machine that netboots installs itself and comes up on mDNS —
with `"hostname": "auto"` they self-name (`box-3f2a1b.local`). Per-machine
handoffs (keyed by MAC in your HTTP server) give per-machine config; one
shared handoff gives identical appliances. An agent with DHCP/HTTP access on
the LAN can drive an entire rack this way without touching a keyboard.

## After install

The Box advertises `http://<hostname>.local:2693` (dashboard, JSON API, and
MCP at `/mcp`). SSH is key-only via the handoff keys; the local console
auto-logs-in as root for physical recovery. If a tunnel token was in the
handoff, the Box is publicly reachable from first boot.
