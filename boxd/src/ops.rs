//! High-level operations: the same code paths back the dashboard, the JSON
//! API, the CLI and the MCP tools. A service is a template + params; applying
//! builds a generation, health-checks it, and auto-rolls-back on failure.

use std::fs;
use std::path::PathBuf;

use anyhow::{bail, Context, Result};
use chrono::Utc;
use serde_json::{json, Value};

use crate::config::{validate_domain, validate_service_name, BoxConfig, ServiceConfig};
use crate::history;
use crate::manifest;
use crate::nixgen;
use crate::paths::Paths;
use crate::store::{self, Builder, GenerationInfo};
use crate::templates;
use crate::util;

/// Re-exported so callers keep a single import site for the default page.
pub use crate::templates::DEFAULT_INDEX;

/// A request to create or update a service from a template.
#[derive(Debug, Clone)]
pub struct DeployRequest {
    pub name: String,
    pub template: String,
    pub params: Value,
    pub domain: Option<String>,
    pub public: bool,
}

impl DeployRequest {
    /// Convenience constructor for the static-site template — the shape the
    /// dashboard/API/MCP have historically spoken.
    pub fn static_site(
        name: impl Into<String>,
        index_html: Option<String>,
        source_path: Option<PathBuf>,
        domain: Option<String>,
        public: bool,
    ) -> Self {
        let mut params = serde_json::Map::new();
        if let Some(html) = index_html {
            params.insert("index_html".into(), json!(html));
        }
        if let Some(path) = source_path {
            params.insert("source_path".into(), json!(path.to_string_lossy()));
        }
        Self {
            name: name.into(),
            template: "static-site".into(),
            params: Value::Object(params),
            domain,
            public,
        }
    }
}

/// Which speed a change needs. Content edits to an existing service stay on
/// boxd's fast path (rebuild the lightweight generation, no system switch);
/// anything that alters the shape of the system — a new or removed service, a
/// changed template/domain/exposure — is structural and needs a full OS-tier
/// rebuild + `switch-to-configuration`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChangeKind {
    Content,
    Structural,
}

/// Classify a deploy against the current config. New services and changes to a
/// service's template, domain or exposure are structural; re-materializing an
/// existing service's content is not.
pub fn classify_deploy(current: &BoxConfig, req: &DeployRequest) -> ChangeKind {
    match current.find(&req.name) {
        None => ChangeKind::Structural,
        Some(existing) => {
            if existing.template != req.template
                || existing.domain.as_deref() != req.domain.as_deref()
                || existing.public != req.public
            {
                ChangeKind::Structural
            } else {
                ChangeKind::Content
            }
        }
    }
}

/// Build the current declarative config into a new generation and switch to
/// it atomically. No health gate — see [`apply_checked`].
pub fn apply(paths: &Paths, builder: &dyn Builder) -> Result<GenerationInfo> {
    let config = BoxConfig::load(paths)?;
    let gensrc = nixgen::write_gensrc(paths, &config)?;
    let output = builder.build(&gensrc)?;
    store::record_and_switch(paths, &output)
}

/// A health check runs against a freshly-switched generation; returning `Err`
/// triggers an automatic rollback to the prior generation.
pub type HealthCheck = dyn Fn(&Paths, &GenerationInfo) -> Result<()>;

/// Default health: ask each service's template whether its outputs are present
/// in the built generation. A static site with no index.html, for instance,
/// fails here and is rolled back rather than served broken.
pub fn default_health(_paths: &Paths, info: &GenerationInfo) -> Result<()> {
    let m = manifest::read_manifest(&info.store_path)
        .context("health check: generation manifest is unreadable")?;
    for svc in &m.services {
        if let Some(t) = templates::get(&svc.template) {
            let www = info.store_path.join("services").join(&svc.name).join("www");
            t.health(&svc.name, &www)?;
        }
    }
    Ok(())
}

/// Apply, then health-check the new generation. On failure, roll back to the
/// previous generation (restoring its config + sources) and return the error.
/// This is the reconciler's core safety property: a bad change never sticks.
pub fn apply_checked(
    paths: &Paths,
    builder: &dyn Builder,
    health: &HealthCheck,
) -> Result<GenerationInfo> {
    let previous = store::current(paths)?;
    let info = apply(paths, builder)?;
    match health(paths, &info) {
        Ok(()) => Ok(info),
        Err(e) => match previous {
            Some(prev) => {
                rollback(paths, prev.number)?;
                bail!(
                    "health check failed — rolled back to generation #{}: {e:#}",
                    prev.number
                );
            }
            None => bail!(
                "health check failed on the first generation (nothing to roll back to): {e:#}"
            ),
        },
    }
}

