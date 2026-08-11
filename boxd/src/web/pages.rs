use std::collections::HashSet;
use std::path::PathBuf;

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
                title { (title) " · The Box Configurator" }
                style { (PreEscaped(CSS)) }
            }
            body {
                header.top {
                    a.brand href="/" {
                        span.mark { (PreEscaped(MARK_SVG)) " THE " b { "BOX" } }
                        span.sub { "Configurator" }
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
                    "THE BOX CONFIGURATOR · boxd " (env!("CARGO_PKG_VERSION"))
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

pub async fn new_service(
    State(_state): State<SharedState>,
    Query(flash): Query<Flash>,
) -> Html<String> {
    let body = html! {
        h2 { "Deploy a static site" }
        p.muted { "Creates or updates a service, builds a new generation and activates it atomically. Roll back any time from the Generations page." }
        form.stack method="post" action="/services" {
            label {
                "Name"
                input type="text" name="name" required placeholder="my-site" pattern="[a-z0-9-]+" autofocus;
            }
            label {
                "Domain " span.muted { "(optional, used when exposing publicly)" }
                input type="text" name="domain" placeholder="site.example.com";
            }
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
    };
    layout("Deploy", &flash, body)
}

#[derive(Deserialize)]
pub struct NewServiceForm {
    name: String,
    #[serde(default)]
    domain: Option<String>,
    #[serde(default)]
    content: Option<String>,
    #[serde(default)]
    source_path: Option<String>,
}

fn none_if_empty(value: Option<String>) -> Option<String> {
    value
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
}

pub async fn create_service(
    State(state): State<SharedState>,
    Form(form): Form<NewServiceForm>,
) -> Redirect {
    let request = ops::DeployRequest::static_site(
        form.name.trim().to_string(),
        none_if_empty(form.content),
        none_if_empty(form.source_path).map(PathBuf::from),
        none_if_empty(form.domain),
        false,
    );
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
    };
    layout("Networking", &flash, body)
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
        b.and_then(|bk| f(bk)).unwrap_or_default()
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
    };
    layout("Backup", &flash, body)
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
                            tr { th { "Host id" } td { (c.host_id) } }
                        }
                    }
                }
            },
            None => {
                section {
                    div.empty {
                        p { "This Box manages service content only — no platform update channel is configured." }
                        p.muted {
                            "Set one on the Box with "
                            code { "boxd channel set --host-id <id>" }
                            " to pull platform updates with automatic rollback."
                        }
                    }
                }
            },
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
                title { "Pair · The Box Configurator" }
                style { (PreEscaped(CSS)) }
            }
            body {
                header.top {
                    a.brand href="/pair" {
                        span.mark { (PreEscaped(MARK_SVG)) " THE " b { "BOX" } }
                        span.sub { "Configurator" }
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
                footer { "THE BOX CONFIGURATOR · pairing" }
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
