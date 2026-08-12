# The Box — Product Plan

## Vision

The Box is a Nix-powered, plug-and-play personal server platform that turns almost any machine (old PC, mini-PC, Raspberry Pi, VPS, or dedicated appliance) into a reliable, publicly reachable host for static sites, full-stack web apps, APIs, mobile backends, databases, and AI-related workloads.

It is designed for two audiences simultaneously:
- Less technical users who want something close to “plug in and it just works.”
- AI agents that treat The Box as durable, controllable, private compute they can deploy to and operate.

**Core differentiator**: Pure Nix reproducibility (atomic upgrades, true rollbacks, no drift) combined with first-class public exposure and a native AI agent interface, while completely hiding Nix complexity from non-technical users.

---

## Goals & Principles

- Local-first and sovereignty-first by default.
- Public-internet ready without requiring open ports on home networks.
- Non-technical users should never need to see or understand Nix.
- AI agents are first-class citizens (MCP + high-level tools).
- Everything is declarative and versioned as Nix generations.
- Free path remains pure self-hosted with zero dependency on our infrastructure for traffic or control.

---

## Target Users

1. Privacy-conscious / self-hosting users who hate maintenance.
2. Indie hackers and solo developers wanting cheap, durable backends.
3. AI agent builders who need persistent private compute and tool hosting.
4. Non-technical people who want to run a personal website, photo library, notes API, etc. without learning servers.

---

## Core Architecture

### Local Components (always present)
- `boxd` — the core agent/daemon (reliable, long-running).
- Local reverse proxy (**nginx**, generated from the declarative service config) for routing.
- Nix-based service management (modules, systemd services, optional microVMs via microvm.nix for stronger isolation).
- **Local web dashboard** — the primary management UI (a real web application running on the Box).
- Local MCP server + OpenAPI surface for AI agents.
- Configuration stored as a user-owned Nix flake (ideally Git-backed).

### Isolation Options (progressive)
1. Preferred: pure NixOS modules / systemd services.
2. Containers (Podman) as escape hatch.
3. microVMs / Firecracker-style for untrusted or agent-generated code.

### Installation
- Simple installer script or NixOS ISO/appliance image that turns any supported Linux machine into a Box.
- Later: physical “The Box” hardware appliances.

---

## Networking Model

### Pure BYO (Bring Your Own) — Free Forever

Everything runs on the user’s machine. Our infrastructure is never in the data path.

**How pure BYO works:**

1. User installs The Box → local dashboard becomes available at `https://box.local`, the machine’s LAN IP, or over Tailscale/WireGuard.
2. User deploys services via the local web app (templates or natural language). Services run locally.
3. To make a service public, the user selects a BYO method inside the dashboard:
   This is a **ladder**, not one method — see "Public ingress" under Current State. Each rung states what it costs the person (a domain? an account? does the address survive a restart? who terminates the TLS?), because the right answer differs for "show a friend tonight" and "this is where my business lives".

4. The local dashboard itself stays private by default (localhost / LAN / Tailscale). Exposing it publicly is optional.

5. AI agents connect to the local MCP server over localhost, LAN, or private overlay.

**Result**: Fully self-hosted, zero ongoing cost or liability for us, maximum user control.

### Managed Networking (“Box Network”) — Paid *(superseded — see Business Model under Current State)*

- Box initiates an **outbound-only** connection to our edge infrastructure.
- Strong mutual authentication via **mTLS** (preferred): each Box receives a short-lived client certificate bound to its identity after enrollment. Certificates are rotated and can be revoked instantly.
- Alternatives/complements: WireGuard with mutual crypto identity, or QUIC-based tunnels.
- We provide:
  - Automatic subdomains under our domain.
  - Custom domain support (CNAME to our edge + automatic certificates).
  - Public TLS termination, routing, basic DDoS protection, rate limiting.
  - Higher reliability and convenience.

This is the natural monetization point (bandwidth, abuse risk, operational overhead).

---

## Local Web Dashboard & UX

