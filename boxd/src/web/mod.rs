pub mod aiproxy;
pub mod api;
pub mod hooks;
pub mod mcp;
pub mod pages;
pub mod sites;
pub mod wizard;

use std::net::SocketAddr;
use std::sync::{Arc, Mutex};

use axum::{
    extract::{ConnectInfo, Request, State},
    http::{header, HeaderValue, StatusCode},
    middleware::{from_fn, from_fn_with_state, Next},
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
    /// How this machine runs repository build steps: the sandbox on a managed
    /// Box, nothing on a bare dev machine. Detected once so every code path
    /// (poller, MCP, link) agrees.
    pub build_exec: crate::build::BuildExec,
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
    ///
    /// Always taken with `unwrap_or_else(|e| e.into_inner())`, never `unwrap()`.
    /// The guarded value is `()` — there is no invariant a panic could leave
    /// half-applied — but a poisoned lock would make every later deploy AND
    /// every rollback fail forever, taking the escape hatch down with the
    /// thing it exists to escape from.
    pub apply_lock: Mutex<()>,
}

impl AppState {
    pub fn new(paths: Paths, builder: Box<dyn Builder>) -> SharedState {
        Self::with_build_exec(paths, builder, crate::build::BuildExec::detect())
    }

    /// Tests pass [`crate::build::BuildExec::Direct`] to run build phases
    /// without a container runtime; everything real goes through `new`, which
    /// detects what this machine actually has.
    pub fn with_build_exec(
        paths: Paths,
        builder: Box<dyn Builder>,
        build_exec: crate::build::BuildExec,
    ) -> SharedState {
        let tunnel = TunnelManager::new(paths.clone());
        Arc::new(Self {
            jobs: crate::jobs::Registry::persistent(paths.data_dir.join("jobs")),
            paths,
            builder,
            tunnel,
            build_exec,
            ceremonies: Arc::new(crate::webauthn::Ceremonies::new()),
            apply_lock: Mutex::new(()),
        })
    }
}

pub type SharedState = Arc<AppState>;

/// Sync every service linked to `repo` on `branch`, under the apply lock, off
/// the async runtime. Shared by the webhook receiver; the poller does the
/// same dance on its own thread.
pub async fn blocking_sync(
    state: SharedState,
    repo: String,
    branch: String,
) -> anyhow::Result<Vec<String>> {
    tokio::task::spawn_blocking(move || {
        let config = crate::config::BoxConfig::load(&state.paths)?;
        let matching: Vec<String> = config
            .services
            .iter()
            .filter(|s| {
                s.repo
                    .as_ref()
                    .is_some_and(|l| l.repo.eq_ignore_ascii_case(&repo) && l.branch == branch)
            })
            .map(|s| s.name.clone())
            .collect();
        let mut synced = Vec::new();
        for name in matching {
            let _guard = state.apply_lock.lock().unwrap_or_else(|e| e.into_inner());
            crate::pull::sync_recorded(
                &state.paths,
                state.builder.as_ref(),
                &state.build_exec,
                &name,
                false,
            )?;
            synced.push(name);
        }
        Ok(synced)
    })
    .await?
}

/// Who is making this request, resolved by the auth middleware from the
/// session the token belongs to. The destructive-op gate reads `autonomous`;
/// approval records read `label` so the human sees who asked; `id` is what
/// keeps a session from being both halves of the trust ceremony — granting
/// itself autonomy, or approving its own request.
#[derive(Clone, Debug)]
pub struct Caller {
    pub id: String,
    pub label: String,
    pub autonomous: bool,
    /// Operator or agent. An agent may run infrastructure; it may not enlarge
    /// anyone's authority, because an agent that can mint a second session and
    /// bless it is both halves of the approval ceremony.
    pub provenance: crate::auth::Provenance,
}

impl Caller {
    pub fn is_agent(&self) -> bool {
        self.provenance == crate::auth::Provenance::Agent
    }
}

