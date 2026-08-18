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
        // Uploading a built site is how someone's own project reaches the Box
        // without an SSH key. Sites are bigger than a JSON control message, so
        // this route gets a bigger body limit than the default.
        .route(
            "/services/{name}/files",
            post(upload_files).layer(axum::extract::DefaultBodyLimit::max(64 * 1024 * 1024)),
        )
        .route("/generations", get(list_generations))
        .route("/generations/{number}/rollback", post(rollback))
        .route("/history", get(list_history))
        .route("/templates", get(list_templates))
        .route("/network", get(network_status).post(configure_network))
        .route("/services/{name}/logs", get(service_logs))
        .route("/jobs/{id}", get(job_status))
        .route("/health", get(health))
        .route("/fleet", get(fleet))
}

#[derive(Deserialize)]
struct LogQuery {
    #[serde(default)]
    lines: Option<usize>,
}

/// Recent log lines for a service — the diagnostic an agent needs to fix
/// something rather than just observe that it is broken.
async fn service_logs(
    State(state): State<SharedState>,
    Path(name): Path<String>,
    axum::extract::Query(q): axum::extract::Query<LogQuery>,
) -> Result<Json<serde_json::Value>, AppError> {
    let lines = q.lines.unwrap_or(100).clamp(1, 1000);
    Ok(Json(
        blocking(move || crate::logs::service_logs(&state.paths, &name, lines)).await?,
    ))
}

/// Live state of one background operation: what the console polls while a
/// deploy or platform update runs.
async fn job_status(
    State(state): State<SharedState>,
    Path(id): Path<String>,
) -> Result<Json<crate::jobs::Job>, AppError> {
    state
        .jobs
        .get(&id)
        .map(Json)
        .ok_or_else(|| AppError(anyhow::anyhow!("no such job")))
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
    id: String,
    title: String,
    description: String,
    /// "primitive" or "preset"; presets also carry the primitive they configure.
    kind: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    base: Option<String>,
}

/// Everything deployable: the primitives plus the box's catalog presets — the
/// same set the dashboard chooser and MCP `list_templates` show.
async fn list_templates(State(state): State<SharedState>) -> Json<Vec<TemplateView>> {
    let mut views: Vec<TemplateView> = templates::all()
        .iter()
        .map(|t| TemplateView {
            id: t.id().into(),
            title: t.title().into(),
            description: t.description().into(),
            kind: "primitive",
            base: None,
        })
        .collect();
    for entry in crate::catalog::for_data_dir(&state.paths.data_dir).values() {
        views.push(TemplateView {
            id: entry.id.clone(),
            title: entry.title.clone(),
            description: entry.description.clone(),
            kind: "preset",
            base: Some(entry.base.clone()),
        });
    }
    Json(views)
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
    /// Absent when nothing reaches this service over HTTP (an internal-only
    /// database, say).
    url: Option<String>,
    state: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    note: Option<String>,
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
        .map(|s| {
            let status = ops::service_status(&state.paths, &s, active.contains(&s.name));
            ServiceView {
                url: status.url,
                state: status.state,
                note: status.note,
                name: s.name,
                template: s.template,
                params: s.params,
                domain: s.domain,
                public: s.public,
                created_at: s.created_at,
            }
        })
        .collect();
    Ok(Json(views))
}

#[derive(Deserialize)]
struct DeployBody {
    name: String,
    /// Template or catalog-preset id. Absent → static-site (the historical
    /// shape of this endpoint, kept for existing clients).
    #[serde(default)]
    template: Option<String>,
    /// Template params, passed through to the same central deploy an agent's
    /// MCP `deploy` uses (validation, ports, secret encryption included).
    #[serde(default)]
    params: Option<serde_json::Value>,
    #[serde(default)]
    port: Option<u16>,
    #[serde(default)]
    domain: Option<String>,
    /// Omitted means "leave it as it is" on a redeploy — see
    /// [`ops::DeployRequest::public`].
    #[serde(default)]
    public: Option<bool>,
    #[serde(default)]
    index_html: Option<String>,
    #[serde(default)]
    source_path: Option<PathBuf>,
}

#[derive(Serialize)]
struct DeployResult {
    service: String,
    generation: u64,
    url: Option<String>,
    state: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    note: Option<String>,
}

