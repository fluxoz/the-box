use std::collections::HashSet;

use axum::{
    extract::{Path, Query, State},
    http::{header, HeaderMap, HeaderValue, StatusCode},
    response::{Html, IntoResponse, Redirect, Response},
    Form,
};
use maud::{html, Markup, PreEscaped, DOCTYPE};
use serde::Deserialize;

use crate::channel::{self, ChannelConfig};
use crate::config::BoxConfig;
use crate::manifest;
use crate::ops;
use crate::ostier;
use crate::store;

use super::{blocking, AppError, SharedState};

/// The platform release label the running system baked in (`/etc/box/platform.json`).
/// Absent on a plain dev host; that's fine — we fall back to the boxd version.
fn platform_release() -> Option<String> {
    let text = std::fs::read_to_string("/etc/box/platform.json").ok()?;
    let v: serde_json::Value = serde_json::from_str(&text).ok()?;
    v.get("release")
        .and_then(|r| r.as_str())
        .map(str::to_string)
}

/// Trim a long flake ref for display without losing the front (the scheme/host).
fn short_ref(s: &str) -> String {
    let n = 32;
    if s.chars().count() <= n {
        s.to_string()
    } else {
        s.chars().take(n - 1).collect::<String>() + "…"
    }
}

/// A platform revision is a git rev or a `sha256-…` content hash; show a stub.
fn short_rev(s: &str) -> String {
    let s = s.strip_prefix("sha256-").unwrap_or(s);
    let n = 12;
    if s.chars().count() <= n {
        s.to_string()
    } else {
        s.chars().take(n).collect::<String>() + "…"
    }
}

const CSS: &str = include_str!("style.css");
const JS: &str = include_str!("dash.js");

// Inline SVG icons — no emoji, so they render identically on every platform.
const MARK_SVG: &str = r##"<svg class="ico" viewBox="0 0 16 16" aria-hidden="true" fill="none" stroke="currentColor" stroke-width="1.3" stroke-linejoin="round" stroke-linecap="round"><path d="M8 2.5 13 5.5V11L8 13.5 3 11V5.5Z"/><path d="M8 8.5 3 5.5M8 8.5 13 5.5M8 8.5V13.5"/></svg>"##;
const THEME_SVG: &str = r##"<svg class="ico" viewBox="0 0 16 16" aria-hidden="true"><circle cx="8" cy="8" r="6.4" fill="none" stroke="currentColor" stroke-width="1.5"/><path d="M8 1.6a6.4 6.4 0 0 0 0 12.8z" fill="currentColor"/></svg>"##;

#[derive(Deserialize, Default)]
pub struct Flash {
    ok: Option<String>,
    err: Option<String>,
}

fn layout(title: &str, flash: &Flash, body: Markup) -> Html<String> {
    let page = html! {
        (DOCTYPE)
        html lang="en" {
            head {
                meta charset="utf-8";
                meta name="viewport" content="width=device-width, initial-scale=1";
                title { (title) " · The Box Console" }
                style { (PreEscaped(CSS)) }
            }
            body {
                header.top {
                    a.brand href="/" {
                        span.mark { (PreEscaped(MARK_SVG)) " THE " b { "BOX" } }
                        span.sub { "Console" }
                    }
                    div.top-right {
                        div.stamp {
                            div { "FIELD OP: " b { "MANAGE" } }
                            div id="clock" { "--:--:--" }
                        }
                        button.theme-btn type="button" id="theme" { (PreEscaped(THEME_SVG)) " Theme" }
                    }
                }
                nav.tabs {
                    a.active[title == "Services"] href="/" { "Services" }
                    a.active[title == "Generations"] href="/generations" { "Generations" }
                    a.active[title == "System"] href="/system" { "System" }
                    a.active[title == "Fleet"] href="/fleet" { "Fleet" }
                    a.active[title == "Backup"] href="/backup" { "Backup" }
                    a.active[title == "Networking"] href="/network" { "Networking" }
                    a.btn.active[title == "Deploy"] href="/services/new" { "+ Deploy" }
                }
                main {
                    @if let Some(msg) = &flash.ok {
                        div.flash.ok { (msg) }
                    }
                    @if let Some(msg) = &flash.err {
                        div.flash.err { (msg) }
                    }
                    (body)
                }
                footer {
                    "THE BOX CONSOLE · boxd " (env!("CARGO_PKG_VERSION"))
                    " · LOCAL · API " code { "/api/v1" } " · MCP " code { "/mcp" }
                }
                script { (PreEscaped(JS)) }
            }
        }
    };
    Html(page.into_string())
}

fn ok_redirect(msg: &str) -> Redirect {
    Redirect::to(&format!("/?ok={}", urlencoding::encode(msg)))
}

fn err_redirect(err: &anyhow::Error) -> Redirect {
    Redirect::to(&format!(
        "/?err={}",
        urlencoding::encode(&format!("{err:#}"))
    ))
}

pub async fn index(
    State(state): State<SharedState>,
    Query(flash): Query<Flash>,
) -> Result<Html<String>, AppError> {
    let config = BoxConfig::load(&state.paths)?;
    let current = store::current(&state.paths)?;
    let active: HashSet<String> = current
        .as_ref()
        .and_then(|c| manifest::read_manifest(&c.store_path).ok())
        .map(|m| m.services.into_iter().map(|s| s.name).collect())
        .unwrap_or_default();
    // A service is only actually public if it has a domain AND the tunnel that
    // carries public traffic is up — otherwise it's reachable on the LAN only.
    let tunnel_running = state.tunnel.status().state == "running";

    let body = html! {
        section.cards {
            div.card {
                h3 { "Generation" }
                p.big {
                    @match &current {
                        Some(c) => { "#" (c.number) },
                        None => { "—" },
                    }
                }
                p.muted {
                    @match &current {
                        Some(c) => {
                            @if let Some(t) = c.created_at { "activated " (t.format("%Y-%m-%d %H:%M UTC")) }
                        },
                        None => { "nothing applied yet" },
                    }
                }
            }
            div.card {
                h3 { "Services" }
                p.big { (config.services.len()) }
                p.muted { "declared in box.toml" }
            }
            div.card {
                h3 { "Builder" }
                p.big { (state.builder.name()) }
                p.muted { "generation backend" }
            }
            div.card {
                h3 { "Platform" }
                p.big { (platform_release().unwrap_or_else(|| "dev".into())) }
                p.muted { a href="/system" { "updates & channel →" } }
            }
        }
        section {
            div.section-head {
                h2 { "Services" }
                a.btn href="/services/new" { "+ Deploy service" }
            }
            @if config.services.is_empty() {
                div.empty {
                    p { "No services yet." }
                    p { a href="/services/new" { "Deploy your first service" } " — a static site takes about ten seconds." }
                }
            } @else {
                table {
                    thead {
                        tr { th { "Name" } th { "Template" } th { "State" } th { "Exposure" } th { "Local URL" } th {} }
                    }
                    tbody {
                        @for s in &config.services {
                            tr {
                                td { strong { (s.name) } }
                                td { (s.template) }
                                td {
                                    @if active.contains(&s.name) { span.badge.on { "active" } }
                                    @else { span.badge { "pending" } }
                                }
                                td {
                                    @match &s.domain {
                                        Some(d) => {
                                            @if tunnel_running {
                                                span.badge.on { "public" }
                                                " "
                                                a href={ "https://" (d) } target="_blank" { (d) }
                                            } @else {
                                                span.badge { "tunnel off" }
                                                " "
                                                span.muted { (d) }
                                            }
                                        },
                                        None => { span.muted { "private (LAN only)" } },
                                    }
                                }
                                td { a href={ "/sites/" (s.name) "/" } target="_blank" { "/sites/" (s.name) "/" } }
                                td {
                                    form method="post" action={ "/services/" (s.name) "/delete" } {
                                        button.danger type="submit" { "Delete" }
                                    }
                                }
                            }
                        }
                    }
                }
            }
        }
    };
    Ok(layout("Services", &flash, body))
}

/// The template/preset chooser: everything deployable from a browser — the
/// catalog's presets plus the raw primitives, same set an agent sees over MCP.
pub async fn new_service(
    State(state): State<SharedState>,
    Query(flash): Query<Flash>,
) -> Html<String> {
    let catalog = crate::catalog::for_data_dir(&state.paths.data_dir);
    let body = html! {
        h2 { "Deploy a service" }
        p.muted { "Creates or updates a service, builds a new generation and activates it atomically. Roll back any time from the Generations page." }
        @if !catalog.is_empty() {
            div.section-head { h3 { "From the catalog" } }
            section.cards {
                @for entry in catalog.values() {
                    a.card href={ "/services/new/" (entry.id) } {
                        h3 {
                            @if !entry.icon.is_empty() { (entry.icon) " " }
                            (entry.title)
                        }
                        p.muted { (entry.description) }
                        @if !entry.category.is_empty() { span.badge { (entry.category) } }
                    }
                }
            }
        }
        div.section-head { h3 { "From a primitive" } }
        section.cards {
            @for t in crate::templates::all() {
                a.card href={ "/services/new/" (t.id()) } {
                    h3 { (t.title()) }
                    p.muted { (t.description()) }
                }
            }
        }
    };
    layout("Deploy", &flash, body)
}

