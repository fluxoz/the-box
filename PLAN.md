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
- Local reverse proxy (Caddy or Traefik) for routing and TLS where applicable.
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
   - **Cloudflare Tunnel** (recommended free path): User pastes a Cloudflare Tunnel token (or API token). The Box declaratively configures and runs `cloudflared`, maps deployed services to the user’s chosen hostnames, and keeps the config in sync. Traffic flow: Internet → Cloudflare → Tunnel → local Box.
   - **Tailscale Funnel**: Box helps enable Funnel for selected services.
   - **Public IP + reverse proxy**: Box configures Caddy/Traefik on ports 80/443 with automatic Let’s Encrypt; user points domain A record (or DDNS) at their IP.
   - Custom: User points any tunnel or reverse proxy at the ports The Box publishes.

4. The local dashboard itself stays private by default (localhost / LAN / Tailscale). Exposing it publicly is optional.

5. AI agents connect to the local MCP server over localhost, LAN, or private overlay.

**Result**: Fully self-hosted, zero ongoing cost or liability for us, maximum user control.

### Managed Networking (“Box Network”) — Paid

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

## Monetization & Free/Paid Boundary

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

## MVP Scope (Software-First)

1. Reliable installer that turns a Linux machine into a Box.
2. Local web dashboard + 6–10 high-quality templates.
3. Excellent pure BYO support (especially Cloudflare Tunnel one-token flow).
4. Local MCP + basic high-level agent tools.
5. Atomic Nix apply + one-click rollback.
6. Secrets handling and basic monitoring/logs.
7. Optional very limited free trial of managed networking (or none at launch).

Later: richer app store, multi-box orchestration, physical appliances, stronger microVM isolation for untrusted agent code, mobile app.

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

## Open Questions / Next Steps

- Exact free managed trial policy (none vs very limited).
- Preferred data-plane technology for managed tunnels (mTLS + HTTP/2, WireGuard, QUIC).
- Initial set of templates to ship.
- Branding / domain strategy for the managed subdomain space.
- Legal review of ToS and intermediary liability posture.
- Binary cache strategy and pre-building of common templates.

---

*This plan reflects the consensus developed across product, architecture, networking, AI, monetization, and risk discussions. It prioritizes a pure, free, local-first core while creating a clear, high-value paid product around managed public networking.*
