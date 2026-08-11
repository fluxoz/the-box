//! High-level operations: the same code paths back the dashboard, the JSON
//! API, the CLI and the MCP tools. A service is a template + params; applying
//! builds a generation, health-checks it, and auto-rolls-back on failure.

use std::fs;
use std::path::PathBuf;

use anyhow::{bail, Context, Result};
use chrono::Utc;
use serde_json::{json, Value};

use crate::agecrypt;
use crate::catalog;
use crate::config::{validate_domain, validate_service_name, BoxConfig, ServiceConfig};
use crate::ports;
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
    /// An explicit port request for a process-backed service. `None` lets the
    /// platform allocate one. Ignored (and rejected if set) for file services.
    pub port: Option<u16>,
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
            port: None,
        }
    }

    /// Convenience constructor for the reverse-proxied-app template.
    pub fn app(
        name: impl Into<String>,
        command: impl Into<String>,
        domain: Option<String>,
        port: Option<u16>,
        public: bool,
    ) -> Self {
        Self {
            name: name.into(),
            template: "reverse-proxied-app".into(),
            params: json!({ "command": command.into() }),
            domain,
            public,
            port,
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

    // A catalog preset (e.g. "postgres") resolves to its base primitive with the
    // preset's defaults, under any params the caller passed.
    if let Some(entry) = catalog::for_data_dir(&paths.data_dir).get(&req.template) {
        let (base, params) = catalog::resolve(entry, &req.params);
        req.template = base;
        req.params = params;
    }

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

    // Secret env values are age-encrypted into <data>/secrets/<name>-env.age and
    // stripped from the params, so no plaintext credential ever lands in the
    // config, the manifest, git, or the Nix store. agenix decrypts them at
    // runtime; nixgen ships the .age next to the service module.
    if let Some(secret_env) = req
        .params
        .get("secret_env")
        .and_then(Value::as_object)
        .filter(|o| !o.is_empty())
        .cloned()
    {
        let env_file: String = secret_env
            .iter()
            .filter_map(|(k, v)| v.as_str().map(|val| format!("{k}={val}\n")))
            .collect();
        let recipients = agecrypt::recipients()?;
        let dir = paths.data_dir.join("secrets");
        fs::create_dir_all(&dir)?;
        agecrypt::encrypt(&env_file, &recipients, &dir.join(format!("{}-env.age", req.name)))?;
    }
    if let Some(obj) = req.params.as_object_mut() {
        obj.remove("secret_env");
    }

    let mut config = BoxConfig::load(paths)?;

    // Resolve the service's port through the central allocator/validator, so an
    // agent or a person gets the same rules: honor an explicit validated
    // request, keep an existing allocation stable, or assign a free one. File
    // services (static-site) take no port.
    let port = if template.exposure(&req.params).needs_port() {
        let in_use: std::collections::BTreeMap<u16, String> = config
            .services
            .iter()
            .filter(|s| s.name != req.name)
            .filter_map(|s| s.port.map(|p| (p, s.name.clone())))
            .collect();
        Some(ports::resolve(req.port, &in_use, &req.name)?)
    } else {
        if req.port.is_some() {
            bail!(
                "template {:?} does not use a port (it is served by the platform proxy)",
                req.template
            );
        }
        None
    };

    match config.services.iter_mut().find(|s| s.name == req.name) {
        Some(existing) => {
            existing.template = req.template.clone();
            existing.params = req.params.clone();
            existing.domain = req.domain.clone();
            existing.public = req.public;
            existing.port = port;
        }
        None => config.services.push(ServiceConfig {
            name: req.name.clone(),
            template: req.template.clone(),
            params: req.params.clone(),
            domain: req.domain.clone(),
            public: req.public,
            port,
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

/// Recreate a box from its config repo — the second half of destroy-and-recreate.
///
/// Clones the config + encrypted secrets from `repo_url`, then re-keys every
/// secret: the cloned `.age` files are readable by the operator (who kept their
/// key) but not by this fresh box, so we decrypt each with the operator
/// `identity` and re-encrypt to `[this box's host key + operator]`. After that
/// the box decrypts them unattended forever, exactly as if it had deployed them
/// itself. Finally we build and switch to the restored generation.
pub fn restore(
    paths: &Paths,
    builder: &dyn Builder,
    repo_url: &str,
    identity: &std::path::Path,
) -> Result<GenerationInfo> {
    paths.ensure()?;
    history::fetch_checkout(paths, repo_url)
        .with_context(|| format!("cloning config repo {repo_url}"))?;

    let secrets_dir = paths.data_dir.join("secrets");
    // Service secrets (parent dir) re-key to [host + operator] — agenix decrypts
    // them at boot with the host key; boxd never holds a key for them.
    rekey_age_dir(&secrets_dir, identity, &agecrypt::recipients()?)?;
    // Operational secrets (op/ subdir) re-key to [host + operator + box identity]
    // so this box's boxd can read them unattended (backup password, tokens).
    rekey_age_dir(
        &secrets_dir.join("op"),
        identity,
        &agecrypt::local_recipients(paths)?,
    )?;

    // Keep pushing future generations back to the same repo.
    history::set_remote(paths, Some(repo_url))?;

    let info = apply_checked(paths, builder, &default_health)?;
    history::commit_soft(
        paths,
        &format!("generation #{}: restore from {repo_url}", info.number),
    );
    Ok(info)
}

/// Re-key every `.age` directly in `dir` (non-recursive) to `recipients`,
/// decrypting with `identity`. A missing dir is fine (no secrets of that kind).
fn rekey_age_dir(dir: &std::path::Path, identity: &std::path::Path, recipients: &[String]) -> Result<()> {
    if !dir.is_dir() {
        return Ok(());
    }
    for entry in fs::read_dir(dir)? {
        let path = entry?.path();
        if path.is_file() && path.extension().and_then(|e| e.to_str()) == Some("age") {
            agecrypt::rekey(&path, identity, recipients)
                .with_context(|| format!("re-keying secret {}", path.display()))?;
        }
    }
    Ok(())
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
                port: None,
                created_at: Utc::now(),
            }],
            ..Default::default()
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