/// Shared form fields every deploy shape offers: exposure identity + overrides.
fn common_fields(domain_hint: &str) -> Markup {
    html! {
        label {
            "Domain " span.muted { "(optional" @if !domain_hint.is_empty() { "; " (domain_hint) } ")" }
            input type="text" name="domain" placeholder="app.example.com";
        }
        label {
            input type="checkbox" name="public";
            " Public — serve on your domain through the tunnel (otherwise LAN-only)"
        }
    }
}

fn env_fields(secret_placeholder: &str) -> Markup {
    html! {
        label {
            "Environment " span.muted { "(one KEY=value per line)" }
            textarea name="env" rows="3" spellcheck="false" placeholder="TZ=UTC" {}
        }
        label {
            "Secret environment " span.muted { "(one KEY=value per line — encrypted on the Box, never stored in config or git)" }
            textarea name="secret_env" rows="3" spellcheck="false" placeholder=(secret_placeholder) {}
        }
    }
}

/// The per-template deploy form. `template` is a primitive id or a catalog
/// preset id — the same names `ops::deploy` accepts, so what the form submits
/// goes through exactly the central validation an agent's deploy does.
pub async fn new_service_form(
    State(state): State<SharedState>,
    Path(template): Path<String>,
    Query(flash): Query<Flash>,
) -> Result<Html<String>, AppError> {
    let catalog = crate::catalog::for_data_dir(&state.paths.data_dir);

    // A catalog preset: identity + env/secret-env overrides; the preset's
    // params supply the rest (image, exposure, volumes…).
    if let Some(entry) = catalog.get(&template) {
        // Prefill secret keys the preset declares, so the operator can see
        // exactly what credentials it expects (values are theirs to fill).
        let secret_keys: Vec<String> = entry
            .params
            .get("secret_env")
            .and_then(|v| v.as_object())
            .map(|o| o.keys().map(|k| format!("{k}=")).collect())
            .unwrap_or_default();
        let defaults = serde_json::to_string_pretty(&entry.params).unwrap_or_default();
        let body = html! {
            h2 { "Deploy " (entry.title) }
            p.muted { (entry.description) }
            form.stack method="post" action="/services" {
                input type="hidden" name="template" value=(entry.id);
                label {
                    "Name"
                    input type="text" name="name" required value=(entry.id) pattern="[a-z0-9-]+" autofocus;
                }
                (common_fields("for web-facing presets"))
                label {
                    "Port " span.muted { "(optional — the platform allocates and validates one)" }
                    input type="number" name="port" min="1" max="65535" placeholder="auto";
                }
                (env_fields(&secret_keys.join("\n")))
                details {
                    summary.muted { "Preset defaults (merged under your values)" }
                    pre { (defaults) }
                }
                button.btn type="submit" { "Deploy" }
            }
        };
        return Ok(layout("Deploy", &flash, body));
    }

    let body = match template.as_str() {
        "static-site" => html! {
            h2 { "Deploy a static site" }
            form.stack method="post" action="/services" {
                input type="hidden" name="template" value="static-site";
                label {
                    "Name"
                    input type="text" name="name" required placeholder="my-site" pattern="[a-z0-9-]+" autofocus;
                }
                (common_fields(""))
                label {
                    "index.html"
                    textarea name="content" rows="14" spellcheck="false" { (ops::DEFAULT_INDEX) }
                }
                label {
                    "…or copy a local directory " span.muted { "(absolute path on the Box; overrides the content above)" }
                    input type="text" name="source_path" placeholder="/home/me/mysite/dist";
                }
                button.btn type="submit" { "Deploy" }
            }
        },
        "container" => html! {
            h2 { "Deploy a container" }
            p.muted { "Any OCI/Docker image. The platform runs it, wires its port, and routes traffic per the exposure you pick." }
            form.stack method="post" action="/services" {
                input type="hidden" name="template" value="container";
                label {
                    "Name"
                    input type="text" name="name" required placeholder="my-app" pattern="[a-z0-9-]+" autofocus;
                }
                label {
                    "Image"
                    input type="text" name="image" required placeholder="nginx:1.27";
                }
                label {
                    "Container port " span.muted { "(the port the app listens on inside the container)" }
                    input type="number" name="container_port" min="1" max="65535" placeholder="80";
                }
                label {
                    "Exposure"
                    select name="expose" {
                        option value="proxied" selected { "Proxied — behind the platform web proxy (web apps)" }
                        option value="internal" { "Internal — loopback only (databases, caches)" }
                        option value="exposed" { "Exposed — LAN-reachable on its own port" }
                    }
                }
                label {
                    "Port " span.muted { "(optional — the platform allocates and validates one)" }
                    input type="number" name="port" min="1" max="65535" placeholder="auto";
                }
                (common_fields("proxied containers only"))
                label {
                    "Command " span.muted { "(optional override, whitespace-separated)" }
                    input type="text" name="cmd" placeholder="redis-server --appendonly yes";
                }
                (env_fields("DB_PASSWORD=…"))
                label {
                    "Volumes " span.muted { "(one host:container per line; host paths persist across updates)" }
                    textarea name="volumes" rows="2" spellcheck="false" placeholder="/var/lib/box/my-app:/data" {}
                }
                button.btn type="submit" { "Deploy" }
            }
        },
        "reverse-proxied-app" => html! {
            h2 { "Deploy a reverse-proxied app" }
            p.muted { "A process the platform supervises and serves behind its web proxy on an allocated port ($PORT)." }
            form.stack method="post" action="/services" {
                input type="hidden" name="template" value="reverse-proxied-app";
                label {
                    "Name"
                    input type="text" name="name" required placeholder="my-app" pattern="[a-z0-9-]+" autofocus;
                }
                label {
                    "Command " span.muted { "(started with $PORT set to the allocated port)" }
                    input type="text" name="command" required placeholder="/usr/bin/my-app --listen 127.0.0.1:$PORT";
                }
                label {
                    "Port " span.muted { "(optional — the platform allocates and validates one)" }
                    input type="number" name="port" min="1" max="65535" placeholder="auto";
                }
                (common_fields(""))
                button.btn type="submit" { "Deploy" }
            }
        },
        _ => html! {
            h2 { "Unknown template" }
            p { "No template or catalog preset named " code { (template) } "." }
            p { a href="/services/new" { "← Back to the chooser" } }
        },
    };
    Ok(layout("Deploy", &flash, body))
}

#[derive(Deserialize)]
pub struct NewServiceForm {
    name: String,
    #[serde(default)]
    template: Option<String>,
    #[serde(default)]
    domain: Option<String>,
    /// HTML checkbox: present ("on") when checked, absent otherwise.
    #[serde(default)]
    public: Option<String>,
    #[serde(default)]
    port: Option<String>,
    // static-site
    #[serde(default)]
    content: Option<String>,
    #[serde(default)]
    source_path: Option<String>,
    // container
    #[serde(default)]
    image: Option<String>,
    #[serde(default)]
    container_port: Option<String>,
    #[serde(default)]
    expose: Option<String>,
    #[serde(default)]
    cmd: Option<String>,
    #[serde(default)]
    volumes: Option<String>,
    // reverse-proxied-app
    #[serde(default)]
    command: Option<String>,
    // shared
    #[serde(default)]
    env: Option<String>,
    #[serde(default)]
    secret_env: Option<String>,
}

fn none_if_empty(value: Option<String>) -> Option<String> {
    value
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
}

/// `KEY=value` lines → a JSON object (blank lines and `#` comments ignored).
fn parse_env_lines(text: &str) -> serde_json::Map<String, serde_json::Value> {
    text.lines()
        .map(str::trim)
        .filter(|l| !l.is_empty() && !l.starts_with('#'))
        .filter_map(|l| {
            l.split_once('=')
                .map(|(k, v)| (k.trim().to_string(), serde_json::Value::from(v.trim())))
        })
        .filter(|(k, _)| !k.is_empty())
        .collect()
}

