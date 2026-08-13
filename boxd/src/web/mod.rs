pub mod api;
pub mod mcp;
pub mod pages;
pub mod sites;
pub mod wizard;

use std::net::SocketAddr;
use std::sync::{Arc, Mutex};

use axum::{
    extract::{ConnectInfo, Request, State},
    http::{header, StatusCode},
    middleware::{from_fn_with_state, Next},
    response::{IntoResponse, Redirect, Response},
    routing::{get, post},
    Router,
};

use crate::paths::Paths;
use crate::store::Builder;
use crate::tunnel::TunnelManager;

pub struct AppState {
    pub paths: Paths,
    pub builder: Box<dyn Builder>,
    pub tunnel: Arc<TunnelManager>,
    /// Long operations (deploy, rollback, recreate, backup, platform update)
    /// run here instead of inside the request, so the console can show progress
    /// rather than hanging on a Nix build.
    pub jobs: Arc<crate::jobs::Registry>,
    /// In-flight WebAuthn ceremonies (security-key enrollment and sign-in).
    /// Deliberately in memory: a challenge is single-use and a restart should
    /// invalidate it rather than leave something replayable on disk.
    pub ceremonies: Arc<crate::webauthn::Ceremonies>,
    /// Serializes apply/deploy/rollback so concurrent requests cannot race
    /// on the generation-src directory or profile numbering.
    pub apply_lock: Mutex<()>,
}

impl AppState {
    pub fn new(paths: Paths, builder: Box<dyn Builder>) -> SharedState {
        let tunnel = TunnelManager::new(paths.clone());
        Arc::new(Self {
            jobs: crate::jobs::Registry::persistent(paths.data_dir.join("jobs")),
            paths,
            builder,
            tunnel,
            ceremonies: Arc::new(crate::webauthn::Ceremonies::new()),
            apply_lock: Mutex::new(()),
        })
    }
}

pub type SharedState = Arc<AppState>;

pub fn router(state: SharedState) -> Router {
    Router::new()
        .route("/", get(pages::index))
        .route("/services/new", get(pages::new_service))
        .route("/services/new/{template}", get(pages::new_service_form))
        .route("/services", post(pages::create_service))
        .route("/services/{name}/delete", post(pages::delete_service))
        .route("/jobs/{id}", get(pages::job_view))
        .route("/generations", get(pages::generations))
        .route("/generations/{number}/rollback", post(pages::rollback))
        .route("/system", get(pages::system))
        .route("/system/channel", post(pages::system_set_channel))
        .route("/system/cloud", post(pages::cloud_enroll))
        .route("/system/cloud/provision", post(pages::cloud_provision))
        .route("/system/check", post(pages::system_check))
        .route("/system/update", post(pages::system_update))
        .route("/system/apply", post(pages::system_apply))
        .route("/fleet", get(pages::fleet))
        .route("/pair", get(pages::pair))
        .route("/pair/redeem", post(pages::pair_redeem))
        .route("/pair/claim", post(pages::pair_claim))
        .route("/devices", get(pages::devices))
        .route("/devices/add", post(pages::add_device))
        .route("/devices/agent", post(pages::create_agent_connection))
        .route("/devices/{id}/revoke", post(pages::revoke_device))
        .route("/devices/keys/start", post(pages::key_register_start))
        .route("/devices/keys/finish", post(pages::key_register_finish))
        .route("/devices/keys/{id}/revoke", post(pages::revoke_key))
        .route("/pair/key/start", post(pages::key_signin_start))
        .route("/pair/key/finish", post(pages::key_signin_finish))
        .route("/backup", get(pages::backup))
        .route("/backup/configure", post(pages::configure_backup))
        .route("/backup/run", post(pages::run_backup_now))
        .route("/backup/restore", post(pages::restore_backup))
        .route("/backup/config-remote", post(pages::set_config_remote))
        .route("/backup/config-push", post(pages::push_config_now))
        .route("/recreate", get(pages::recreate_page).post(pages::recreate_run))
        .route("/network", get(pages::network))
        .route("/network/connect", post(pages::connect_enroll))
        .route("/network/connect/down", post(pages::connect_down))
        .route("/network/cloudflare", post(pages::configure_cloudflare))
        .route(
            "/network/cloudflare/disable",
            post(pages::disable_cloudflare),
        )
        .route("/sites/{name}", get(sites::redirect_to_slash))
        .route("/sites/{name}/", get(sites::serve_index))
        .route("/sites/{name}/{*path}", get(sites::serve_path))
        .route(
            "/mcp",
            post(mcp::handle)
                .get(mcp::method_not_allowed)
                .delete(mcp::end_session),
        )
        .nest("/api/v1", api::router())
        .with_state(state.clone())
        // Inner: operator auth. Outer: host_dispatch (peels off tunnel/public-
        // site traffic first, so auth only ever guards management requests).
        .layer(from_fn_with_state(state.clone(), require_auth))
        .layer(from_fn_with_state(state, sites::host_dispatch))
}

