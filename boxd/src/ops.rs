//! High-level operations: the same code paths back the dashboard, the JSON
//! API, the CLI and (later) the MCP tools.

use std::fs;
use std::path::PathBuf;

use anyhow::{bail, Context, Result};
use chrono::Utc;

use crate::config::{validate_domain, validate_service_name, BoxConfig, ServiceConfig, Template};
use crate::manifest;
use crate::nixgen;
use crate::paths::Paths;
use crate::store::{self, Builder, GenerationInfo};
use crate::util;

pub const DEFAULT_INDEX: &str = r#"<!doctype html>
<html lang="en">
<head>
  <meta charset="utf-8">
  <meta name="viewport" content="width=device-width, initial-scale=1">
  <title>Hello from The Box</title>
</head>
<body style="font-family: system-ui, sans-serif; display: grid; place-items: center; min-height: 100vh; margin: 0;">
  <div style="text-align: center;">
    <h1>&#128230; It works!</h1>
    <p>This site is served by The Box.</p>
  </div>
</body>
</html>
"#;

#[derive(Debug, Clone, Default)]
pub struct DeployRequest {
    pub name: String,
    pub domain: Option<String>,
    pub public: bool,
    /// Inline content for a single-page deploy (index.html).
    pub index_html: Option<String>,
    /// Alternative: copy an existing directory as the site root. Wins over
    /// `index_html` when both are given.
    pub source_path: Option<PathBuf>,
}

/// Build the current declarative config into a new generation and switch to
/// it atomically.
pub fn apply(paths: &Paths, builder: &dyn Builder) -> Result<GenerationInfo> {
    let config = BoxConfig::load(paths)?;
    let gensrc = nixgen::write_gensrc(paths, &config)?;
    let output = builder.build(&gensrc)?;
    store::record_and_switch(paths, &output)
}

/// Create or update a static-site service, then apply.
pub fn deploy(
    paths: &Paths,
    builder: &dyn Builder,
    mut req: DeployRequest,
) -> Result<GenerationInfo> {
    validate_service_name(&req.name)?;
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

    let source_dir = paths.source_dir(&req.name);
    if let Some(from) = &req.source_path {
        let from = fs::canonicalize(from)
            .with_context(|| format!("resolving source path {}", from.display()))?;
        if !from.is_dir() {
            bail!("source path {} is not a directory", from.display());
        }
        util::remove_dir_all_forced(&source_dir)?;
        util::copy_dir_recursive(&from, &source_dir)?;
    } else {
        let html = req.index_html.as_deref().unwrap_or(DEFAULT_INDEX);
        util::remove_dir_all_forced(&source_dir)?;
        fs::create_dir_all(&source_dir)?;
        fs::write(source_dir.join("index.html"), html)?;
    }

    let mut config = BoxConfig::load(paths)?;
    match config.services.iter_mut().find(|s| s.name == req.name) {
        Some(existing) => {
            existing.domain = req.domain;
            existing.public = req.public;
        }
        None => config.services.push(ServiceConfig {
            name: req.name.clone(),
            template: Template::StaticSite,
            domain: req.domain,
            public: req.public,
            created_at: Utc::now(),
        }),
    }
    config.save(paths)?;
    apply(paths, builder)
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
    apply(paths, builder)
}

/// Switch to an existing generation and restore the declarative state
/// (config and sources) that generation was built from, so a later apply
/// starts from the rolled-back world, not the abandoned one.
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
    Ok(current)
}
