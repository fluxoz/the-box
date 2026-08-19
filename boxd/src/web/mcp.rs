//! Local MCP server (streamable HTTP transport, JSON-RPC 2.0) exposing
//! high-level agent tools over POST /mcp. Stateless: no session tracking,
//! single JSON responses (no SSE) — sufficient for request/response tools.
//! The tools wrap the same ops used by the dashboard, API and CLI.

use axum::{
    extract::State,
    http::StatusCode,
    response::{IntoResponse, Response},
    Json,
};
use serde_json::{json, Value};

use crate::channel;
use crate::config::BoxConfig;
use crate::manifest;
use crate::ops;
use crate::ostier;
use crate::store;
use crate::templates;

use super::{blocking, SharedState};

pub const PROTOCOL_VERSION: &str = "2025-06-18";

pub async fn method_not_allowed() -> Response {
    // GET /mcp opens an SSE stream in the full spec; this server does not
    // push server-initiated messages, so advertise that with a 405.
    StatusCode::METHOD_NOT_ALLOWED.into_response()
}

pub async fn end_session() -> Response {
    StatusCode::NO_CONTENT.into_response()
}

pub async fn handle(
    State(state): State<SharedState>,
    caller: Option<axum::Extension<crate::web::Caller>>,
    Json(message): Json<Value>,
) -> Response {
    let caller = caller
        .map(|axum::Extension(c)| c)
        .unwrap_or(crate::web::Caller {
            // No session extension means an unauthenticated path let this through
            // (tests drive the router directly); treat it as a non-autonomous
            // stranger so the destructive gate still holds.
            id: String::new(),
            label: "unknown".into(),
            autonomous: false,
            provenance: crate::auth::Provenance::Operator,
        });
    // Notifications (no id) need no response body.
    let Some(id) = message.get("id").cloned() else {
        return StatusCode::ACCEPTED.into_response();
    };
    let method = message
        .get("method")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string();
    let params = message.get("params").cloned().unwrap_or(Value::Null);

    let result: Result<Value, (i64, String)> = match method.as_str() {
        "initialize" => Ok(json!({
            "protocolVersion": PROTOCOL_VERSION,
            "capabilities": { "tools": {} },
            "serverInfo": {
                "name": "the-box",
                "title": "The Box",
                "version": env!("CARGO_PKG_VERSION"),
            },
            "instructions": "Deploy and manage services on this Box. Call list_templates to see what you can deploy, then deploy(name, template, params). Deploys are atomic Nix generations; any generation can be rolled back. Deployed sites are served at /sites/<name>/ and, when a domain is set and a tunnel is configured, at that public domain. channel_status / channel_check report the platform update channel (applying updates is done by the Box itself, not via MCP).",
        })),
        "ping" => Ok(json!({})),
        "tools/list" => Ok(json!({ "tools": tool_definitions() })),
        "tools/call" => call_tool(state, params, caller).await,
        _ => Err((-32601, format!("method not found: {method}"))),
    };

    let body = match result {
        Ok(result) => json!({ "jsonrpc": "2.0", "id": id, "result": result }),
        Err((code, msg)) => {
            json!({ "jsonrpc": "2.0", "id": id, "error": { "code": code, "message": msg } })
        }
    };
    Json(body).into_response()
}

