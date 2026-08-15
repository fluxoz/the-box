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

## First contact (the bootstrap)

| Pain | Status |
|---|---|
| The 401 signpost says "POST /pair/redeem with JSON `{code}`" — and the endpoint parsed only browser form bodies, so the Box refused the very instructions it had handed out | **fixed** — the redeem endpoint reads `{code}` in either framing (JSON or form); the first request an agent ever authenticates with cannot fail over pedantry |
| An x86 Box came up with **no update channel binding at all** — the first-boot binding lived only in the Pi image module, so `channel_check`/`channel_update` answered "no update channel configured" and nothing over MCP could write one | **fixed** — the binding now comes from the shared platform module; every Box can update itself from birth |

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
| **A failed update left the pin advanced, and every later check said "up to date" while the system ran old code** — self-concealing, hit live | **fixed** — the pin-restore guard now covers the bump step itself (the hole was a `?` that walked out before restoring); `channel_check` additionally reports `running_release` and `channel_update` takes `force: true` to close any pin/system gap by hand |
| **The update job vanished mid-update** — the switch restarts boxd, boxd forgot its in-memory jobs, and `job_status` said "no such job" at the exact moment it mattered | **fixed** — jobs persist to disk; a job found still "running" after a restart is re-marked *interrupted* with a message saying where to look for the outcome (for an update, the restart usually IS the success path) |
| Updating over SSH-as-root hit two Box-OS quirks: nix's libgit2 refuses the boxd-owned data-dir flake input without root `safe.directory`, and git/nix live only on the boxd *unit's* PATH | **obsolete by design** — `channel_update` over MCP runs as the boxd user and hits neither; the SSH path remains documented here for whoever insists |
| **Root-owned files in the data dir killed the boxd-user update path**: the first-boot channel-init ran as root (root-owned channel.toml) and root-run update/apply oneshots regenerate os-config (root-owned tree) — the first MCP `channel_update` then died on "removing os-config: Permission denied", hit live on the Pi | **fixed** — channel-init runs as the boxd user, and both `channel.toml` saves and os-config regeneration chown back to the data-dir owner whoever wrote them (the 7c66195 pattern); VM-asserted. Existing boxes need a one-time `chown -R boxd:boxd /var/lib/boxd/{channel.toml,os-config}` or a root-run update to pick up the fixed binary first |
| **`channel_update` over MCP could never actually switch**: past the ownership fix, the in-process update built the new system as the boxd user and then died registering the system generation — root-only by nature. The tool that ended "the last SSH-only operation" had never completed a real switch | **fixed** — on a managed Box the job now hands the whole update to the root oneshot (`boxd-channel-update.service`, the polkit-blessed path the dashboard button already took) and reports its outcome; in-process remains only where no unit exists (dev). The job-interrupted-by-restart story is unchanged and true. And `/etc/gitconfig` now ships `safe.directory` for the boxd-owned os-config repo, so root's nix accepts the git+file input without the hand-written /root/.gitconfig the first Pi needed |
| Quick-share URLs change when the tunnel restarts (updates included) | **by design** on the free rung — the agent should re-read `ingress_status` after any update and re-share the URL; the real answer is the BYO-domain rung (stable), live since 2026-08-14 |
| Polling means a push takes up to a minute to deploy | **fixed** — `webhook_setup` registers a push webhook (GitHub) at `https://hooks.<zone>/hooks/github`; the receiver authenticates by HMAC signature and syncs the linked services immediately, previews included. The tunnel's edge routes ONLY `/hooks/*` on that hostname to boxd — the console stays off the internet. Polling continues as the fallback, so a lost webhook costs latency, never correctness. Registration needs the App to hold "Repository webhooks: Read & write" (owner-only; the error says so). Not yet run against live GitHub |

## Your own domain (the BYO rung)

| Pain | Status |
|---|---|
| A **working** Cloudflare token was refused as "Invalid API Token" — the Box verified via `/user/tokens/verify`, which only answers for user-owned tokens, and Cloudflare's dashboard now mints account-owned (`cfat_…`) tokens by default | **fixed** — verification now lists zones instead, which proves the token works and holds the Zone:Read the Box needs, for both token kinds |
| People hand over whatever Cloudflare credential they have — live, the "token" was actually the R2 storage token (right account, wrong scopes: could create tunnels, could not touch DNS, expired in a week) | **flow** — `ingress_setup` already reports partial results honestly (`did` + `still_needed`); the agent should read `still_needed` back and, on a permission refusal, send the person the pre-filled token link rather than debugging further. The link creates a user token with exactly the three scopes needed and no expiry |

## Backups (the BYO tier)