/// Turn the submitted form into the same `DeployRequest` an agent's MCP deploy
/// produces — all validation (template params, ports, domains, secret
/// encryption) stays central in `ops::deploy`.
fn deploy_request_from_form(form: NewServiceForm) -> Result<ops::DeployRequest, String> {
    let template = none_if_empty(form.template).unwrap_or_else(|| "static-site".into());

    let port = match none_if_empty(form.port) {
        None => None,
        Some(p) => Some(
            p.parse::<u16>()
                .map_err(|_| format!("invalid port {p:?}"))?,
        ),
    };

    let mut params = serde_json::Map::new();
    if let Some(content) = none_if_empty(form.content) {
        params.insert("index_html".into(), content.into());
    }
    if let Some(path) = none_if_empty(form.source_path) {
        params.insert("source_path".into(), path.into());
    }
    if let Some(image) = none_if_empty(form.image) {
        params.insert("image".into(), image.into());
    }
    if let Some(cp) = none_if_empty(form.container_port) {
        let cp: u64 = cp
            .parse()
            .map_err(|_| format!("invalid container port {cp:?}"))?;
        params.insert("container_port".into(), cp.into());
    }
    if let Some(expose) = none_if_empty(form.expose) {
        params.insert("expose".into(), expose.into());
    }
    if let Some(cmd) = none_if_empty(form.cmd) {
        let argv: Vec<serde_json::Value> =
            cmd.split_whitespace().map(serde_json::Value::from).collect();
        params.insert("cmd".into(), argv.into());
    }
    if let Some(volumes) = none_if_empty(form.volumes) {
        let vols: Vec<serde_json::Value> = volumes
            .lines()
            .map(str::trim)
            .filter(|l| !l.is_empty())
            .map(serde_json::Value::from)
            .collect();
        params.insert("volumes".into(), vols.into());
    }
    if let Some(command) = none_if_empty(form.command) {
        params.insert("command".into(), command.into());
    }
    if let Some(env) = none_if_empty(form.env) {
        let map = parse_env_lines(&env);
        if !map.is_empty() {
            params.insert("env".into(), map.into());
        }
    }
    if let Some(secret) = none_if_empty(form.secret_env) {
        // Entries left as bare "KEY=" prefills are not credentials; drop them.
        let map: serde_json::Map<_, _> = parse_env_lines(&secret)
            .into_iter()
            .filter(|(_, v)| v.as_str().is_some_and(|s| !s.is_empty()))
            .collect();
        if !map.is_empty() {
            params.insert("secret_env".into(), map.into());
        }
    }

    Ok(ops::DeployRequest {
        name: form.name.trim().to_string(),
        template,
        params: serde_json::Value::Object(params),
        domain: none_if_empty(form.domain),
        public: form.public.is_some(),
        port,
    })
}

pub async fn create_service(
    State(state): State<SharedState>,
    Form(form): Form<NewServiceForm>,
) -> Redirect {
    let request = match deploy_request_from_form(form) {
        Ok(r) => r,
        Err(msg) => return err_redirect(&anyhow::anyhow!(msg)),
    };
    let name = request.name.clone();
    let result = {
        let state = state.clone();
        blocking(move || {
            let _guard = state.apply_lock.lock().unwrap();
            ops::deploy(&state.paths, state.builder.as_ref(), request)
        })
        .await
    };
    match result {
        Ok(info) => ok_redirect(&format!(
            "Deployed {name} — now at generation #{}",
            info.number
        )),
        Err(err) => err_redirect(&err),
    }
}

pub async fn delete_service(
    State(state): State<SharedState>,
    Path(name): Path<String>,
) -> Redirect {
    let result = {
        let state = state.clone();
        let name = name.clone();
        blocking(move || {
            let _guard = state.apply_lock.lock().unwrap();
            ops::delete_service(&state.paths, state.builder.as_ref(), &name)
        })
        .await
    };
    match result {
        Ok(info) => ok_redirect(&format!(
            "Deleted {name} — now at generation #{}",
            info.number
        )),
        Err(err) => err_redirect(&err),
    }
}

pub async fn generations(
    State(state): State<SharedState>,
    Query(flash): Query<Flash>,
) -> Result<Html<String>, AppError> {
    let mut gens = store::list(&state.paths)?;
    gens.reverse();
    let history = crate::history::log(&state.paths, 50).unwrap_or_default();

    let body = html! {
        h2 { "Generations" }
        p.muted { "Every apply produces an immutable generation. Rolling back switches the profile atomically and restores that generation's configuration." }
        @if gens.is_empty() {
            div.empty { p { "No generations yet — deploy a service to create the first one." } }
        } @else {
            table {
                thead {
                    tr { th { "#" } th { "Activated" } th { "Services" } th { "State" } th {} }
                }
                tbody {
                    @for g in &gens {
                        tr {
                            td { strong { "#" (g.number) } }
                            td {
                                @match g.created_at {
                                    Some(t) => { (t.format("%Y-%m-%d %H:%M UTC")) },
                                    None => { "—" },
                                }
                            }
                            td {
                                @match manifest::read_manifest(&g.store_path) {
                                    Ok(m) => {
                                        @if m.services.is_empty() { span.muted { "(none)" } }
                                        @else { (m.services.iter().map(|s| s.name.clone()).collect::<Vec<_>>().join(", ")) }
                                    },
                                    Err(_) => { span.muted { "(unreadable)" } },
                                }
                            }
                            td {
                                @if g.current { span.badge.on { "current" } } @else { span.badge { "inactive" } }
                            }
                            td {
                                @if !g.current {
                                    form method="post" action={ "/generations/" (g.number) "/rollback" } {
                                        button type="submit" { "Roll back" }
                                    }
                                }
                            }
                        }
                    }
                }
            }
        }
        section {
            h2 { "Change log" }
            p.muted { "Every apply, rollback and delete is committed to this Box's config history — newest first." }
            @if history.is_empty() {
                div.empty { p { "No history yet." } }
            } @else {
                table {
                    thead { tr { th { "When" } th { "Change" } } }
                    tbody {
                        @for e in &history {
                            tr {
                                td {
                                    @match chrono::DateTime::from_timestamp(e.timestamp, 0) {
                                        Some(t) => { (t.format("%Y-%m-%d %H:%M UTC")) },
                                        None => { "—" },
                                    }
                                }
                                td { (e.message) }
                            }
                        }
                    }
                }
            }
        }
    };
    Ok(layout("Generations", &flash, body))
}

pub async fn network(State(state): State<SharedState>, Query(flash): Query<Flash>) -> Html<String> {
    let status = state.tunnel.status();
    let body = html! {
        h2 { "Networking" }
        p.muted {
            "The Box is local-first: the dashboard stays private. To make services public without opening ports, "
            "bring your own tunnel — traffic flows Internet → Cloudflare → tunnel → this Box, which routes "
            "requests to the service whose domain matches the request's Host header."
        }
        section.cards {
            div.card {
                h3 { "Cloudflare Tunnel" }
                p.big {
                    @if status.state == "running" { span.badge.on { "running" } }
                    @else if status.enabled { span.badge { (status.state) } }
                    @else { span.badge { "disabled" } }
                }
                p.muted {
                    @if let Some(pid) = status.pid { "cloudflared pid " (pid) }
                    @else { "connector process" }
                }
            }
            div.card {
                h3 { "cloudflared" }
                p.big { @if status.installed { "installed" } @else { "not found" } }
                p.muted { "must be on the Box's PATH" }
            }
            div.card {
                h3 { "Tunnel token" }
                p.big { @if status.token_saved { "saved" } @else { "not set" } }
                p.muted { "stored with 0600 permissions, never in config" }
            }
        }
        section {
            h2 { "Set up" }
            ol {
                li { "In the Cloudflare Zero Trust dashboard, create a tunnel and copy its token." }
                li { "Point the tunnel's public hostname(s) at " code { "http://localhost:2693" } " (this daemon)." }
                li { "Paste the token below and enable the tunnel." }
                li { "Give each service the matching domain — requests arriving for that Host are routed to it." }
            }
            form.stack method="post" action="/network/cloudflare" {
                label {
                    "Cloudflare tunnel token"
                    input type="password" name="token" placeholder=(if status.token_saved { "•••••• (keep current token)" } else { "eyJh..." });
                }
                button.btn type="submit" { @if status.enabled { "Update & restart" } @else { "Save & enable" } }
            }
            @if status.enabled {
                form method="post" action="/network/cloudflare/disable" style="margin-top:.75rem" {
                    button.danger type="submit" { "Disable tunnel" }
                }
            }
        }

        @let mesh = crate::connect::status().ok();
        @let mesh_state = mesh.as_ref()
            .and_then(|s| s.get("BackendState").and_then(|v| v.as_str()))
            .unwrap_or("not installed")
            .to_string();
        @let mesh_name = mesh.as_ref()
            .and_then(|s| s.pointer("/Self/DNSName").and_then(|v| v.as_str()))
            .map(|n| n.trim_end_matches('.').to_string());
        section {
            div.section-head {
                h2 { "Box Connect" }
                @if mesh_state == "Running" {
                    form method="post" action="/network/connect/down" {
                        button.danger type="submit" { "Leave the mesh" }
                    }
                }
            }
            p.muted {
                "Private remote access: a WireGuard mesh that reaches this Box from "
                "anywhere without opening a port or making anything public. The tunnel "
                "above publishes services to the Internet; this does the opposite, and "
                "carries any service, not just web."
            }
            section.cards {
                div.card {
                    h3 { "Mesh" }
                    p.big {
                        @if mesh_state == "Running" { span.badge.on { "connected" } }
                        @else { span.badge { (mesh_state) } }
                    }
                    p.muted { "WireGuard, via your coordinator" }
                }
                div.card {
                    h3 { "This Box" }
                    p.big { (mesh_name.clone().unwrap_or_else(|| "—".into())) }
                    p.muted { "name on the mesh" }
                }
            }
            @if mesh_state != "Running" {
                p.muted {
                    "Bring your own coordinator (self-hosted Headscale, or Tailscale). "
                    "Generate a pre-auth key there, then join:"
                }
                form.stack method="post" action="/network/connect" {
                    label {
                        "Coordinator URL"
                        input type="text" name="server" required placeholder="https://headscale.example.com";
                    }
                    label {
                        "Pre-auth key"
                        input type="password" name="authkey" required placeholder="key...";
                    }
                    label {
                        "Name on the mesh " span.muted { "(optional)" }
                        input type="text" name="hostname" placeholder=(crate::connect::default_hostname());
                    }
                    button.btn type="submit" { "Join the mesh" }
                }
            }
        }
    };
    layout("Networking", &flash, body)
}