fn tool_definitions() -> Value {
    let no_args = json!({ "type": "object", "properties": {}, "additionalProperties": false });
    json!([
        {
            "name": "get_status",
            "description": "Current status of this Box: active generation, declared services, generation builder backend.",
            "inputSchema": no_args,
        },
        {
            "name": "list_services",
            "description": "List all declared services with their template, domain, URL and whether they are active in the current generation.",
            "inputSchema": no_args,
        },
        {
            "name": "list_templates",
            "description": "List the service templates this Box can deploy — each with its id, title and description. Use the id with the deploy tool.",
            "inputSchema": no_args,
        },
        {
            "name": "deploy_static_site",
            "description": "Create or update a static-site service and activate it atomically as a new generation. Provide index_html for a single-page deploy, or source_path to copy a directory on the Box as the site root.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "name": { "type": "string", "description": "Service name: 1-32 chars of a-z, 0-9 and '-'" },
                    "index_html": { "type": "string", "description": "HTML content served as index.html" },
                    "source_path": { "type": "string", "description": "Absolute path to a directory on the Box to copy as the site root (overrides index_html)" },
                    "domain": { "type": "string", "description": "Public domain to route to this service for tunnel traffic, e.g. site.example.com" }
                },
                "required": ["name"],
                "additionalProperties": false,
            },
        },
        {
            "name": "deploy",
            "description": "Create or update a service from any template and activate it atomically as a new generation. Call list_templates first to see valid template ids and their params.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "name": { "type": "string", "description": "Service name: 1-32 chars of a-z, 0-9 and '-'" },
                    "template": { "type": "string", "description": "Template id from list_templates, e.g. 'static-site'" },
                    "params": { "type": "object", "description": "Template-specific parameters (see list_templates)" },
                    "domain": { "type": "string", "description": "Public domain to route to this service, e.g. site.example.com" },
                    "public": { "type": "boolean", "description": "Whether the service is intended to be publicly exposed" },
                    "port": { "type": "integer", "description": "For process-backed templates (reverse-proxied-app): an explicit port to run on. Omit to let the platform assign a free one. Rejected if reserved, privileged, colliding, or set on a file service." }
                },
                "required": ["name", "template"],
                "additionalProperties": false,
            },
        },
        {
            "name": "upload_files",
            "description": "Put files on the Box and (by default) publish them as a static site. This is how you deploy a project you just built — a Next.js/Vite export, a generated site, any folder of files — without needing SSH. Send the built output, not the source tree. Text files can be sent as plain strings; anything binary (images, fonts) as {\"base64\": \"...\"}. Paths are relative to the site root, so 'index.html' and 'assets/app.js'.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "name": { "type": "string", "description": "Service name: 1-32 chars of a-z, 0-9 and '-'" },
                    "files": {
                        "type": "object",
                        "description": "Map of relative path to contents. A string is text; {\"base64\": \"...\"} is raw bytes.",
                    },
                    "replace": { "type": "boolean", "description": "Replace everything previously uploaded for this service (default true). An upload is a whole site, not a layer on the last one." },
                    "deploy": { "type": "boolean", "description": "Publish the uploaded files immediately as a static site (default true)." },
                    "domain": { "type": "string", "description": "Public domain to route to this service, e.g. site.example.com" },
                    "public": { "type": "boolean", "description": "Let people outside this network reach it through the Box's tunnel." }
                },
                "required": ["name", "files"],
                "additionalProperties": false,
            },
        },
        {
            "name": "ingress_options",
            "description": "The ways this Box can be put on the internet, and what each one costs the person: whether they need a domain, whether they need an account, whether the address survives a restart. Read this before offering someone a choice — the right answer depends on whether they own a domain and whether the link is for showing a friend or for telling people where to find them.",
            "inputSchema": no_args,
        },
        {
            "name": "ingress_status",
            "description": "How this Box is currently reachable from the internet: which way in is configured, whether it is working, and the public address of every service that has been published.",
            "inputSchema": no_args,
        },
        {
            "name": "ingress_connect_account",
            "description": "Connect the person's Cloudflare account to this Box, so the Box can set publishing up for them instead of them doing it in Cloudflare's dashboard. Call with no arguments FIRST to get the token-creation links and instructions. The best path is the PARENT token (one permission: API Tokens Edit): the Box then mints its own exactly-scoped working token and remints it automatically if it ever breaks — no human ever debugs Cloudflare permissions again. A directly-scoped token works too. Tokens are stored encrypted on the Box; you cannot read them back, which is deliberate: they can rewrite DNS for every domain on the account.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "api_token": { "type": "string", "description": "The Cloudflare API token they created. Omit to just get the link and instructions." }
                },
                "additionalProperties": false,
            },
        },
        {
            "name": "ingress_setup",
            "description": "Set publishing up from nothing on a connected account: creates the tunnel, points the whole domain at this Box, stores the tunnel's credential, and creates the DNS record. One call replaces six steps in Cloudflare's dashboard. Requires ingress_connect_account first. Reports exactly what it changed on their account, and what (if anything) is still theirs to do.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "zone": { "type": "string", "description": "The domain they own, e.g. example.com. It must already be on their Cloudflare account." },
                    "hostname": { "type": "string", "description": "DNS label to create; defaults to '*' so every service gets <service>.<domain> with no further setup." },
                    "enable": { "type": "boolean", "description": "Start the tunnel when setup succeeds (default true)." }
                },
                "required": ["zone"],
                "additionalProperties": false,
            },
        },
        {
            "name": "ingress_configure",
            "description": "Choose how this Box is reachable from the internet, and turn it on or off. Call ingress_options first and help the person pick: it depends on whether they own a domain and whether the link is for showing someone once or for telling people where to find them. Refuses with a reason when the chosen way in cannot work yet.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "provider": { "type": "string", "description": "An id from ingress_options, e.g. quick-share" },
                    "enable": { "type": "boolean", "description": "Turn it on (default true) or off" },
                    "zone": { "type": "string", "description": "For ways in that use your own domain: the domain itself, e.g. example.com. Services are then published at <service>.example.com." }
                },
                "required": ["provider"],
                "additionalProperties": false,
            },
        },
        {
            "name": "webhook_setup",
            "description": "Upgrade a repo-linked service from ~1-minute polling to push-to-deploy in seconds, AND turn on pull-request previews: registers a webhook (push + pull_request events) pointing at this Box's receiver at https://hooks.<zone>/hooks/github. From then on every same-repo PR gets its own preview at <service>-pr-<N>.<zone> (built with the parent's recipe, commented on the PR when the App may, removed when the PR closes). Needs BYO-domain ingress configured (the hooks route rides the tunnel; ingress_setup adds it) and the GitHub App must hold 'Repository webhooks: Read & write' — if it does not, the error names that owner-only fix. Polling continues as the fallback either way, so a lost webhook only ever costs latency.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "name": { "type": "string", "description": "A repo-linked service" }
                },
                "required": ["name"],
                "additionalProperties": false,
            },
        },
        {
            "name": "forge_options",
            "description": "List the places this Box can deploy code from (GitHub, GitLab), what each one needs before it will work, and whether an account is already connected. Call this first when the person wants to deploy from their existing repository. Read 'shares_only_chosen_repos' out loud when it is false — on that forge the Box will be able to read every project they can see, which is the forge's model and not something the Box chooses.",
            "inputSchema": { "type": "object", "properties": {}, "additionalProperties": false },
        },
        {
            "name": "forge_connect",
            "description": "Start connecting the person's GitHub or GitLab account. Returns a short code and a link IMMEDIATELY — it does not wait for them. Give them both, tell them to open the link and type the code, then call forge_connect_status to see whether they have finished. The code expires in about fifteen minutes. If the Box has no application configured for that forge this call says exactly what is missing; for a self-hosted GitLab you must pass client_id (and base_url) once, which the person gets by adding an application in their GitLab settings.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "provider": { "type": "string", "description": "An id from forge_options: 'github' or 'gitlab'" },
                    "base_url": { "type": "string", "description": "For a self-hosted forge, where it lives, e.g. https://git.example.com. Stored for next time." },
                    "client_id": { "type": "string", "description": "OAuth application/client id, if this Box needs its own. Stored for next time. Not a secret." },
                    "app_slug": { "type": "string", "description": "GitHub App slug, used to build the link for sharing more repositories." }
                },
                "required": ["provider"],
                "additionalProperties": false,
            },
        },
        {
            "name": "forge_connect_status",
            "description": "Check whether the person has finished authorizing, and store the token if they have. Poll this rather than waiting; leave a few seconds between calls, because the forge will refuse polls that come too fast. Reports 'waiting' with the code repeated back so you can re-show it, 'connected' when done, or 'failed' with a reason that is worth reading — 'device flow not enabled' means a checkbox is missing on the application and retrying will never help.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "provider": { "type": "string", "description": "'github' or 'gitlab'" }
                },
                "required": ["provider"],
                "additionalProperties": false,
            },
        },
        {
            "name": "forge_repos",
            "description": "List the repositories a connected account has shared with this Box, ready to deploy from. An empty list on GitHub is the normal first-connect case rather than an error: it means they authorized the app but picked no repositories, and the returned 'share_more_url' is the one-click fix — send it to them.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "provider": { "type": "string", "description": "'github' or 'gitlab'" }
                },
                "required": ["provider"],
                "additionalProperties": false,
            },
        },
        {
            "name": "link_repo",
            "description": "Deploy a repository from a connected forge as a service, and keep it deployed: the Box checks for new commits about once a minute and redeploys automatically, so after this one call, pushing to the branch IS deploying. Static file trees deploy as-is; a repository that needs a build first (Vite, Astro, a static-export Next site, anything whose site is not committed) gets one by passing build_command — the build runs on the Box in a sandboxed container (Node, npm/yarn/pnpm; install runs with the network, the build itself without, hard memory and time limits) and the resulting directory is what gets served. Build failures come back with the tail of the build log. The repository must already be shared with this Box (see forge_repos). Creates the service if it does not exist. The service starts on the local network only unless public is set; publishing is a deliberate act. A box.toml IN the repository ([build] command/install/output_dir/subdir) declares the same things as code: explicit arguments here win, the file fills the gaps, and committing a change to it changes the next deploy. PREVIEWS are just this call again: same repo, a branch, a new service name (e.g. myapp-pr42) — every branch can have its own URL; unlink_repo + delete_service cleans one up.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "name": { "type": "string", "description": "Service name: 1-32 chars of a-z, 0-9 and '-'" },
                    "forge": { "type": "string", "description": "'github' or 'gitlab'" },
                    "repo": { "type": "string", "description": "owner/name, exactly as forge_repos lists it" },
                    "branch": { "type": "string", "description": "Branch to track. Defaults to the repository's default branch." },
                    "subdir": { "type": "string", "description": "Without build_command: deploy this subdirectory instead of the repo root, e.g. 'public' for a repo that commits its built site. With build_command: run the build in this subdirectory (the monorepo case)." },
                    "build_command": { "type": "string", "description": "Build the site on every new commit with this command, e.g. 'npm run build', in the sandbox. Dependencies are installed first (from the lockfile: npm ci / yarn / pnpm; override with install_command)." },
                    "install_command": { "type": "string", "description": "Override the detected dependency install step that runs before build_command." },
                    "output_dir": { "type": "string", "description": "Where the build writes the site, relative to the app root, e.g. 'dist'. Detected (dist/, build/, out/, public/, …) when omitted." },
                    "domain": { "type": "string", "description": "Serve at this domain (must be configured for ingress)." },
                    "public": { "type": "boolean", "description": "Publish to the internet immediately (default false)." }
                },
                "required": ["name", "forge", "repo"],
                "additionalProperties": false,
            },
        },
        {
            "name": "sync_repo",
            "description": "Check a repo-linked service for new commits right now instead of waiting for the next automatic poll, and deploy if there is anything new. Reports up_to_date with the current commit, or deployed with the new one. Useful right after someone says they pushed.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "name": { "type": "string", "description": "A repo-linked service" }
                },
                "required": ["name"],
                "additionalProperties": false,
            },
        },
        {
            "name": "unlink_repo",
            "description": "Stop keeping a service in step with its repository. The service and its currently deployed content stay exactly as they are — this stops future automatic deploys, it does not take anything down or delete the service.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "name": { "type": "string", "description": "A repo-linked service" }
                },
                "required": ["name"],
                "additionalProperties": false,
            },
        },
        {
            "name": "forge_disconnect",
            "description": "Forget a connected account, deleting the stored token from this Box. Tell the person plainly that this does NOT revoke anything at the forge — only they can do that, from their own account settings — so if the worry is a leaked token, deleting it here is not sufficient.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "provider": { "type": "string", "description": "'github' or 'gitlab'" }
                },
                "required": ["provider"],
                "additionalProperties": false,
            },
        },
        {
            "name": "publish_service",
            "description": "Put a deployed service on the internet, or take it off. This is the only thing that makes something reachable by anyone, so do it deliberately and tell the person what the address is. A service that is not published is served only on their own network.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "name": { "type": "string", "description": "Name of a deployed service" },
                    "public": { "type": "boolean", "description": "true to publish, false to take it off the internet (default true)" },
                    "domain": { "type": "string", "description": "The hostname it should answer on, when the configured way in uses your own domain, e.g. app.example.com" }
                },
                "required": ["name"],
                "additionalProperties": false,
            },
        },
        {
            "name": "verify_service",
            "description": "Prove a service is actually reachable, end to end, instead of assuming from configuration. Checks the whole chain — is the system apply finished, is the web server running, is anything listening where the tunnel points, is the tunnel up — and then FETCHES the service's real URLs, including the public one through the actual internet edge. Call this after deploying or publishing, and any time someone says 'it's not working': the verdict names the first broken link in the chain, which beats reading four green statuses over a dark site. May take ~15 seconds when it fetches the public URL.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "name": { "type": "string", "description": "The service to verify" }
                },
                "required": ["name"],
                "additionalProperties": false,
            },
        },
        {
            "name": "channel_update",
            "description": "Apply a platform update: rebuild this Box's system from its update channel and switch to it, rolling back automatically if the new system fails its health check. Call channel_check first to see whether there is anything to update to. This takes minutes, so it runs as a background job — you get a job id back immediately; poll job_status to follow it. The switch restarts boxd, so expect one blip mid-job: if job_status briefly errors and then reports the job as interrupted, check get_status — a new version there means the update SUCCEEDED. Safe by construction (every generation remains bootable and a failed switch rolls itself back), but tell the person before you run it: services blip during the switch, and a demo link from the quick-share rung gets a NEW address afterward.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "force": { "type": "boolean", "description": "Rebuild and switch even when the pin says current — the fix when running_release lags the pin (default false)." }
                },
                "additionalProperties": false,
            },
        },
        {
            "name": "provision_machine",
            "description": "Turn a spare machine on the network into ANOTHER Box, hands-off: ship it an identity over SSH, run the takeover installer, wait for it to boot as a Box, and pair with it. THIS ERASES THE TARGET'S DISK — name the machine to the person and get their explicit yes before calling (a wrong address here wipes the wrong computer; this Box's network may contain machines that matter). Requirements: the target must be a Linux machine this Box can reach as root over SSH, and at least one SSH public key to authorize on the new Box. Runs as a background job (up to ~15 minutes): poll job_status; the finished job's message carries the new Box's address and a session token — manage it by connecting a NEW MCP endpoint at http://<address>/mcp with that token, exactly like this one. Unless this session has autonomy the call QUEUES for a human tap first (pending_approval id — follow with approval_status); the person's console approval IS the explicit yes.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "target": { "type": "string", "description": "SSH target, e.g. root@192.168.1.42" },
                    "ssh_public_keys": { "type": "array", "items": { "type": "string" }, "description": "Public keys to authorize on the new Box (the operator's, so a human can always get in)" },
                    "hostname": { "type": "string", "description": "Hostname for the new Box; 'auto' (default) derives a stable per-machine name" },
                    "layout": { "type": "string", "description": "Storage layout: single | mirror | pool (default: decided on-box)" },
                    "static_ip": { "type": "object", "description": "Pin the new Box's LAN address instead of DHCP. The Box comes up at this address (provisioning waits there, not at the target's old lease).", "properties": {
                        "address": { "type": "string", "description": "IPv4 with prefix, e.g. 192.168.1.50/24" },
                        "gateway": { "type": "string", "description": "Default gateway (also the DNS fallback)" },
                        "dns": { "type": "array", "items": { "type": "string" }, "description": "DNS servers; default: the gateway" }
                    }, "required": ["address"], "additionalProperties": false }
                },
                "required": ["target", "ssh_public_keys"],
                "additionalProperties": false,
            },
        },
        {
            "name": "approval_status",
            "description": "Check on a destructive call that was queued for the operator's approval (you got a pending_approval id back instead of a result). pending means the person has not decided; approved carries the executed call's result; denied means no — respect it and tell the person why you asked. Do not re-submit the same call to nag; ask the human instead.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "id": { "type": "string", "description": "The pending_approval id a destructive tool returned" }
                },
                "required": ["id"],
                "additionalProperties": false,
            },
        },
        {
            "name": "router_configure",
            "description": "Give the Box's /v1 endpoint a cloud fallback: when no local model server is running, requests forward to this OpenAI-compatible endpoint with the owner's own key. Local models always win when present. Pass enabled:false to stand it down.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "base_url": { "type": "string", "description": "OpenAI-compatible base, e.g. https://api.x.ai/v1" },
                    "api_key": { "type": "string", "description": "The owner's key for that endpoint (stored encrypted)" },
                    "enabled": { "type": "boolean", "description": "Default true" }
                },
                "additionalProperties": false,
            },
        },
        {
            "name": "router_status",
            "description": "Where this Box's /v1 traffic has been going: local vs cloud request and token counts, and a conservative estimate of dollars saved by serving locally.",
            "inputSchema": no_args,
        },
        {
            "name": "console_remote",
            "description": "Serve this Box's console at https://console.<zone> through the tunnel (or stop). The point is passkeys: Face ID, fingerprints and security keys only work on an https origin, so a phone needs this to sign in without codes. Off by default; the console's own sessions and CSRF rules front it, and proxied traffic is never treated as local.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "enabled": { "type": "boolean", "description": "Default true" }
                },
                "additionalProperties": false,
            },
        },
        {
            "name": "memory_save",
            "description": "Leave a note in the Box's memory vault: durable, private, never leaves the machine. For context worth keeping across sessions and agents ('the staging DB is the one named blue', 'the owner prefers tabs'). Recall with memory_search.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "text": { "type": "string", "description": "The note itself, one fact per note" },
                    "tags": { "type": "array", "items": { "type": "string" }, "description": "Optional labels for recall" }
                },
                "required": ["text"],
                "additionalProperties": false,
            },
        },
        {
            "name": "memory_search",
            "description": "Recall notes from the Box's memory vault. Every query term must match (text or tags); most recent first. Keyword recall today, semantic when the Box has an embedding model.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "query": { "type": "string", "description": "Terms to match" },
                    "limit": { "type": "integer", "description": "Max notes (default 10)" }
                },
                "required": ["query"],
                "additionalProperties": false,
            },
        },
        {
            "name": "memory_forget",
            "description": "Remove every vault note matching the query terms. Returns the count removed. There is no undo; the vault belongs to the owner.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "query": { "type": "string", "description": "Terms that select the notes to remove" }
                },
                "required": ["query"],
                "additionalProperties": false,
            },
        },
        {
            "name": "resident_configure",
            "description": "Give this Box its own resident caretaker: a scheduled agent that reads the Box's state daily, writes a plain-sentence report into the journal, names concerns, and queues any destructive suggestion for the human's approval (the same leash you are on). The brain is any OpenAI-compatible endpoint — a metered provider, or this Box's own /v1 once a model is pulled. Pass enabled:false to stand it down.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "base_url": { "type": "string", "description": "OpenAI-compatible base, e.g. https://api.x.ai/v1 or http://127.0.0.1:2693/v1" },
                    "model": { "type": "string", "description": "Model name at that endpoint" },
                    "api_key": { "type": "string", "description": "Key for that endpoint (stored encrypted; use a minted boxai_ key for the Box's own)" },
                    "enabled": { "type": "boolean", "description": "Default true" }
                },
                "additionalProperties": false,
            },
        },
        {
            "name": "resident_report_now",
            "description": "Run the resident's report immediately instead of waiting for the schedule. The summary lands in the journal; suggestions land on the Approvals page.",
            "inputSchema": no_args,
        },
        {
            "name": "work_configure",
            "description": "Store the API key the Box's work runner uses to drive a coding agent (Claude Code, headless) inside the build sandbox. Stored encrypted like every secret; overwrite any time.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "api_key": { "type": "string", "description": "An Anthropic API key for the agent runs" }
                },
                "required": ["api_key"],
                "additionalProperties": false,
            },
        },
        {
            "name": "work_start",
            "description": "Put a coding agent to work overnight ON this Box, against a service's linked repository. The agent runs inside the hardened build sandbox (its world is one scratch checkout; box credentials never enter the container), commits to a work/<commit> branch, and a pull request is opened for human review. Returns a job id — follow it with job_status. Nothing merges on the Box's say-so.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "service": { "type": "string", "description": "A service with a linked repository" },
                    "prompt": { "type": "string", "description": "What the agent should do, e.g. 'fix the failing date parsing and add a test'" }
                },
                "required": ["service", "prompt"],
                "additionalProperties": false,
            },
        },
        {
            "name": "ai_key_create",
            "description": "Mint an API key for this Box's OpenAI-compatible endpoint (POST /v1/chat/completions, GET /v1/models on the Box's own port). Any app or SDK that takes a base_url runs against the Box's models by setting base_url to http://<box>:2693/v1 and pasting this key. Shown ONCE; store it now. Revocable per key, like devices.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "label": { "type": "string", "description": "What will hold this key, e.g. 'my-notes-app'" }
                },
                "required": ["label"],
                "additionalProperties": false,
            },
        },
        {
            "name": "ai_keys",
            "description": "List the minted AI keys (labels and ids, never secrets) for this Box's OpenAI-compatible endpoint.",
            "inputSchema": no_args,
        },
        {
            "name": "ai_key_revoke",
            "description": "Revoke one AI key by id. The app holding it loses model access immediately; nothing else changes.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "id": { "type": "string", "description": "A key id from ai_keys" }
                },
                "required": ["id"],
                "additionalProperties": false,
            },
        },
        {
            "name": "journal",
            "description": "The Box's own story, newest first: what it deployed, updated, backed up, rolled back, and what the person approved or denied. Read this to catch up on what happened while you were away, or to answer 'what changed?' -- it is the same page the owner reads.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "limit": { "type": "integer", "description": "How many recent entries (default 50, max 200)" }
                },
                "additionalProperties": false,
            },
        },
        {
            "name": "job_status",
            "description": "Follow a background job (a platform update, a long deploy) by the id a tool handed back: current phase, recent log lines, and whether it finished or failed. Poll every few seconds while narrating progress to the person; 'failed' comes with the reason.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "id": { "type": "string", "description": "The job id another tool returned" }
                },
                "required": ["id"],
                "additionalProperties": false,
            },
        },
        {
            "name": "service_logs",
            "description": "Recent log lines for a deployed service, straight from the system journal — what you read to find out why something is not working. Returns the most recent lines, oldest first.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "name": { "type": "string", "description": "Service name as deployed" },
                    "lines": { "type": "integer", "description": "How many recent lines to return (default 100, max 1000)" }
                },
                "required": ["name"],
                "additionalProperties": false,
            },
        },
        {
            "name": "delete_service",
            "description": "Remove a service and activate a new generation without it. The previous generation remains available for rollback. Destructive, so unless this session has autonomy it QUEUES for a human tap: you get a pending_approval id — follow it with approval_status.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "name": { "type": "string", "description": "Name of the service to delete" }
                },
                "required": ["name"],
                "additionalProperties": false,
            },
        },
        {
            "name": "list_generations",
            "description": "List all generations (deployment history), newest last. The current one is marked.",
            "inputSchema": no_args,
        },
        {
            "name": "list_history",
            "description": "Git commit history of this Box's declarative config, newest first. Each commit corresponds to a generation.",
            "inputSchema": no_args,
        },
        {
            "name": "channel_status",
            "description": "The platform update channel: the running platform release, whether an OS-tier channel is configured, what it tracks, and the pinned platform revision. Read-only.",
            "inputSchema": no_args,
        },
        {
            "name": "channel_check",
            "description": "Check the update channel for a newer platform than the one pinned. Returns current/latest revisions and whether an update is available. Applying an update is done by the Box's root updater, not via MCP.",
            "inputSchema": no_args,
        },
        {
            "name": "rollback",
            "description": "Atomically switch back to a previous generation, restoring its services and configuration.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "generation": { "type": "integer", "description": "Generation number to roll back to" }
                },
                "required": ["generation"],
                "additionalProperties": false,
            },
        },
        json!({
            "name": "backup_status",
            "description": "Backup status: whether the destination is reachable, how many snapshots exist, and when the last backup ran. Read-only.",
            "inputSchema": no_args,
        }),
        json!({
            "name": "backup_snapshots",
            "description": "List backup snapshots (id, time, paths), newest last. Read-only.",
            "inputSchema": no_args,
        }),
        json!({
            "name": "backup_now",
            "description": "Take a backup snapshot now (e.g. before a risky change), then apply the retention policy. Runs as a background job — you get a job id back; poll job_status. A first backup uploads everything and takes as long as the connection allows.",
            "inputSchema": no_args,
        }),
        json!({
            "name": "backup_restore",
            "description": "Restore from a backup snapshot, in place. Destructive — overwrites current files. Scope with 'config' (Box config only, the default), 'all' (everything), or a service name (that service's data). Unless this session has autonomy it QUEUES for a human tap: you get a pending_approval id — follow it with approval_status.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "snapshot": { "type": "string", "description": "Snapshot id, or 'latest' (default)" },
                    "scope": { "type": "string", "description": "'config' (default) | 'all' | a service name" }
                },
                "additionalProperties": false,
            },
        }),
    ])
}

