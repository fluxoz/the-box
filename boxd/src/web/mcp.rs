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

use crate::config::BoxConfig;
use crate::manifest;
use crate::ops;
use crate::store;

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
            "instructions": "Deploy and manage services on this Box. Deploys are atomic Nix generations; any generation can be rolled back. Deployed sites are served at /sites/<name>/ and, when a domain is set and a tunnel is configured, at that public domain.",
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
                Ok(json!({
                    "service": name,
                    "generation": info.number,
                    "url": format!("/sites/{name}/"),
                }))
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
            json!({
                "name": s.name,
                "template": s.template,
                "params": s.params,
                "domain": s.domain,
                "url": format!("/sites/{}/", s.name),
                "state": if active.contains(&s.name) { "active" } else { "pending" },
            })
        })
        .collect();
    Ok(json!(list))
}

fn config_history(state: &SharedState) -> anyhow::Result<Value> {
    Ok(serde_json::to_value(crate::history::log(
        &state.paths,
        100,
    )?)?)
}

fn generations(state: &SharedState) -> anyhow::Result<Value> {
    Ok(serde_json::to_value(store::list(&state.paths)?)?)
}
