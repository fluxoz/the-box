//! Push-to-deploy in seconds: the webhook receiver.
//!
//! Polling is the floor (works with ingress off, from the first minute); a
//! webhook is the upgrade a Box registers for itself once a stable domain
//! exists. GitHub POSTs here on push; the request is verified against a
//! shared secret (HMAC, GitHub's `X-Hub-Signature-256`) and the matching
//! repo-linked services sync immediately — the same sync the poller runs,
//! just now instead of within a minute.
//!
//! Reachability without exposing the console: the tunnel's edge config routes
//! ONLY `/hooks/*` on the hooks hostname to boxd (see `cfapi::put_ingress`);
//! every other path stays on the published-services plane. The endpoint is a
//! public path in the auth middleware — its authentication IS the signature.

use axum::extract::State;
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use serde_json::{json, Value};

use crate::web::SharedState;

/// The secret shared with the forge at registration time; stored encrypted
/// like every other credential.
pub const WEBHOOK_SECRET: &str = "webhook-secret";

pub async fn github(
    State(state): State<SharedState>,
    headers: HeaderMap,
    body: String,
) -> Response {
    // No secret means webhooks were never set up: answer as if the route
    // does not exist, so an unset Box reveals nothing.
    let Ok(Some(secret)) = crate::secrets::get(&state.paths, WEBHOOK_SECRET) else {
        return StatusCode::NOT_FOUND.into_response();
    };
    let expected = crate::util::hmac_sha256(secret.as_bytes(), body.as_bytes());
    let received = headers
        .get("x-hub-signature-256")
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("sha256="))
        .unwrap_or_default();
    if !crate::util::constant_time_eq_hex(&expected, received) {
        return (
            StatusCode::UNAUTHORIZED,
            axum::Json(json!({ "error": "signature mismatch" })),
        )
            .into_response();
    }

    let payload: Value = match serde_json::from_str(&body) {
        Ok(v) => v,
        Err(_) => {
            return (
                StatusCode::BAD_REQUEST,
                axum::Json(json!({ "error": "not JSON" })),
            )
                .into_response()
        }
    };
    // GitHub's ping event proves the registration; answer politely.
    if payload.get("zen").is_some() {
        return axum::Json(json!({ "pong": true })).into_response();
    }
    let repo = payload
        .get("repository")
        .and_then(|r| r.get("full_name"))
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string();
    let branch = payload
        .get("ref")
        .and_then(Value::as_str)
        .and_then(|r| r.strip_prefix("refs/heads/"))
        .unwrap_or_default()
        .to_string();
    if repo.is_empty() || branch.is_empty() {
        return axum::Json(json!({ "ignored": "not a branch push" })).into_response();
    }

    // Sync every service linked to this repo+branch — previews included,
    // since each preview is its own service on its own branch.
    let state = state.clone();
    let outcome = crate::web::blocking_sync(state, repo.clone(), branch.clone()).await;
    match outcome {
        Ok(synced) => axum::Json(json!({ "synced": synced })).into_response(),
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            axum::Json(json!({ "error": format!("{e:#}") })),
        )
            .into_response(),
    }
}