/// The operations that need a human tap unless the session was explicitly
/// granted autonomy: they erase machines, remove services, or overwrite live
/// data, and an agent's mistaken yes must not be enough on its own.
const NEEDS_APPROVAL: [&str; 3] = ["provision_machine", "delete_service", "backup_restore"];

/// One line for the human, in consequences rather than tool names.
fn destructive_summary(name: &str, args: &Value) -> String {
    let a = |k: &str| args.get(k).and_then(Value::as_str).unwrap_or("?");
    match name {
        "provision_machine" => format!(
            "ERASE {} and turn it into a new Box (its current disk contents are destroyed)",
            a("target")
        ),
        "delete_service" => format!("delete the service {:?} and stop serving it", a("name")),
        "backup_restore" => format!(
            "restore snapshot {:?} over the current data (scope: {})",
            args.get("snapshot")
                .and_then(Value::as_str)
                .unwrap_or("latest"),
            args.get("scope")
                .and_then(Value::as_str)
                .unwrap_or("config"),
        ),
        other => format!("run {other}"),
    }
}

async fn call_tool(
    state: SharedState,
    params: Value,
    caller: crate::web::Caller,
) -> Result<Value, (i64, String)> {
    let name = params
        .get("name")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string();
    let args = params
        .get("arguments")
        .cloned()
        .unwrap_or_else(|| json!({}));

    if NEEDS_APPROVAL.contains(&name.as_str()) && !caller.autonomous {
        let summary = destructive_summary(&name, &args);
        let action = crate::approvals::request(
            &state.paths,
            &name,
            args,
            &summary,
            &caller.label,
            &caller.id,
        )
        .map_err(|e| (-32603, format!("{e:#}")))?;
        let body = json!({
            "pending_approval": action.id,
            "would": summary,
            "message": "This is a destructive operation and this session does not have \
                        autonomous leave for those, so it is QUEUED, not run. Tell the \
                        person it is waiting on the console's Approvals page. Poll \
                        approval_status with this id — an approval runs the exact call \
                        you made and the result lands there. (The operator can grant \
                        this session autonomy for future destructive calls in the \
                        device list.)",
        });
        return Ok(json!({
            "content": [{
                "type": "text",
                "text": serde_json::to_string_pretty(&body).unwrap_or_default(),
            }],
            "isError": false,
        }));
    }

    let outcome = execute_as(state, &name, args, &caller.label).await?;
    Ok(match outcome {
        Ok(value) => json!({
            "content": [{
                "type": "text",
                "text": serde_json::to_string_pretty(&value).unwrap_or_default(),
            }],
            "isError": false,
        }),
        // Tool-level failures are results, not protocol errors, per MCP.
        Err(err) => json!({
            "content": [{ "type": "text", "text": format!("{err:#}") }],
            "isError": true,
        }),
    })
}