#[derive(Deserialize)]
pub struct ConnectForm {
    server: String,
    authkey: String,
    #[serde(default)]
    hostname: Option<String>,
}

pub async fn connect_enroll(
    State(_state): State<SharedState>,
    Form(form): Form<ConnectForm>,
) -> Redirect {
    let redirect =
        |key: &str, msg: &str| Redirect::to(&format!("/network?{key}={}", urlencoding::encode(msg)));
    let host = none_if_empty(form.hostname).unwrap_or_else(crate::connect::default_hostname);
    let server = form.server.trim().to_string();
    let authkey = form.authkey.trim().to_string();
    if server.is_empty() || authkey.is_empty() {
        return redirect("err", "A coordinator URL and a pre-auth key are both required");
    }
    let result = blocking(move || crate::connect::enroll(&server, &authkey, &host)).await;
    match result {
        Ok(()) => redirect("ok", "Joined the mesh"),
        Err(e) => redirect("err", &format!("{e:#}")),
    }
}

pub async fn connect_down(State(_state): State<SharedState>) -> Redirect {
    let redirect =
        |key: &str, msg: &str| Redirect::to(&format!("/network?{key}={}", urlencoding::encode(msg)));
    match blocking(crate::connect::down).await {
        Ok(()) => redirect("ok", "Left the mesh"),
        Err(e) => redirect("err", &format!("{e:#}")),
    }
}

#[derive(Deserialize)]
pub struct CloudflareForm {
    #[serde(default)]
    token: Option<String>,
}

fn network_redirect(result: anyhow::Result<()>, ok_msg: &str) -> Redirect {
    match result {
        Ok(()) => Redirect::to(&format!("/network?ok={}", urlencoding::encode(ok_msg))),
        Err(err) => Redirect::to(&format!(
            "/network?err={}",
            urlencoding::encode(&format!("{err:#}"))
        )),
    }
}

pub async fn configure_cloudflare(
    State(state): State<SharedState>,
    Form(form): Form<CloudflareForm>,
) -> Redirect {
    let token = none_if_empty(form.token);
    let result = state.tunnel.configure(token.as_deref(), true).map(|_| ());
    network_redirect(result, "Cloudflare tunnel enabled")
}

pub async fn disable_cloudflare(State(state): State<SharedState>) -> Redirect {
    let result = state.tunnel.configure(None, false).map(|_| ());
    network_redirect(result, "Cloudflare tunnel disabled")
}

// ---- Backup ---------------------------------------------------------------

fn backup_redirect(result: anyhow::Result<()>, ok_msg: &str) -> Redirect {
    match result {
        Ok(()) => Redirect::to(&format!("/backup?ok={}", urlencoding::encode(ok_msg))),
        Err(err) => Redirect::to(&format!(
            "/backup?err={}",
            urlencoding::encode(&format!("{err:#}"))
        )),
    }
}

fn backup_form(config: &BoxConfig, submit: &str) -> Markup {
    let b = config.backup.as_ref().map(|c| &c.backend);
    let val = |f: fn(&crate::config::BackendConfig) -> Option<String>| -> String {
        b.and_then(f).unwrap_or_default()
    };
    let kind = b.map(|bk| bk.kind.clone()).unwrap_or_default();
    let sched = config
        .backup
        .as_ref()
        .map(|c| c.schedule.clone())
        .unwrap_or_else(|| "daily".into());
    html! {
        form.stack method="post" action="/backup/configure" {
            div.row2 {
                label { "Backend"
                    select name="kind" {
                        @for k in ["local", "s3", "sftp", "rest"] {
                            option value=(k) selected[kind == k] { (k) }
                        }
                    }
                }
                label { "Schedule"
                    select name="schedule" {
                        @for s in ["hourly", "daily", "weekly"] {
                            option value=(s) selected[sched == s] { (s) }
                        }
                    }
                }
            }
            p.field-note { "Fill the fields for your chosen backend; leave the rest blank." }
            label { "Path " span.muted { "(local dir, or SFTP remote path)" }
                input type="text" name="path" value=(val(|b| b.path.clone())) placeholder="/mnt/usb/box-backups";
            }
            div.row2 {
                label { "S3 endpoint" input type="text" name="endpoint" value=(val(|b| b.endpoint.clone())) placeholder="s3.us-west-002.backblazeb2.com"; }
                label { "S3 bucket" input type="text" name="bucket" value=(val(|b| b.bucket.clone())) placeholder="my-box-backups"; }
            }
            div.row2 {
                label { "S3 prefix" input type="text" name="prefix" value=(val(|b| b.prefix.clone())) placeholder="box-1"; }
                label { "S3 access key" input type="password" name="access_key" placeholder="•••••• (keep current)"; }
            }
            label { "S3 secret key" input type="password" name="secret_key" placeholder="•••••• (keep current)"; }
            div.row2 {
                label { "SFTP host" input type="text" name="host" value=(val(|b| b.host.clone())); }
                label { "SFTP user" input type="text" name="user" value=(val(|b| b.user.clone())); }
            }
            div.row2 {
                label { "SFTP port" input type="text" name="port" value=(b.and_then(|bk| bk.port).map(|p| p.to_string()).unwrap_or_default()) placeholder="22"; }
                label { "REST url" input type="text" name="url" value=(val(|b| b.url.clone())); }
            }
            button.btn type="submit" { (submit) }
        }
    }
}

pub async fn backup(State(state): State<SharedState>, Query(flash): Query<Flash>) -> Html<String> {
    let config = BoxConfig::load(&state.paths).unwrap_or_default();
    let has_key = crate::backup::has_key(&state.paths);
    let bc = config.backup.clone();
    let ready = has_key && bc.as_ref().is_some_and(|c| !c.backend.kind.is_empty());

    let (status, snaps) = if ready {
        let bc = bc.as_ref().unwrap();
        (
            Some(crate::backup::status(&state.paths, bc)),
            crate::backup::snapshots(&state.paths, bc).unwrap_or_default(),
        )
    } else {
        (None, Vec::new())
    };

    let body = html! {
        h2 { "Backup" }
        p.muted {
            "Client-side encrypted backups to a destination you own — your S3/Backblaze bucket, "
            "an SFTP server, another Box, or a USB disk. The key is generated here and never leaves "
            "this Box, so we (and your storage provider) can't read a backup. What gets backed up is "
            "derived from your services automatically; the OS rebuilds from config."
        }

        @if ready {
            @let st = status.as_ref().unwrap();
            section.cards {
                div.card { h3 { "Last backup" }
                    p.big { (st.last.as_ref().map(|s| s.time.split('T').next().unwrap_or(&s.time).to_string()).unwrap_or_else(|| "never".into())) } }
                div.card { h3 { "Snapshots" } p.big { (st.count) } }
                div.card { h3 { "Destination" } p.big { (bc.as_ref().unwrap().backend.kind.clone()) } }
                div.card { h3 { "Repository" }
                    p.big { @if st.reachable { span.badge.on { "reachable" } } @else { span.badge { "unreachable" } } } }
            }
            form method="post" action="/backup/run" style="margin:.5rem 0 1.5rem" {
                button.btn type="submit" { "Back up now" }
            }

            section {
                h2 { "Snapshots" }
                @if snaps.is_empty() { p.muted { "No snapshots yet — run a backup." } }
                @else {
                    table.gen {
                        thead { tr { th { "id" } th { "time" } th { "paths" } } }
                        tbody { @for s in &snaps {
                            tr { td { code { (s.id.chars().take(8).collect::<String>()) } }
                                 td { (s.time.replace('T', " ").chars().take(19).collect::<String>()) }
                                 td.muted { (s.paths.join(" ")) } }
                        } }
                    }
                }
            }

            section {
                h2 { "Restore" }
                p.field-note { "Writes files back in place. Restoring the whole snapshot on a live Box overwrites its config — usually you restore a single service, or restore everything onto a fresh Box." }
                form.stack method="post" action="/backup/restore" {
                    div.row2 {
                        label { "Snapshot"
                            select name="snapshot" {
                                option value="latest" { "latest" }
                                @for s in &snaps { option value=(s.id) { (s.time.replace('T', " ").chars().take(19).collect::<String>()) } }
                            }
                        }
                        label { "What"
                            select name="scope" {
                                option value="config" { "Config only" }
                                option value="all" { "Everything" }
                                @for svc in &config.services { option value=(svc.name) { "Service: " (svc.name) } }
                            }
                        }
                    }
                    button.danger type="submit" { "Restore" }
                }
            }

            details style="margin-top:1.5rem" {
                summary { "Reveal recovery key" }
                p.field-note { "Save this off the Box. Without it your backups cannot be restored — we cannot recover it for you." }
                @match crate::secrets::get(&state.paths, crate::backup::PW_SECRET) {
                    Ok(Some(key)) => code.mono { (key) },
                    _ => p.muted { "no key" },
                }
            }
            details style="margin-top:1rem" {
                summary { "Change destination" }
                div style="margin-top:.75rem" { (backup_form(&config, "Save destination")) }
            }
        } @else {
            section {
                h2 { "Set up a destination" }
                p.muted { "Pick where encrypted backups go. A recovery key is generated on save — write it down." }
                (backup_form(&config, "Save & enable"))
            }
        }

        @let remote = crate::history::remote(&state.paths);
        section {
            div.section-head {
                h2 { "Config repo" }
                @if remote.is_some() {
                    form method="post" action="/backup/config-push" {
                        button.btn type="submit" { "Push now" }
                    }
                }
            }
            p.muted {
                "Your declarative config and encrypted secrets, pushed to a git repo you own "
                "(GitHub, Gitea, anywhere). With it, a lost or wiped Box is recreated from "
                a href="/recreate" { "Recreate" }
                " — config, services and secrets included. Plaintext never leaves the Box."
            }
            @match &remote {
                Some(url) => {
                    p { "Pushing to " code { (url) } " on every change." }
                    details {
                        summary.muted { "Change or remove the remote" }
                        form.stack method="post" action="/backup/config-remote" {
                            label {
                                "Remote URL " span.muted { "(empty to stop pushing)" }
                                input type="text" name="url" value=(url);
                            }
                            button.btn type="submit" { "Save remote" }
                        }
                    }
                },
                None => {
                    form.stack method="post" action="/backup/config-remote" {
                        label {
                            "Remote URL"
                            input type="text" name="url" placeholder="git@github.com:you/box-config.git";
                        }
                        button.btn type="submit" { "Set remote & push" }
                    }
                },
            }
        }
    };
    layout("Backup", &flash, body)
}

