//! High-level operations: the same code paths back the dashboard, the JSON
//! API, the CLI and the MCP tools. A service is a template + params; applying
//! builds a generation, health-checks it, and auto-rolls-back on failure.

use std::fs;
use std::path::PathBuf;

use anyhow::{bail, Context, Result};
use chrono::Utc;
use serde::Serialize;
use serde_json::{json, Value};

use crate::agecrypt;
use crate::catalog;
use crate::config::{validate_domain, validate_service_name, BoxConfig, ServiceConfig};
use crate::history;
use crate::manifest;
use crate::nixgen;
use crate::ostier;
use crate::paths::Paths;
use crate::ports;
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
    /// `None` means "leave whatever this service already has" — an agent that
    /// updates only params must not unpublish it. `Some("")` clears it.
    pub domain: Option<String>,
    /// `None` means "unchanged" (`false` for a service being created).
    pub public: Option<bool>,
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
            public: Some(public),
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
            public: Some(public),
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

/// Classify a deploy against the current config. New services, and changes to a
/// service's template, domain, visibility or exposure, are structural.
///
/// So is *any* param change to a process-backed service: a container's image,
/// env and volumes compile into the OS-tier module that runs it, so changing
/// them is a change to the system, not to a file tree. Only a file service's
/// content (a static site's pages) stays on the fast path.
///
/// Omitted fields follow the same merge rule as [`deploy`]: absent means
/// unchanged, and unchanged is never structural.
pub fn classify_deploy(current: &BoxConfig, req: &DeployRequest) -> ChangeKind {
    let Some(existing) = current.find(&req.name) else {
        return ChangeKind::Structural;
    };
    if existing.template != req.template {
        return ChangeKind::Structural;
    }

    let domain_changed = match req.domain.as_deref().map(str::trim) {
        None => false,
        Some("") => existing.domain.is_some(),
        Some(d) => existing.domain.as_deref() != Some(d.to_ascii_lowercase().as_str()),
    };
    let public_changed = req.public.is_some_and(|p| p != existing.public);
    if domain_changed || public_changed {
        return ChangeKind::Structural;
    }

    match templates::get(&req.template) {
        Some(t) => {
            let exposure_changed = t.exposure(&existing.params) != t.exposure(&req.params);
            let process_backed = t.exposure(&req.params).needs_port();
            if exposure_changed || (process_backed && existing.params != req.params) {
                ChangeKind::Structural
            } else {
                ChangeKind::Content
            }
        }
        None => ChangeKind::Structural,
    }
}

/// One uploaded file: a path relative to the site root, and its bytes.
pub struct UploadedFile {
    pub path: String,
    pub bytes: Vec<u8>,
}

/// Stage files for a service, to be deployed as its content.
///
/// This is how someone's own project gets onto the Box. Without it a multi-file
/// site could only arrive by `scp`, which needs an SSH key on the Box — so the
/// person whose agent just built them a site had no way to publish it, and
/// nothing in the docs said so.
///
/// `replace` empties the staging area first, so an upload is the whole site
/// rather than an accumulation of every version of it.
pub fn upload_files(
    paths: &Paths,
    service: &str,
    files: Vec<UploadedFile>,
    replace: bool,
) -> Result<(usize, u64)> {
    validate_service_name(service)?;
    let dir = paths.upload_dir(service);
    if replace {
        util::remove_dir_all_forced(&dir)?;
    }
    fs::create_dir_all(&dir)?;

    let mut count = 0usize;
    let mut bytes = 0u64;
    for file in files {
        // The client picks these paths. An absolute one is refused rather than
        // quietly reinterpreted as relative — "/etc/passwd" landing at
        // "<uploads>/etc/passwd" is safe but not what anyone meant, and a
        // surprise like that is worth an error.
        if file.path.starts_with('/') {
            bail!(
                "upload path {:?} must be relative to the site root (write \"index.html\", not \"/index.html\")",
                file.path
            );
        }
        // Otherwise checked the same way a request path for a served file is:
        // nothing may escape the tree.
        let target = crate::web::sites::resolve_safe(&dir, &file.path)
            .with_context(|| format!("refusing unsafe upload path {:?}", file.path))?;
        if let Some(parent) = target.parent() {
            fs::create_dir_all(parent)?;
        }
        bytes += file.bytes.len() as u64;
        count += 1;
        fs::write(&target, &file.bytes).with_context(|| format!("writing {}", target.display()))?;
    }
    Ok((count, bytes))
}

