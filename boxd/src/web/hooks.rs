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
    // Pull requests get previews: every PR its own service, its own URL,
    // alive exactly as long as the PR is.
    if payload.get("pull_request").is_some() {
        return pull_request_event(state, &payload).await;
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

/// PR lifecycle → preview lifecycle. Opened/updated same-repo PRs against a
/// linked repository get (or refresh) a preview service named
/// `<parent>-pr<N>`; a closed PR takes its preview down. Fork PRs are
/// deliberately ignored: the sandbox would contain the build, but the Box
/// only auto-builds branches of repositories the operator linked, not
/// whatever a stranger pointed at them.
async fn pull_request_event(state: SharedState, payload: &Value) -> Response {
    let action = payload
        .get("action")
        .and_then(Value::as_str)
        .unwrap_or_default();
    let repo = payload
        .get("repository")
        .and_then(|r| r.get("full_name"))
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string();
    let pr = payload.get("pull_request").cloned().unwrap_or_default();
    let number = pr.get("number").and_then(Value::as_u64).unwrap_or_default();
    let head_branch = pr
        .get("head")
        .and_then(|h| h.get("ref"))
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string();
    let head_repo = pr
        .get("head")
        .and_then(|h| h.get("repo"))
        .and_then(|r| r.get("full_name"))
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string();
    if repo.is_empty() || number == 0 {
        return axum::Json(json!({ "ignored": "not a pull request event" })).into_response();
    }
    if !head_repo.is_empty() && !head_repo.eq_ignore_ascii_case(&repo) {
        return axum::Json(json!({
            "ignored": "fork pull request",
            "note": "previews build only branches of the linked repository itself",
        }))
        .into_response();
    }

    let outcome = match action {
        "opened" | "synchronize" | "reopened" | "ready_for_review" => {
            let state = state.clone();
            tokio::task::spawn_blocking(move || {
                crate::web::preview_open(&state, &repo, number, &head_branch)
            })
            .await
            .map_err(anyhow::Error::from)
            .and_then(|r| r)
        }
        "closed" => {
            let state = state.clone();
            tokio::task::spawn_blocking(move || crate::web::preview_close(&state, &repo, number))
                .await
                .map_err(anyhow::Error::from)
                .and_then(|r| r)
        }
        other => {
            return axum::Json(json!({ "ignored": format!("pull request action {other}") }))
                .into_response()
        }
    };
    match outcome {
        Ok(v) => axum::Json(v).into_response(),
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            axum::Json(json!({ "error": format!("{e:#}") })),
        )
            .into_response(),
    }
}
