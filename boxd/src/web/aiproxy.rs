//! The Box's OpenAI-compatible endpoint: `/v1/chat/completions` and
//! `/v1/models`, fronting the Ollama the Box already runs.
//!
//! The point is coverage, not novelty. Every SDK, framework, and editor
//! agent speaks this wire format; Ollama itself answers it natively. What
//! this layer adds is what makes it an appliance instead of an open port:
//! per-app keys (minted, listed, revoked like devices), routing to whichever
//! service is the model server, and honest errors that name the fix.
//! "Run your app on your own hardware" becomes: change `base_url`, paste a
//! `boxai_` key.

use axum::extract::State;
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use serde_json::json;

use crate::web::SharedState;

/// Where the model server lives on this Box: the first container service
/// whose image is an Ollama. `None` names the fix at the call site.
fn upstream_port(paths: &crate::paths::Paths) -> Option<u16> {
    let config = crate::config::BoxConfig::load(paths).ok()?;
    config.services.iter().find_map(|s| {
        let image = s.params.get("image").and_then(|v| v.as_str())?;
        // "llama" matches both ollama and llama.cpp images; either one
        // speaks the OpenAI wire format on its /v1.
        (s.template == "container" && image.contains("llama")).then_some(s.port?)
    })
}

fn unauthorized() -> Response {
    (
        StatusCode::UNAUTHORIZED,
        axum::Json(json!({
            "error": {
                "message": "missing or unknown API key. Mint one on the Box (Devices page, or \
                            the ai_key_create MCP tool) and send it as 'Authorization: Bearer \
                            boxai_...'.",
                "type": "invalid_request_error",
            }
        })),
    )
        .into_response()
}

fn no_upstream() -> Response {
    (
        StatusCode::SERVICE_UNAVAILABLE,
        axum::Json(json!({
            "error": {
                "message": "this Box has no model server and no fallback. Deploy the 'llamacpp' or \
                            'ollama' catalog preset (and pull a model), or point router_configure at \
                            a cloud endpoint with your own key; then this endpoint serves either way.",
                "type": "server_error",
            }
        })),
    )
        .into_response()
}

fn authed(state: &SharedState, headers: &HeaderMap) -> bool {
    headers
        .get("authorization")
        .and_then(|v| v.to_str().ok())
        .and_then(|a| a.strip_prefix("Bearer "))
        .is_some_and(|k| crate::aikeys::verify(&state.paths, k.trim()))
}

/// GET /v1/models — passthrough, so clients can enumerate what is pulled.
pub async fn models(State(state): State<SharedState>, headers: HeaderMap) -> Response {
    if !authed(&state, &headers) {
        return unauthorized();
    }
    let Some(t) = crate::router::resolve(&state.paths, upstream_port(&state.paths)) else {
        return no_upstream();
    };
    let mut args: Vec<String> = vec!["-sS".into(), "-m".into(), "20".into()];
    if let Some(auth) = &t.auth {
        args.extend(["-H".into(), auth.clone()]);
    }
    args.push(format!("{}/models", t.base));
    let out = tokio::process::Command::new("curl")
        .args(&args)
        .output()
        .await;
    match out {
        Ok(o) if o.status.success() => (
            StatusCode::OK,
            [("content-type", "application/json")],
            o.stdout,
        )
            .into_response(),
        _ => no_upstream(),
    }
}

/// POST /v1/chat/completions — the endpoint apps point at. Streaming requests
/// pass through as a live SSE body; buffered ones return whole.
pub async fn chat_completions(
    State(state): State<SharedState>,
    headers: HeaderMap,
    body: String,
) -> Response {
    if !authed(&state, &headers) {
        return unauthorized();
    }
    let Some(t) = crate::router::resolve(&state.paths, upstream_port(&state.paths)) else {
        return no_upstream();
    };
    let url = format!("{}/chat/completions", t.base);
    let wants_stream = serde_json::from_str::<serde_json::Value>(&body)
        .ok()
        .and_then(|v| v.get("stream").and_then(serde_json::Value::as_bool))
        .unwrap_or(false);

    if wants_stream {
        // -N: no buffering; the child's stdout becomes the response body, so
        // tokens reach the client as the model produces them.
        let mut args: Vec<String> = vec![
            "-sN".into(),
            "-X".into(),
            "POST".into(),
            "-H".into(),
            "Content-Type: application/json".into(),
        ];
        if let Some(auth) = &t.auth {
            args.extend(["-H".into(), auth.clone()]);
        }
        args.extend(["-d".into(), body.clone(), url.clone()]);
        // Streaming counts the request but not its tokens; honesty over guesswork.
        crate::router::record(&state.paths, t.local, 0);
        let mut child = match tokio::process::Command::new("curl")
            .args(&args)
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::null())
            .spawn()
        {
            Ok(c) => c,
            Err(_) => return no_upstream(),
        };
        let Some(stdout) = child.stdout.take() else {
            return no_upstream();
        };
        let stream = tokio_util::io::ReaderStream::new(stdout);
        return (
            StatusCode::OK,
            [
                ("content-type", "text/event-stream"),
                ("cache-control", "no-cache"),
            ],
            axum::body::Body::from_stream(stream),
        )
            .into_response();
    }

    let mut args: Vec<String> = vec![
        "-sS".into(),
        "-m".into(),
        "600".into(),
        "-X".into(),
        "POST".into(),
        "-H".into(),
        "Content-Type: application/json".into(),
    ];
    if let Some(auth) = &t.auth {
        args.extend(["-H".into(), auth.clone()]);
    }
    args.extend([
        "-d".into(),
        body.clone(),
        "-w".into(),
        "\n%{http_code}".into(),
        url,
    ]);
    let out = tokio::process::Command::new("curl")
        .args(&args)
        .output()
        .await;
    match out {
        Ok(o) if o.status.success() => {
            let text = String::from_utf8_lossy(&o.stdout);
            let (payload, status) = text.rsplit_once('\n').unwrap_or((text.as_ref(), "502"));
            let code = StatusCode::from_u16(status.trim().parse().unwrap_or(502))
                .unwrap_or(StatusCode::BAD_GATEWAY);
            let tokens = serde_json::from_str::<serde_json::Value>(payload.trim())
                .ok()
                .and_then(|v| {
                    v.get("usage")
                        .and_then(|u| u.get("completion_tokens"))
                        .and_then(serde_json::Value::as_u64)
                })
                .unwrap_or(0);
            crate::router::record(&state.paths, t.local, tokens);
            (
                code,
                [("content-type", "application/json")],
                payload.trim().to_string(),
            )
                .into_response()
        }
        _ => no_upstream(),
    }
}