The local web app is the heart of the non-technical experience even in pure BYO mode.

Capabilities:
- One-click / guided template deploys.
- Natural-language chat interface (“I want a private notes API with search at notes.example.com”).
- Service status, logs, metrics, resource usage.
- Tunnel configuration (BYO or managed).
- One-click rollback to any previous Nix generation.
- Secrets management.
- Optional Git integration (config lives in a flake the user owns).

Nix is completely hidden; the dashboard is a friendly frontend that edits the declarative config and applies it safely (dry-run + atomic apply).

---

## AI Agent Interface (MCP)

First-class MCP server (plus OpenAPI) running on the Box.

High-level tools (Nix is abstracted away for most use):
- `list_services`, `get_status`, `get_logs`, `get_metrics`
- `deploy_service(description, source?, resources?, public?, domain?)`
- `update_service`, `rollback`, `delete_service`
- `expose_public`, `manage_secret`
- Lower-level escape hatches for power users/agents (`apply_flake`, `validate_config`)

Agents can:
- Spin up temporary or persistent services.
- Treat The Box as their long-term “home” (state + compute).
- Optionally receive MCP endpoints from the services they create so they can use them immediately.

In pure BYO the MCP server is fully local. In managed mode agents can also interact with the public endpoints we host.

---

## Monetization & Free/Paid Boundary *(superseded — see “Business Model — settled”)*

| Feature                        | Free (BYO)          | Paid (Managed)                  |
|--------------------------------|---------------------|---------------------------------|
| Core software + local dashboard| Yes                 | Yes                             |
| Local MCP / AI interface       | Yes                 | Yes                             |
| Templates & Nix under the hood | Yes                 | Yes                             |
| User’s own Cloudflare / Tailscale / public IP | Yes            | —                               |
| Our subdomains + edge routing  | No                  | Yes                             |
| Custom domains via our edge    | No                  | Yes                             |
| Bandwidth through our infra    | No                  | Yes (metered or tiered)         |
| Multi-box hosted control plane | Optional self-host  | Convenience hosted version      |

**Pricing direction**:
- Free forever for pure self-hosted / BYO.
- Simple subscription tiers for managed networking (Starter / Pro) based on number of public endpoints, bandwidth, custom domains, and support.
- Later: hardware appliances, marketplace, team features, priority support.

**Recommendation**: Be conservative with any free managed public endpoints (or offer only a short limited trial). The excellent free BYO + Cloudflare path is the primary acquisition funnel; convert users who want convenience to paid managed networking.

---

## Security, KYC & Abuse Prevention

- Outbound-only connections (firewall/NAT friendly).
- mTLS (or equivalent mutual auth) for every managed connection + instant revocation.
- Clear Acceptable Use Policy.
- Payment method on file acts as soft KYC for paid tiers.
- Hard rate limits, bandwidth caps, automated heuristics, and rapid suspension tools.
- Connection metadata logging sufficient for abuse response.
- DMCA / notice-and-takedown process.
- User is solely responsible for content; ToS makes this explicit.
- Warnings about home ISP terms of service.

---

## Current State — 2026-08-12

The original MVP scope is **done**. What follows is what actually exists, what is
verified, and what is not — kept honest, because the gap between "built" and
"works for a stranger" is where every real problem has been found.

### Built and working

- **Installer.** `curl … | sudo sh` converts a running Linux machine (kexec →
  disko wipe → Box OS). Verified on a 4 GB VPS from the live `thebox.build`.
  USB/ISO and Raspberry Pi 3/4/5 images. Storage layouts: single, mirror, pool,
  decided on-box or in the order.
- **The reconciler.** Every change is a generation. Two speeds: content edits
  take a fast path; structural changes rebuild the NixOS system through a root
  oneshot and roll themselves back if the box does not come up healthy. This is
  the differentiator — you cannot brick it.
- **Services.** Static sites, reverse-proxied apps, and OCI containers via
  podman, with central port allocation, an nginx reverse proxy, a closed
  firewall, and encrypted per-service secrets (agenix). Catalog presets
  (Postgres, Redis, MinIO).
