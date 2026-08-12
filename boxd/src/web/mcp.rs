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

pub async fn handle(State(state): State<SharedState>, Json(message): Json<Value>) -> Response {
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
        "tools/call" => call_tool(state, params).await,
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
            "name": "delete_service",
            "description": "Remove a service and activate a new generation without it. The previous generation remains available for rollback.",
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
            "description": "Take a backup snapshot now (e.g. before a risky change), then apply the retention policy. Blocks until the backup finishes.",
            "inputSchema": no_args,
        }),
        json!({
            "name": "backup_restore",
            "description": "Restore from a backup snapshot, in place. Destructive — overwrites current files. Scope with 'config' (Box config only, the default), 'all' (everything), or a service name (that service's data).",
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

async fn call_tool(state: SharedState, params: Value) -> Result<Value, (i64, String)> {
    let name = params
        .get("name")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string();
    let args = params
        .get("arguments")
        .cloned()
        .unwrap_or_else(|| json!({}));

    let outcome = execute(state, &name, args).await?;
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

async fn execute(
    state: SharedState,
    tool: &str,
    args: Value,
) -> Result<anyhow::Result<Value>, (i64, String)> {
    let str_arg = |key: &str| {
        args.get(key)
            .and_then(Value::as_str)
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
    };

    match tool {
        "get_status" => Ok(status(&state)),
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
            let state = state.clone();
            Ok(blocking(move || backup_now(&state)).await)
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
        let _guard = state.apply_lock.lock().unwrap();
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
    Ok(serde_json::to_value(channel::check(&state.paths, &cfg)?)?)
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

fn backup_now(state: &SharedState) -> anyhow::Result<Value> {
    let (config, bc) = backup_bc(state)?;
    crate::backup::run(&state.paths, &config, &bc)?;
    Ok(serde_json::json!({ "ok": true, "message": "backup complete" }))
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
