use std::collections::HashSet;
use std::path::PathBuf;

use axum::{
    extract::{Path, Query, State},
    response::{Html, Redirect},
    Form,
};
use maud::{html, Markup, PreEscaped, DOCTYPE};
use serde::Deserialize;

use crate::config::BoxConfig;
use crate::manifest;
use crate::ops;
use crate::store;

use super::{blocking, AppError, SharedState};

const CSS: &str = include_str!("style.css");

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
                title { (title) " · The Box" }
                style { (PreEscaped(CSS)) }
            }
            body {
                nav {
                    a.brand href="/" { "📦 The Box" }
                    div.links {
                        a href="/" { "Services" }
                        a href="/generations" { "Generations" }
                        a href="/network" { "Networking" }
                        a.btn href="/services/new" { "Deploy" }
                    }
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
                    "boxd " (env!("CARGO_PKG_VERSION"))
                    " — local dashboard · JSON API at "
                    code { "/api/v1" }
                }
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
                        tr { th { "Name" } th { "Template" } th { "Domain" } th { "State" } th { "URL" } th {} }
                    }
                    tbody {
                        @for s in &config.services {
                            tr {
                                td { strong { (s.name) } }
                                td { (s.template.as_str()) }
                                td {
                                    @match &s.domain {
                                        Some(d) => { (d) },
                                        None => { "—" },
                                    }
                                }
                                td {
                                    @if active.contains(&s.name) { span.badge.on { "active" } }
                                    @else { span.badge { "pending" } }
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
    let request = ops::DeployRequest {
        name: form.name.trim().to_string(),
        domain: none_if_empty(form.domain),
        public: false,
        index_html: none_if_empty(form.content),
        source_path: none_if_empty(form.source_path).map(PathBuf::from),
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