- **Public ingress — a ladder**, behind one provider seam (`boxd/src/ingress.rs`):
  a share-right-now link needing no account or domain; a Tailscale address
  (stable, no domain, and the only rung where no third party sees plaintext);
  your own domain through your own Cloudflare tunnel. **Two doors:** port 80 is
  your own network, a loopback-only listener is the internet, and a service is
  *absent* from the second unless published — not filtered, absent.
- **Agent-first.** Everything is drivable over MCP: deploy, upload a built
  project, publish it, read logs, roll back, choose and configure a way in. The
  Console hands a non-technical person a paste-ready agent connection.
- **Auth.** Operator pairing, session tokens, security keys (WebAuthn).
  Reaching the console is not authority — every service on the box can reach
  loopback too.
- **Backups.** restic, client-side encrypted, manifest-derived. Destroy-and-
  recreate from a config repo with re-keying.
- **Fleet.** mDNS discovery, coarse public health, tailnet discovery.
- **Release + CI.** One version literal; boxes track a `release` branch that
  only advances when caches are warm; tests and NixOS VM checks run on every
  push.

### Verified against real infrastructure (not mocks)

- A site deployed through the API, served over a **Cloudflare quick tunnel**,
  fetched from the public internet.
- The same over **Tailscale Funnel**, on a real tailnet — and an unpublished
  service confirmed unreachable at the same address.
- Container deploy through boxd, offline, in a VM check.
- OS-tier switch and rollback on a live system.

### Known gaps

- **Your own domain is untested end to end** — blocked on attaching a domain to
  a Cloudflare account. The tunnel connects; nothing has routed to it yet.
- **No git push-to-deploy, no build step, no previews.** The closest thing is
  uploading already-built files. This is the biggest product gap (see Roadmap).
- **No TLS of our own.** Fine while a tunnel terminates it; needed for the
  direct/port-forward path we have not built.
- **No Console page for the ingress ladder** — agents can drive it, people
  cannot.
- **Paid tier is scaffolding.** Box Connect and Box Backup exist end to end in
  code, but against `Mock` providers; real B2, Stripe and a live coordinator are
  not wired.
- The console still lacks: backup integrity check, an add-a-box affordance,
  cache headers on static vhosts.

---

## Business Model — settled

**Everything that makes a Box work is free and self-hosted.** Money comes from
two operated services, both of which put us between the owner and *their own
box* — never between the public and the owner's content:

1. **Box Connect** — private remote access to your own Box, for management and
   development.
2. **Box Backup** — offsite encrypted backups, reselling cheap object storage.

**Public serving of user content stays the user's own** (their domain, their
Cloudflare account). This is deliberate: hosting other people's public content
makes us an intermediary with an abuse, takedown and KYC burden, and the
ingress code is built behind a seam so a managed option *could* be switched on
later without redesign — into the user's own account, never ours.

---

## Roadmap — in priority order

### 1. The deploy loop (Vercel parity where it matters)

Nobody chooses a host for edge middleware or ISR. What people experience as
"Vercel" is a loop: **push to git → it builds → it is live → every branch gets a
preview → roll back instantly.** Match the loop; skip the features.

Designed and adversarially verified; six concrete showstoppers documented before
a line is written. Shape: **the Box hosts the git remote** (works behind a home
router with ingress off, and sidesteps the fact that we have no outbound git
credentials at all), a build runs in a container started by a root oneshot
mirroring the existing os-apply bridge, and a preview is a service that emits no
OS module so it stays on the fast path instead of rebuilding the system on every
push.

Explicitly not building: functions, ISR, image optimization, edge middleware.

### 2. Finish the ingress ladder

Console page for the ladder; the wildcard-hostname flow (one Cloudflare route,
then every later service costs zero steps); surfacing "tunnel connected but
nothing routes to it", which we can already detect and currently show as
healthy.

### 3. Make the paid tier real

Real B2 behind `StorageProvider`, Stripe behind `Billing`, a live coordinator
behind `ConnectProvider`.