/// Whether it is safe to accept a private key over THIS connection. The
/// operator identity is the crown jewel of the whole secrets model, and the
/// console is plain HTTP on the LAN — so the paste form is offered only on a
/// loopback connection (the on-box browser or an SSH port-forward) or through
/// the TLS-terminated tunnel. Everywhere else we point at the CLI instead of
/// inviting someone to put their key on the wire in the clear.
fn key_entry_is_safe(peer: std::net::SocketAddr, headers: &HeaderMap) -> bool {
    crate::auth::is_trusted_local(peer.ip().is_loopback(), headers)
        || crate::auth::is_proxied(headers)
}

// Named "recreate", not "restore": the Backup page already restores FILES from
// a snapshot. This rebuilds the whole Box from its config repo. Two different
// operations should not share one word in the UI.
pub async fn recreate_page(
    State(state): State<SharedState>,
    axum::extract::ConnectInfo(peer): axum::extract::ConnectInfo<std::net::SocketAddr>,
    headers: HeaderMap,
    Query(flash): Query<Flash>,
) -> Html<String> {
    let remote = crate::history::remote(&state.paths);
    let safe = key_entry_is_safe(peer, &headers);
    let services = BoxConfig::load(&state.paths)
        .map(|c| c.services.len())
        .unwrap_or(0);

    let body = html! {
        h2 { "Restore this Box" }
        p.muted {
            "Rebuild this Box from a config repo: its services, its declarative "
            "config, and its encrypted secrets, which are re-keyed to this Box as "
            "part of the restore. This is the second half of destroy-and-recreate "
            "— the machine that made the backup does not have to be the machine "
            "that comes back."
        }

        @if services > 0 {
            div.flash.err {
                "This Box already runs " (services) " service"
                @if services != 1 { "s" }
                ". Restoring REPLACES its config with the repo's."
            }
        }

        @if !safe {
            div.empty {
                p { strong { "Restore is not offered over this connection." } }
                p.muted {
                    "It needs your operator private key, and this page reached you "
                    "over plain HTTP on the local network, where the key would "
                    "travel in the clear. Use a connection that protects it:"
                }
                ul {
                    li { "SSH to the Box and run " code { "boxd restore <repo-url> --identity <key>" } }
                    li { "or forward the console over SSH: " code { "ssh -L 2693:localhost:2693 <box>" } ", then open " code { "http://localhost:2693/recreate" } }
                    li { "or reach the console through your tunnel, which is encrypted end to end" }
                }
            }
        } @else {
            form.stack method="post" action="/recreate" {
                label {
                    "Config repo URL"
                    @match &remote {
                        Some(u) => input type="text" name="repo_url" required value=(u);,
                        None => input type="text" name="repo_url" required placeholder="git@github.com:you/box-config.git";,
                    }
                }
                label {
                    "Operator private key " span.muted { "(the SSH or age key that can decrypt this repo's secrets)" }
                    textarea name="identity" rows="6" required spellcheck="false"
                        placeholder="-----BEGIN OPENSSH PRIVATE KEY-----" {}
                }
                p.muted {
                    "The key is written to memory-backed storage for the length of the "
                    "restore, used to re-encrypt each secret to this Box's own host key, "
                    "then overwritten and deleted. It is never written to the config, "
                    "the repo, or the Nix store."
                }
                label {
                    input type="checkbox" name="confirm" required;
                    " I understand this replaces this Box's configuration with the repo's."
                }
                button.btn type="submit" { "Restore this Box" }
            }
        }
    };
    layout("Recreate", &flash, body)
}

#[derive(Deserialize)]
pub struct RecreateForm {
    repo_url: String,
    identity: String,
    #[serde(default)]
    confirm: Option<String>,
}

pub async fn recreate_run(
    State(state): State<SharedState>,
    axum::extract::ConnectInfo(peer): axum::extract::ConnectInfo<std::net::SocketAddr>,
    headers: HeaderMap,
    Form(form): Form<RecreateForm>,
) -> Redirect {
    let redirect =
        |key: &str, msg: &str| Redirect::to(&format!("/recreate?{key}={}", urlencoding::encode(msg)));

    // Enforced server-side too: the form is merely hidden on an unsafe
    // connection, and a hidden control is not a security boundary.
    if !key_entry_is_safe(peer, &headers) {
        return redirect(
            "err",
            "Restore needs your operator key and this connection is not encrypted. Use the CLI over SSH, or forward the console over SSH.",
        );
    }
    if form.confirm.is_none() {
        return redirect("err", "Confirm the replacement before restoring");
    }
    let repo_url = form.repo_url.trim().to_string();
    let identity = form.identity.trim().to_string();
    if repo_url.is_empty() || identity.is_empty() {
        return redirect("err", "A repo URL and an operator key are both required");
    }

    let result = {
        let state = state.clone();
        let repo = repo_url.clone();
        blocking(move || {
            let key = crate::util::TransientSecret::new("restore-identity", &identity)?;
            let _guard = state.apply_lock.lock().unwrap();
            ops::restore(&state.paths, state.builder.as_ref(), &repo, key.path())
            // `key` drops here: overwritten and unlinked, however this returned.
        })
        .await
    };
    match result {
        Ok(info) => Redirect::to(&format!(
            "/?ok={}",
            urlencoding::encode(&format!(
                "Restored from {repo_url} — now at generation #{}",
                info.number
            ))
        )),
        Err(err) => redirect("err", &format!("{err:#}")),
    }
}

#[derive(Deserialize)]
pub struct ConfigRemoteForm {
    #[serde(default)]
    url: Option<String>,
}

pub async fn set_config_remote(
    State(state): State<SharedState>,
    Form(form): Form<ConfigRemoteForm>,
) -> Redirect {
    let redirect =
        |key: &str, msg: &str| Redirect::to(&format!("/backup?{key}={}", urlencoding::encode(msg)));
    let url = none_if_empty(form.url);
    let result = {
        let state = state.clone();
        let url = url.clone();
        blocking(move || -> anyhow::Result<()> {
            crate::history::set_remote(&state.paths, url.as_deref())?;
            if url.is_some() {
                // Prove the remote works right away rather than on the next deploy.
                crate::history::commit(&state.paths, "config push")?;
                crate::history::push(&state.paths)?;
            }
            Ok(())
        })
        .await
    };
    match (result, url) {
        (Ok(()), Some(u)) => redirect("ok", &format!("Config repo remote set — pushed to {u}")),
        (Ok(()), None) => redirect("ok", "Config repo remote removed"),
        (Err(e), _) => redirect("err", &format!("{e:#}")),
    }
}