async fn create_service(
    State(state): State<SharedState>,
    Json(body): Json<DeployBody>,
) -> Result<Json<DeployResult>, AppError> {
    let request = match body.template {
        // Any template or catalog preset, same as MCP deploy.
        Some(template) => ops::DeployRequest {
            name: body.name.trim().to_string(),
            template,
            params: body.params.unwrap_or_else(|| serde_json::json!({})),
            domain: body.domain,
            public: body.public,
            port: body.port,
        },
        // Historical shape: no template → a static site.
        None => ops::DeployRequest::static_site(
            body.name.trim().to_string(),
            body.index_html,
            body.source_path,
            body.domain,
            body.public.unwrap_or(false),
        ),
    };
    let name = request.name.clone();
    let info = {
        let state = state.clone();
        blocking(move || {
            let _guard = state.apply_lock.lock().unwrap_or_else(|e| e.into_inner());
            ops::deploy(&state.paths, state.builder.as_ref(), request)
        })
        .await?
    };
    // Report the URL that actually serves it, and whether it is really
    // running: `/sites/<name>/` is only true for a file service.
    let status = BoxConfig::load(&state.paths)
        .ok()
        .and_then(|c| c.find(&name).cloned())
        .map(|s| ops::service_status(&state.paths, &s, true));
    Ok(Json(DeployResult {
        url: status.as_ref().and_then(|s| s.url.clone()),
        state: status.as_ref().map(|s| s.state).unwrap_or("active"),
        note: status.and_then(|s| s.note),
        service: name,
        generation: info.number,
    }))
}

/// Files for a service, as an agent would send them: a map of relative path to
/// contents. A string is text; `{"base64": "..."}` carries anything else.
#[derive(Deserialize)]
struct UploadBody {
    files: std::collections::BTreeMap<String, FileContent>,
    /// Replace everything staged for this service (the default) rather than
    /// adding to it — an upload is a whole site, not another layer of one.
    #[serde(default = "default_true")]
    replace: bool,
    /// Deploy the uploaded files as a static site straight away.
    #[serde(default)]
    deploy: bool,
    #[serde(default)]
    domain: Option<String>,
    #[serde(default)]
    public: Option<bool>,
}

fn default_true() -> bool {
    true
}

#[derive(Deserialize)]
#[serde(untagged)]
enum FileContent {
    Text(String),
    Base64 { base64: String },
}

impl FileContent {
    fn bytes(self) -> Result<Vec<u8>, AppError> {
        match self {
            FileContent::Text(t) => Ok(t.into_bytes()),
            FileContent::Base64 { base64 } => crate::provision::base64_decode(&base64)
                .map_err(|e| AppError(anyhow::anyhow!("invalid base64: {e:#}"))),
        }
    }
}

#[derive(Serialize)]
struct UploadResult {
    service: String,
    files: usize,
    bytes: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    generation: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    url: Option<String>,
}

/// Put a built site on the Box over HTTP, so publishing your own project needs
/// no SSH key and no `scp`.
async fn upload_files(
    State(state): State<SharedState>,
    Path(name): Path<String>,
    Json(body): Json<UploadBody>,
) -> Result<Json<UploadResult>, AppError> {
    let mut files = Vec::with_capacity(body.files.len());
    for (path, content) in body.files {
        files.push(ops::UploadedFile {
            path,
            bytes: content.bytes()?,
        });
    }

    let deploy = body.deploy;
    let domain = body.domain.clone();
    let public = body.public;
    let service = name.clone();
    let result = {
        let state = state.clone();
        blocking(move || {
            let _guard = state.apply_lock.lock().unwrap_or_else(|e| e.into_inner());
            let (count, bytes) = ops::upload_files(&state.paths, &service, files, body.replace)?;
            if !deploy {
                return Ok((count, bytes, None));
            }
            let info = ops::deploy(
                &state.paths,
                state.builder.as_ref(),
                ops::DeployRequest {
                    name: service.clone(),
                    template: "static-site".into(),
                    params: serde_json::json!({
                        "source_path": state.paths.upload_dir(&service).display().to_string(),
                    }),
                    domain,
                    public,
                    port: None,
                },
            )?;
            Ok((count, bytes, Some(info.number)))
        })
        .await?
    };

    let (files, bytes, generation) = result;
    let url = BoxConfig::load(&state.paths)
        .ok()
        .and_then(|c| c.find(&name).cloned())
        .and_then(|s| ops::service_status(&state.paths, &s, generation.is_some()).url);
    Ok(Json(UploadResult {
        service: name,
        files,
        bytes,
        generation,
        url,
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
            let _guard = state.apply_lock.lock().unwrap_or_else(|e| e.into_inner());
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
            let _guard = state.apply_lock.lock().unwrap_or_else(|e| e.into_inner());
            ops::rollback(&state.paths, number)
        })
        .await?
    };
    Ok(Json(GenerationResult {
        generation: info.number,
    }))
}