pub(crate) async fn execute(
    state: SharedState,
    tool: &str,
    args: Value,
) -> Result<anyhow::Result<Value>, (i64, String)> {
    execute_as(state, tool, args, "someone").await
}

/// Like execute, but with the caller's label for tools that attribute their
/// writes (the vault). Approval re-dispatch passes the original requester.
pub(crate) async fn execute_as(
    state: SharedState,
    tool: &str,
    args: Value,
    by: &str,
) -> Result<anyhow::Result<Value>, (i64, String)> {
    let str_arg = |key: &str| {
        args.get(key)
            .and_then(Value::as_str)
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
    };

    match tool {
        "get_status" => Ok(status(&state)),
        "work_configure" => {
            let api_key = str_arg("api_key");
            let state = state.clone();
            Ok(blocking(move || {
                use anyhow::Context as _;
                let key = api_key.context("api_key is required")?;
                crate::secrets::set(&state.paths, crate::work::API_KEY_SECRET, &key)?;
                crate::journal::record(
                    &state.paths,
                    "work",
                    "the work runner got its agent credentials",
                );
                Ok(json!({ "work": "configured" }))
            })
            .await)
        }
        "work_start" => {
            let service = str_arg("service");
            let prompt = str_arg("prompt");
            let state = state.clone();
            Ok(blocking(move || {
                use anyhow::Context as _;
                let service = service.context("service is required")?;
                let prompt = prompt.context("prompt is required")?;
                let id = crate::work::start(&state, &service, &prompt)?;
                Ok(json!({
                    "job": id,
                    "note": "Poll job_status with this id. The agent works in the sandbox; \
                             the result is a branch and, on GitHub, a pull request.",
                }))
            })
            .await)
        }
        "console_remote" => {
            let enabled = args.get("enabled").and_then(Value::as_bool).unwrap_or(true);
            let state = state.clone();
            Ok(blocking(move || {
                let url = crate::ingress::set_console_remote(&state.paths, enabled)?;
                Ok(json!({
                    "console": url.unwrap_or_else(|| "stopped".into()),
                    "note": "passkeys (Face ID, fingerprint, security key) enroll and sign in \
                             at this address; plain-HTTP LAN addresses cannot offer them",
                }))
            })
            .await)
        }
        "memory_save" => {
            let text_arg = str_arg("text");
            let tags: Vec<String> = args
                .get("tags")
                .and_then(Value::as_array)
                .map(|a| {
                    a.iter()
                        .filter_map(Value::as_str)
                        .map(str::to_string)
                        .collect()
                })
                .unwrap_or_default();
            let by = by.to_string();
            let state = state.clone();
            Ok(blocking(move || {
                use anyhow::Context as _;
                let text = text_arg.context("text is required")?;
                crate::vault::save(&state.paths, &by, &text, tags)?;
                Ok(json!({ "saved": true }))
            })
            .await)
        }
        "memory_search" => {
            let query = str_arg("query");
            let limit = args
                .get("limit")
                .and_then(Value::as_u64)
                .unwrap_or(10)
                .min(100) as usize;
            let state = state.clone();
            Ok(blocking(move || {
                use anyhow::Context as _;
                let query = query.context("query is required")?;
                Ok(json!({ "notes": crate::vault::search(&state.paths, &query, limit) }))
            })
            .await)
        }
        "memory_forget" => {
            let query = str_arg("query");
            let state = state.clone();
            Ok(blocking(move || {
                use anyhow::Context as _;
                let query = query.context("query is required")?;
                let removed = crate::vault::forget(&state.paths, &query)?;
                Ok(json!({ "removed": removed }))
            })
            .await)
        }
        "router_configure" => {
            let enabled = args.get("enabled").and_then(Value::as_bool).unwrap_or(true);
            let base_url = str_arg("base_url");
            let api_key = str_arg("api_key");
            let state = state.clone();
            Ok(blocking(move || {
                use anyhow::Context as _;
                let mut config = crate::config::BoxConfig::load(&state.paths)?;
                if !enabled {
                    config.router = None;
                    config.save(&state.paths)?;
                    return Ok(json!({ "router": "fallback stood down" }));
                }
                let base_url = base_url.context("base_url is required to enable the fallback")?;
                // The fallback is by definition somewhere else: local models
                // already win when present, so a loopback or LAN address here
                // is either a mistake or an attempt to aim the Box at itself.
                crate::util::validate_outbound_url(&base_url, crate::util::Loopback::Deny)?;
                if let Some(k) = api_key {
                    crate::secrets::set(&state.paths, crate::router::FALLBACK_KEY_SECRET, &k)?;
                }
                anyhow::ensure!(
                    crate::secrets::exists(&state.paths, crate::router::FALLBACK_KEY_SECRET),
                    "no api_key stored yet — pass one"
                );
                config.router = Some(crate::router::RouterConfig {
                    enabled: true,
                    base_url,
                });
                config.save(&state.paths)?;
                crate::journal::record(
                    &state.paths,
                    "router",
                    "the /v1 endpoint got a cloud fallback (local models still win)",
                );
                Ok(json!({ "router": "fallback configured" }))
            })
            .await)
        }
        "router_status" => {
            let state = state.clone();
            Ok(blocking(move || crate::router::status_json(&state.paths)).await)
        }
        "resident_configure" => {
            let enabled = args.get("enabled").and_then(Value::as_bool).unwrap_or(true);
            let base_url = str_arg("base_url");
            let model = str_arg("model");
            let api_key = str_arg("api_key");
            let state = state.clone();
            Ok(blocking(move || {
                use anyhow::Context as _;
                let mut config = crate::config::BoxConfig::load(&state.paths)?;
                if !enabled {
                    config.resident = None;
                    config.save(&state.paths)?;
                    return Ok(json!({ "resident": "stood down" }));
                }
                let base_url = base_url.context("base_url is required to enable the resident")?;
                // The resident's brain may legitimately be this Box's own /v1
                // once a model is pulled, so loopback is allowed here — but the
                // scheme still has to be a URL rather than a curl option.
                crate::util::validate_outbound_url(&base_url, crate::util::Loopback::Allow)?;
                let model = model.context("model is required to enable the resident")?;
                if let Some(k) = api_key {
                    crate::secrets::set(&state.paths, crate::resident::API_KEY_SECRET, &k)?;
                }
                anyhow::ensure!(
                    crate::secrets::exists(&state.paths, crate::resident::API_KEY_SECRET),
                    "no api_key stored yet — pass one"
                );
                config.resident = Some(crate::resident::ResidentConfig {
                    enabled: true,
                    base_url,
                    model,
                    schedule: "daily".into(),
                });
                config.save(&state.paths)?;
                Ok(json!({
                    "resident": "on duty",
                    "note": "A daily report will land in the journal; resident_report_now runs one immediately.",
                }))
            })
            .await)
        }
        "resident_report_now" => {
            let state = state.clone();
            Ok(blocking(move || {
                let report = crate::resident::run_report(&state.paths)?;
                Ok(serde_json::json!({
                    "summary": report.summary,
                    "concerns": report.concerns,
                    "suggestions_queued": report.suggested_actions.len(),
                }))
            })
            .await)
        }
        "ai_key_create" => {
            let Some(label) = str_arg("label") else {
                return Err((-32602, "missing required argument: label".into()));
            };
            Ok((|| {
                let key = crate::aikeys::mint(&state.paths, &label)?;
                Ok(json!({
                    "key": key,
                    "label": label,
                    "base_url": "http://<this-box>:2693/v1",
                    "note": "Shown once. Point the app's base_url at this Box's /v1 and use the \
                             key as its API key. Works once an Ollama service is deployed.",
                }))
            })())
        }
        "ai_keys" => Ok(Ok(
            serde_json::to_value(crate::aikeys::list(&state.paths)).unwrap_or_default()
        )),
        "ai_key_revoke" => {
            let Some(id) = str_arg("id") else {
                return Err((-32602, "missing required argument: id".into()));
            };
            Ok((|| {
                anyhow::ensure!(
                    crate::aikeys::revoke(&state.paths, &id)?,
                    "no AI key with id {id:?}"
                );
                Ok(json!({ "revoked": id }))
            })())
        }
        "journal" => {
            let limit = args
                .get("limit")
                .and_then(Value::as_u64)
                .unwrap_or(50)
                .min(200) as usize;
            Ok(Ok(serde_json::to_value(crate::journal::recent(
                &state.paths,
                limit,
            ))
            .unwrap_or_default()))
        }
        "approval_status" => {
            let Some(id) = str_arg("id") else {
                return Err((-32602, "missing required argument: id".into()));
            };
            Ok((|| {
                let action = crate::approvals::get(&state.paths, &id)
                    .ok_or_else(|| anyhow::anyhow!("no pending action {id:?}"))?;
                Ok(serde_json::to_value(&action)?)
            })())
        }
        "list_services" => Ok(services(&state)),
        "list_generations" => Ok(generations(&state)),
        "list_history" => Ok(config_history(&state)),
        "list_templates" => Ok(templates_list(&state)),
        "channel_status" => Ok(channel_status(&state)),
        "channel_check" => {
            let state = state.clone();
            Ok(blocking(move || channel_check(&state)).await)
        }
        "backup_status" => {
            let state = state.clone();
            Ok(blocking(move || backup_status(&state)).await)
        }
        "backup_snapshots" => {
            let state = state.clone();
            Ok(blocking(move || backup_snapshots(&state)).await)
        }
        "backup_now" => {
            // A job, not a blocking call: a first backup uploads everything
            // and takes as long as the uplink allows, and the one time it
            // blocked it sat past a ten-minute client timeout with nothing to
            // show. The job pattern is what the description promises anyway.
            let state = state.clone();
            Ok(blocking(move || {
                let paths = state.paths.clone();
                let (config, bc) = backup_bc(&state)?;
                let id = state.jobs.start(
                    "backup",
                    "backup to the configured backend".to_string(),
                    "backup",
                    move |progress| {
                        progress.phase("backing up");
                        crate::backup::run(&paths, &config, &bc)?;
                        Ok("backup complete".to_string())
                    },
                );
                Ok(json!({
                    "started": true,
                    "job": id,
                    "note": "Poll job_status with this id; backup_status shows the last snapshot when it finishes.",
                }))
            })
            .await)
        }
        "backup_restore" => {
            let snapshot = str_arg("snapshot").unwrap_or_else(|| "latest".into());
            let scope = str_arg("scope").unwrap_or_else(|| "config".into());
            let state = state.clone();
            Ok(blocking(move || backup_restore(&state, &snapshot, &scope)).await)
        }
        "deploy_static_site" => {
            let Some(name) = str_arg("name") else {
                return Err((-32602, "missing required argument: name".into()));
            };
            let request = ops::DeployRequest::static_site(
                name.clone(),
                args.get("index_html")
                    .and_then(Value::as_str)
                    .map(String::from),
                str_arg("source_path").map(Into::into),
                str_arg("domain"),
                false,
            );
            Ok(run_locked(&state, move |s| {
                let info = ops::deploy(&s.paths, s.builder.as_ref(), request)?;
                Ok(deploy_result(s, &name, info.number))
            })
            .await)
        }
        "deploy" => {
            let Some(name) = str_arg("name") else {
                return Err((-32602, "missing required argument: name".into()));
            };
            let Some(template) = str_arg("template") else {
                return Err((-32602, "missing required argument: template".into()));
            };
            let request = ops::DeployRequest {
                name: name.clone(),
                template,
                params: args.get("params").cloned().unwrap_or_else(|| json!({})),
                domain: str_arg("domain"),
                // Absent means unchanged, so an agent updating params alone
                // cannot silently take a service off its domain.
                public: args.get("public").and_then(Value::as_bool),
                port: args
                    .get("port")
                    .and_then(Value::as_u64)
                    .and_then(|p| u16::try_from(p).ok()),
            };
            Ok(run_locked(&state, move |s| {
                let info = ops::deploy(&s.paths, s.builder.as_ref(), request)?;
                Ok(deploy_result(s, &name, info.number))
            })
            .await)
        }
        "delete_service" => {
            let Some(name) = str_arg("name") else {
                return Err((-32602, "missing required argument: name".into()));
            };
            Ok(run_locked(&state, move |s| {
                let info = ops::delete_service(&s.paths, s.builder.as_ref(), &name)?;
                Ok(json!({ "deleted": name, "generation": info.number }))
            })
            .await)
        }
        "upload_files" => {
            let Some(name) = str_arg("name") else {
                return Err((-32602, "missing required argument: name".into()));
            };
            let Some(files_obj) = args.get("files").and_then(Value::as_object).cloned() else {
                return Err((
                    -32602,
                    "missing required argument: files (an object of path → contents)".into(),
                ));
            };
            let mut files = Vec::with_capacity(files_obj.len());
            for (path, content) in files_obj {
                let bytes = match &content {
                    Value::String(text) => text.clone().into_bytes(),
                    Value::Object(o) => match o.get("base64").and_then(Value::as_str) {
                        Some(b) => match crate::provision::base64_decode(b) {
                            Ok(bytes) => bytes,
                            Err(e) => {
                                return Err((-32602, format!("{path}: invalid base64: {e:#}")))
                            }
                        },
                        None => {
                            return Err((
                                -32602,
                                format!(
                                    "{path}: expected a string, or an object with a \"base64\" key"
                                ),
                            ))
                        }
                    },
                    _ => {
                        return Err((
                            -32602,
                            format!(
                                "{path}: expected a string, or an object with a \"base64\" key"
                            ),
                        ))
                    }
                };
                files.push(crate::ops::UploadedFile { path, bytes });
            }
            let replace = args.get("replace").and_then(Value::as_bool).unwrap_or(true);
            let deploy = args.get("deploy").and_then(Value::as_bool).unwrap_or(true);
            let domain = str_arg("domain");
            let public = args.get("public").and_then(Value::as_bool);
            let service = name.clone();
            Ok(run_locked(&state, move |s| {
                let (count, bytes) = ops::upload_files(&s.paths, &service, files, replace)?;
                if !deploy {
                    return Ok(json!({ "service": service, "files": count, "bytes": bytes }));
                }
                let info = ops::deploy(
                    &s.paths,
                    s.builder.as_ref(),
                    ops::DeployRequest {
                        name: service.clone(),
                        template: "static-site".into(),
                        params: json!({
                            "source_path": s.paths.upload_dir(&service).display().to_string(),
                        }),
                        domain,
                        public,
                        port: None,
                    },
                )?;
                let mut out = deploy_result(s, &service, info.number);
                out["files"] = json!(count);
                out["bytes"] = json!(bytes);
                Ok(out)
            })
            .await)
        }
        "ingress_options" => {
            let list: Vec<Value> = crate::ingress::providers()
                .iter()
                .map(|p| {
                    let c = p.capabilities();
                    json!({
                        "id": p.id(),
                        "title": p.title(),
                        "description": p.description(),
                        "available_on_this_box": p.available(),
                        "needs_domain": c.needs_domain,
                        "needs_account": c.needs_account,
                        "address_survives_restart": c.stable_url,
                        "https": c.terminates_tls,
                        "third_party_sees_traffic": c.third_party_sees_traffic,
                        "steps_the_person_must_do": p.steps(),
                    })
                })
                .collect();
            Ok(Ok(json!(list)))
        }
        "ingress_status" => {
            let state = state.clone();
            Ok(blocking(move || {
                let status = state.tunnel.status();
                let urls: Vec<Value> =
                    crate::ingress::published_urls(&state.paths, status.address.as_deref())
                        .into_iter()
                        .map(|(service, url)| json!({ "service": service, "url": url }))
                        .collect();
                // The trap this closes: tunnel "running", service "published",
                // and the public URL answering 502 — because nothing on this
                // machine listens where the tunnel points. All three tools used
                // to look green while the site was dark.
                let origin = origin_listening();
                let mut value = json!({
                    "ingress": status,
                    "published": urls,
                    "origin_listening": origin,
                });
                if status.enabled && !origin {
                    value["warning"] = json!(
                        "The tunnel is up but nothing on this Box is listening on the public \
                         port yet, so public URLs will answer 502. On a Box this resolves when \
                         the system apply after publishing finishes (give it a minute, then \
                         check again, or call verify_service). If this is a dev machine with no \
                         OS tier, there is no public listener at all and that is why."
                    );
                }
                Ok(value)
            })
            .await)
        }
        "ingress_connect_account" => {
            let state = state.clone();
            let token = str_arg("api_token");
            Ok(blocking(move || {
                let Some(token) = token else {
                    return Ok(json!({
                        "connected": crate::secrets::exists(&state.paths, crate::cfapi::API_TOKEN_SECRET),
                        "create_token_url": crate::cfapi::PARENT_TOKEN_URL,
                        "fallback_token_url": crate::cfapi::TOKEN_TEMPLATE_URL,
                        "instructions": "Best path: send them create_token_url — ONE permission \
                                         (API Tokens: Edit), and the Box mints and maintains its own \
                                         exactly-scoped working token from it, forever. Fallback: \
                                         fallback_token_url pre-selects the three direct permissions \
                                         (Cloudflare Tunnel: Edit, DNS: Edit, Zone: Read) if they \
                                         prefer to hand over only those. Either way: they click \
                                         Create, copy the token, and give it to you; call this again \
                                         with api_token set.",
                    }));
                };
                // A parent token (one that can create tokens) is the good path:
                // the Box mints its own scoped child and keeps the parent for
                // reminting, so no human ever assembles the working token by
                // hand — the failure class the first live run spent a night in.
                if let Some(child) = crate::ingress::try_self_mint(&token) {
                    crate::secrets::set(&state.paths, crate::cfapi::PARENT_TOKEN_SECRET, &token)?;
                    crate::secrets::set(&state.paths, crate::cfapi::API_TOKEN_SECRET, &child)?;
                    return Ok(json!({
                        "connected": true,
                        "minted": true,
                        "message": "Parent token accepted. The Box minted its own exactly-scoped \
                                    working token (tunnel + DNS + zone read, no expiry) and will \
                                    remint it itself if it ever stops working. Call ingress_setup \
                                    with their domain and the Box does the rest.",
                    }));
                }
                // Not a parent — prove it works as a direct credential before
                // storing it, so a typo fails here rather than halfway through
                // changing their account.
                use anyhow::Context as _;
                crate::cfapi::call(&token, &crate::cfapi::verify_token())
                    .context("that token was refused by Cloudflare")?;
                crate::secrets::set(&state.paths, crate::cfapi::API_TOKEN_SECRET, &token)?;
                Ok(json!({
                    "connected": true,
                    "minted": false,
                    "message": "Cloudflare account connected with the token as-is (it cannot mint \
                                tokens, so the Box will use it directly — if it lacks a permission \
                                or expires, that surfaces as an Authentication error later; the \
                                parent-token path avoids that class). Call ingress_setup with their \
                                domain and the Box will do the rest.",
                }))
            })
            .await)
        }
        "ingress_setup" => {
            let Some(zone) = str_arg("zone") else {
                return Err((-32602, "missing required argument: zone".into()));
            };
            let hostname = str_arg("hostname");
            let enable = args.get("enable").and_then(Value::as_bool).unwrap_or(true);
            let state = state.clone();
            Ok(blocking(move || {
                let outcome =
                    crate::ingress::setup_cloudflare(&state.paths, &zone, hostname.as_deref())?;
                let mut value = serde_json::to_value(&outcome)?;
                if enable {
                    let status =
                        state
                            .tunnel
                            .set_ingress("cloudflare-tunnel", Some(zone.clone()), true)?;
                    value["ingress"] = serde_json::to_value(status)?;
                    value["note"] = json!(format!(
                        "Publishing is on. A service published now answers at \
                         https://<service>.{zone}/ — DNS and the certificate take a few minutes \
                         to settle the first time."
                    ));
                }
                Ok(value)
            })
            .await)
        }
        "forge_options" => {
            let state = state.clone();
            Ok(blocking(move || {
                let list: Vec<Value> = crate::forge::forges()
                    .iter()
                    .map(|f| {
                        let r = f.requirements();
                        let cfg = crate::forge::config_for(&state.paths, f.id()).ok();
                        json!({
                            "id": f.id(),
                            "title": f.title(),
                            "description": f.description(),
                            // Whether this Box could start a device flow right
                            // now, or still needs an application id from them.
                            "ready_to_connect": cfg
                                .as_ref()
                                .map(|c| f.endpoints(c).is_ok())
                                .unwrap_or(false),
                            "connected": crate::secrets::exists(
                                &state.paths,
                                &crate::forge::token_secret(f.id()),
                            ),
                            "needs_its_own_application": r.needs_own_app,
                            "can_be_self_hosted": r.needs_base_url,
                            "shares_only_chosen_repos": r.per_repo_consent,
                            "steps_the_person_must_do": f.steps(),
                        })
                    })
                    .collect();
                Ok(json!(list))
            })
            .await)
        }
        "forge_connect" => {
            let Some(provider) = str_arg("provider") else {
                return Err((-32602, "missing required argument: provider".into()));
            };
            let (base_url, client_id, app_slug) = (
                str_arg("base_url"),
                str_arg("client_id"),
                str_arg("app_slug"),
            );
            let state = state.clone();
            Ok(blocking(move || {
                if crate::secrets::exists(&state.paths, &crate::forge::token_secret(&provider)) {
                    return Ok(json!({
                        "state": "connected",
                        "message": format!(
                            "A {provider} account is already connected. Call forge_repos to see \
                             what it can reach, or forge_disconnect to forget it first."
                        ),
                    }));
                }
                let cfg = if base_url.is_some() || client_id.is_some() || app_slug.is_some() {
                    crate::forge::configure(&state.paths, &provider, base_url, client_id, app_slug)?
                } else {
                    crate::forge::config_for(&state.paths, &provider)?
                };
                let (p, fresh) = crate::forge::start(&state.paths, &cfg)?;
                Ok(json!({
                    "state": "waiting",
                    "user_code": p.user_code,
                    "verification_uri": p.verification_uri,
                    "code_is_new": fresh,
                    "expires_in_seconds": p.expires_at.saturating_sub(
                        std::time::SystemTime::now()
                            .duration_since(std::time::UNIX_EPOCH)
                            .map(|d| d.as_secs())
                            .unwrap_or_default(),
                    ),
                    "poll_every_seconds": p.interval,
                    "instructions": if fresh {
                        "Give them the link and the code, in that order, and let them go and do \
                         it. Then call forge_connect_status. Calling this tool again is safe — \
                         it returns this same code while it is valid, and a new one once it has \
                         expired."
                    } else {
                        "A code is already waiting on them — this is that same code, not a new \
                         one, so whatever they have in front of them is still right. Call \
                         forge_connect_status to see whether they have finished."
                    },
                }))
            })
            .await)
        }
        "forge_connect_status" => {
            let Some(provider) = str_arg("provider") else {
                return Err((-32602, "missing required argument: provider".into()));
            };
            let state = state.clone();
            Ok(blocking(move || {
                let cfg = crate::forge::config_for(&state.paths, &provider)?;
                let status = crate::forge::poll(&state.paths, &cfg)?;
                let mut value = serde_json::to_value(&status)?;
                if matches!(status, crate::forge::Status::Connected) {
                    value["next"] =
                        json!("Call forge_repos to show them what this Box can now see.");
                }
                Ok(value)
            })
            .await)
        }
        "forge_repos" => {
            let Some(provider) = str_arg("provider") else {
                return Err((-32602, "missing required argument: provider".into()));
            };
            let state = state.clone();
            Ok(blocking(move || {
                let forge = crate::forge::get(&provider)
                    .ok_or_else(|| anyhow::anyhow!("unknown forge {provider:?}"))?;
                let cfg = crate::forge::config_for(&state.paths, &provider)?;
                let token = crate::forge::token(&state.paths, &provider)?;
                let repos = forge.list_repos(&token, &cfg)?;
                let mut value = json!({ "repositories": repos, "count": repos.len() });
                if repos.is_empty() {
                    // Not an error on an app-based forge: they consented, then
                    // shared nothing. But "zero repositories" has more than one
                    // cause, and the diagnosis says which one this is — the
                    // difference between a fix the person clicks and a fix only
                    // the app's owner can make.
                    value["share_more_url"] = json!(forge.grant_more_url(&cfg));
                    value["message"] = json!(forge.empty_hint(&token, &cfg).unwrap_or_else(|| {
                        "The account is connected but no repositories are shared with this Box \
                         yet. If there is a share_more_url, send it to them — they tick the \
                         repositories they want and this call then returns them."
                            .into()
                    }));
                }
                Ok(value)
            })
            .await)
        }
        "link_repo" => {
            let Some(name) = str_arg("name") else {
                return Err((-32602, "missing required argument: name".into()));
            };
            let Some(forge) = str_arg("forge") else {
                return Err((-32602, "missing required argument: forge".into()));
            };
            let Some(repo) = str_arg("repo") else {
                return Err((-32602, "missing required argument: repo".into()));
            };
            let (branch, subdir, domain) =
                (str_arg("branch"), str_arg("subdir"), str_arg("domain"));
            let public = args.get("public").and_then(Value::as_bool).unwrap_or(false);
            let build = match (
                str_arg("build_command"),
                str_arg("install_command"),
                str_arg("output_dir"),
            ) {
                (Some(command), install, output_dir) => Some(crate::build::BuildSpec {
                    command,
                    install,
                    output_dir,
                }),
                (None, None, None) => None,
                _ => {
                    return Err((
                        -32602,
                        "install_command and output_dir only mean something alongside \
                         build_command — pass build_command, or neither"
                            .into(),
                    ))
                }
            };
            Ok(run_locked(&state, move |state| {
                let (link, outcome, warning) = crate::pull::link(
                    &state.paths,
                    state.builder.as_ref(),
                    &state.build_exec,
                    &name,
                    &forge,
                    &repo,
                    branch,
                    subdir,
                    build,
                    domain,
                    public,
                )?;
                let generation = match &outcome {
                    crate::pull::SyncOutcome::Deployed { generation, .. } => *generation,
                    _ => 0,
                };
                let mut value = deploy_result(state, &name, generation);
                value["linked"] = serde_json::to_value(&link)?;
                value["sync"] = serde_json::to_value(&outcome)?;
                if let Some(w) = warning {
                    // "It deployed" and "it will 404" can both be true; say so.
                    value["warning"] = json!(w);
                }
                value["note"] = json!(format!(
                    "From now on, pushing to {} on {} deploys automatically within about a minute.",
                    link.branch, link.repo
                ));
                Ok(value)
            })
            .await)
        }
        "sync_repo" => {
            let Some(name) = str_arg("name") else {
                return Err((-32602, "missing required argument: name".into()));
            };
            Ok(run_locked(&state, move |state| {
                let outcome = crate::pull::sync_recorded(
                    &state.paths,
                    state.builder.as_ref(),
                    &state.build_exec,
                    &name,
                    false,
                )?;
                Ok(serde_json::to_value(&outcome)?)
            })
            .await)
        }
        "unlink_repo" => {
            let Some(name) = str_arg("name") else {
                return Err((-32602, "missing required argument: name".into()));
            };
            let state = state.clone();
            Ok(blocking(move || {
                crate::pull::unlink(&state.paths, &name)?;
                Ok(json!({
                    "unlinked": name,
                    "message": "Automatic deploys are off. The service and its current content are untouched.",
                }))
            })
            .await)
        }
        "webhook_setup" => {
            let Some(name) = str_arg("name") else {
                return Err((-32602, "missing required argument: name".into()));
            };
            let state = state.clone();
            Ok(blocking(move || {
                use anyhow::Context as _;
                let config = crate::config::BoxConfig::load(&state.paths)?;
                let svc = config
                    .find(&name)
                    .with_context(|| format!("no service named {name:?}"))?;
                let link = svc.repo.clone().with_context(|| {
                    format!("service {name:?} has no linked repository — link_repo first")
                })?;
                if link.forge != "github" {
                    anyhow::bail!(
                        "webhooks are wired for GitHub so far; {} stays on the ~1-minute poll",
                        link.forge
                    );
                }
                let zone = config
                    .ingress
                    .as_ref()
                    .and_then(|i| i.zone.clone())
                    .context(
                        "webhooks need your own domain (the receiver lives at hooks.<domain>) — \
                         run ingress_setup with a zone first; polling keeps working meanwhile",
                    )?;
                let secret =
                    match crate::secrets::get(&state.paths, crate::web::hooks::WEBHOOK_SECRET)? {
                        Some(s) => s,
                        None => {
                            let s = crate::auth::random_hex(16)?;
                            crate::secrets::set(
                                &state.paths,
                                crate::web::hooks::WEBHOOK_SECRET,
                                &s,
                            )?;
                            s
                        }
                    };
                let url = format!("https://hooks.{zone}/hooks/github");
                let token = crate::forge::token(&state.paths, "github")?;
                let created = crate::ghapi::register_hook(&token, &link.repo, &url, &secret)
                    .map_err(|e| {
                        let text = format!("{e:#}");
                        if text.contains("Resource not accessible") || text.contains("Not Found") {
                            anyhow::anyhow!(
                                "{text}. This usually means the App registration lacks \
                                 'Repository webhooks: Read & write' — only the App's OWNER can \
                                 add that (Permissions & events), and existing installations must \
                                 approve the change. Polling continues meanwhile."
                            )
                        } else {
                            anyhow::anyhow!("{text}")
                        }
                    })?;
                let hook_id = created.get("id").and_then(Value::as_u64);
                Ok(json!({
                    "registered": true,
                    "repo": link.repo,
                    "receiver": url,
                    "hook_id": hook_id,
                    "note": "Pushes now deploy in seconds. GitHub sends a ping first — if the \
                             tunnel route was created before this Box's current release, re-run \
                             ingress_setup once so hooks.<zone> routes to the receiver. Polling \
                             stays on as the fallback.",
                }))
            })
            .await)
        }
        "forge_disconnect" => {
            let Some(provider) = str_arg("provider") else {
                return Err((-32602, "missing required argument: provider".into()));
            };
            let state = state.clone();
            Ok(blocking(move || {
                crate::forge::disconnect(&state.paths, &provider)?;
                Ok(json!({
                    "state": "disconnected",
                    "message": format!(
                        "The stored {provider} token has been deleted from this Box. It is NOT \
                         revoked — to do that they must remove the authorization in their own \
                         {provider} account settings."
                    ),
                }))
            })
            .await)
        }
        "verify_service" => {
            let Some(name) = str_arg("name") else {
                return Err((-32602, "missing required argument: name".into()));
            };
            let state = state.clone();
            Ok(blocking(move || verify_service(&state, &name)).await)
        }
        "channel_update" => {
            let force = args.get("force").and_then(Value::as_bool).unwrap_or(false);
            let state = state.clone();
            Ok(blocking(move || {
                let cfg = crate::channel::ChannelConfig::load(&state.paths)?
                    .ok_or_else(|| anyhow::anyhow!("no update channel configured on this Box"))?;
                let status = crate::channel::check(&state.paths, &cfg)?;
                if !status.update_available && !force {
                    return Ok(json!({
                        "started": false,
                        "message": format!(
                            "Already current ({}). If the running release looks older than \
                             that, pass force: true to rebuild and switch anyway.",
                            status.latest
                        ),
                    }));
                }
                let paths = state.paths.clone();
                let jobs = state.jobs.clone();
                let id = jobs.start(
                    "platform-update",
                    format!("platform update to {}", status.latest),
                    "system",
                    move |progress| {
                        // On a managed Box, the switch MUST go through the
                        // root oneshot: registering the system generation
                        // needs root, and this process is not root. The
                        // in-process path below built the system fine and
                        // then died exactly there, live, on the first real
                        // MCP-driven update — the polkit-blessed unit is the
                        // same path the dashboard's "Update now" takes.
                        if crate::ostier::update_unit_available() {
                            progress.phase("handing the switch to the system updater");
                            crate::ostier::run_update_unit()?;
                            let release = platform_release().unwrap_or_else(|| "unknown".into());
                            return Ok(format!("updated; running platform release {release}"));
                        }
                        // No unit means no OS tier gating (a root-run dev
                        // serve, tests): do the whole thing here.
                        progress.phase("building the new system");
                        let config = crate::config::BoxConfig::load(&paths)?;
                        let toplevel = crate::channel::update_and_switch(
                            &paths,
                            &config,
                            &cfg,
                            true,
                            &crate::ostier::default_system_health,
                        )?;
                        Ok(format!("switched to {}", toplevel.display()))
                    },
                );
                Ok(json!({
                    "started": true,
                    "job": id,
                    "updating_to": status.latest,
                    "note": "Poll job_status with this id. Services blip during the switch; a \
                             quick-share demo address will be NEW afterward (check \
                             ingress_status when the job finishes).",
                }))
            })
            .await)
        }
        "provision_machine" => {
            let Some(target) = str_arg("target") else {
                return Err((-32602, "missing required argument: target".into()));
            };
            let keys: Vec<String> = args
                .get("ssh_public_keys")
                .and_then(Value::as_array)
                .map(|a| {
                    a.iter()
                        .filter_map(|v| v.as_str().map(str::to_string))
                        .collect()
                })
                .unwrap_or_default();
            if keys.is_empty() {
                return Err((
                    -32602,
                    "ssh_public_keys must carry at least one public key — \
                                     without one, no human can ever SSH into the new Box"
                        .into(),
                ));
            }
            let hostname = str_arg("hostname").unwrap_or_else(|| "auto".into());
            let layout = str_arg("layout");
            let static_ip = args.get("static_ip").and_then(Value::as_object).map(|s| {
                crate::provision::StaticIp {
                    address: s
                        .get("address")
                        .and_then(Value::as_str)
                        .unwrap_or_default()
                        .to_string(),
                    gateway: s
                        .get("gateway")
                        .and_then(Value::as_str)
                        .map(str::to_string),
                    dns: s
                        .get("dns")
                        .and_then(Value::as_array)
                        .map(|a| {
                            a.iter()
                                .filter_map(|v| v.as_str().map(str::to_string))
                                .collect()
                        })
                        .unwrap_or_default(),
                }
            });
            let state = state.clone();
            Ok(blocking(move || {
                let opts = crate::provision::ProvisionOpts {
                    target: target.clone(),
                    hostname,
                    ssh_keys: crate::provision::resolve_ssh_keys(keys)?,
                    layout,
                    static_ip,
                    install_url: crate::provision::DEFAULT_INSTALL_URL.into(),
                    reach_host: None,
                    boot_timeout_secs: 900,
                    ssh_opts: Vec::new(),
                };
                let id = state.jobs.start(
                    "provision",
                    format!("provisioning {target} as a new Box"),
                    "fleet",
                    move |progress| {
                        progress.phase("taking the machine over (its disk is being erased)");
                        let out = crate::provision::run(&opts)?;
                        Ok(format!(
                            "New Box at {} — connect MCP at {} with session token {} \
                             (shown once; store it now)",
                            out.address, out.mcp_url, out.token
                        ))
                    },
                );
                Ok(json!({
                    "started": true,
                    "job": id,
                    "note": "Poll job_status. The takeover wipes, installs and reboots the \
                             machine, so expect several minutes of silence before it comes up.",
                }))
            })
            .await)
        }
        "job_status" => {
            let Some(id) = str_arg("id") else {
                return Err((-32602, "missing required argument: id".into()));
            };
            let state = state.clone();
            Ok(blocking(move || {
                let job = state.jobs.get(&id).ok_or_else(|| {
                    anyhow::anyhow!(
                        "no job with id {id:?} (jobs are kept in memory; a restart forgets them)"
                    )
                })?;
                Ok(serde_json::to_value(&job)?)
            })
            .await)
        }
        "ingress_configure" => {
            let Some(provider) = str_arg("provider") else {
                return Err((-32602, "missing required argument: provider".into()));
            };
            let enable = args.get("enable").and_then(Value::as_bool).unwrap_or(true);
            let zone = str_arg("zone");
            let state = state.clone();
            Ok(blocking(move || {
                let status = state.tunnel.set_ingress(&provider, zone, enable)?;
                Ok(serde_json::to_value(status)?)
            })
            .await)
        }
        "publish_service" => {
            let Some(name) = str_arg("name") else {
                return Err((-32602, "missing required argument: name".into()));
            };
            let public = args.get("public").and_then(Value::as_bool).unwrap_or(true);
            let domain = str_arg("domain");
            Ok(run_locked(&state, move |s| {
                let address = s.tunnel.status().address;
                let out = ops::publish(
                    &s.paths,
                    s.builder.as_ref(),
                    &name,
                    public,
                    domain,
                    address.as_deref(),
                )?;
                Ok(json!({
                    "service": name,
                    "public": public,
                    "url": out.url,
                    "note": out.note,
                    "generation": out.generation.number,
                }))
            })
            .await)
        }
        "service_logs" => {
            let Some(name) = str_arg("name") else {
                return Err((-32602, "missing required argument: name".into()));
            };
            let lines = args
                .get("lines")
                .and_then(Value::as_u64)
                .unwrap_or(100)
                .clamp(1, 1000);
            let state = state.clone();
            Ok(
                blocking(move || crate::logs::service_logs(&state.paths, &name, lines as usize))
                    .await,
            )
        }
        "rollback" => {
            let Some(number) = args.get("generation").and_then(Value::as_u64) else {
                return Err((-32602, "missing required argument: generation".into()));
            };
            Ok(run_locked(&state, move |s| {
                let info = ops::rollback(&s.paths, number)?;
                Ok(json!({ "current_generation": info.number }))
            })
            .await)
        }
        _ => Err((-32602, format!("unknown tool: {tool}"))),
    }
}

