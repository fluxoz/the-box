# The agent experience: every pain point from the first real run, and what changed

On 2026-08-12/13 an agent drove the whole journey against real infrastructure
for the first time: register a GitHub App, connect an account, link a private
repository, deploy it on a real Pi, and put it on the internet through a
Cloudflare tunnel. The person at the keyboard did only the things no machine
can do — type a device code, tick repositories, flip a registration checkbox.

Everything below was actually hit, in order. **Fixed** means code shipped;
**flow** means the answer is how the agent should drive the existing tools;
**open** means worth building and not built yet.

The standing lesson, again: every one of these was invisible to unit tests and
obvious within an hour on real hardware.

## The connect flow (forge → Box)

| Pain | Status |
|---|---|
| GitHub App registration form demands Callback/Homepage/Webhook URLs that the device flow never uses | **flow** — any syntactically valid URL works; untick Webhook → Active and the webhook URL stops being required. Only the product owner ever does this. |
| "Enable Device Flow" ships unticked and nothing works without it (`device_flow_disabled`) | **fixed** — the error is translated to "tick it in the application's settings" and treated as terminal, so an agent doesn't retry forever |
| GitHub offers a client secret nobody needs, inviting a leak | **flow** — delete it; a test now fails the build if a client secret ever appears in shipping code |
| Calling `forge_connect` twice invalidated the code someone was mid-typing | **fixed** — idempotent: the same code comes back while it is valid, a fresh one only after expiry. Recovery from an expired code is calling it again |
| Codes expired unused while the person was away (15-minute lifetime, twice) | **fixed** (same change) + **flow** — the agent should treat `forge_connect_status: failed` as "mint a fresh code and re-show it", never as an error to report |
| "0 repositories" had three causes with three different fixes, indistinguishable | **fixed** — `forge_repos` now diagnoses which it is: app not installed (send the install link), nothing ticked (send the link), or the app registration requests no repository permissions (only the app's OWNER can fix that — the message says so) |
| Every Box needs its own device-flow grant (the Pi needed a new code after the dev box) | **by design** — boxes hold their own credentials and never share them; the cost is ~30 seconds per box and the agent smooths it by chaining everything after the code automatically |

## The serving chain (deploy → publish → tunnel → world)

| Pain | Status |
|---|---|
| Two domain-less services → both claimed nginx `default_server` → nginx dead, every site dark | **fixed** (v0.3.8) — one deterministic winner; VM test boots the exact shape that killed the real Box |
| The switch that killed nginx reported success — health only watched boxd | **fixed** (v0.3.8) — any unit the new system enables must actually run, or the switch rolls back |
| Deleting a service never told the OS tier; nginx kept serving the "deleted" site | **fixed** (v0.3.8) — deletion requests the same structural apply a deploy does |
| Tunnel "running" + service "published" + public URL answering 502: three green tools over a dark site | **fixed** — `ingress_status` now reports `origin_listening` and warns; and `verify_service` walks the whole chain and *fetches the real public URL through the real edge*, reporting the first broken link with a fix hint |
| Right after `publish_service`, the OS apply lags the tunnel by ~a minute; an eager fetch sees 502 | **flow** — publish, then poll `verify_service` until the verdict is "reachable", then tell the person the URL. Never hand over an unverified URL |
| A dev machine has no OS tier: tunnels 502 forever, and system checks misread a laptop as a broken Box | **fixed** — diagnostics distinguish "managed Box" from "dev machine" and say which they are looking at |

## Staying current (the update path)

| Pain | Status |
|---|---|
| A Box on an old release had `channel_check` but **no way to apply an update over MCP** — a human had to SSH, which a non-technical user cannot do | **fixed** — `channel_update` applies it as a background job (health-checked, auto-rollback), `job_status` follows it. The last SSH-only operation in the core journey is gone |
| Long operations blocked MCP calls or were invisible | **fixed** — job pattern: kick off, get an id, poll `job_status`, narrate phases to the person |
| Quick-share URLs change when the tunnel restarts (updates included) | **by design** on the free rung — the agent should re-read `ingress_status` after any update and re-share the URL; the real answer is the BYO-domain rung (stable), still blocked on attaching a domain to a Cloudflare account |

## The repo loop

| Pain | Status |
|---|---|
| A repo without `index.html` at its root deploys "successfully" and serves 404s | **fixed** — `link_repo` warns at link time and, when the site clearly lives in `public/`, `dist/`, etc., names the exact `subdir` to pass |
| Poller failures were visible only in server logs; a link failing for a week looked like a healthy one | **fixed** — every sync records its outcome; `list_services` carries `last_sync` (when, ok, commit or error) |
| Repos that need a build step | **open** — the sandboxed builder (trusted image, install-with-network then build-without) is the next big increment; until then `link_repo`'s description says so honestly |

## Flows an agent should know (the recipes)

1. **First contact with a fresh Box**: `get_status` → `forge_options` →
   `forge_connect` (relay code+link, poll status) → `forge_repos` →
   `link_repo` → `verify_service` → done: pushing deploys.
2. **Putting it on the internet**: `ingress_options` (read the trade-offs to
   the person — quick-share's address changes, Funnel is private-by-default,
   BYO-domain is forever) → `ingress_configure` → `publish_service` → poll
   `verify_service` until "reachable" → share the URL.
3. **"It's not working"**: `verify_service` first, always. It names the first
   broken link; `service_logs` and `job_status` fill in the story.
4. **Updates**: `channel_check` → tell the person what's in it → `channel_update`
   → `job_status` until done → `verify_service` on anything published →
   re-share any quick-share URL that changed.
5. **A preview**: `link_repo` again with the same repo, a `branch`, and a new
   service name. Previews are just services.

## Still open, in value order

1. **Adopting an existing Box without SSH** — pairing exists (Console shows a
   code, `/pair/redeem` is machine-readable), but the "agent discovers a Box on
   the LAN and asks to pair" flow has no MCP surface on the agent's side.
2. **The sandboxed build step** — the largest remaining "my repo doesn't
   deploy" class.
3. **BYO-domain live test** (`ingress_setup`) — written, never run against a
   real zone; quick-share URL churn stays until this lands.
4. **Webhook upgrade** once stable ingress exists — push-to-deploy in seconds
   instead of a minute, registered by the Box itself.
5. **`auto_update` on by default for appliance users**, with `get_status`
   carrying a cached "update available" hint so agents mention it naturally.
6. **Token lifetime**: the shipped App has token expiry off; a self-registered
   App with 8-hour tokens would silently kill the poller — refresh-token
   support or a clear `last_sync` error is the guard today.