pub fn router(state: SharedState) -> Router {
    Router::new()
        .route("/", get(pages::index))
        .route("/services/new", get(pages::new_service))
        .route("/services/new/{template}", get(pages::new_service_form))
        .route("/services", post(pages::create_service))
        .route("/services/{name}/delete", post(pages::delete_service))
        .route("/service/{name}", get(pages::service_detail))
        .route("/service/{name}/sync", post(pages::service_sync_now))
        .route("/service/{name}/access", post(pages::service_access))
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
        .route("/system/meter-rate", post(pages::set_meter_rate))
        .route("/fleet", get(pages::fleet))
        .route("/pair", get(pages::pair))
        .route("/pair/redeem", post(pages::pair_redeem))
        .route("/hooks/github", post(hooks::github))
        .route("/v1/chat/completions", post(aiproxy::chat_completions))
        .route("/v1/models", get(aiproxy::models))
        .route("/pair/claim", post(pages::pair_claim))
        .route("/devices", get(pages::devices))
        .route("/devices/add", post(pages::add_device))
        .route("/devices/agent", post(pages::create_agent_connection))
        .route("/devices/ai-key", post(pages::mint_ai_key))
        .route("/devices/ai-key/{id}/revoke", post(pages::revoke_ai_key))
        .route("/devices/{id}/revoke", post(pages::revoke_device))
        .route("/devices/{id}/autonomy", post(pages::set_device_autonomy))
        .route("/approvals", get(pages::approvals))
        .route("/journal", get(pages::journal_page))
        .route("/approvals/{id}/approve", post(pages::approve_action))
        .route("/approvals/{id}/deny", post(pages::deny_action))
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
        .route(
            "/backup/config-repo/github",
            post(pages::create_config_repo_github),
        )
        .route("/backup/config-push", post(pages::push_config_now))
        .route(
            "/recreate",
            get(pages::recreate_page).post(pages::recreate_run),
        )
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
        // Outermost, so it covers every response including errors and served
        // sites.
        .layer(from_fn(security_headers))
}