async fn run_locked(
    state: &SharedState,
    f: impl FnOnce(&SharedState) -> anyhow::Result<Value> + Send + 'static,
) -> anyhow::Result<Value> {
    let state = state.clone();
    blocking(move || {
        let _guard = state.apply_lock.lock().unwrap_or_else(|e| e.into_inner());
        f(&state)
    })
    .await
}

fn status(state: &SharedState) -> anyhow::Result<Value> {
    let config = BoxConfig::load(&state.paths)?;
    let current = store::current(&state.paths)?;
    Ok(json!({
        "version": env!("CARGO_PKG_VERSION"),
        "builder": state.builder.name(),
        "current_generation": current.map(|g| g.number),
        "services": config.services.len(),
        "tunnel": state.tunnel.status(),
        // Whether this machine has an NVIDIA card — what tells an agent the
        // ollama preset can take `gpu: true` (after the channel's gpu axis is
        // set) instead of guessing from the model's speed.
        "gpu_hardware": if crate::board::nvidia_present() { json!("nvidia") } else { json!(null) },
    }))
}

/// Is anything listening where the tunnel points? One TCP dial answers the
/// question three green dashboards failed to.
fn origin_listening() -> bool {
    std::net::TcpStream::connect_timeout(
        &std::net::SocketAddr::from(([127, 0, 0, 1], crate::ingress::PUBLIC_PORT)),
        std::time::Duration::from_secs(1),
    )
    .is_ok()
}