/// Create or update a service from a template, then apply with the health gate.
pub fn deploy(
    paths: &Paths,
    builder: &dyn Builder,
    mut req: DeployRequest,
) -> Result<GenerationInfo> {
    validate_service_name(&req.name)?;

    let template = templates::get(&req.template)
        .with_context(|| format!("unknown template {:?}", req.template))?;
    template.validate(&req.params)?;

    if let Some(domain) = req.domain.take() {
        let domain = domain.trim().to_ascii_lowercase();
        validate_domain(&domain)?;
        let config = BoxConfig::load(paths)?;
        if let Some(other) = config
            .services
            .iter()
            .find(|s| s.name != req.name && s.domain.as_deref() == Some(domain.as_str()))
        {
            bail!(
                "domain {domain:?} is already used by service {:?}",
                other.name
            );
        }
        req.domain = Some(domain);
    }

    template.materialize(&req.params, &paths.source_dir(&req.name))?;

    let mut config = BoxConfig::load(paths)?;
    match config.services.iter_mut().find(|s| s.name == req.name) {
        Some(existing) => {
            existing.template = req.template.clone();
            existing.params = req.params.clone();
            existing.domain = req.domain.clone();
            existing.public = req.public;
        }
        None => config.services.push(ServiceConfig {
            name: req.name.clone(),
            template: req.template.clone(),
            params: req.params.clone(),
            domain: req.domain.clone(),
            public: req.public,
            created_at: Utc::now(),
        }),
    }
    config.save(paths)?;

    let info = apply_checked(paths, builder, &default_health)?;
    history::commit_soft(
        paths,
        &format!(
            "generation #{}: deploy {} ({})",
            info.number, req.name, req.template
        ),
    );
    Ok(info)
}

/// Remove a service from the config and its sources, then apply.
pub fn delete_service(paths: &Paths, builder: &dyn Builder, name: &str) -> Result<GenerationInfo> {
    let mut config = BoxConfig::load(paths)?;
    let before = config.services.len();
    config.services.retain(|s| s.name != name);
    if config.services.len() == before {
        bail!("no service named {name:?}");
    }
    config.save(paths)?;
    util::remove_dir_all_forced(&paths.source_dir(name))?;
    let info = apply_checked(paths, builder, &default_health)?;
    history::commit_soft(
        paths,
        &format!("generation #{}: delete {}", info.number, name),
    );
    Ok(info)
}

/// Switch to an existing generation and restore the declarative state (config
/// and sources) that generation was built from, so a later apply starts from
/// the rolled-back world, not the abandoned one.
pub fn rollback(paths: &Paths, number: u64) -> Result<GenerationInfo> {
    store::switch(paths, number)?;
    let current = store::current(paths)?.context("no current generation after switch")?;

    let m = manifest::read_manifest(&current.store_path)
        .context("reading manifest of rolled-back generation")?;
    fs::write(paths.config_file(), &m.config_toml)?;

    util::remove_dir_all_forced(&paths.sources_dir())?;
    fs::create_dir_all(paths.sources_dir())?;
    for service in &m.services {
        let www = current
            .store_path
            .join("services")
            .join(&service.name)
            .join("www");
        if www.is_dir() {
            util::copy_dir_recursive(&www, &paths.source_dir(&service.name))?;
        }
    }
    history::commit_soft(
        paths,
        &format!("generation #{}: rollback (restored)", current.number),
    );
    Ok(current)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::ServiceConfig;
    use chrono::Utc;

    fn config_with(name: &str, domain: Option<&str>) -> BoxConfig {
        BoxConfig {
            services: vec![ServiceConfig {
                name: name.into(),
                template: "static-site".into(),
                params: json!({}),
                domain: domain.map(Into::into),
                public: false,
                created_at: Utc::now(),
            }],
        }
    }

    #[test]
    fn new_service_is_structural() {
        let current = BoxConfig::default();
        let req = DeployRequest::static_site("blog", Some("<h1/>".into()), None, None, false);
        assert_eq!(classify_deploy(&current, &req), ChangeKind::Structural);
    }

    #[test]
    fn content_edit_is_fast_path() {
        let current = config_with("blog", None);
        // Same template/domain/exposure, new body → content only.
        let req =
            DeployRequest::static_site("blog", Some("<h1>new</h1>".into()), None, None, false);
        assert_eq!(classify_deploy(&current, &req), ChangeKind::Content);
    }

    #[test]
    fn adding_a_domain_is_structural() {
        let current = config_with("blog", None);
        let req =
            DeployRequest::static_site("blog", None, None, Some("blog.example.com".into()), false);
        assert_eq!(classify_deploy(&current, &req), ChangeKind::Structural);
    }
}