pub async fn push_config_now(State(state): State<SharedState>) -> Redirect {
    let redirect =
        |key: &str, msg: &str| Redirect::to(&format!("/backup?{key}={}", urlencoding::encode(msg)));
    let result = {
        let state = state.clone();
        blocking(move || -> anyhow::Result<()> {
            crate::history::commit(&state.paths, "config push")?;
            crate::history::push(&state.paths)
        })
        .await
    };
    match result {
        Ok(()) => redirect("ok", "Config + encrypted secrets pushed"),
        Err(e) => redirect("err", &format!("{e:#}")),
    }
}

#[derive(Deserialize)]
pub struct BackupForm {
    kind: String,
    #[serde(default)]
    schedule: Option<String>,
    #[serde(default)]
    path: Option<String>,
    #[serde(default)]
    endpoint: Option<String>,
    #[serde(default)]
    bucket: Option<String>,
    #[serde(default)]
    prefix: Option<String>,
    #[serde(default)]
    host: Option<String>,
    #[serde(default)]
    port: Option<String>,
    #[serde(default)]
    user: Option<String>,
    #[serde(default)]
    url: Option<String>,
    #[serde(default)]
    access_key: Option<String>,
    #[serde(default)]
    secret_key: Option<String>,
}

pub async fn configure_backup(
    State(state): State<SharedState>,
    Form(f): Form<BackupForm>,
) -> Redirect {
    let result = (|| -> anyhow::Result<()> {
        let mut config = BoxConfig::load(&state.paths)?;
        let backend = crate::config::BackendConfig {
            kind: f.kind.clone(),
            path: none_if_empty(f.path),
            endpoint: none_if_empty(f.endpoint),
            bucket: none_if_empty(f.bucket),
            prefix: none_if_empty(f.prefix),
            host: none_if_empty(f.host),
            user: none_if_empty(f.user),
            port: none_if_empty(f.port).and_then(|p| p.parse().ok()),
            url: none_if_empty(f.url),
        };
        let retention = config
            .backup
            .as_ref()
            .map(|c| c.retention.clone())
            .unwrap_or_default();
        config.backup = Some(crate::config::BackupConfig {
            enabled: true,
            schedule: f.schedule.filter(|s| !s.is_empty()).unwrap_or_else(|| "daily".into()),
            retention,
            backend,
            extra_paths: Vec::new(),
        });
        config.save(&state.paths)?;
        if let Some(k) = none_if_empty(f.access_key) {
            crate::secrets::set(&state.paths, "backup-s3-access-key", &k)?;
        }
        if let Some(s) = none_if_empty(f.secret_key) {
            crate::secrets::set(&state.paths, "backup-s3-secret-key", &s)?;
        }
        if !crate::backup::has_key(&state.paths) {
            crate::backup::init_key(&state.paths)?;
        }
        Ok(())
    })();
    backup_redirect(result, "Backup destination saved — reveal and save your recovery key below")
}

pub async fn run_backup_now(State(state): State<SharedState>) -> Redirect {
    let paths = state.paths.clone();
    tokio::spawn(async move {
        let _ = crate::web::blocking(move || {
            let config = BoxConfig::load(&paths)?;
            let bc = config
                .backup
                .clone()
                .ok_or_else(|| anyhow::anyhow!("no backup configured"))?;
            crate::backup::run(&paths, &config, &bc)
        })
        .await;
    });
    backup_redirect(Ok(()), "Backup started — snapshots update when it finishes")
}

#[derive(Deserialize)]
pub struct RestoreForm {
    snapshot: String,
    #[serde(default)]
    scope: String,
}

pub async fn restore_backup(
    State(state): State<SharedState>,
    Form(f): Form<RestoreForm>,
) -> Redirect {
    let paths = state.paths.clone();
    let snap = f.snapshot.clone();
    let scope = f.scope.clone();
    tokio::spawn(async move {
        let _ = crate::web::blocking(move || {
            let config = BoxConfig::load(&paths)?;
            let bc = config
                .backup
                .clone()
                .ok_or_else(|| anyhow::anyhow!("no backup configured"))?;
            let includes = match scope.as_str() {
                "all" => Vec::new(),
                "config" => crate::backup::config_includes(&paths),
                svc => crate::backup::service_includes(&config, svc),
            };
            crate::backup::restore(&paths, &bc, &snap, std::path::Path::new("/"), &includes)
        })
        .await;
    });
    backup_redirect(Ok(()), &format!("Restore of {} started", f.snapshot))
}

pub async fn system(
    State(state): State<SharedState>,
    Query(flash): Query<Flash>,
) -> Result<Html<String>, AppError> {
    let channel = ChannelConfig::load(&state.paths)?;
    let pinned = channel::locked_platform_id(&state.paths.os_config_dir())
        .ok()
        .flatten();
    let release = platform_release();
    let os_available = ostier::available();

    let body = html! {
        h2 { "System" }
        p.muted {
            "What this Box is running, and where its platform updates come from. A "
            "platform update is just another atomic generation — a failed one rolls the "
            "whole system back on its own."
        }
        section.cards {
            div.card {
                h3 { "Platform" }
                p.big { (release.clone().unwrap_or_else(|| "dev".into())) }
                p.muted { "boxd " (env!("CARGO_PKG_VERSION")) }
            }
            div.card {
                h3 { "Update channel" }
                p.big {
                    @match &channel {
                        Some(_) => span.badge.on { "configured" },
                        None => span.badge { "not set" },
                    }
                }
                p.muted {
                    @match &channel {
                        Some(c) => (short_ref(&c.platform_ref)),
                        None => "content-only",
                    }
                }
            }
            div.card {
                h3 { "OS updates" }
                p.big {
                    @match &channel {
                        Some(c) => @if c.auto_update { "automatic" } @else { "manual" },
                        None => "—",
                    }
                }
                p.muted {
                    @if os_available { "system switch available" }
                    @else { "not an OS-tier host" }
                }
            }
        }
        @match &channel {
            Some(c) => {
                section {
                    div.section-head {
                        h2 { "Platform channel" }
                        div.deploybar {
                            form method="post" action="/system/check" {
                                button.btn type="submit" { "Check for updates" }
                            }
                            form method="post" action="/system/update" {
                                button.btn.primary type="submit" { "Update now" }
                            }
                        }
                    }
                    table {
                        tbody {
                            tr { th { "Tracking" } td { code { (c.platform_ref) } } }
                            tr {
                                th { "Pinned to" }
                                td {
                                    @match &pinned {
                                        Some(p) => code { (short_rev(p)) },
                                        None => span.muted { "not built yet" },
                                    }
                                }
                            }
                            tr { th { "Auto-update" } td { @if c.auto_update { "on" } @else { "off" } } }
                            tr { th { "System" } td { (c.system) } }
                            tr { th { "Board" } td { (c.board.as_deref().unwrap_or("generic")) } }
                            tr { th { "Host id" } td { (c.host_id) } }
                        }
                    }
                    details {
                        summary.muted { "Change the channel binding" }
                        (channel_form(Some(c)))
                    }
                }
            },
            None => {
                section {
                    div.section-head { h2 { "Bind the update channel" } }
                    p.muted {
                        "This Box manages service content only until it's bound to a platform "
                        "channel. Binding enables \"Update now\": whole-system updates with a "
                        "health check and automatic rollback. The hardware board is detected "
                        "automatically."
                    }
                    (channel_form(None))
                }
            },
        }
        @let cloud_linked = crate::secrets::exists(&state.paths, "cloud-api-token");
        section {
            div.section-head { h2 { "Box Cloud" } }
            p.muted {
                "Optional, paid: link this Box to a cloud account for managed offsite "
                "backups and managed Box Connect. Everything the Box does works without "
                "it. Backups stay encrypted with a key that never leaves this Box, so a "
                "cloud account cannot read them."
            }
            @if cloud_linked {
                @let usage = crate::cloud::usage(&state.paths).ok();
                section.cards {
                    div.card {
                        h3 { "Account" }
                        p.big { span.badge.on { "linked" } }
                        p.muted { (crate::secrets::get(&state.paths, "cloud-server").ok().flatten().unwrap_or_default()) }
                    }
                    div.card {
                        h3 { "Managed backup" }
                        p.big {
                            @match usage.as_ref().and_then(|u| u.get("bytes")).and_then(|b| b.as_u64()) {
                                Some(b) => (format!("{:.2} MiB", b as f64 / 1_048_576.0)),
                                None => "—",
                            }
                        }
                        p.muted { "stored offsite" }
                    }
                }
                form method="post" action="/system/cloud/provision" {
                    button.btn type="submit" { "Refresh storage credentials" }
                }
            } @else {
                form.stack method="post" action="/system/cloud" {
                    label {
                        "Cloud server"
                        input type="text" name="server" required placeholder="https://cloud.thebox.build";
                    }
                    label {
                        "One-time enrollment token " span.muted { "(from your cloud account)" }
                        input type="password" name="token" required;
                    }
                    button.btn type="submit" { "Link this Box" }
                }
            }
        }
        section {
            p.muted {
                "Every change is atomic and reversible. Roll back manually any time from the "
                a href="/generations" { "Generations" } " page."
            }
            p.muted {
                "Control who can manage this Box on the "
                a href="/devices" { "Paired devices" } " page."
            }
        }
    };
    Ok(layout("System", &flash, body))
}

