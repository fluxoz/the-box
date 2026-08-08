//! JSON API under /api/v1 — the machine surface the MCP tools will wrap.

use std::path::PathBuf;

use axum::{
    extract::{Path, State},
    routing::{delete, get, post},
    Json, Router,
};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::config::BoxConfig;
use crate::history::{self, HistoryEntry};
use crate::manifest;
use crate::ops;
use crate::store::{self, GenerationInfo};
use crate::templates;

use super::{blocking, AppError, SharedState};

pub fn router() -> Router<SharedState> {
    Router::new()
        .route("/status", get(status))
        .route("/services", get(list_services).post(create_service))
        .route("/services/{name}", delete(delete_service))
        .route("/generations", get(list_generations))
        .route("/generations/{number}/rollback", post(rollback))
        .route("/history", get(list_history))
        .route("/templates", get(list_templates))
        .route("/network", get(network_status).post(configure_network))
        .route("/health", get(health))
        .route("/fleet", get(fleet))
}

/// Coarse public health — what peers read to render the fleet map. Public by
/// design (discovery ≠ authorization); reveals only a summary.
async fn health(State(state): State<SharedState>) -> Json<crate::fleet::CoarseHealth> {
    Json(crate::fleet::self_health(&state.paths))
}

#[derive(Serialize)]
struct FleetView {
    this_box: crate::fleet::CoarseHealth,
    peers: Vec<crate::fleet::Peer>,
}

/// This Box plus the peers it can currently discover on the LAN.
async fn fleet(State(state): State<SharedState>) -> Result<Json<FleetView>, AppError> {
    let this_box = crate::fleet::self_health(&state.paths);
    let peers = blocking(|| Ok(crate::fleet::discover())).await?;
    Ok(Json(FleetView { this_box, peers }))
}

#[derive(Serialize)]
struct TemplateView {
    id: &'static str,
    title: &'static str,
    description: &'static str,
}

async fn list_templates() -> Json<Vec<TemplateView>> {
    Json(
        templates::all()
            .iter()
            .map(|t| TemplateView {
                id: t.id(),
                title: t.title(),
                description: t.description(),
            })
            .collect(),
    )
}

async fn list_history(
    State(state): State<SharedState>,
) -> Result<Json<Vec<HistoryEntry>>, AppError> {
    Ok(Json(history::log(&state.paths, 100)?))
}

#[derive(Serialize)]
struct Status {
    version: &'static str,
    builder: &'static str,
    data_dir: String,
    current_generation: Option<u64>,
    services: usize,
}

async fn status(State(state): State<SharedState>) -> Result<Json<Status>, AppError> {
    let config = BoxConfig::load(&state.paths)?;
    let current = store::current(&state.paths)?;
    Ok(Json(Status {
        version: env!("CARGO_PKG_VERSION"),
        builder: state.builder.name(),
        data_dir: state.paths.data_dir.display().to_string(),
        current_generation: current.map(|g| g.number),
        services: config.services.len(),
    }))
}

#[derive(Serialize)]
struct ServiceView {
    name: String,
    template: String,
    params: serde_json::Value,
    domain: Option<String>,
    public: bool,
    created_at: DateTime<Utc>,
    url: String,
    state: &'static str,
}

async fn list_services(
    State(state): State<SharedState>,
) -> Result<Json<Vec<ServiceView>>, AppError> {
    let config = BoxConfig::load(&state.paths)?;
    let active: Vec<String> = store::current(&state.paths)?
        .and_then(|c| manifest::read_manifest(&c.store_path).ok())
        .map(|m| m.services.into_iter().map(|s| s.name).collect())
        .unwrap_or_default();
    let views = config
        .services
        .into_iter()
        .map(|s| ServiceView {
            url: format!("/sites/{}/", s.name),
            state: if active.contains(&s.name) {
                "active"
            } else {
                "pending"
            },
            name: s.name,
            template: s.template,
            params: s.params,
            domain: s.domain,
            public: s.public,
            created_at: s.created_at,
        })
        .collect();
    Ok(Json(views))
}

#[derive(Deserialize)]
struct DeployBody {
    name: String,
    #[serde(default)]
    domain: Option<String>,
    #[serde(default)]
    public: bool,
    #[serde(default)]
    index_html: Option<String>,
    #[serde(default)]
    source_path: Option<PathBuf>,
}

#[derive(Serialize)]
struct DeployResult {
    service: String,
    generation: u64,
    url: String,
}

async fn create_service(
    State(state): State<SharedState>,
    Json(body): Json<DeployBody>,
) -> Result<Json<DeployResult>, AppError> {
    let request = ops::DeployRequest::static_site(
        body.name.trim().to_string(),
        body.index_html,
        body.source_path,
        body.domain,
        body.public,
    );
    let name = request.name.clone();
    let info = {
        let state = state.clone();
        blocking(move || {
            let _guard = state.apply_lock.lock().unwrap();
            ops::deploy(&state.paths, state.builder.as_ref(), request)
        })
        .await?
    };
    Ok(Json(DeployResult {
        url: format!("/sites/{name}/"),
        service: name,
        generation: info.number,
    }))
}

#[derive(Serialize)]
struct GenerationResult {
    generation: u64,
}

async fn delete_service(
    State(state): State<SharedState>,
    Path(name): Path<String>,
) -> Result<Json<GenerationResult>, AppError> {
    let info = {
        let state = state.clone();
        blocking(move || {
            let _guard = state.apply_lock.lock().unwrap();
            ops::delete_service(&state.paths, state.builder.as_ref(), &name)
        })
        .await?
    };
    Ok(Json(GenerationResult {
        generation: info.number,
    }))
}

async fn list_generations(
    State(state): State<SharedState>,
) -> Result<Json<Vec<GenerationInfo>>, AppError> {
    Ok(Json(store::list(&state.paths)?))
}

#[derive(Deserialize)]
struct NetworkBody {
    #[serde(default)]
    token: Option<String>,
    enabled: bool,
}

async fn network_status(State(state): State<SharedState>) -> Json<crate::tunnel::TunnelStatus> {
    Json(state.tunnel.status())
}

async fn configure_network(
    State(state): State<SharedState>,
    Json(body): Json<NetworkBody>,
) -> Result<Json<crate::tunnel::TunnelStatus>, AppError> {
    let status = state
        .tunnel
        .configure(body.token.as_deref(), body.enabled)?;
    Ok(Json(status))
}

async fn rollback(
    State(state): State<SharedState>,
    Path(number): Path<u64>,
) -> Result<Json<GenerationResult>, AppError> {
    let info = {
        let state = state.clone();
        blocking(move || {
            let _guard = state.apply_lock.lock().unwrap();
            ops::rollback(&state.paths, number)
        })
        .await?
    };
    Ok(Json(GenerationResult {
        generation: info.number,
    }))
}
