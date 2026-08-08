pub mod api;
pub mod mcp;
pub mod pages;
pub mod sites;

use std::sync::{Arc, Mutex};

use axum::{
    http::StatusCode,
    middleware::from_fn_with_state,
    response::{IntoResponse, Response},
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
    /// Serializes apply/deploy/rollback so concurrent requests cannot race
    /// on the generation-src directory or profile numbering.
    pub apply_lock: Mutex<()>,
}

impl AppState {
    pub fn new(paths: Paths, builder: Box<dyn Builder>) -> SharedState {
        let tunnel = TunnelManager::new(paths.clone());
        Arc::new(Self {
            paths,
            builder,
            tunnel,
            apply_lock: Mutex::new(()),
        })
    }
}

pub type SharedState = Arc<AppState>;

pub fn router(state: SharedState) -> Router {
    Router::new()
        .route("/", get(pages::index))
        .route("/services/new", get(pages::new_service))
        .route("/services", post(pages::create_service))
        .route("/services/{name}/delete", post(pages::delete_service))
        .route("/generations", get(pages::generations))
        .route("/generations/{number}/rollback", post(pages::rollback))
        .route("/system", get(pages::system))
        .route("/system/check", post(pages::system_check))
        .route("/network", get(pages::network))
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
        .layer(from_fn_with_state(state, sites::host_dispatch))
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