/// The channel-binding form, blank for first-time setup or prefilled to edit.
/// Board and system are detected server-side, so a person can't bind a Pi to a
/// generic build by mistake (the same guard the CLI has).
fn channel_form(current: Option<&ChannelConfig>) -> Markup {
    let hostname = std::fs::read_to_string("/proc/sys/kernel/hostname")
        .map(|h| h.trim().to_string())
        .unwrap_or_default();
    let host_id = current.map(|c| c.host_id.clone()).unwrap_or(hostname);
    let platform = current
        .map(|c| c.platform_ref.clone())
        .unwrap_or_else(|| channel::DEFAULT_PLATFORM_REF.to_string());
    let auto = current.is_some_and(|c| c.auto_update);
    html! {
        form.stack method="post" action="/system/channel" {
            label {
                "Host id"
                input type="text" name="host_id" required value=(host_id) pattern="[a-z0-9-]+";
            }
            label {
                "Platform channel " span.muted { "(a flake ref; ours by default, or your fork/mirror/pin)" }
                input type="text" name="platform" required value=(platform);
            }
            label {
                @if auto { input type="checkbox" name="auto_update" checked; }
                @else { input type="checkbox" name="auto_update"; }
                " Apply platform updates automatically (always health-checked, auto-rollback)"
            }
            button.btn type="submit" { @if current.is_some() { "Save channel" } @else { "Bind channel" } }
        }
    }
}

#[derive(Deserialize)]
pub struct ChannelForm {
    host_id: String,
    platform: String,
    #[serde(default)]
    auto_update: Option<String>,
}

pub async fn system_set_channel(
    State(state): State<SharedState>,
    Form(form): Form<ChannelForm>,
) -> Redirect {
    let result = (|| -> anyhow::Result<ChannelConfig> {
        crate::hostgen::validate_host_id(form.host_id.trim())?;
        // Detect the hardware here, exactly like `channel set --board auto`:
        // binding a Pi to a generic build must be impossible from the GUI too.
        let board = crate::board::detect()?;
        let mut cfg = ChannelConfig::new(form.host_id.trim().to_string());
        cfg.platform_ref = form.platform.trim().to_string();
        cfg.system = format!("{}-linux", std::env::consts::ARCH);
        cfg.board = board;
        cfg.auto_update = form.auto_update.is_some();
        cfg.save(&state.paths)?;
        Ok(cfg)
    })();
    let redirect =
        |key: &str, msg: &str| Redirect::to(&format!("/system?{key}={}", urlencoding::encode(msg)));
    match result {
        Ok(cfg) => redirect(
            "ok",
            &format!(
                "Channel bound — {} tracking {} ({})",
                cfg.host_id,
                short_ref(&cfg.platform_ref),
                cfg.board.as_deref().unwrap_or("generic")
            ),
        ),
        Err(e) => redirect("err", &format!("{e:#}")),
    }
}

#[derive(Deserialize)]
pub struct CloudForm {
    server: String,
    token: String,
}

pub async fn cloud_enroll(State(state): State<SharedState>, Form(form): Form<CloudForm>) -> Redirect {
    let redirect =
        |key: &str, msg: &str| Redirect::to(&format!("/system?{key}={}", urlencoding::encode(msg)));
    let server = form.server.trim().to_string();
    let token = form.token.trim().to_string();
    if server.is_empty() || token.is_empty() {
        return redirect("err", "A cloud server and an enrollment token are both required");
    }
    let result = {
        let state = state.clone();
        blocking(move || crate::cloud::enroll(&state.paths, &server, &token)).await
    };
    match result {
        Ok(()) => redirect("ok", "Linked — managed backup is on. Reveal your recovery key on the Backup page."),
        Err(e) => redirect("err", &format!("{e:#}")),
    }
}

pub async fn cloud_provision(State(state): State<SharedState>) -> Redirect {
    let redirect =
        |key: &str, msg: &str| Redirect::to(&format!("/system?{key}={}", urlencoding::encode(msg)));
    let result = {
        let state = state.clone();
        blocking(move || crate::cloud::provision(&state.paths)).await
    };
    match result {
        Ok(()) => redirect("ok", "Managed storage credentials refreshed"),
        Err(e) => redirect("err", &format!("{e:#}")),
    }
}

pub async fn system_check(State(state): State<SharedState>) -> Redirect {
    let result = {
        let state = state.clone();
        blocking(move || {
            let cfg = ChannelConfig::load(&state.paths)?
                .ok_or_else(|| anyhow::anyhow!("no update channel configured"))?;
            channel::check(&state.paths, &cfg)
        })
        .await
    };
    let redirect =
        |key: &str, msg: &str| Redirect::to(&format!("/system?{key}={}", urlencoding::encode(msg)));
    match result {
        Ok(status) if status.update_available => redirect(
            "ok",
            &format!("Update available — latest {}", short_rev(&status.latest)),
        ),
        Ok(status) => redirect(
            "ok",
            &format!("Platform up to date ({})", short_rev(&status.latest)),
        ),
        Err(err) => redirect("err", &format!("{err:#}")),
    }
}

/// Apply an available platform update. Rebuilding + switching the system needs
/// root, and boxd is unprivileged — so we trigger the root oneshot unit (a
/// narrow polkit rule permits exactly this one unit). It runs in the background:
/// the Box rebuilds, switches, health-checks, and rolls back if unhealthy.
/// `channel update` no-ops when already current, so this is safe to click.
pub async fn system_update(State(_state): State<SharedState>) -> Redirect {
    let out = std::process::Command::new("systemctl")
        .args(["start", "--no-block", "boxd-channel-update.service"])
        .output();
    let redirect =
        |key: &str, msg: &str| Redirect::to(&format!("/system?{key}={}", urlencoding::encode(msg)));
    match out {
        Ok(o) if o.status.success() => redirect(
            "ok",
            "Platform update started — the Box rebuilds, switches, and rolls back automatically if it doesn't come up healthy.",
        ),
        Ok(o) => redirect(
            "err",
            &format!("could not start update: {}", String::from_utf8_lossy(&o.stderr).trim()),
        ),
        Err(e) => redirect("err", &format!("could not start update: {e}")),
    }
}

pub async fn pair(
    State(state): State<SharedState>,
    headers: HeaderMap,
    Query(flash): Query<Flash>,
) -> Html<String> {
    // First-run claim: a Box no one has set up yet can be claimed from the LAN
    // without a code, but never through a tunnel (a public visitor must not be
    // able to seize an unclaimed Box).
    let claimable =
        crate::auth::is_claimable(&state.paths) && !crate::auth::is_proxied(&headers);
    let page = html! {
        (DOCTYPE)
        html lang="en" {
            head {
                meta charset="utf-8";
                meta name="viewport" content="width=device-width, initial-scale=1";
                title { "Pair · The Box Console" }
                style { (PreEscaped(CSS)) }
            }
            body {
                header.top {
                    a.brand href="/pair" {
                        span.mark { (PreEscaped(MARK_SVG)) " THE " b { "BOX" } }
                        span.sub { "Console" }
                    }
                }
                main {
                    @if let Some(msg) = &flash.err { div.flash.err { (msg) } }
                    @if let Some(msg) = &flash.ok { div.flash.ok { (msg) } }
                    section style="max-width:34rem;margin:2.5rem auto" {
                        @if claimable {
                            h2 { "Claim this Box" }
                            p.muted {
                                "No one has set up this Box yet. Claim it to become its operator. "
                                "After that, any other device needs a one-time code to pair, so do "
                                "this now, from your own network."
                            }
                            form.stack method="post" action="/pair/claim" {
                                button.btn type="submit" { "Claim this Box" }
                            }
                            details style="margin-top:1.5rem" {
                                summary.muted { "Have a pairing code instead?" }
                                form.stack method="post" action="/pair/redeem" style="margin-top:.8rem" {
                                    label {
                                        "Pairing code"
                                        input type="text" name="code" required
                                            placeholder="abcd1234ef" autocomplete="one-time-code";
                                    }
                                    button.btn type="submit" { "Pair" }
                                }
                            }
                        } @else {
                            h2 { "Pair this device" }
                            p.muted {
                                "Management on this Box answers to you. Enter your one-time pairing "
                                "code, from your setup recovery kit, or from “Add device” on a device "
                                "that is already paired."
                            }
                            form.stack method="post" action="/pair/redeem" {
                                label {
                                    "Pairing code"
                                    input type="text" name="code" required autofocus
                                        placeholder="abcd1234ef" autocomplete="one-time-code";
                                }
                                button.btn type="submit" { "Pair" }
                            }
                            p.muted style="margin-top:1.5rem" {
                                "No code? On the Box run " code { "boxd auth enroll" }
                                " (over SSH or its console) to mint one."
                            }
                        }
                    }
                }
                footer { "THE BOX CONSOLE · pairing" }
            }
        }
    };
    Html(page.into_string())
}