/// Walk the serving chain for one service and report the FIRST broken link.
///
/// Every check here earns its place by having actually fooled someone: a
/// switch that "succeeded" with the web server dead, a tunnel "running" into a
/// port nobody listened on, a publish whose system apply had not landed yet.
/// The last step fetches the real public URL through the real edge, because
/// the only statement worth making to a person is "I loaded it and it loaded".
fn verify_service(state: &SharedState, name: &str) -> anyhow::Result<Value> {
    use anyhow::Context as _;
    let config = BoxConfig::load(&state.paths)?;
    let svc = config
        .find(name)
        .with_context(|| format!("no service named {name:?}"))?;

    let active: Vec<String> = store::current(&state.paths)?
        .and_then(|c| manifest::read_manifest(&c.store_path).ok())
        .map(|m| m.services.into_iter().map(|s| s.name).collect())
        .unwrap_or_default();
    let status = crate::ops::service_status(&state.paths, svc, active.contains(&svc.name.clone()));

    let mut checks = serde_json::Map::new();
    let mut verdict: Option<String> = None;
    let mut fix: Option<String> = None;

    // These first two checks judge the machine by Box rules, so they apply
    // only where the platform module manages the system. A dev server is not
    // a broken Box; it is not a Box at all, and saying otherwise sent this
    // very tool chasing a "failed system" that was a laptop.
    let managed = crate::ostier::managed_system();
    if managed {
        // 1. Is the OS tier caught up with the config?
        let pending = crate::ostier::is_pending(&state.paths);
        checks.insert("os_apply_pending".into(), json!(pending));
        if pending {
            let reason = crate::ostier::pending_reason(&state.paths);
            checks.insert("os_apply_reason".into(), json!(reason));
            verdict.get_or_insert(
                "the system has not caught up with the configuration — what you configured is \
                 not what is running yet"
                    .into(),
            );
            fix.get_or_insert(
                "wait for or retry the system apply; the reason field says why it is pending"
                    .into(),
            );
        }

        // 2. Is the web server alive?
        let nginx = std::process::Command::new("systemctl")
            .args(["is-active", "--quiet", "nginx.service"])
            .status()
            .map(|s| s.success())
            .unwrap_or(false);
        checks.insert("web_server_running".into(), json!(nginx));
        if !nginx {
            verdict.get_or_insert(
                "nginx is not running, so NOTHING on this Box is being served, this service \
                 included"
                    .into(),
            );
            fix.get_or_insert("check service_logs and the system journal for nginx; a recent structural change may have failed".into());
        }
    } else {
        checks.insert("managed_system".into(), json!(false));
        checks.insert(
            "note".into(),
            json!("not a managed Box (dev mode) — system-level checks skipped"),
        );
    }

    // 3+4. The public half, only for a published service.
    if svc.public {
        let listening = origin_listening();
        checks.insert("origin_listening".into(), json!(listening));
        if !listening {
            verdict.get_or_insert(
                "published, but nothing listens on the Box's public port — a tunnel pointing \
                 here answers 502"
                    .into(),
            );
            fix.get_or_insert(
                "on a Box this appears when the system apply after publishing finishes; on a \
                 dev machine there is no public listener at all"
                    .into(),
            );
        }

        let tunnel = state.tunnel.status();
        checks.insert("tunnel".into(), json!(tunnel.state));
        let public_url = crate::ingress::published_urls(&state.paths, tunnel.address.as_deref())
            .into_iter()
            .find(|(s, _)| s == name)
            .map(|(_, u)| u);
        checks.insert("public_url".into(), json!(public_url));

        if !tunnel.enabled {
            verdict.get_or_insert("published, but no way in from the internet is turned on".into());
            fix.get_or_insert(
                "ask for ingress_options and turn one on with ingress_configure".into(),
            );
        } else if public_url.is_none() {
            verdict.get_or_insert(
                "published and the way in is starting, but it has no address yet".into(),
            );
            fix.get_or_insert(
                "check back in a moment (ingress_status carries the address when it exists)".into(),
            );
        } else if verdict.is_none() {
            // Nothing known to be broken: prove it. Through the actual edge,
            // like a stranger would reach it.
            let url = public_url.clone().unwrap();
            let code = std::process::Command::new("curl")
                .args([
                    "-sS",
                    "-o",
                    "/dev/null",
                    "-w",
                    "%{http_code}",
                    "-m",
                    "15",
                    &url,
                ])
                .output()
                .ok()
                .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
                .unwrap_or_default();
            checks.insert("public_fetch_http_status".into(), json!(code));
            match code.as_str() {
                c if c.starts_with('2') || c.starts_with('3') => {
                    verdict = Some(format!(
                        "reachable — fetched {url} from here and it answered {c}"
                    ));
                }
                "502" | "503" => {
                    verdict = Some(format!(
                        "the edge answers but this Box does not ({code} at {url})"
                    ));
                    fix = Some("the tunnel or the origin came up in the last few seconds, or the system apply is still settling — try once more shortly".into());
                }
                "" => {
                    verdict = Some(format!("could not fetch {url} from this Box at all"));
                    fix = Some("check this Box's own internet connection".into());
                }
                other => {
                    verdict = Some(format!("{url} answers, but with HTTP {other}"));
                    fix = Some("a 404 here usually means the content has no index.html at its root — for a repo-linked service, see link_repo's subdir (or, for one with a build step, output_dir)".into());
                }
            }
        }
    } else if verdict.is_none() {
        verdict = Some(format!(
            "healthy but not published — served on your own network only ({})",
            status.url.as_deref().unwrap_or("no URL")
        ));
    }

    Ok(json!({
        "service": name,
        "state": status.state,
        "note": status.note,
        "checks": checks,
        "verdict": verdict,
        "fix": fix,
    }))
}