/// Where a service can actually be reached, and whether it is really running.
#[derive(Debug, Clone, Serialize)]
pub struct ServiceStatus {
    /// The URL that actually serves this service, if there is one. A file
    /// service is served by the platform proxy at `/sites/<name>/`; a process
    /// or container is reached by its domain, or on its loopback port. It used
    /// to report `/sites/<name>/` for everything, which 404s for anything that
    /// is not a file tree.
    pub url: Option<String>,
    /// `active` — in the current generation and, for anything that needs the
    /// system, actually applied to it. `pending` — declared but not yet built.
    /// `not-running` — built, but the system has not caught up, so nothing is
    /// running it yet.
    pub state: &'static str,
    /// Why, when the state is not `active`.
    pub note: Option<String>,
}

/// Report a service honestly: what URL reaches it and whether it is running.
pub fn service_status(
    paths: &Paths,
    service: &ServiceConfig,
    in_current_generation: bool,
) -> ServiceStatus {
    let exposure = templates::get(&service.template).map(|t| t.exposure(&service.params));
    let file_served = matches!(exposure, Some(templates::Exposure::Files) | None);

    let url = if file_served {
        Some(format!("/sites/{}/", service.name))
    } else {
        match (&service.domain, service.port) {
            (Some(d), _) => Some(format!("http://{d}/")),
            (None, Some(p)) => Some(format!("http://127.0.0.1:{p}/")),
            (None, None) => None,
        }
    };

    if !in_current_generation {
        return ServiceStatus {
            url,
            state: "pending",
            note: Some("declared, but not in the current generation yet".into()),
        };
    }
    // A file service is served by boxd itself, so being in the generation IS
    // being live. Anything process-backed needs the OS tier to have applied.
    if !file_served && ostier::is_pending(paths) {
        return ServiceStatus {
            url,
            state: "not-running",
            note: Some(
                ostier::pending_reason(paths)
                    .unwrap_or_else(|| "the system has not applied this change yet".into()),
            ),
        };
    }
    ServiceStatus {
        url,
        state: "active",
        note: None,
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
    let info = match apply(paths, builder) {
        Ok(info) => info,
        Err(e) => {
            // A build failure must not leave the failed service in box.toml.
            // The caller saved the config BEFORE building (it has to — the
            // build reads it), so without this the broken service persists and
            // every later deploy of anything re-renders the same failing
            // generation: one bad deploy wedges the whole Box until someone
            // deletes the service by hand.
            if let Some(prev) = &previous {
                if let Err(re) = rollback(paths, prev.number) {
                    bail!(
                        "build failed ({e:#}) and restoring generation #{} also failed: {re:#}",
                        prev.number
                    );
                }
                bail!("build failed — restored generation #{}: {e:#}", prev.number);
            }
            return Err(e);
        }
    };
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

    // Some params are syntax, not data: they land in generated Nix unquoted,
    // where escaping cannot help. Only the platform may write those.
    //
    // Stripped rather than refused, and stripped AFTER the preset merge (a
    // preset carries params of its own, so checking before it would look at the
    // wrong object). Refusing would turn any config that already carries one
    // into a service that can never be redeployed — publish and repo-sync both
    // replay stored params through here — and nixgen re-derives the real value
    // from disk on every render anyway, so dropping it loses nothing.
    if let Some(obj) = req.params.as_object_mut() {
        for key in templates::RESERVED_PARAMS {
            if obj.remove(*key).is_some() {
                tracing::warn!(
                    service = %req.name,
                    "ignoring {key}: that param is set by the platform, not by callers"
                );
            }
        }
    }

    let template = templates::get(&req.template)
        .with_context(|| format!("unknown template {:?}", req.template))?;
    template.validate(&req.params)?;

    template.materialize(&req.params, &paths.source_dir(&req.name))?;

    // Secret env values are age-encrypted into <data>/secrets/<name>-env.age and
    // stripped from the params, so no plaintext credential ever lands in the
    // config, the manifest, git, or the Nix store. agenix decrypts them at
    // runtime; nixgen ships the .age next to the service module.
    // A catalog preset declares the credentials its service needs. Asking a
    // person to invent a root password is how "change-me-please" ends up in
    // production, so any placeholder left untouched is replaced with a real
    // generated one before it is encrypted. The operator never has to know the
    // service's credential conventions to run it safely.
    let mut generated: Vec<(String, String)> = Vec::new();
    if let Some(obj) = req
        .params
        .get_mut("secret_env")
        .and_then(Value::as_object_mut)
    {
        for (key, value) in obj.iter_mut() {
            let weak = value
                .as_str()
                .map(|v| v.trim().is_empty() || is_placeholder(v))
                .unwrap_or(false);
            if weak && looks_secret(key) {
                let secret = generated_secret_matching(value.as_str().unwrap_or(""))?;
                generated.push((key.clone(), secret.clone()));
                *value = Value::String(secret);
            }
        }
    }
    // Keep a copy of anything WE invented, so the console can show it later.
    //
    // Service secrets are encrypted to the box's SSH host key and decrypted by
    // agenix as root; boxd cannot read them back, which is the right shape for
    // a credential the operator supplied. But a credential the platform
    // generated is one nobody has ever seen — without this it would be
    // unknowable, and you could never log in to the thing you just deployed.
    // The copy lives in boxd's own encrypted store (see crate::secrets), so it
    // is readable by the console, which already has full authority anyway.
    for (key, value) in &generated {
        let slot = format!(
            "service-{}-{}",
            req.name,
            key.to_ascii_lowercase().replace('_', "-")
        );
        if let Err(e) = crate::secrets::set(paths, &slot, value) {
            tracing::warn!("could not record generated credential {slot}: {e:#}");
        }
    }

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
        agecrypt::encrypt(
            &env_file,
            &recipients,
            &dir.join(format!("{}-env.age", req.name)),
        )?;
    }
    if let Some(obj) = req.params.as_object_mut() {
        obj.remove("secret_env");
    }

    let mut config = BoxConfig::load(paths)?;

    // Classify BEFORE the config is mutated: whether this needs the OS tier is
    // a question about the change, and after the save there is nothing to
    // compare against.
    let kind = classify_deploy(&config, &req);

    // Merge, don't overwrite: an omitted domain or `public` keeps what the
    // service already has, so `deploy(name, template, params)` — the shape the
    // MCP tool documents for an update — can't silently take a service off its
    // domain. An explicitly empty domain clears it.
    let existing = config.find(&req.name);
    let existing_domain = existing.and_then(|s| s.domain.clone());
    let existing_public = existing.map(|s| s.public).unwrap_or(false);

    let domain = match req.domain.take() {
        None => existing_domain,
        Some(d) if d.trim().is_empty() => None,
        Some(d) => {
            let d = d.trim().to_ascii_lowercase();
            validate_domain(&d)?;
            if let Some(other) = config
                .services
                .iter()
                .find(|s| s.name != req.name && s.domain.as_deref() == Some(d.as_str()))
            {
                bail!("domain {d:?} is already used by service {:?}", other.name);
            }
            Some(d)
        }
    };
    req.domain = domain;
    let public = req.public.unwrap_or(existing_public);

    // Resolve the service's port through the central allocator/validator, so an
    // agent or a person gets the same rules: honor an explicit validated
    // request, keep an existing allocation stable, or assign a free one. File
    // services (static-site) take no port.
    let port = if template.exposure(&req.params).needs_port() {
        // Include THIS service's current allocation: that is how `resolve`
        // recognizes a redeploy and keeps the port stable. Filtering it out
        // made every redeploy re-allocate from scratch, so a service could
        // silently move to a port freed by a deleted neighbour while whatever
        // pointed at the old one broke. `validate` already allows a service to
        // collide with itself.
        let in_use: std::collections::BTreeMap<u16, String> = config
            .services
            .iter()
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
            existing.public = public;
            existing.port = port;
        }
        None => config.services.push(ServiceConfig {
            name: req.name.clone(),
            template: req.template.clone(),
            params: req.params.clone(),
            domain: req.domain.clone(),
            public,
            port,
            // A link is attached by crate::pull AFTER the first deploy; and on
            // updates the arm above deliberately leaves `repo` alone, so a
            // redeploy never silently severs a service from its repository.
            repo: None,
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
    // In ops, not in the callers: every door (console, MCP, webhook sync,
    // CLI) writes the same journal, or "what changed?" answers differently
    // depending on who asked.
    crate::journal::record(
        paths,
        "deploy",
        format!(
            "deployed {} ({}); generation #{}",
            req.name, req.template, info.number
        ),
    );

    // The second speed. The fast path above builds the generation (content +
    // manifest + modules); a structural change also has to be applied to the
    // running system, or a container is declared and never started. This is
    // the dispatch `classify_deploy` was written for.
    if kind == ChangeKind::Structural {
        match ostier::request_apply(paths) {
            ostier::ApplyOutcome::Applied | ostier::ApplyOutcome::NotABox => {}
            ostier::ApplyOutcome::Failed(detail) => {
                bail!(
                    "generation #{} was built, but applying it to the system failed, so {} is not running yet \
                     (the system rolled itself back): {detail}",
                    info.number,
                    req.name
                );
            }
        }
    }
    crate::journal::record(
        paths,
        "deploy",
        format!("deployed {} as generation #{}", req.name, info.number),
    );
    Ok(info)
}

/// Make an existing service reachable from the internet, or stop it being so.
///
/// A separate verb from `deploy` because it is a separate decision, and the one
/// an agent is most likely to get wrong by accident: this is the only thing
/// that puts something in front of the whole world, so it says what it did and
/// where the thing now answers.
pub fn publish(
    paths: &Paths,
    builder: &dyn Builder,
    name: &str,
    public: bool,
    domain: Option<String>,
    runtime_address: Option<&str>,
) -> Result<PublishOutcome> {
    let config = BoxConfig::load(paths)?;
    let service = config
        .find(name)
        .with_context(|| format!("no service named {name:?}"))?
        .clone();

    // Everything else about the service stays exactly as it is.
    let info = deploy(
        paths,
        builder,
        DeployRequest {
            name: service.name.clone(),
            template: service.template.clone(),
            params: service.params.clone(),
            domain,
            public: Some(public),
            port: service.port,
        },
    )?;

    // What to tell them: the address on the internet if there is one, and
    // otherwise why there isn't — "published" with no way in is the confusing
    // state, so it is the one that gets an explanation.
    let config = BoxConfig::load(paths)?;
    let ingress = config.ingress.as_ref();
    let (url, note) = match (public, ingress) {
        (false, _) => (
            None,
            Some(format!("{name} is no longer reachable from the internet.")),
        ),
        (true, Some(i)) if i.enabled => match crate::ingress::get(&i.provider) {
            Some(p) => match p.url_for(i, name, runtime_address) {
                Some(url) => (Some(url), None),
                None => (
                    None,
                    Some("published, but the way in has not been given its address yet — check back in a moment.".into()),
                ),
            },
            None => (None, Some(format!("unknown way in: {:?}", i.provider))),
        },
        (true, _) => (
            None,
            Some(
                "published, but this Box has no way in from the internet yet. Ask for ingress_options \
                 and set one up — until then it is reachable only on your own network."
                    .into(),
            ),
        ),
    };
    Ok(PublishOutcome {
        generation: info,
        url,
        note,
    })
}

pub struct PublishOutcome {
    pub generation: GenerationInfo,
    /// Where it answers on the internet, when there is such a place.
    pub url: Option<String>,
    /// Why there isn't, when there isn't.
    pub note: Option<String>,
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
    // A repo-linked service leaves a clone behind; deleting the service means
    // deleting the copy of someone's repository too.
    let _ = util::remove_dir_all_forced(&paths.repo_git_dir(name));
    let _ = util::remove_dir_all_forced(&paths.repo_tree_dir(name));
    let _ = std::fs::remove_file(paths.repo_marker(name));
    let info = apply_checked(paths, builder, &default_health)?;
    history::commit_soft(
        paths,
        &format!("generation #{}: delete {}", info.number, name),
    );
    crate::journal::record(
        paths,
        "delete",
        format!("deleted {name}; generation #{}", info.number),
    );
    // Deleting is always structural: the config no longer has the service, but
    // until the OS tier catches up, nginx keeps serving it — a "deleted" site
    // that still answers, found live on a Box where the deletion also needed to
    // repair the web server and nothing ever asked the system to change.
    match ostier::request_apply(paths) {
        ostier::ApplyOutcome::Applied | ostier::ApplyOutcome::NotABox => {}
        ostier::ApplyOutcome::Failed(detail) => {
            bail!(
                "generation #{} was built without {}, but applying it to the system failed \
                 (the system rolled itself back), so it may still be served: {detail}",
                info.number,
                name
            );
        }
    }
    crate::journal::record(
        paths,
        "service",
        format!(
            "removed the service {name}; generation #{} is live without it",
            info.number
        ),
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

/// Does this secret name hold something that must be strong (as opposed to a
/// username or a bucket name, which a preset legitimately hard-codes)?
fn looks_secret(key: &str) -> bool {
    let k = key.to_ascii_uppercase();
    ["PASSWORD", "SECRET", "TOKEN", "KEY", "PASS"]
        .iter()
        .any(|needle| k.contains(needle))
}

/// Catalog presets ship obvious stand-ins so the file reads clearly. Treat
/// those as "not set" rather than as a credential.
/// A generated credential that respects a length the app demands.
///
/// Some apps refuse to boot unless a key is EXACTLY n characters (Laravel's
/// APP_KEY is 32, and Firefly III inherits that). A preset author encodes
/// that requirement the only way a TOML placeholder can: by being exactly
/// that long. So when a placeholder is long enough to be deliberate, the
/// generated secret matches its length rather than the generator's natural
/// one. Found by deploying every catalog preset and reading what the app
/// actually said: "Unsupported cipher or incorrect key length", from a key
/// the platform itself had substituted.
fn generated_secret_matching(placeholder: &str) -> Result<String> {
    let want = placeholder.trim().chars().count();
    let mut s = generated_secret()?;
    // Below this, a placeholder is a shrug ("change-me-please"), not a
    // specification, and the generator's own strength wins.
    if want >= 24 && s.chars().count() != want {
        while s.chars().count() < want {
            s.push('-');
            s.push_str(&generated_secret()?);
        }
        s = s.chars().take(want).collect();
    }
    Ok(s)
}

fn is_placeholder(value: &str) -> bool {
    let v = value.to_ascii_lowercase();
    v.contains("change") || v.contains("replace") || v.contains("example") || v == "password"
}

/// A generated credential, in the shape a person can actually deal with:
/// five short words and two digits, `crisp-walnut-meadow-gusto-plaza-47`.
///
/// Hex is stronger per character and useless in practice — nobody reads 48 hex
/// digits off a screen and types them into a phone correctly. Five words from
/// the embedded list carry ~51.7 bits, plus ~6.6 from the digits: comfortably
/// beyond anything a person would have chosen, while staying readable aloud.
/// Lowercase letters, digits and hyphens only, so it is safe unquoted in an env
/// file, a URL or a shell.
fn generated_secret() -> Result<String> {
    let words = crate::words::WORDS;
    let mut parts: Vec<String> = Vec::with_capacity(6);
    for _ in 0..5 {
        parts.push(words[uniform_below(words.len() as u32)? as usize].to_string());
    }
    parts.push(format!("{:02}", uniform_below(100)?));
    Ok(parts.join("-"))
}

/// A uniform value in `0..n`, drawn from the system CSPRNG.
///
/// Rejection sampling, not `% n`: the modulo shortcut skews toward low indices
/// whenever n does not divide the range, which for a 1295-word list would make
/// some words measurably likelier than others and quietly cost entropy.
fn uniform_below(n: u32) -> Result<u32> {
    use std::io::Read;
    let mut file = std::fs::File::open("/dev/urandom").context("opening /dev/urandom")?;
    let limit = u32::MAX - (u32::MAX % n) - 1;
    loop {
        let mut buf = [0u8; 4];
        file.read_exact(&mut buf).context("reading /dev/urandom")?;
        let value = u32::from_le_bytes(buf);
        if value <= limit {
            return Ok(value % n);
        }
    }
}

/// Re-key every `.age` directly in `dir` (non-recursive) to `recipients`,
/// decrypting with `identity`. A missing dir is fine (no secrets of that kind).
fn rekey_age_dir(
    dir: &std::path::Path,
    identity: &std::path::Path,
    recipients: &[String],
) -> Result<()> {
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
    // A rollback run as root (over SSH, or by the root os-apply unit) would
    // otherwise leave this tree owned by root, and the next deploy from the
    // console fails with a bare "Permission denied".
    util::chown_tree_like(&paths.data_dir, &paths.sources_dir());
    history::commit_soft(
        paths,
        &format!(
            "rolled back: generation #{} is running again",
            current.number
        ),
    );
    crate::journal::record(
        paths,
        "rollback",
        format!("rolled back to generation #{}", current.number),
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
                repo: None,
                created_at: Utc::now(),
            }],
            ..Default::default()
        }
    }

    /// `secret_env_file` becomes an unquoted Nix path in a module that gets
    /// built and activated as root. Only nixgen may set it, so a caller's copy
    /// is dropped before it can be stored and later rendered.
    #[test]
    fn deploy_strips_platform_only_params() {
        let tmp = tempfile::TempDir::new().unwrap();
        let paths = Paths::new(tmp.path().to_path_buf());
        paths.ensure().unwrap();
        let builder = crate::store::local::LocalBuilder::new(&paths);

        deploy(
            &paths,
            &builder,
            DeployRequest {
                name: "web".into(),
                template: "static-site".into(),
                params: json!({
                    "index_html": "hi",
                    "secret_env_file": "null; }; systemd.services.pwn = {}; x = { y = null",
                }),
                domain: None,
                public: None,
                port: None,
            },
        )
        .expect("a poisoned param is dropped, not a hard failure");

        // The dangerous value must not survive into the stored config, which is
        // what every later render (including the root OS-tier one) reads.
        let config = BoxConfig::load(&paths).unwrap();
        let svc = config.find("web").expect("service was created");
        assert!(
            svc.params.get("secret_env_file").is_none(),
            "the platform-only param must not be stored: {:?}",
            svc.params
        );
    }

    /// Nobody should have to invent a root password to run a database, and a
    /// preset's stand-in must never reach a real service.
    #[test]
    fn placeholder_credentials_are_replaced_with_generated_ones() {
        assert!(is_placeholder("change-me-please"));
        // An app that demands an exact key length gets one: the preset's
        // placeholder length is the specification.
        let exact = generated_secret_matching("change-me-to-32-chars-exactly-ok").unwrap();
        assert_eq!(exact.chars().count(), 32, "{exact}");
        // A short shrug of a placeholder does not weaken the generator.
        let shrug = generated_secret_matching("change-me-please").unwrap();
        assert!(shrug.chars().count() >= 24, "{shrug}");
        assert!(
            exact.chars().all(|c| c.is_ascii_alphanumeric() || c == '-'),
            "{exact}"
        );
        assert!(is_placeholder("REPLACE_ME"));
        assert!(is_placeholder("example"));
        assert!(!is_placeholder("s7f3a91c2b"));

        // Only things that must be strong are generated; a preset may hard-code
        // a username or a bucket name on purpose.
        assert!(looks_secret("MINIO_ROOT_PASSWORD"));
        assert!(looks_secret("POSTGRES_PASSWORD"));
        assert!(looks_secret("API_TOKEN"));
        assert!(looks_secret("SECRET_KEY"));
        assert!(!looks_secret("MINIO_ROOT_USER"));
        assert!(!looks_secret("POSTGRES_DB"));

        let a = generated_secret().unwrap();
        let b = generated_secret().unwrap();
        assert_ne!(a, b, "must not be deterministic");

        // Readable: five words and a two-digit number, joined by hyphens.
        let parts: Vec<&str> = a.split('-').collect();
        assert_eq!(parts.len(), 6, "got {a}");
        assert!(
            parts[..5]
                .iter()
                .all(|w| w.chars().all(|c| c.is_ascii_lowercase())),
            "got {a}"
        );
        assert!(parts[5].len() == 2 && parts[5].chars().all(|c| c.is_ascii_digit()));
        // Safe unquoted in an env file, a URL or a shell.
        assert!(a
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-'));
        assert!(a.len() >= 20, "too short to be a credential: {a}");
    }

    /// The draw must be uniform. `% n` would bias toward low indices and
    /// quietly cost entropy on a list whose length is not a power of two.
    #[test]
    fn word_choice_is_uniform_enough() {
        use std::collections::HashMap;
        let mut counts: HashMap<String, usize> = HashMap::new();
        for _ in 0..400 {
            for w in generated_secret().unwrap().split('-').take(5) {
                *counts.entry(w.to_string()).or_default() += 1;
            }
        }
        // 2000 draws from 1295 words: a healthy spread, and nothing dominating.
        assert!(counts.len() > 900, "only {} distinct words", counts.len());
        let worst = counts.values().max().copied().unwrap_or(0);
        assert!(worst < 15, "one word came up {worst} times");
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