### 4. Cheap wins, any time

`Cache-Control` headers on static vhosts — which turns the tunnel we already run
into a free global CDN; headers/redirects/rewrites as declarative nginx.

### 5. Parked deliberately

Storage (add-a-disk, GPU) and the Jetson board. Both planned in detail, both
shelved to fix the core product first.

---

## Exact next steps

1. **Attach a domain to Cloudflare**, then run `~/cf-test.sh <domain>`. The one
   thing to learn: **does a wildcard hostname work on a dashboard-managed
   tunnel?** If yes, adding your second service costs zero Cloudflare steps and
   the domain flow is done. If no, the scoped-API-token path stops being
   optional. Everything else about that rung is already built.
2. **Re-run the two research agents that died** (competitor push-to-deploy
   practice; sandboxing an untrusted build) before building the build step.
   Running someone else's build script is the one part of the loop with no
   external verification behind it.
3. **Build deploy-loop increment 1**: the Box as a git remote — smart HTTP
   behind existing auth, a scoped token so the credential in `.git/config`
   cannot delete every service, and build workspaces added to the config repo's
   gitignore (the data dir *is* a git repo).
4. **Add `Cache-Control` headers.** Small, independent, and it answers the
   latency objection without operating any infrastructure.
5. **Then stop building and go find ten users.** The engineering is not the
   risk; the market is. Expect them to stall at "own a machine" and at "delegate
   your nameservers" — we hit the second one ourselves today, before ever
   seeing a site work.

---

## Differentiation

| Aspect              | Typical self-host PaaS (Coolify, CasaOS, etc.) | The Box                          |
|---------------------|-----------------------------------------------|----------------------------------|
| Base technology     | Docker                                        | Nix generations                  |
| Reproducibility     | Good until drift                              | Strong (atomic, pure)            |
| Rollback            | Partial                                       | Full system or service generations |
| Public exposure     | Supported                                     | Tunnel-first, designed for it    |
| AI / MCP            | Emerging                                      | Native + prompt → validated declarative config |
| Isolation for agents| Containers                                    | Modules → containers → microVMs  |
| Free path purity    | Varies                                        | Fully local, zero dependency on us |

Closest pure-Nix inspiration: Self Host Blocks. The Box focuses far more on GUI, natural language, public tunnels, and AI agents.

---

## Key Risks & Mitigations

- Complexity leakage → Obsessive UX, pre-built binary caches for templates, AI that explains errors in plain English.
- Abuse on managed networking → Conservative free tier (or none), payment as soft KYC, strong automated + human response tools.
- Home ISP / bandwidth / CGNAT → Tunnel-first design; clear guidance; VPS Boxes as alternative.
- Support burden → Excellent templates, documentation, and AI debugger.
- Competition (especially Coolify + MCP) → Win on reproducibility, atomic rollbacks, deeper declarative generation, and “agent home” semantics.

---

## Questions that are now answered

- **Monetization**: Box Connect + Box Backup. Public hosting of user content is
  deliberately not ours.
- **Managed tunnel data plane**: not needed — the free path is BYO, and Connect
  rides WireGuard/Tailscale.
- **Templates to ship**: static-site, reverse-proxied-app, container, plus a
  data-driven catalog anyone can extend.
- **Domain**: `thebox.build`, live, serving the installer and landing page.
- **Binary cache**: `fluxoz.cachix.org`, with the release pipeline pushing
  closures so a Box never compiles a kernel.

## Still open

- Whether a wildcard hostname works on a dashboard-managed Cloudflare tunnel
  (decides the domain onboarding flow — see Exact next steps).
- Legal/ToS review, if a managed ingress option is ever switched on.
- Whether the target user is really the non-technical vibe coder, or the
  homelabber who already owns hardware. Unresolved by argument; resolvable only
  by talking to people.

---

*Originally a consensus vision doc; now maintained as the live picture of what
exists, what is verified, and what is next. Detail lives in the commit history
and in `docs/`.*