fn services(state: &SharedState) -> anyhow::Result<Value> {
    let config = BoxConfig::load(&state.paths)?;
    let active: Vec<String> = store::current(&state.paths)?
        .and_then(|c| manifest::read_manifest(&c.store_path).ok())
        .map(|m| m.services.into_iter().map(|s| s.name).collect())
        .unwrap_or_default();
    let list: Vec<Value> = config
        .services
        .iter()
        .map(|s| {
            let status = crate::ops::service_status(&state.paths, s, active.contains(&s.name));
            json!({
                "name": s.name,
                "template": s.template,
                "params": s.params,
                "domain": s.domain,
                "url": status.url,
                "state": status.state,
                "note": status.note,
                // Present only for repo-linked services: where the content
                // comes from, so an agent can see which sites update themselves
                // — and whether the last pull worked, because a poller that has
                // been failing quietly for a week must not look like one that
                // deployed an hour ago.
                "repo": s.repo,
                "last_sync": s.repo.as_ref().and_then(|_| crate::pull::read_sync_state(&state.paths, &s.name)),
            })
        })
        .collect();
    Ok(json!(list))
}

/// What an agent is told after a deploy. The URL has to be the one that
/// actually serves the thing — reporting `/sites/<name>/` for a container sent
/// agents to a 404 and made a service that wasn't running look deployed.
fn deploy_result(state: &SharedState, name: &str, generation: u64) -> Value {
    let status = crate::config::BoxConfig::load(&state.paths)
        .ok()
        .and_then(|c| c.find(name).cloned())
        .map(|s| crate::ops::service_status(&state.paths, &s, true));
    match status {
        Some(st) => json!({
            "service": name,
            "generation": generation,
            "url": st.url,
            "state": st.state,
            "note": st.note,
        }),
        None => json!({ "service": name, "generation": generation }),
    }
}