/// Gate management behind operator auth: coarse-public paths pass; everything
/// else needs a valid session, wherever it came from. Browsers without one are
/// sent to pair; machines get a 401.
async fn require_auth(
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    State(state): State<SharedState>,
    request: Request,
    next: Next,
) -> Response {
    let path = request.uri().path();
    if crate::auth::is_public_path(path) {
        // `/sites/<name>/` is credential-free on your own network — that is
        // what the console promises. It is NOT credential-free through the
        // tunnel unless the operator published that service.
        if let Some(name) = path.strip_prefix("/sites/").map(|rest| {
            rest.split('/').next().unwrap_or_default().to_string()
        }) {
            if !name.is_empty() && crate::auth::is_proxied(request.headers()) {
                match sites::site_is_public(&state, &name) {
                    Some(true) => {}
                    _ => {
                        return (
                            StatusCode::NOT_FOUND,
                            axum::Json(serde_json::json!({ "error": "service not found" })),
                        )
                            .into_response()
                    }
                }
            }
        }
        return next.run(request).await;
    }
    let headers = request.headers();
    // Before any authorization: a write that a browser started on another site
    // is refused outright. Loopback access needs no credential, so without this
    // any page the operator visits could drive the console (see
    // auth::cross_site_write).
    if crate::auth::cross_site_write(request.method().as_str(), headers) {
        return (
            StatusCode::FORBIDDEN,
            axum::Json(serde_json::json!({ "error": "cross-site request refused" })),
        )
            .into_response();
    }
    // Authority comes from holding a session, never from where the connection
    // came from. Loopback used to be treated as proof of ownership ("if you can
    // reach it you already own the box"), but every service the Box runs can
    // also reach 127.0.0.1:2693 — so a deployed app could mint a pairing code
    // and take the console over. A session token requires reading the data dir
    // (0700, boxd's own), which those services cannot do.
    let authorized = crate::auth::extract_token(headers)
        .is_some_and(|t| crate::auth::verify(&state.paths, &t));
    let _ = peer;
    if authorized {
        return next.run(request).await;
    }
    let wants_html = headers
        .get(header::ACCEPT)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|a| a.contains("text/html"));
    if wants_html {
        Redirect::to("/pair").into_response()
    } else {
        // An agent's first contact with a Box lands exactly here, so this is
        // where the door has to explain its own handle. "pairing required"
        // alone sent a real agent off to SSH; the two fields below are the
        // difference between a dead end and a bootstrap.
        (
            StatusCode::UNAUTHORIZED,
            axum::Json(serde_json::json!({
                "error": "pairing required",
                "how": "Ask the person for this Box's pairing code (the installer and the \
                        Console both show it). Then POST /pair/redeem with JSON \
                        {\"code\": \"<the code>\", \"label\": \"<who you are>\"} — the \
                        response carries a session token. Send it on every request as \
                        'Authorization: Bearer <token>'.",
                "mcp": "/mcp",
            })),
        )
            .into_response()
    }
}

/// Errors bubbling out of handlers become a JSON 500; user-facing flows
/// catch errors themselves and surface them as flash messages instead.
pub struct AppError(pub anyhow::Error);

impl<E: Into<anyhow::Error>> From<E> for AppError {
    fn from(err: E) -> Self {
        Self(err.into())
    }
}

impl IntoResponse for AppError {
    fn into_response(self) -> Response {
        tracing::error!("request failed: {:#}", self.0);
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            axum::Json(serde_json::json!({ "error": format!("{:#}", self.0) })),
        )
            .into_response()
    }
}

/// Run filesystem/subprocess-heavy work off the async executor.
pub async fn blocking<T, F>(f: F) -> anyhow::Result<T>
where
    T: Send + 'static,
    F: FnOnce() -> anyhow::Result<T> + Send + 'static,
{
    tokio::task::spawn_blocking(f)
        .await
        .map_err(|e| anyhow::anyhow!("background task failed: {e}"))?
}