| Pain | Status |
|---|---|
| A dead S3 endpoint made the first backup look HUNG: restic retries backend errors with exponential backoff and no output, so `backup_now` sat silent past a ten-minute timeout — live, the endpoint was a fresh R2 account whose S3 TLS **does not exist** until R2 is enabled once in Cloudflare's dashboard | **fixed** — the endpoint is probed with one HTTPS request before restic is involved; "cannot even talk to it" now fails in seconds and the TLS case names the R2 activation step. `backup_now` over MCP is also a proper job now (it claimed to block "until finished", and the one real run outlived its caller) |

## The repo loop

| Pain | Status |
|---|---|
| A repo without `index.html` at its root deploys "successfully" and serves 404s | **fixed** — `link_repo` warns at link time and, when the site clearly lives in `public/`, `dist/`, etc., names the exact `subdir` to pass |
| Poller failures were visible only in server logs; a link failing for a week looked like a healthy one | **fixed** — every sync records its outcome; `list_services` carries `last_sync` (when, ok, commit or error) |
| Repos that need a build step | **fixed and proven live** (2026-08-13, on the real Pi) — `link_repo` takes `build_command` (plus `install_command` / `output_dir` when detection isn't enough) and the build runs on the Box in the sandboxed builder: a trusted Node image shipped in the OS closure, install with the network, build with `--network=none`, hard memory/pids/time limits. First real run: npm pulled 13 packages from the registry, vite 7 built offline in 245 ms, the Box served the hashed bundle. Build failures return the tail of the build log — the compiler's words, not "it failed". |

## The leash (destructive operations)

| Pain | Status |
|---|---|
| Any paired agent could wipe a machine, delete a service, or restore over live data on its own say-so — "your LAN, your trust" was the whole model, which reads badly the moment a stranger's agent is the operator | **fixed** — `provision_machine`, `delete_service` and `backup_restore` now QUEUE for a human tap on the console's Approvals page unless the session was explicitly granted autonomy in the device list. The agent gets a `pending_approval` id, follows it with `approval_status`, and an approval runs the exact call it made. Agents: never re-submit to nag — the queue is the ask; talk to the person instead |

## Flows an agent should know (the recipes)

0. **Bootstrap** — the only two steps that are not MCP, both by nature:
   the machine gets Box OS (installer USB, lab VM, or another Box's
   `provision_machine`), and the agent redeems the pairing code the person
   reads to it (`POST /pair/redeem {code, label}` → session token). An
   unauthenticated request to any Box answers 401 *with these instructions
   in the body*, so first contact is self-explaining. Everything after —
   including OS updates and provisioning further machines — is MCP.
1. **First contact with a fresh Box**: `get_status` → `forge_options` →
   `forge_connect` (relay code+link, poll status) → `forge_repos` →
   `link_repo` → `verify_service` → done: pushing deploys. If the repo is not
   a ready file tree (no committed index.html, a framework in package.json),
   pass `build_command` right in the `link_repo` call — the link fails cleanly
   if the build does, with the log tail, so there is no broken half-service to
   clean up. A build-step link takes as long as the build; that is the call
   working, not hanging.
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
6. **Growing the fleet**: `provision_machine {target, ssh_public_keys}` on any
   existing Box turns a spare machine into a new one — a job whose result is
   the new Box's MCP address and session token. It ERASES the target's disk:
   name the machine and get an explicit yes first. The new Box is then driven
   exactly like this one, from recipe 1.

## Still open, in value order

1. **BYO-domain live test** (`ingress_setup`) — written, never run against a
   real zone; quick-share URL churn stays until this lands.
1a. **Build syncs are synchronous.** A build runs inside the sync that wants
   it: `link_repo`/`sync_repo` hold their MCP call (and the deploy lock) for
   the build's duration, up to the 15-minute phase ceiling. Fine for the small
   sites this rung serves; if real builds prove long, syncs join the job
   pattern like updates did.
3. **Webhook upgrade** once stable ingress exists — push-to-deploy in seconds
   instead of a minute, registered by the Box itself.
4. **`auto_update` on by default for appliance users**, with `get_status`
   carrying a cached "update available" hint so agents mention it naturally.
5. **Token lifetime**: the shipped App has token expiry off; a self-registered
   App with 8-hour tokens would silently kill the poller — refresh-token
   support or a clear `last_sync` error is the guard today.
6. **`provision_machine` live proof** — the tool wraps the CLI flow that was
   proven against real hardware, but the tool itself has not yet wiped a real
   machine end to end. Treat the first run as the verification.
7. **Restore with a repo-linked service** — designed to just work (the link is
   in box.toml, the token re-keys, the clone is refetchable cache) but never
   yet run through destroy-and-recreate.