fn config_history(state: &SharedState) -> anyhow::Result<Value> {
    Ok(serde_json::to_value(crate::history::log(
        &state.paths,
        100,
    )?)?)
}

fn templates_list(state: &SharedState) -> anyhow::Result<Value> {
    // Primitives (code) plus catalog presets (data) — deploy either by its id.
    let mut list: Vec<Value> = templates::all()
        .iter()
        .map(|t| {
            json!({
                "id": t.id(),
                "title": t.title(),
                "description": t.description(),
                "kind": "primitive",
            })
        })
        .collect();
    for e in crate::catalog::for_data_dir(&state.paths.data_dir).values() {
        list.push(json!({
            "id": e.id,
            "title": e.title,
            "description": e.description,
            "category": e.category,
            "base": e.base,
            "kind": "preset",
        }));
    }
    Ok(json!(list))
}

fn platform_release() -> Option<String> {
    let text = std::fs::read_to_string("/etc/box/platform.json").ok()?;
    let v: Value = serde_json::from_str(&text).ok()?;
    v.get("release").and_then(Value::as_str).map(str::to_string)
}

fn channel_status(state: &SharedState) -> anyhow::Result<Value> {
    let cfg = channel::ChannelConfig::load(&state.paths)?;
    let pinned = channel::locked_platform_id(&state.paths.os_config_dir())
        .ok()
        .flatten();
    Ok(json!({
        "platform_release": platform_release(),
        "os_tier_available": ostier::available(),
        "channel": cfg.map(|c| json!({
            "host_id": c.host_id,
            "platform_ref": c.platform_ref,
            "system": c.system,
            "auto_update": c.auto_update,
        })),
        "pinned_revision": pinned,
    }))
}

fn channel_check(state: &SharedState) -> anyhow::Result<Value> {
    let cfg = channel::ChannelConfig::load(&state.paths)?
        .ok_or_else(|| anyhow::anyhow!("no update channel configured"))?;
    let status = channel::check(&state.paths, &cfg)?;
    let mut value = serde_json::to_value(&status)?;
    // The check compares the PIN against upstream; the pin and the running
    // system can disagree (a failed update once left them that way, and the
    // box answered "up to date" while running old code). Surfacing what is
    // actually running lets an agent notice the gap — and force close it.
    value["running_release"] = json!(platform_release());
    if !status.update_available {
        value["note"] = json!(
            "\"Up to date\" means the pin matches upstream. If running_release looks older \
             than the latest release, the pin advanced without the system switching — call \
             channel_update with force: true."
        );
    }
    Ok(value)
}

fn generations(state: &SharedState) -> anyhow::Result<Value> {
    Ok(serde_json::to_value(store::list(&state.paths)?)?)
}

fn backup_bc(state: &SharedState) -> anyhow::Result<(BoxConfig, crate::config::BackupConfig)> {
    let config = BoxConfig::load(&state.paths)?;
    let bc = config
        .backup
        .clone()
        .ok_or_else(|| anyhow::anyhow!("no backup destination configured"))?;
    Ok((config, bc))
}

fn backup_status(state: &SharedState) -> anyhow::Result<Value> {
    let (_, bc) = backup_bc(state)?;
    let st = crate::backup::status(&state.paths, &bc);
    Ok(serde_json::json!({
        "reachable": st.reachable,
        "snapshots": st.count,
        "last": st.last.map(|s| s.time),
    }))
}

fn backup_snapshots(state: &SharedState) -> anyhow::Result<Value> {
    let (_, bc) = backup_bc(state)?;
    Ok(serde_json::json!({ "snapshots": crate::backup::snapshots(&state.paths, &bc)? }))
}

fn backup_restore(state: &SharedState, snapshot: &str, scope: &str) -> anyhow::Result<Value> {
    let (config, bc) = backup_bc(state)?;
    let includes = crate::backup::resolve_scope(&state.paths, &config, scope)?;
    crate::backup::restore(
        &state.paths,
        &bc,
        snapshot,
        std::path::Path::new("/"),
        &includes,
    )?;
    Ok(serde_json::json!({ "ok": true, "restored": snapshot, "scope": scope }))
}
