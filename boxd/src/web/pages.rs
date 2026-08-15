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
                    a.active[title == "Devices"] href="/devices" { "Devices" }
                    a.active[title == "Approvals"] href="/approvals" { "Approvals" }
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
            }
            // The system is behind the config: something was deployed that
            // needs systemd or podman, and the apply hasn't succeeded. Say so
            // where the services are, rather than showing them as running.
            @if let Some(reason) = ostier::pending_reason(&state.paths) {
                div.flash.err {
                    "Some services are declared but not running: " (reason) " "
                    form method="post" action="/system/apply" style="display:inline" {
                        button.btn type="submit" { "Apply now" }
                    }
                }
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
                            @let creds = crate::secrets::with_prefix(
                                &state.paths, &format!("service-{}-", s.name));
                            @let status = ops::service_status(
                                &state.paths, s, active.contains(&s.name));
                            tr {
                                td { strong { a href={ "/service/" (s.name) } { (s.name) } } }
                                td { (s.template) }
                                td {
                                    @match status.state {
                                        "active" => { span.badge.on { "active" } },
                                        "not-running" => { span.badge.warn { "not running" } },
                                        _ => { span.badge { "pending" } },
                                    }
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
                                td {
                                    @match &status.url {
                                        Some(u) => { a href=(u) target="_blank" { (u) } },
                                        None => { span.muted { "no web address" } },
                                    }
                                }
                                td {
                                    form method="post" action={ "/services/" (s.name) "/delete" } {
                                        button.danger type="submit" { "Delete" }
                                    }
                                }
                            }
                            @if !creds.is_empty() {
                                tr.credrow {
                                    td colspan="6" {
                                        details {
                                            summary.muted { "Sign-in details the Box created for " (s.name) }
                                            div.creds {
                                                p.hint {
                                                    "Generated when this was deployed, because nobody should have "
                                                    "to invent a password to run a database. Written down here so "
                                                    "you can actually sign in."
                                                }
                                                @for (slot, value) in &creds {
                                                    div.cred {
                                                        span.credkey {
                                                            (slot.trim_start_matches(&format!("service-{}-", s.name))
                                                                 .replace('-', "_").to_uppercase())
                                                        }
                                                        code { (value) }
                                                    }
                                                }
                                            }
                                        }
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

/// Live view of a running operation. The page renders the current phase and
/// log server-side (so it works with JS off, and on first paint), and the
/// console's script refreshes it in place until the job ends, then moves on to
/// wherever the job said it was going.
pub async fn job_view(
    State(state): State<SharedState>,
    Path(id): Path<String>,
    Query(flash): Query<Flash>,
) -> Html<String> {
    let Some(job) = state.jobs.get(&id) else {
        return layout(
            "Working",
            &flash,
            html! {
                h2 { "That job is gone" }
                p.muted {
                    "Jobs are only kept while the daemon is running. Whatever it "
                    "did is recorded in " a href="/generations" { "Generations" } "."
                }
            },
        );
    };
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);
    let running = job.state == crate::jobs::State::Running;
    let failed = job.state == crate::jobs::State::Failed;

    let body = html! {
        section.job data-job=(job.id) data-state=(format!("{:?}", job.state).to_lowercase())
                    data-target=(job.target) {
            div.section-head {
                h2 { (job.label) }
                span.muted { (job.elapsed_secs(now)) "s" }
            }
            @if running {
                div.bar { div.fill {} }
                p.phase { (job.phase) }
            } @else if failed {
                div.flash.err { (job.message) }
            } @else {
                div.flash.ok { (job.message) }
            }
            @if !job.log.is_empty() {
                pre.joblog { @for line in &job.log { (line) "\n" } }
            }
            @if !running {
                p { a.btn href=(job.target) { "Continue" } }
            }
        }
    };
    layout(&job.label, &flash, body)
}

/// The lead sentence of a description. Card grids want one scannable line;
/// the full text stays on the template's own form (and in the API and MCP,
/// where an agent benefits from the detail).
fn first_sentence(text: &str) -> &str {
    // Cut at the first sentence boundary that leaves something worth reading —
    // "Run a web app." on its own says nothing the title didn't. If no boundary
    // qualifies, hand back the whole line; the card clamps it to two lines.
    for (i, _) in text.match_indices(". ") {
        if i + 1 >= 30 {
            return &text[..=i];
        }
    }
    text
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
        p.muted { "Pick what to run. Every deploy builds a new generation and activates it atomically, so anything here is reversible from Generations." }
        @if !catalog.is_empty() {
            div.section-head { h3 { "Ready to run" } }
            section.cards.pick {
                @for entry in catalog.values() {
                    a.card href={ "/services/new/" (entry.id) } {
                        // entry.icon is deliberately not rendered: the values are
                        // names ("elephant", "bucket"), and the design system
                        // allows inline SVG only — no emoji, no pictographs. It
                        // stays in the schema for a future SVG mapping.
                        h3 { (entry.title) }
                        p.muted { (first_sentence(&entry.description)) }
                        @if !entry.category.is_empty() { span.badge { (entry.category) } }
                    }
                }
            }
        }
        div.section-head { h3 { "Build your own" } }
        section.cards.pick {
            @for t in crate::templates::all() {
                a.card href={ "/services/new/" (t.id()) } {
                    h3 { (t.title()) }
                    p.muted { (first_sentence(t.description())) }
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
            "Web address"
            span.hint {
                "A domain you own, if you want to reach this from a browser"
                @if !domain_hint.is_empty() { " — " (domain_hint) }
                ". Leave blank to keep it on your Box only."
            }
            input type="text" name="domain" placeholder="app.example.com";
        }
        label.check {
            input type="checkbox" name="public";
            span {
                "Let people outside your home reach it"
                span.hint { "Off means only devices on your own network can. Needs a tunnel set up under Networking." }
            }
        }
    }
}

fn env_fields() -> Markup {
    html! {
        label {
            "Settings passed to the app"
            span.hint { "One NAME=value per line. Most people never need this." }
            textarea name="env" rows="3" spellcheck="false" placeholder="TZ=UTC" {}
        }
        label {
            "Passwords and keys"
            span.hint {
                "One NAME=value per line. Encrypted on the Box, never written to "
                "config or pushed to your repo. Anything the service needs and you "
                "leave out is generated for you."
            }
            textarea name="secret_env" rows="3" spellcheck="false" {}
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
        // A preset already knows how it wants to run. Someone deploying
        // "PostgreSQL" should not have to answer questions about ports,
        // exposure or environment variables to get a database — the whole point
        // of the catalog is that those answers are already made. Credentials it
        // needs are generated on the Box (see secret_env handling in deploy),
        // so the form asks for a name and nothing else. Everything that CAN be
        // overridden still can be, one disclosure away.
        let defaults = serde_json::to_string_pretty(&entry.params).unwrap_or_default();
        let body = html! {
            h2 { "Deploy " (entry.title) }
            p.muted { (entry.description) }
            form.stack method="post" action="/services" {
                input type="hidden" name="template" value=(entry.id);
                label {
                    "Name"
                    span.hint { "What you will call it on this Box. Fine as it is." }
                    input type="text" name="name" required value=(entry.id) pattern="[a-z0-9-]+" autofocus;
                }
                button.btn type="submit" { "Deploy " (entry.title) }

                details.adv {
                    summary { "Advanced" }
                    div.fields {
                        (common_fields("only matters if this should be reachable in a browser"))
                        label {
                            "Port"
                            span.hint { "Leave blank and the Box picks a free one and checks it does not clash." }
                            input type="number" name="port" min="1" max="65535" placeholder="chosen for you";
                        }
                        (env_fields())
                        details.adv {
                            summary { "What this preset sets up" }
                            pre { (defaults) }
                        }
                    }
                }
            }
        };
        return Ok(layout("Deploy", &flash, body));
    }

    let body = match template.as_str() {
        "static-site" => html! {
            h2 { "Put a web page online" }
            p.muted { "A page or a whole site, served by your Box. Edit it here or point at a folder." }
            form.stack method="post" action="/services" {
                input type="hidden" name="template" value="static-site";
                label {
                    "Name"
                    span.hint { "Used in the address on your Box, like /sites/my-site/." }
                    input type="text" name="name" required placeholder="my-site" pattern="[a-z0-9-]+" autofocus;
                }
                label {
                    "The page"
                    textarea name="content" rows="12" spellcheck="false" { (ops::DEFAULT_INDEX) }
                }
                button.btn type="submit" { "Deploy" }

                details.adv {
                    summary { "Advanced" }
                    div.fields {
                        label {
                            "Use a folder instead"
                            span.hint { "A path on the Box. If set, its contents replace the page above." }
                            input type="text" name="source_path" placeholder="/home/me/mysite/dist";
                        }
                        (common_fields(""))
                    }
                }
            }
        },
        "container" => html! {
            h2 { "Run an app from an image" }
            p.muted { "Point the Box at any published app image and it runs it, keeps it running, and puts it where you say." }
            form.stack method="post" action="/services" {
                input type="hidden" name="template" value="container";
                label {
                    "Name"
                    input type="text" name="name" required placeholder="my-app" pattern="[a-z0-9-]+" autofocus;
                }
                label {
                    "Image"
                    span.hint { "The app to run, from a container registry — for example nginx:1.27." }
                    input type="text" name="image" required placeholder="nginx:1.27";
                }
                label {
                    "Port inside the container"
                    span.hint { "What the app listens on. Its documentation will say; 80 is typical." }
                    input type="number" name="container_port" min="1" max="65535" placeholder="80";
                }
                label {
                    "Who should be able to reach it?"
                    select name="expose" {
                        option value="proxied" selected { "Anyone I give the web address to" }
                        option value="internal" { "Only my other apps on this Box" }
                        option value="exposed" { "Devices on my home network, on its own port" }
                    }
                }
                button.btn type="submit" { "Deploy" }

                details.adv {
                    summary { "Advanced" }
                    div.fields {
                        (common_fields("only if you chose the web address option above"))
                        label {
                            "Port"
                            span.hint { "Leave blank and the Box picks a free one and checks it does not clash." }
                            input type="number" name="port" min="1" max="65535" placeholder="chosen for you";
                        }
                        label {
                            "Start command"
                            span.hint { "Only if the app needs something other than its default." }
                            input type="text" name="cmd" placeholder="redis-server --appendonly yes";
                        }
                        (env_fields())
                        label {
                            "Folders to keep"
                            span.hint { "One per line, as on-the-Box:in-the-app. Anything written here survives updates." }
                            textarea name="volumes" rows="2" spellcheck="false" placeholder="/var/lib/box/my-app:/data" {}
                        }
                    }
                }
            }
        },
        "reverse-proxied-app" => html! {
            h2 { "Run a program you already have" }
            p.muted { "The Box starts it, keeps it running, restarts it if it dies, and serves it on the web." }
            form.stack method="post" action="/services" {
                input type="hidden" name="template" value="reverse-proxied-app";
                label {
                    "Name"
                    input type="text" name="name" required placeholder="my-app" pattern="[a-z0-9-]+" autofocus;
                }
                label {
                    "How to start it"
                    span.hint { "The command to run. $PORT is filled in with the port the Box picked." }
                    input type="text" name="command" required placeholder="/usr/bin/my-app --listen 127.0.0.1:$PORT";
                }
                button.btn type="submit" { "Deploy" }

                details.adv {
                    summary { "Advanced" }
                    div.fields {
                        label {
                            "Port"
                            span.hint { "Leave blank and the Box picks a free one and checks it does not clash." }
                            input type="number" name="port" min="1" max="65535" placeholder="chosen for you";
                        }
                        (common_fields(""))
                    }
                }
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
        let argv: Vec<serde_json::Value> = cmd
            .split_whitespace()
            .map(serde_json::Value::from)
            .collect();
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
        // The console form always posts every field, so an unchecked box
        // genuinely means "not public" here — unlike an API/MCP caller that
        // simply omitted it.
        public: Some(form.public.is_some()),
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
    // A deploy runs a build; it does not belong inside the request. Start it and
    // send the browser to the job view, which follows the phases live.
    let jobs = state.jobs.clone();
    let id = jobs.start("deploy", format!("Deploying {name}"), "/", move |p| {
        p.phase("waiting for other changes to finish");
        let _guard = state.apply_lock.lock().unwrap();
        p.phase(format!("building generation for {name}"));
        let info = ops::deploy(&state.paths, state.builder.as_ref(), request)?;
        p.phase("activated");
        Ok(format!(
            "Deployed {name} — now at generation #{}",
            info.number
        ))
    });
    Redirect::to(&format!("/jobs/{id}"))
}

pub async fn delete_service(
    State(state): State<SharedState>,
    Path(name): Path<String>,
) -> Redirect {
    let id = {
        let state = state.clone();
        let name = name.clone();
        state
            .jobs
            .clone()
            .start("delete", format!("Removing {name}"), "/", move |p| {
                p.phase("waiting for other changes to finish");
                let _guard = state.apply_lock.lock().unwrap();
                p.phase(format!("rebuilding without {name}"));
                let info = ops::delete_service(&state.paths, state.builder.as_ref(), &name)?;
                Ok(format!(
                    "Deleted {name} — now at generation #{}",
                    info.number
                ))
            })
    };
    Redirect::to(&format!("/jobs/{id}"))
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
                // Port 80, not the console's 2693. The web server routes every
                // kind of service by domain — a static site, an app, a
                // container — where the console only ever served files, so
                // anything else 404'd through the tunnel. It also means this
                // console is not on the far end of your public hostname.
                li { "Point the tunnel's public hostname(s) at " code { "http://localhost:2694" } " — the Box's public entrance. Only services you have published are reachable there, so a tunnel cannot expose anything by accident." }
                li { "Paste the token below and enable the tunnel." }
                li { "Give each service the matching domain, and tick “let people outside your home reach it”." }
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
    let redirect = |key: &str, msg: &str| {
        Redirect::to(&format!("/network?{key}={}", urlencoding::encode(msg)))
    };
    let host = none_if_empty(form.hostname).unwrap_or_else(crate::connect::default_hostname);
    let server = form.server.trim().to_string();
    let authkey = form.authkey.trim().to_string();
    if server.is_empty() || authkey.is_empty() {
        return redirect(
            "err",
            "A coordinator URL and a pre-auth key are both required",
        );
    }
    let result = blocking(move || crate::connect::enroll(&server, &authkey, &host)).await;
    match result {
        Ok(()) => redirect("ok", "Joined the mesh"),
        Err(e) => redirect("err", &format!("{e:#}")),
    }
}

pub async fn connect_down(State(_state): State<SharedState>) -> Redirect {
    let redirect = |key: &str, msg: &str| {
        Redirect::to(&format!("/network?{key}={}", urlencoding::encode(msg)))
    };
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
    let redirect = |key: &str, msg: &str| {
        Redirect::to(&format!("/recreate?{key}={}", urlencoding::encode(msg)))
    };

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

    let id = {
        let state = state.clone();
        let repo = repo_url.clone();
        state.jobs.clone().start(
            "recreate",
            "Recreating this Box from its config repo",
            "/",
            move |p| {
                let key = crate::util::TransientSecret::new("restore-identity", &identity)?;
                p.phase("waiting for other changes to finish");
                let _guard = state.apply_lock.lock().unwrap();
                p.phase("cloning config and secrets");
                let info = ops::restore(&state.paths, state.builder.as_ref(), &repo, key.path())?;
                p.phase("re-keyed secrets, generation built");
                Ok(format!("Restored — now at generation #{}", info.number))
                // `key` drops here: overwritten and unlinked, however this ended.
            },
        )
    };
    Redirect::to(&format!("/jobs/{id}"))
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
            schedule: f
                .schedule
                .filter(|s| !s.is_empty())
                .unwrap_or_else(|| "daily".into()),
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
    backup_redirect(
        result,
        "Backup destination saved — reveal and save your recovery key below",
    )
}

pub async fn run_backup_now(State(state): State<SharedState>) -> Redirect {
    // This used to be fire-and-forget: the page said "started" and then told you
    // nothing until you came back and refreshed. As a job it reports what it is
    // doing and whether it worked.
    let paths = state.paths.clone();
    let id = state
        .jobs
        .clone()
        .start("backup", "Backing up", "/backup", move |p| {
            p.phase("reading config");
            let config = BoxConfig::load(&paths)?;
            let bc = config
                .backup
                .clone()
                .ok_or_else(|| anyhow::anyhow!("no backup configured"))?;
            p.phase(format!("sending to {}", bc.backend.kind));
            crate::backup::run(&paths, &config, &bc)?;
            Ok("Backup complete".to_string())
        });
    Redirect::to(&format!("/jobs/{id}"))
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
    // A restore writes files back over the live filesystem, so it runs as a
    // tracked job under the apply lock like every other mutating operation:
    // errors surface on the job page instead of vanishing into a spawned task
    // that told the operator "started" no matter what happened.
    let id = {
        let state = state.clone();
        let snap = f.snapshot.clone();
        let scope = f.scope.clone();
        state.jobs.clone().start(
            "restore",
            format!("Restoring {snap}"),
            "/backup",
            move |p| {
                p.phase("waiting for other changes to finish");
                let _guard = state.apply_lock.lock().unwrap();
                let config = BoxConfig::load(&state.paths)?;
                let bc = config
                    .backup
                    .clone()
                    .ok_or_else(|| anyhow::anyhow!("no backup configured"))?;
                let includes = crate::backup::resolve_scope(&state.paths, &config, &scope)?;
                p.phase(format!("restoring {scope} from {snap}"));
                crate::backup::restore(
                    &state.paths,
                    &bc,
                    &snap,
                    std::path::Path::new("/"),
                    &includes,
                )?;
                Ok(format!("Restored {scope} from {snap}"))
            },
        )
    };
    Redirect::to(&format!("/jobs/{id}"))
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
    let os_available = ostier::available() && channel_update_available();

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
                            @if os_available {
                                form method="post" action="/system/update" {
                                    button.btn.primary type="submit" { "Update now" }
                                }
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

pub async fn cloud_enroll(
    State(state): State<SharedState>,
    Form(form): Form<CloudForm>,
) -> Redirect {
    let redirect =
        |key: &str, msg: &str| Redirect::to(&format!("/system?{key}={}", urlencoding::encode(msg)));
    let server = form.server.trim().to_string();
    let token = form.token.trim().to_string();
    if server.is_empty() || token.is_empty() {
        return redirect(
            "err",
            "A cloud server and an enrollment token are both required",
        );
    }
    let result = {
        let state = state.clone();
        blocking(move || crate::cloud::enroll(&state.paths, &server, &token)).await
    };
    match result {
        Ok(()) => redirect(
            "ok",
            "Linked — managed backup is on. Reveal your recovery key on the Backup page.",
        ),
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
/// Is the root-only unit that applies a platform update actually installed?
///
/// A NixOS system profile is not enough to judge by: a developer's own NixOS
/// machine has one, but not this unit, which ships with the Box's platform
/// module. Checking the profile alone is what produced "Unit
/// boxd-channel-update.service not found" from a button that should not have
/// been there.
fn channel_update_available() -> bool {
    std::process::Command::new("systemctl")
        .args(["cat", "boxd-channel-update.service"])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

/// Retry making the system match the config, after a structural deploy could
/// not apply. Runs as a job so the operator sees the build output and the real
/// outcome rather than a redirect that claims success.
pub async fn system_apply(State(state): State<SharedState>) -> Redirect {
    let id = {
        let state = state.clone();
        state.jobs.clone().start(
            "os-apply",
            "Making the system match the config",
            "/",
            move |p| {
                p.phase("waiting for other changes to finish");
                let _guard = state.apply_lock.lock().unwrap();
                p.phase("building and switching the system");
                match ostier::request_apply(&state.paths) {
                    ostier::ApplyOutcome::Applied => {
                        Ok("The system now matches your configuration.".to_string())
                    }
                    ostier::ApplyOutcome::NotABox => anyhow::bail!(
                        "This machine is not a Box: it has no OS tier, so services that need \
                         systemd or podman cannot run here. This works on a Box."
                    ),
                    ostier::ApplyOutcome::Failed(detail) => {
                        anyhow::bail!("The system apply failed and rolled itself back: {detail}")
                    }
                }
            },
        )
    };
    Redirect::to(&format!("/jobs/{id}"))
}

pub async fn system_update(State(_state): State<SharedState>) -> Redirect {
    // Applying a platform update means switching the whole NixOS system, which
    // a root-only systemd unit does. That unit is part of the Box's own OS, so
    // it does not exist on a development machine — say that, rather than
    // surfacing "Unit boxd-channel-update.service not found".
    if !channel_update_available() {
        return Redirect::to(&format!(
            "/system?err={}",
            urlencoding::encode(
                "This machine is not a Box: the platform update service is not installed, so there is no whole-system update to apply here. This works on a Box."
            )
        ));
    }
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
    let claimable = crate::auth::is_claimable(&state.paths) && !crate::auth::is_proxied(&headers);
    let has_keys = crate::auth::has_security_keys(&state.paths);
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
                            // Only where a key can actually work: this Box is
                            // claimed and has one enrolled.
                            @if has_keys {
                                div #keysigninbox style="margin-bottom:1.2rem" {
                                    button.btn type="button" #keysignin { "Sign in with a security key" }
                                    p.hint #keysigninmsg {}
                                }
                                p.muted style="margin-bottom:1.2rem" { "or enter a pairing code" }
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
        Ok(None) => Redirect::to("/pair?err=This+Box+is+already+claimed.+Enter+a+pairing+code")
            .into_response(),
        Err(err) => Redirect::to(&format!(
            "/pair?err={}",
            urlencoding::encode(&format!("{err:#}"))
        ))
        .into_response(),
    }
}

pub async fn pair_redeem(
    State(state): State<SharedState>,
    headers: HeaderMap,
    body: String,
) -> Response {
    // Agents ask for JSON and get the token in the body (used as a Bearer
    // token); browsers get a redirect that sets the session cookie. This is the
    // machine-readable side of `boxd provision`: after a Box comes up, the agent
    // redeems its one-time pairing code here for a session it can drive /mcp with.
    //
    // The body may be JSON or a form, because both spell {code} and this is
    // the BOOTSTRAP — the first request an agent ever authenticates with. The
    // 401 signpost tells agents to POST JSON, an agent did, and the form-only
    // parser refused the very instructions the Box itself had handed out.
    // Refusing over framing is pedantry a front door cannot afford.
    let json_body = serde_json::from_str::<serde_json::Value>(&body).ok();
    let json_str = |key: &str| {
        json_body
            .as_ref()
            .and_then(|v| v.get(key).and_then(serde_json::Value::as_str))
            .map(str::to_string)
    };
    let code = json_str("code")
        .or_else(|| {
            body.split('&').find_map(|pair| {
                let (k, v) = pair.split_once('=')?;
                if k != "code" {
                    return None;
                }
                let plus = v.replace('+', " ");
                let decoded = urlencoding::decode(&plus).ok()?;
                Some(decoded.trim().to_string())
            })
        })
        .unwrap_or_default();
    let wants_json = headers
        .get(header::ACCEPT)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|a| a.contains("application/json"));
    // The label names the session in the operator's device list — "what is
    // this thing that can drive my Box". Callers that know (the connect CLI
    // says which editor it wired up) say so; the defaults are the old ones.
    let label = json_str("label")
        .filter(|l| !l.trim().is_empty())
        .map(|l| l.chars().take(64).collect::<String>())
        .unwrap_or_else(|| {
            if wants_json {
                "agent".into()
            } else {
                "browser".into()
            }
        });
    let label = label.as_str();
    match crate::auth::redeem_code(&state.paths, &code, label) {
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

pub async fn devices(
    State(state): State<SharedState>,
    headers: HeaderMap,
    Query(flash): Query<Flash>,
) -> Html<String> {
    devices_page(&state, &headers, &flash, None)
}

/// Mint a session for an agent and show it once, with the command to paste.
///
/// Minting an agent credential was CLI-only, over SSH — which the person this
/// product is for does not have and should not need. If an agent is meant to be
/// the operator, connecting one has to be something you can do from here.
///
/// This renders the page rather than redirecting, so the credential never
/// travels in a URL and never lands in browser history.
pub async fn create_agent_connection(
    State(state): State<SharedState>,
    headers: HeaderMap,
) -> Html<String> {
    match crate::auth::mint_session(&state.paths, "agent") {
        Ok(token) => {
            let flash = Flash {
                ok: Some("Agent connection created — copy it now, it is shown once.".into()),
                err: None,
            };
            devices_page(&state, &headers, &flash, Some(token))
        }
        Err(err) => {
            let flash = Flash {
                ok: None,
                err: Some(format!("{err:#}")),
            };
            devices_page(&state, &headers, &flash, None)
        }
    }
}

fn devices_page(
    state: &SharedState,
    headers: &HeaderMap,
    flash: &Flash,
    agent_token: Option<String>,
) -> Html<String> {
    let sessions = crate::auth::list(&state.paths);
    // Whether this browser could use a security key at all is a property of the
    // ADDRESS it reached us on, so it is computed per request.
    let avail = key_availability(headers);
    let keys = crate::auth::list_keys(&state.paths);
    // Only the hash of a session is stored, so the one moment this can be
    // shown is right after minting it.
    let mcp_url = mcp_url_for(headers);
    let body = html! {
        h2 { "Paired devices" }
        p.muted {
            "Every browser or agent with management access holds a session here — including one "
            "opened on the Box itself. Add a device by handing it a one-time code; revoke any "
            "device without affecting the others. Being able to reach this console is not "
            "authority on its own: every service the Box runs can reach it too."
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
                thead { tr { th { "Id" } th { "Label" } th { "Paired" } th { "Destructive ops" } th {} } }
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
                                // The leash, per session: destructive calls
                                // either wait on the Approvals page or run
                                // straight through. Granting this is a real
                                // decision; the button says which way it is.
                                @if s.autonomous {
                                    form method="post" action={ "/devices/" (s.id) "/autonomy" } {
                                        input type="hidden" name="on" value="false";
                                        button type="submit" title="This session may run destructive operations without asking. Click to require approval again." {
                                            "Autonomous — revoke"
                                        }
                                    }
                                } @else {
                                    form method="post" action={ "/devices/" (s.id) "/autonomy" } {
                                        input type="hidden" name="on" value="true";
                                        button type="submit" title="Destructive operations from this session wait for your tap on the Approvals page. Click to let it act alone." {
                                            "Needs approval"
                                        }
                                    }
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

        section {
            div.section-head { h2 { "Connect an agent" } }
            p.muted {
                "Give a coding agent — Claude Code, or anything else that speaks MCP — its own "
                "access to this Box, so it can deploy what it builds for you, check whether it "
                "worked, and read the logs when it didn't. It gets its own credential, listed "
                "above like any other device, and you can revoke it on its own."
            }
            @match &agent_token {
                Some(token) => {
                    p { "Paste this into a terminal on the computer where you use the agent:" }
                    pre.copyable { code {
                        "claude mcp add --transport http the-box " (mcp_url)
                        " --header \"Authorization: Bearer " (token) "\""
                    } }
                    p.muted { "Or, for anything that reads a .mcp.json file:" }
                    pre.copyable { code {
                        (PreEscaped(mcp_json(&mcp_url, token)))
                    } }
                    p.field-note {
                        "This is shown once. If you lose it, make another — they are independent, "
                        "and revoking one leaves the rest working."
                    }
                }
                None => {
                    form method="post" action="/devices/agent" {
                        button.btn type="submit" { "Create an agent connection" }
                    }
                }
            }
        }

        section {
            div.section-head { h2 { "Security keys" } }
            p.muted {
                "Sign in by touching a YubiKey or using a passkey, instead of copying a "
                "one-time code. The private half never leaves the key, and what it signs is "
                "tied to this Box's address, so it cannot be replayed anywhere else."
            }

            @if avail.usable {
                form.stack #keyform onsubmit="return false" {
                    label {
                        "Name this key"
                        span.hint { "So you can tell them apart later." }
                        input type="text" #keylabel value="Security key";
                    }
                    button.btn type="button" #keyadd { "Enroll a security key" }
                    p.hint #keymsg {}
                }
            } @else {
                div.empty {
                    p { strong { "Not available at this address." } }
                    p.muted { (avail.reason) }
                    p.muted {
                        "You are on " code { (avail.origin) } ". That is a rule of the web "
                        "platform rather than a setting here: browsers only offer security "
                        "keys over an encrypted connection, and never to a bare IP address."
                    }
                }
            }

            @if keys.is_empty() {
                p.muted { "No security keys enrolled yet." }
            } @else {
                table {
                    thead { tr { th { "Name" } th { "Works at" } th { "Last used" } th {} } }
                    tbody {
                        @for k in &keys {
                            tr {
                                td { strong { (k.label) } }
                                td {
                                    code { (k.rp_id) }
                                    @if k.rp_id != avail.rp_id { " " span.badge { "other address" } }
                                }
                                td {
                                    @match k.last_used_at.and_then(|t| chrono::DateTime::from_timestamp(t, 0)) {
                                        Some(t) => { (t.format("%Y-%m-%d %H:%M UTC").to_string()) },
                                        None => { "never" },
                                    }
                                }
                                td {
                                    form method="post" action={ "/devices/keys/" (k.id) "/revoke" } {
                                        button.danger type="submit" { "Remove" }
                                    }
                                }
                            }
                        }
                    }
                }
            }
        }
    };
    layout("Devices", flash, body)
}

/// The address an agent on another machine should use to reach this Box. The
/// browser just reached the console at some address that works from where the
/// person is sitting, so reuse it rather than guessing a hostname.
fn mcp_url_for(headers: &HeaderMap) -> String {
    let host = headers
        .get(axum::http::header::HOST)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("box.local:2693");
    format!("http://{host}/mcp")
}

/// The `.mcp.json` an agent that reads a config file expects.
fn mcp_json(url: &str, token: &str) -> String {
    let value = serde_json::json!({
        "mcpServers": {
            "the-box": {
                "type": "http",
                "url": url,
                "headers": { "Authorization": format!("Bearer {token}") },
            }
        }
    });
    let text = serde_json::to_string_pretty(&value).unwrap_or_default();
    maud::PreEscaped(text).0
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

/// One service, in full: what it is, where it serves, where it came from,
/// and what it is saying right now. The console's "why is it not working"
/// page — logs refresh live (data-poll), so watching a deploy settle or an
/// app crash-loop needs no reloading.
pub async fn service_detail(
    State(state): State<SharedState>,
    Path(name): Path<String>,
    Query(flash): Query<Flash>,
) -> Result<Html<String>, AppError> {
    let config = BoxConfig::load(&state.paths)?;
    let Some(svc) = config.find(&name).cloned() else {
        return Ok(layout(
            "Service",
            &flash,
            html! {
                h2 { "No service named " (name) }
                p.muted { a href="/" { "← back to services" } }
            },
        ));
    };
    let current = store::current(&state.paths)?;
    let active: HashSet<String> = current
        .as_ref()
        .and_then(|c| manifest::read_manifest(&c.store_path).ok())
        .map(|m| m.services.into_iter().map(|s| s.name).collect())
        .unwrap_or_default();
    let status = ops::service_status(&state.paths, &svc, active.contains(&svc.name));
    let tunnel_running = state.tunnel.status().state == "running";
    let sync = crate::pull::read_sync_state(&state.paths, &name);
    let logs = crate::logs::service_logs(&state.paths, &name, 80).ok();
    let build_log = std::fs::read_to_string(state.paths.build_log(&name))
        .ok()
        .map(|t| {
            let lines: Vec<&str> = t.lines().collect();
            let start = lines.len().saturating_sub(40);
            lines[start..].join("\n")
        });
    let when = |unix: u64| {
        chrono::DateTime::from_timestamp(unix as i64, 0)
            .map(|t| t.format("%Y-%m-%d %H:%M UTC").to_string())
            .unwrap_or_else(|| "—".into())
    };

    let body = html! {
        div.section-head {
            h2 { (svc.name) }
            span.muted { (svc.template) }
        }
        section.cards {
            div.card {
                h3 { "State" }
                p.big {
                    @match status.state {
                        "active" => { span.badge.on { "active" } },
                        "not-running" => { span.badge.warn { "not running" } },
                        other => { span.badge { (other) } },
                    }
                }
            }
            div.card {
                h3 { "Reach" }
                p.big {
                    @match &svc.domain {
                        Some(d) => {
                            @if tunnel_running { a href={ "https://" (d) } target="_blank" { (d) } }
                            @else { span.muted { (d) } " " span.badge { "tunnel off" } }
                        },
                        None => { span.muted { "your network only" } },
                    }
                }
                p.muted {
                    @if let Some(url) = &status.url { a href=(url) { (url) } }
                }
            }
            @if let Some(link) = &svc.repo {
                div.card {
                    h3 { "Source" }
                    p.big { (link.repo) }
                    p.muted {
                        "branch " code { (link.branch) }
                        @if link.build.is_some() { " · built on the Box" }
                    }
                }
            }
        }

        @if svc.repo.is_some() {
            section data-poll="4" {
                div.section-head {
                    h2 { "Deploys" }
                    form method="post" action={ "/service/" (svc.name) "/sync" } {
                        button.btn type="submit" { "Sync now" }
                    }
                }
                @match &sync {
                    Some(s) if s.ok => {
                        p {
                            span.badge.on { "in step" } " "
                            @if let Some(c) = &s.commit { code { (c.chars().take(10).collect::<String>()) } " " }
                            span.muted { "last checked " (when(s.at)) " — pushing deploys within a minute; Sync now skips the wait" }
                        }
                    }
                    Some(s) => {
                        p {
                            span.badge.warn { "failing" } " "
                            span.muted { "since " (when(s.at)) }
                        }
                        @if let Some(e) = &s.error { pre.logs { (e) } }
                    }
                    None => { p.muted { "No sync has run yet." } }
                }
                @if let Some(tail) = &build_log {
                    h3 { "Last build" }
                    pre.logs { (tail) }
                }
            }
        }

        section data-poll="4" {
            div.section-head { h2 { "Logs" } }
            @match &logs {
                Some(l) => {
                    @let lines = l.get("lines").and_then(|v| v.as_array()).cloned().unwrap_or_default();
                    @if lines.is_empty() && svc.template == "static-site" && status.state == "active" {
                        // An active static site on the fast path has no unit of
                        // its own — boxd serves the files. The journal note
                        // ("declared but not running") would contradict the
                        // ACTIVE badge two sections up, and the badge is right.
                        p.muted {
                            "Static content — served directly, no process to log. "
                            "Once the OS tier's web server fronts it, its logs appear here."
                        }
                    } @else if lines.is_empty() {
                        p.muted {
                            @match l.get("note").and_then(|n| n.as_str()) {
                                Some(n) => (n),
                                None => "Nothing in the journal yet.",
                            }
                        }
                    } @else {
                        @if let Some(unit) = l.get("unit").and_then(|u| u.as_str()) {
                            p.muted { "from " code { (unit) } " — refreshes live" }
                        }
                        pre.logs {
                            @for line in &lines {
                                (line.as_str().unwrap_or_default()) "\n"
                            }
                        }
                    }
                }
                None => { p.muted { "Logs are not readable here (no journal on this machine)." } }
            }
        }

        section {
            div.section-head { h2 { "Danger" } }
            form method="post" action={ "/services/" (svc.name) "/delete" }
                 onsubmit="return confirm('Delete this service and stop serving it?')" {
                button.danger type="submit" { "Delete service" }
            }
        }
    };
    Ok(layout(&svc.name.clone(), &flash, body))
}

/// The console's "Sync now": the same sync the poller and the webhook run,
/// on demand, with the outcome in the flash.
pub async fn service_sync_now(
    State(state): State<SharedState>,
    Path(name): Path<String>,
) -> Redirect {
    let n = name.clone();
    let state2 = state.clone();
    let outcome = tokio::task::spawn_blocking(move || {
        let _guard = state2.apply_lock.lock().unwrap();
        crate::pull::sync_recorded(
            &state2.paths,
            state2.builder.as_ref(),
            &state2.build_exec,
            &n,
            false,
        )
    })
    .await;
    let dest = |kind: &str, msg: &str| {
        Redirect::to(&format!(
            "/service/{}?{}={}",
            name,
            kind,
            urlencoding::encode(msg)
        ))
    };
    match outcome {
        Ok(Ok(crate::pull::SyncOutcome::Deployed { generation, .. })) => {
            dest("ok", &format!("Deployed as generation #{generation}"))
        }
        Ok(Ok(crate::pull::SyncOutcome::UpToDate { .. })) => {
            dest("ok", "Already in step with the repository")
        }
        Ok(Err(e)) => dest("err", &format!("{e:#}")),
        Err(e) => dest("err", &format!("{e}")),
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

#[derive(Deserialize)]
pub struct AutonomyForm {
    on: String,
}

/// The per-session leash: whether this device's destructive calls run alone
/// or wait on the Approvals page. An operator decision, taken here in the
/// device list where the session's label and age are in view.
pub async fn set_device_autonomy(
    State(state): State<SharedState>,
    Path(id): Path<String>,
    Form(f): Form<AutonomyForm>,
) -> Redirect {
    let on = f.on == "true";
    match crate::auth::set_autonomous(&state.paths, &id, on) {
        Ok(true) if on => Redirect::to(
            "/devices?ok=This+session+may+now+run+destructive+operations+without+asking",
        ),
        Ok(true) => Redirect::to("/devices?ok=Destructive+operations+now+need+your+approval"),
        Ok(false) => Redirect::to("/devices?err=No+such+device"),
        Err(err) => Redirect::to(&format!(
            "/devices?err={}",
            urlencoding::encode(&format!("{err:#}"))
        )),
    }
}

/// The human half of the trust ceremony: what agents asked to do that they
/// may not do alone. Approving runs the exact call; denying records the no.
pub async fn approvals(
    State(state): State<SharedState>,
    Query(flash): Query<Flash>,
) -> Html<String> {
    let actions = crate::approvals::list(&state.paths);
    let pending: Vec<_> = actions
        .iter()
        .filter(|a| a.state == crate::approvals::State::Pending)
        .collect();
    let decided: Vec<_> = actions
        .iter()
        .filter(|a| a.state != crate::approvals::State::Pending)
        .take(20)
        .collect();
    let when = |unix: u64| {
        chrono::DateTime::from_timestamp(unix as i64, 0)
            .map(|t| t.format("%Y-%m-%d %H:%M UTC").to_string())
            .unwrap_or_else(|| "—".into())
    };
    let body = html! {
        h2 { "Approvals" }
        p.muted {
            "Agents run this Box, but destructive operations — erasing a machine, deleting a "
            "service, restoring over live data — wait here for you unless you have granted a "
            "session autonomy in the device list. Approving runs exactly what was asked; "
            "denying tells the agent no."
        }
        @if pending.is_empty() {
            div.empty { p { "Nothing is waiting on you." } }
        } @else {
            table {
                thead { tr { th { "Asked" } th { "By" } th { "Would" } th {} } }
                tbody {
                    @for a in &pending {
                        tr {
                            td { (when(a.created_unix)) }
                            td { (a.requested_by) }
                            td { (a.summary) }
                            td {
                                form method="post" action={ "/approvals/" (a.id) "/approve" } style="display:inline" {
                                    button.danger type="submit" { "Approve & run" }
                                }
                                " "
                                form method="post" action={ "/approvals/" (a.id) "/deny" } style="display:inline" {
                                    button type="submit" { "Deny" }
                                }
                            }
                        }
                    }
                }
            }
        }
        @if !decided.is_empty() {
            div.section-head { h2 { "Decided" } }
            table {
                thead { tr { th { "Asked" } th { "By" } th { "Would" } th { "Decision" } } }
                tbody {
                    @for a in &decided {
                        tr {
                            td { (when(a.created_unix)) }
                            td { (a.requested_by) }
                            td { (a.summary) }
                            td {
                                @match a.state {
                                    crate::approvals::State::Approved => "approved",
                                    crate::approvals::State::Denied => "denied",
                                    crate::approvals::State::Pending => "pending",
                                }
                            }
                        }
                    }
                }
            }
        }
    };
    layout("Approvals", &flash, body)
}

pub async fn approve_action(State(state): State<SharedState>, Path(id): Path<String>) -> Redirect {
    let Some(action) = crate::approvals::get(&state.paths, &id) else {
        return Redirect::to("/approvals?err=No+such+pending+action");
    };
    if action.state != crate::approvals::State::Pending {
        return Redirect::to("/approvals?err=Already+decided");
    }
    // The tap IS the consent: run the exact call the agent made, through the
    // same code path it would have taken, and record what happened.
    let outcome = crate::web::mcp::execute(state.clone(), &action.tool, action.args.clone()).await;
    let result = match outcome {
        Ok(Ok(v)) => serde_json::to_string_pretty(&v).unwrap_or_default(),
        Ok(Err(e)) => format!("failed: {e:#}"),
        Err((_, e)) => format!("failed: {e}"),
    };
    let failed = result.starts_with("failed:");
    if let Err(e) = crate::approvals::resolve(
        &state.paths,
        &id,
        crate::approvals::State::Approved,
        Some(result),
    ) {
        return Redirect::to(&format!(
            "/approvals?err={}",
            urlencoding::encode(&format!("{e:#}"))
        ));
    }
    if failed {
        Redirect::to("/approvals?err=Approved%2C+but+the+operation+itself+failed+%E2%80%94+the+agent+sees+the+error")
    } else {
        Redirect::to("/approvals?ok=Approved+and+run+%E2%80%94+the+agent+sees+the+result")
    }
}

pub async fn deny_action(State(state): State<SharedState>, Path(id): Path<String>) -> Redirect {
    match crate::approvals::resolve(&state.paths, &id, crate::approvals::State::Denied, None) {
        Ok(()) => Redirect::to("/approvals?ok=Denied+%E2%80%94+the+agent+is+told+no"),
        Err(e) => Redirect::to(&format!(
            "/approvals?err={}",
            urlencoding::encode(&format!("{e:#}"))
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
    let id = state.jobs.clone().start(
        "rollback",
        format!("Rolling back to generation #{number}"),
        "/generations",
        move |p| {
            p.phase("waiting for other changes to finish");
            let _guard = state.apply_lock.lock().unwrap();
            p.phase(format!("switching to generation #{number}"));
            let info = ops::rollback(&state.paths, number)?;
            Ok(format!("Rolled back to generation #{}", info.number))
        },
    );
    Redirect::to(&format!("/jobs/{id}"))
}

// ---- security keys (WebAuthn) ---------------------------------------------

/// What the browser can do here, decided from the request's own headers.
fn key_availability(headers: &HeaderMap) -> crate::webauthn::Availability {
    let host = headers
        .get("host")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("localhost");
    let proto = headers
        .get("x-forwarded-proto")
        .and_then(|v| v.to_str().ok());
    crate::webauthn::availability(host, proto)
}

fn json_err(e: impl std::fmt::Display) -> Response {
    (
        StatusCode::BAD_REQUEST,
        axum::Json(serde_json::json!({ "error": e.to_string() })),
    )
        .into_response()
}

#[derive(Deserialize)]
pub struct KeyStart {
    #[serde(default)]
    label: String,
}

/// Enroll a key, step 1: hand the browser a challenge for the authenticator.
pub async fn key_register_start(
    State(state): State<SharedState>,
    headers: HeaderMap,
    Form(form): Form<KeyStart>,
) -> Response {
    let avail = key_availability(&headers);
    if !avail.usable {
        return json_err(&avail.reason);
    }
    let existing: Vec<_> = crate::auth::keys_for(&state.paths, &avail.rp_id)
        .iter()
        .filter_map(|k| k.passkey().ok())
        .collect();
    let operator = match crate::auth::operator_id(&state.paths) {
        Ok(id) => id,
        Err(e) => return json_err(e),
    };
    match state
        .ceremonies
        .start_registration(&avail, operator, &existing)
    {
        Ok((challenge, handle)) => axum::Json(serde_json::json!({
            "challenge": challenge, "handle": handle, "label": form.label
        }))
        .into_response(),
        Err(e) => json_err(e),
    }
}

#[derive(Deserialize)]
pub struct KeyFinish {
    handle: String,
    #[serde(default)]
    label: String,
    response: serde_json::Value,
}

/// Enroll a key, step 2: verify and store it.
pub async fn key_register_finish(
    State(state): State<SharedState>,
    headers: HeaderMap,
    axum::Json(body): axum::Json<KeyFinish>,
) -> Response {
    let avail = key_availability(&headers);
    let label = if body.label.trim().is_empty() {
        "Security key".to_string()
    } else {
        body.label.trim().to_string()
    };
    match state
        .ceremonies
        .finish_registration(&body.handle, body.response)
    {
        Ok(passkey) => match serde_json::to_value(&passkey)
            .map_err(anyhow::Error::from)
            .and_then(|v| crate::auth::add_key(&state.paths, &label, &avail.rp_id, v))
        {
            Ok(()) => axum::Json(serde_json::json!({ "ok": true, "label": label })).into_response(),
            Err(e) => json_err(e),
        },
        Err(e) => json_err(format!("{e:#}")),
    }
}

pub async fn revoke_key(State(state): State<SharedState>, Path(id): Path<String>) -> Redirect {
    let msg = match crate::auth::revoke_key(&state.paths, &id) {
        Ok(true) => "Security key removed".to_string(),
        Ok(false) => "No such security key".to_string(),
        Err(e) => format!("{e:#}"),
    };
    Redirect::to(&format!("/devices?ok={}", urlencoding::encode(&msg)))
}

/// Sign in with a key, step 1. Public, like the rest of /pair: you cannot have
/// a session yet, which is the point.
pub async fn key_signin_start(State(state): State<SharedState>, headers: HeaderMap) -> Response {
    let avail = key_availability(&headers);
    if !avail.usable {
        return json_err(&avail.reason);
    }
    let keys: Vec<_> = crate::auth::keys_for(&state.paths, &avail.rp_id)
        .iter()
        .filter_map(|k| k.passkey().ok())
        .collect();
    match state.ceremonies.start_authentication(&avail, &keys) {
        Ok((challenge, handle)) => {
            axum::Json(serde_json::json!({ "challenge": challenge, "handle": handle }))
                .into_response()
        }
        Err(e) => json_err(e),
    }
}

#[derive(Deserialize)]
pub struct KeySignin {
    handle: String,
    response: serde_json::Value,
}

/// Sign in with a key, step 2: verify the assertion and mint the same kind of
/// session a pairing code would have produced.
pub async fn key_signin_finish(
    State(state): State<SharedState>,
    axum::Json(body): axum::Json<KeySignin>,
) -> Response {
    match state
        .ceremonies
        .finish_authentication(&body.handle, body.response)
    {
        Ok(result) => {
            let cred_id: &[u8] = result.cred_id().as_ref();
            match crate::auth::session_from_key(&state.paths, cred_id, "security key") {
                Ok(token) => {
                    let mut resp =
                        axum::Json(serde_json::json!({ "ok": true, "next": "/" })).into_response();
                    if let Ok(cookie) = HeaderValue::from_str(&crate::auth::session_cookie(&token))
                    {
                        resp.headers_mut().insert(header::SET_COOKIE, cookie);
                    }
                    resp
                }
                Err(e) => json_err(e),
            }
        }
        Err(e) => json_err(format!("{e:#}")),
    }
}