/// Headers every response carries.
///
/// `frame-ancestors 'none'` is the load-bearing one: the CSRF defence compares
/// Origin against Host, which a clickjacked console passes with flying colours
/// because the POST really does come from the console's own origin. Framing is
/// the bypass, so framing is refused.
///
/// This deliberately sets no `default-src`. The console inlines its stylesheet
/// and script, so a content policy would need `'unsafe-inline'` and buy nothing
/// until those move to their own files; `form-action` and `base-uri` are the
/// parts that work regardless.
async fn security_headers(request: Request, next: Next) -> Response {
    let mut response = next.run(request).await;
    let h = response.headers_mut();
    h.insert(
        "content-security-policy",
        HeaderValue::from_static("frame-ancestors 'none'; base-uri 'none'; form-action 'self'"),
    );
    // Belt and braces for browsers that predate frame-ancestors.
    h.insert("x-frame-options", HeaderValue::from_static("DENY"));
    // An uploaded file must never be re-interpreted as script because a
    // browser sniffed the bytes and disagreed with the declared type.
    h.insert(
        "x-content-type-options",
        HeaderValue::from_static("nosniff"),
    );
    // Box hostnames and service paths are not the wider internet's business.
    h.insert("referrer-policy", HeaderValue::from_static("no-referrer"));
    response
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
    // BEFORE the public-path branch, not after. `/pair/claim` and
    // `/pair/redeem` are public because a Box with no operator yet cannot ask
    // for a credential — but "no credential needed" is not "any website may
    // POST here". Without this, a page the operator merely visits could burn
    // the one-shot first-run claim on every unclaimed Box it can reach.
    if crate::auth::cross_site_write(request.method().as_str(), request.headers()) {
        return (
            StatusCode::FORBIDDEN,
            axum::Json(serde_json::json!({ "error": "cross-site request refused" })),
        )
            .into_response();
    }
    if crate::auth::is_public_path(path) {
        // `/sites/<name>/` is credential-free on your own network — that is
        // what the console promises. It is NOT credential-free through the
        // tunnel unless the operator published that service.
        if let Some(name) = path
            .strip_prefix("/sites/")
            .map(|rest| rest.split('/').next().unwrap_or_default().to_string())
        {
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
    // Authority comes from holding a session, never from where the connection
    // came from. Loopback used to be treated as proof of ownership ("if you can
    // reach it you already own the box"), but every service the Box runs can
    // also reach 127.0.0.1:2693 — so a deployed app could mint a pairing code
    // and take the console over. A session token requires reading the data dir
    // (0700, boxd's own), which those services cannot do.
    let session = crate::auth::extract_token(headers)
        .and_then(|t| crate::auth::session_for(&state.paths, &t));
    let _ = peer;
    if let Some(session) = session {
        // Handlers that must know WHO is asking (the destructive-op gate)
        // read this; everything else ignores it.
        let mut request = request;
        request.extensions_mut().insert(Caller {
            id: session.id,
            label: session.label,
            autonomous: session.autonomous,
            provenance: session.provenance,
        });
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

/// Create or refresh the preview for one PR: for every linked service whose
/// repository matches, a sibling service named `<parent>-pr<N>` tracks the
/// PR's head branch with the parent's own build recipe. Domains ride the
/// wildcard the parent's zone already has, so a preview URL costs nothing.
pub fn preview_open(
    state: &SharedState,
    repo: &str,
    number: u64,
    head_branch: &str,
) -> anyhow::Result<serde_json::Value> {
    use anyhow::Context as _;
    if head_branch.is_empty() {
        anyhow::bail!("pull request event carried no head branch");
    }
    let _guard = state.apply_lock.lock().unwrap_or_else(|e| e.into_inner());
    let config = crate::config::BoxConfig::load(&state.paths)?;
    let zone = config.ingress.as_ref().and_then(|i| i.zone.clone());
    let parents: Vec<crate::config::ServiceConfig> = config
        .services
        .iter()
        .filter(|s| {
            s.repo.as_ref().is_some_and(|l| l.repo.eq_ignore_ascii_case(repo))
                // A preview is never a parent: no previews of previews.
                && !s.name.contains("-pr")
        })
        .cloned()
        .collect();
    if parents.is_empty() {
        return Ok(serde_json::json!({ "ignored": format!("no linked service for {repo}") }));
    }

    let mut previews = Vec::new();
    for parent in parents {
        // Service names cap at 32 chars; leave room for the suffix.
        let suffix = format!("-pr{number}");
        let stem: String = parent.name.chars().take(32 - suffix.len()).collect();
        let name = format!("{stem}{suffix}");
        let domain = zone.as_ref().map(|z| format!("{stem}-pr-{number}.{z}"));

        let mut config = crate::config::BoxConfig::load(&state.paths)?;
        if config.find(&name).is_none() {
            let mut params = parent.params.clone();
            if let Some(obj) = params.as_object_mut() {
                obj.remove("source_path");
                obj.insert(
                    "index_html".into(),
                    serde_json::Value::String(format!(
                        "preview of {repo} #{number} — first sync pending"
                    )),
                );
            }
            let link = parent.repo.clone().map(|mut l| {
                l.branch = head_branch.to_string();
                l
            });
            config.services.push(crate::config::ServiceConfig {
                name: name.clone(),
                template: parent.template.clone(),
                params,
                domain: domain.clone(),
                public: parent.public,
                port: None,
                repo: link,
                created_at: chrono::Utc::now(),
            });
            config.save(&state.paths)?;
        } else if let Some(svc) = config.services.iter_mut().find(|s| s.name == name) {
            // A reopened PR may have a new head branch; keep the link true.
            if let Some(l) = svc.repo.as_mut() {
                l.branch = head_branch.to_string();
            }
            config.save(&state.paths)?;
        }

        crate::pull::sync_recorded(
            &state.paths,
            state.builder.as_ref(),
            &state.build_exec,
            &name,
            false,
        )
        .with_context(|| format!("syncing preview {name}"))?;

        // The comment is the finishing touch, not the feature: without the
        // App's PR-write permission it fails, and the preview still serves.
        let url = domain
            .as_ref()
            .map(|d| format!("https://{d}/"))
            .unwrap_or_else(|| format!("/sites/{name}/ (on the Box's own network)"));
        let commented = crate::forge::token(&state.paths, "github")
            .ok()
            .map(|token| {
                crate::ghapi::comment_on_issue(
                    &token,
                    repo,
                    number,
                    &format!(
                        "The Box deployed a preview of this pull request: {url}\n\n\
                         It rebuilds on every push here and disappears when the PR closes."
                    ),
                )
                .is_ok()
            })
            .unwrap_or(false);
        crate::journal::record(
            &state.paths,
            "preview",
            format!("pull request #{number} on {repo} got its preview at {url}"),
        );
        previews.push(serde_json::json!({
            "service": name,
            "url": url,
            "commented": commented,
        }));
    }
    Ok(serde_json::json!({ "previews": previews }))
}

/// A closed PR takes its preview down: unlink, delete, new generation. This
/// is the Box's own lifecycle bookkeeping for a service it created itself —
/// not an agent asking to destroy operator data — so it does not queue for
/// approval.
pub fn preview_close(
    state: &SharedState,
    repo: &str,
    number: u64,
) -> anyhow::Result<serde_json::Value> {
    let _guard = state.apply_lock.lock().unwrap_or_else(|e| e.into_inner());
    let config = crate::config::BoxConfig::load(&state.paths)?;
    let suffix = format!("-pr{number}");
    let doomed: Vec<String> = config
        .services
        .iter()
        .filter(|s| {
            s.name.ends_with(&suffix)
                && s.repo
                    .as_ref()
                    .is_some_and(|l| l.repo.eq_ignore_ascii_case(repo))
        })
        .map(|s| s.name.clone())
        .collect();
    let mut removed = Vec::new();
    for name in doomed {
        let _ = crate::pull::unlink(&state.paths, &name);
        crate::ops::delete_service(&state.paths, state.builder.as_ref(), &name)?;
        crate::journal::record(
            &state.paths,
            "preview",
            format!("pull request #{number} on {repo} closed; its preview {name} is gone"),
        );
        removed.push(name);
    }
    Ok(serde_json::json!({ "removed": removed }))
}