/// First-run claim: mint the first operator session for an unclaimed Box, from
/// a direct LAN/loopback connection only. Once claimed, this is a no-op and
/// callers fall back to code entry.
pub async fn pair_claim(State(state): State<SharedState>, headers: HeaderMap) -> Response {
    if crate::auth::is_proxied(&headers) {
        return Redirect::to("/pair?err=This+Box+can+only+be+claimed+from+your+local+network")
            .into_response();
    }
    let wants_json = headers
        .get(header::ACCEPT)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|a| a.contains("application/json"));
    match crate::auth::claim(&state.paths, "first device") {
        Ok(Some(token)) => {
            if wants_json {
                axum::Json(serde_json::json!({ "token": token, "label": "first device" }))
                    .into_response()
            } else {
                let mut resp = Redirect::to("/?ok=Box+claimed").into_response();
                if let Ok(cookie) = HeaderValue::from_str(&crate::auth::session_cookie(&token)) {
                    resp.headers_mut().insert(header::SET_COOKIE, cookie);
                }
                resp
            }
        }
        Ok(None) => {
            Redirect::to("/pair?err=This+Box+is+already+claimed.+Enter+a+pairing+code")
                .into_response()
        }
        Err(err) => Redirect::to(&format!(
            "/pair?err={}",
            urlencoding::encode(&format!("{err:#}"))
        ))
        .into_response(),
    }
}

#[derive(Deserialize)]
pub struct CodeForm {
    code: String,
}

pub async fn pair_redeem(
    State(state): State<SharedState>,
    headers: HeaderMap,
    Form(form): Form<CodeForm>,
) -> Response {
    // Agents ask for JSON and get the token in the body (used as a Bearer
    // token); browsers get a redirect that sets the session cookie. This is the
    // machine-readable side of `boxd provision`: after a Box comes up, the agent
    // redeems its one-time pairing code here for a session it can drive /mcp with.
    let wants_json = headers
        .get(header::ACCEPT)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|a| a.contains("application/json"));
    let label = if wants_json { "agent" } else { "browser" };
    match crate::auth::redeem_code(&state.paths, &form.code, label) {
        Ok(token) => {
            if wants_json {
                axum::Json(serde_json::json!({ "token": token, "label": label })).into_response()
            } else {
                let mut resp = Redirect::to("/?ok=Device+paired").into_response();
                if let Ok(cookie) = HeaderValue::from_str(&crate::auth::session_cookie(&token)) {
                    resp.headers_mut().insert(header::SET_COOKIE, cookie);
                }
                resp
            }
        }
        Err(err) => {
            if wants_json {
                (
                    StatusCode::UNAUTHORIZED,
                    axum::Json(serde_json::json!({ "error": format!("{err:#}") })),
                )
                    .into_response()
            } else {
                Redirect::to(&format!(
                    "/pair?err={}",
                    urlencoding::encode(&format!("{err:#}"))
                ))
                .into_response()
            }
        }
    }
}

pub async fn devices(State(state): State<SharedState>, Query(flash): Query<Flash>) -> Html<String> {
    let sessions = crate::auth::list(&state.paths);
    let body = html! {
        h2 { "Paired devices" }
        p.muted {
            "Every browser or agent with management access holds a session here. Add a device "
            "by handing it a one-time code; revoke any device without affecting the others. "
            "Trusted local access (loopback / SSH) always works and isn't listed."
        }
        div.section-head {
            h2 { "Devices" }
            form method="post" action="/devices/add" { button.btn type="submit" { "+ Add device" } }
        }
        @if sessions.is_empty() {
            div.empty {
                p { "No paired devices yet." }
                p.muted { "This Box is currently managed from trusted local access only." }
            }
        } @else {
            table {
                thead { tr { th { "Id" } th { "Label" } th { "Paired" } th {} } }
                tbody {
                    @for s in &sessions {
                        tr {
                            td { code { (s.id) } }
                            td { (s.label) }
                            td {
                                @match chrono::DateTime::from_timestamp(s.created_at, 0) {
                                    Some(t) => (t.format("%Y-%m-%d %H:%M UTC")),
                                    None => "—",
                                }
                            }
                            td {
                                form method="post" action={ "/devices/" (s.id) "/revoke" } {
                                    button.danger type="submit" { "Revoke" }
                                }
                            }
                        }
                    }
                }
            }
        }
    };
    layout("Devices", &flash, body)
}

pub async fn add_device(State(state): State<SharedState>) -> Redirect {
    match crate::auth::mint_code(&state.paths, "device") {
        Ok(code) => Redirect::to(&format!(
            "/devices?ok={}",
            urlencoding::encode(&format!(
                "Pairing code (15 min, single use): {code} — enter it at /pair on the new device"
            ))
        )),
        Err(err) => Redirect::to(&format!(
            "/devices?err={}",
            urlencoding::encode(&format!("{err:#}"))
        )),
    }
}

pub async fn revoke_device(State(state): State<SharedState>, Path(id): Path<String>) -> Redirect {
    match crate::auth::revoke(&state.paths, &id) {
        Ok(true) => Redirect::to("/devices?ok=Device+revoked"),
        Ok(false) => Redirect::to("/devices?err=No+such+device"),
        Err(err) => Redirect::to(&format!(
            "/devices?err={}",
            urlencoding::encode(&format!("{err:#}"))
        )),
    }
}

pub async fn fleet(
    State(state): State<SharedState>,
    Query(flash): Query<Flash>,
) -> Result<Html<String>, AppError> {
    let me = crate::fleet::self_health(&state.paths);
    let peers = blocking(|| Ok(crate::fleet::discover())).await?;

    let body = html! {
        h2 { "Fleet" }
        p.muted {
            "Boxes on this network announce themselves over mDNS. This view is "
            "peer-federated — your Box discovers the others directly and reads each "
            "one's coarse health. Seeing a Box here doesn't grant control; open a "
            "peer to manage it, and pair with it there (Boxes trust you, not each other)."
        }
        section.cards {
            div.card {
                h3 { "This Box" }
                p.big { (me.name) }
                p.muted { (me.services) " service" (if me.services == 1 { "" } else { "s" }) " · boxd " (me.version) }
            }
            div.card {
                h3 { "Peers found" }
                p.big { (peers.len()) }
                p.muted { "on this LAN, right now" }
            }
        }
        section {
            div.section-head { h2 { "Boxes" } }
            table {
                thead {
                    tr { th { "Box" } th { "Health" } th { "Services" } th { "Version" } th { "Address" } th { "Manage" } }
                }
                tbody {
                    tr {
                        td { strong { (me.name) } " " span.muted { "(this box)" } }
                        td {
                            @if me.health == "ok" { span.badge.on { "ok" } }
                            @else { span.badge { (me.health) } }
                        }
                        td { (me.services) }
                        td { (me.version) }
                        td { span.muted { "local" } }
                        td { a href="/" { "dashboard" } }
                    }
                    @for p in &peers {
                        tr {
                            td { strong { (p.host) } }
                            td {
                                @match &p.health {
                                    Some(h) => {
                                        @if h.health == "ok" { span.badge.on { "ok" } }
                                        @else { span.badge { (h.health) } }
                                    },
                                    None => span.badge { "unreachable" },
                                }
                            }
                            td {
                                @match &p.health { Some(h) => (h.services), None => "—" }
                            }
                            td {
                                @match &p.health {
                                    Some(h) => (h.version.as_str()),
                                    None => "—",
                                }
                            }
                            td { code { (p.address) ":" (p.port) } }
                            td {
                                // Open the peer's own dashboard — the operator pairs
                                // (or SSH-tunnels) there. No box-to-box trust: this is
                                // the operator's browser reaching the peer directly.
                                @match &p.health {
                                    Some(_) => a href={ "http://" (p.address) ":" (p.port) "/" }
                                        target="_blank" rel="noopener" { "open ↗" },
                                    None => span.muted { "—" },
                                }
                            }
                        }
                    }
                }
            }
            @if peers.is_empty() {
                div.empty {
                    p { "No other Boxes found on this network." }
                    p.muted { "Boxes discover each other over mDNS on the same LAN — check that any peers are on the same subnet and advertising." }
                }
            }
        }
    };
    Ok(layout("Fleet", &flash, body))
}

pub async fn rollback(State(state): State<SharedState>, Path(number): Path<u64>) -> Redirect {
    let result = {
        let state = state.clone();
        blocking(move || {
            let _guard = state.apply_lock.lock().unwrap();
            ops::rollback(&state.paths, number)
        })
        .await
    };
    match result {
        Ok(info) => ok_redirect(&format!("Rolled back to generation #{}", info.number)),
        Err(err) => err_redirect(&err),
    }
}
