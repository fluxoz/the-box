//! Keeping a service in step with the repository it came from.
//!
//! This is the deploy loop's engine: link a service to a repository once, and
//! from then on the Box pulls — fetch, compare, deploy — until told otherwise.
//! Pulling rather than receiving webhooks is a deliberate reversal of how the
//! comparable products work, because a webhook needs the Box reachable from
//! the internet and a Box starts life behind a home router with nothing open.
//! A poll costs a little latency; it works from the first minute.
//!
//! Three decisions here are about credentials, and all have the same shape —
//! the token must never be somewhere it can be read back:
//!
//! - git authenticates through a header passed in the *environment*
//!   (`GIT_CONFIG_*`), never in the URL (which lands in `.git/config` and every
//!   error message) and never in argv (world-readable in `/proc`);
//! - the header is scoped to the forge's own URL prefix, so a submodule or
//!   redirect pointing anywhere else never sees it;
//! - what gets published is a clean checkout with no `.git` — serving a
//!   private repository's history to the internet would leak the whole thing.
//!
//! The checkout is deployed through [`crate::ops::deploy`] like any other
//! static site, so it inherits generations, rollback, health checks and the
//! config-repo history for free. A new commit is a content change, which stays
//! on the fast path — no OS rebuild, which matters on a Pi.

use std::fs;
use std::path::PathBuf;
use std::process::Command;
use std::time::Duration;

use anyhow::{bail, Context, Result};
use serde::Serialize;

use crate::config::{BoxConfig, RepoLink};
use crate::forge::GitAuth;
use crate::ops::DeployRequest;
use crate::paths::Paths;
use crate::store::Builder;

/// How often the poller looks for new commits. A fetch with nothing new is a
/// couple of packets, so this can be tight enough to feel like push.
pub const POLL_SECONDS: u64 = 60;

/// What one sync did.
#[derive(Debug, Clone, Serialize)]
#[serde(tag = "result", rename_all = "snake_case")]
pub enum SyncOutcome {
    /// The deployed commit is the branch tip; nothing happened.
    UpToDate { commit: String },
    /// A new commit was deployed as a new generation.
    Deployed {
        commit: String,
        previous: Option<String>,
        generation: u64,
    },
}

/// A subdirectory someone typed. It becomes part of a filesystem path, so the
/// rules are the same as for an uploaded file's name: relative, and no way to
/// climb out of the tree it is joined to.
pub fn validate_subdir(subdir: &str) -> Result<()> {
    let ok = !subdir.is_empty()
        && !subdir.starts_with('/')
        && !subdir.ends_with('/')
        && subdir
            .split('/')
            .all(|seg| !seg.is_empty() && seg != "." && seg != "..");
    if !ok {
        bail!("subdir {subdir:?} must be a relative path inside the repository, like \"site\" or \"docs/public\"");
    }
    Ok(())
}

/// The environment that authenticates one git invocation, built pure so the
/// tests can hold it up to the light. Empty when no auth applies.
pub fn auth_env(auth: Option<&GitAuth>) -> Vec<(String, String)> {
    let mut env = vec![
        // Never sit waiting for a username on a machine with no one at it.
        ("GIT_TERMINAL_PROMPT".into(), "0".into()),
    ];
    if let Some(a) = auth {
        env.push(("GIT_CONFIG_COUNT".into(), "1".into()));
        env.push((
            "GIT_CONFIG_KEY_0".into(),
            format!("http.{}.extraheader", a.url_prefix),
        ));
        env.push(("GIT_CONFIG_VALUE_0".into(), a.header.clone()));
    }
    env
}

/// The credential for a link, if it needs one. Local paths (used by tests, and
/// harmless as an escape hatch) fetch without auth; anything remote requires
/// its forge to be connected.
pub fn auth_for(paths: &Paths, link: &RepoLink) -> Result<Option<GitAuth>> {
    if link.clone_url.starts_with("file://") || link.clone_url.starts_with('/') {
        return Ok(None);
    }
    let forge = crate::forge::get(&link.forge)
        .with_context(|| format!("unknown forge {:?}", link.forge))?;
    let cfg = crate::forge::config_for(paths, &link.forge)?;
    let token = crate::forge::token(paths, &link.forge)?;
    Ok(Some(forge.git_auth(&cfg, &token)))
}

fn git(args: &[&str], env: &[(String, String)]) -> Result<()> {
    let mut cmd = Command::new("git");
    cmd.args(args);
    for (k, v) in env {
        cmd.env(k, v);
    }
    let out = cmd.output().context("running git")?;
    if !out.status.success() {
        bail!(
            "git {} failed: {}",
            args.first().copied().unwrap_or(""),
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    Ok(())
}

fn git_stdout(args: &[&str]) -> Result<String> {
    let out = Command::new("git").args(args).output().context("running git")?;
    if !out.status.success() {
        bail!(
            "git {} failed: {}",
            args.first().copied().unwrap_or(""),
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
}

/// Fetch the branch tip. Returns the commit id without touching the checkout,
/// so a poll that finds nothing new costs no disk churn at all.
pub fn fetch(paths: &Paths, service: &str, link: &RepoLink) -> Result<String> {
    let auth = auth_for(paths, link)?;
    let git_dir = paths.repo_git_dir(service);
    let gd = git_dir.to_string_lossy().to_string();
    if !git_dir.join("HEAD").exists() {
        fs::create_dir_all(&git_dir)?;
        git(&["init", "--bare", "--quiet", &gd], &[])?;
    }
    let env = auth_env(auth.as_ref());
    // Shallow: the Box deploys tips, it does not archive history. `--force`
    // because a rebased branch must not wedge the loop.
    git(
        &[
            "--git-dir", &gd,
            "fetch", "--depth", "1", "--no-tags", "--force", "--quiet",
            &link.clone_url,
            &link.branch,
        ],
        &env,
    )
    .with_context(|| format!("fetching {} ({})", link.repo, link.branch))?;
    git_stdout(&["--git-dir", &gd, "rev-parse", "FETCH_HEAD"])
}

/// Materialize a commit as a clean tree — no `.git`, nothing that is not in
/// the commit. The bare-repo-plus-explicit-work-tree pattern, with a private
/// index so nothing depends on or disturbs repository state.
pub fn checkout(paths: &Paths, service: &str, commit: &str) -> Result<PathBuf> {
    let git_dir = paths.repo_git_dir(service);
    let gd = git_dir.to_string_lossy().to_string();
    let tree = paths.repo_tree_dir(service);
    crate::util::remove_dir_all_forced(&tree)?;
    fs::create_dir_all(&tree)?;
    let wt = tree.to_string_lossy().to_string();
    let index = git_dir.join("pull-index");
    git(
        &["--git-dir", &gd, "--work-tree", &wt, "checkout", "--quiet", "-f", commit, "--", "."],
        &[("GIT_INDEX_FILE".into(), index.to_string_lossy().into_owned())],
    )
    .with_context(|| format!("checking out {commit}"))?;
    Ok(tree)
}

fn deployed_commit(paths: &Paths, service: &str) -> Option<String> {
    fs::read_to_string(paths.repo_marker(service))
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
}

/// Where the deployable content is, once a commit is checked out.
fn source_for(link: &RepoLink, tree: &std::path::Path) -> Result<PathBuf> {
    match &link.subdir {
        None => Ok(tree.to_path_buf()),
        Some(sub) => {
            validate_subdir(sub)?;
            let dir = crate::web::sites::resolve_safe(tree, sub)
                .with_context(|| format!("subdir {sub:?} escapes the repository"))?;
            if !dir.is_dir() {
                bail!(
                    "subdir {sub:?} does not exist in {} at the fetched commit",
                    link.repo
                );
            }
            Ok(dir)
        }
    }
}

/// One turn of the loop for one service: fetch, compare, deploy if new.
///
/// The deploy is [`crate::ops::deploy`] with the service's own shape and only
/// the source pointed at the fresh checkout — domain, visibility and the rest
/// stay exactly as configured, so a poll can never take a site off its domain.
pub fn sync(paths: &Paths, builder: &dyn Builder, name: &str, force: bool) -> Result<SyncOutcome> {
    let config = BoxConfig::load(paths)?;
    let svc = config
        .find(name)
        .with_context(|| format!("no service named {name:?}"))?;
    let link = svc
        .repo
        .clone()
        .with_context(|| format!("service {name:?} has no linked repository"))?;

    let commit = fetch(paths, name, &link)?;
    let previous = deployed_commit(paths, name);
    if !force && previous.as_deref() == Some(commit.as_str()) {
        return Ok(SyncOutcome::UpToDate { commit });
    }

    let tree = checkout(paths, name, &commit)?;
    let source = source_for(&link, &tree)?;

    let mut params = svc.params.clone();
    if let Some(obj) = params.as_object_mut() {
        obj.insert("source_path".into(), source.to_string_lossy().into());
        // A linked service's content comes from the repository, full stop.
        obj.remove("index_html");
    }
    let req = DeployRequest {
        name: name.to_string(),
        template: svc.template.clone(),
        params,
        domain: None,
        public: None,
        port: None,
    };
    let info = crate::ops::deploy(paths, builder, req)?;

    fs::create_dir_all(paths.repos_dir())?;
    fs::write(paths.repo_marker(name), &commit)?;
    Ok(SyncOutcome::Deployed {
        commit,
        previous,
        generation: info.number,
    })
}

/// Link a service to a repository and deploy it now.
///
/// The repository must already be shared with this Box — the lookup doubles as
/// the access check, and its failure message carries the fix. Everything that
/// can fail does so *before* the config is touched: a typo'd branch name must
/// not leave behind a service that never worked.
#[allow(clippy::too_many_arguments)]
pub fn link(
    paths: &Paths,
    builder: &dyn Builder,
    service: &str,
    forge_id: &str,
    repo_name: &str,
    branch: Option<String>,
    subdir: Option<String>,
    domain: Option<String>,
    public: bool,
) -> Result<(RepoLink, SyncOutcome)> {
    let forge = crate::forge::get(forge_id)
        .with_context(|| format!("unknown forge {forge_id:?}"))?;
    if let Some(sub) = &subdir {
        validate_subdir(sub)?;
    }

    // v1 is file trees. A repository that needs a build step is the next
    // increment (the sandboxed builder), and saying so beats a broken deploy.
    let config = BoxConfig::load(paths)?;
    if let Some(existing) = config.find(service) {
        if existing.template != "static-site" {
            bail!(
                "service {service:?} is a {} — only static-site services can be \
                 repo-linked for now",
                existing.template
            );
        }
    }

    let fcfg = crate::forge::config_for(paths, forge_id)?;
    let token = crate::forge::token(paths, forge_id)?;
    let repos = forge.list_repos(&token, &fcfg)?;
    let repo = repos
        .iter()
        .find(|r| r.full_name.eq_ignore_ascii_case(repo_name))
        .with_context(|| {
            let hint = forge
                .grant_more_url(&fcfg)
                .map(|u| format!(" To share it, send them {u}"))
                .unwrap_or_default();
            format!("{repo_name:?} is not among the repositories shared with this Box.{hint}")
        })?;

    let link = RepoLink {
        forge: forge_id.to_string(),
        repo: repo.full_name.clone(),
        clone_url: repo.clone_url.clone(),
        branch: branch.unwrap_or_else(|| repo.default_branch.clone()),
        subdir,
    };

    // Prove the fetch and the checkout before the config learns anything.
    let commit = fetch(paths, service, &link)?;
    let tree = checkout(paths, service, &commit)?;
    let source = source_for(&link, &tree)?;

    let req = DeployRequest::static_site(
        service,
        None,
        Some(source),
        domain,
        public,
    );
    let info = crate::ops::deploy(paths, builder, req)?;

    // deploy() saved the service; attach the link to it.
    let mut config = BoxConfig::load(paths)?;
    if let Some(svc) = config.services.iter_mut().find(|s| s.name == service) {
        svc.repo = Some(link.clone());
    }
    config.save(paths)?;

    fs::create_dir_all(paths.repos_dir())?;
    fs::write(paths.repo_marker(service), &commit)?;

    Ok((
        link,
        SyncOutcome::Deployed {
            commit,
            previous: None,
            generation: info.number,
        },
    ))
}

/// Forget the link. The service and its current content stay exactly as they
/// are — this stops future pulls, it does not undeploy anything.
pub fn unlink(paths: &Paths, service: &str) -> Result<()> {
    let mut config = BoxConfig::load(paths)?;
    let Some(svc) = config.services.iter_mut().find(|s| s.name == service) else {
        bail!("no service named {service:?}");
    };
    if svc.repo.take().is_none() {
        bail!("service {service:?} has no linked repository");
    }
    config.save(paths)?;
    let _ = crate::util::remove_dir_all_forced(&paths.repo_git_dir(service));
    let _ = crate::util::remove_dir_all_forced(&paths.repo_tree_dir(service));
    let _ = fs::remove_file(paths.repo_marker(service));
    Ok(())
}

/// The poller: one thread, one pass over the linked services every
/// [`POLL_SECONDS`]. Errors are logged and the loop continues — one broken
/// remote must not stall every other service's deploys.
pub fn spawn(state: crate::web::SharedState) {
    std::thread::Builder::new()
        .name("repo-pull".into())
        .spawn(move || loop {
            std::thread::sleep(Duration::from_secs(POLL_SECONDS));
            let linked: Vec<String> = match BoxConfig::load(&state.paths) {
                Ok(c) => c
                    .services
                    .iter()
                    .filter(|s| s.repo.is_some())
                    .map(|s| s.name.clone())
                    .collect(),
                Err(e) => {
                    tracing::warn!("pull: cannot read config: {e:#}");
                    continue;
                }
            };
            for name in linked {
                // The same lock every deploy path takes, so a poll can never
                // race an operator's own deploy on the generation directory.
                let _guard = state.apply_lock.lock().unwrap();
                match sync(&state.paths, state.builder.as_ref(), &name, false) {
                    Ok(SyncOutcome::Deployed { commit, generation, .. }) => {
                        tracing::info!("pull: {name}: deployed {commit} as generation #{generation}");
                    }
                    Ok(SyncOutcome::UpToDate { .. }) => {}
                    Err(e) => tracing::warn!("pull: {name}: {e:#}"),
                }
            }
        })
        .expect("spawning the repo poller");
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_subdir_cannot_climb_out_of_the_repository() {
        assert!(validate_subdir("site").is_ok());
        assert!(validate_subdir("docs/public").is_ok());
        for bad in ["", "/abs", "a/", "..", "a/../b", ".", "a//b"] {
            assert!(validate_subdir(bad).is_err(), "{bad:?} should be refused");
        }
    }

    #[test]
    fn the_credential_rides_in_the_environment_scoped_to_the_forge() {
        let auth = GitAuth::basic("https://github.com/", "x-access-token", "ghu_secret");
        let env = auth_env(Some(&auth));
        let get = |k: &str| {
            env.iter()
                .find(|(key, _)| key == k)
                .map(|(_, v)| v.as_str())
                .unwrap_or_default()
        };
        // Never interactive on a headless machine.
        assert_eq!(get("GIT_TERMINAL_PROMPT"), "0");
        // The header applies to github.com and to nothing else — a submodule
        // pointing at attacker.example must never receive it.
        assert_eq!(get("GIT_CONFIG_KEY_0"), "http.https://github.com/.extraheader");
        assert!(get("GIT_CONFIG_VALUE_0").starts_with("AUTHORIZATION: basic "));
        // And the token appears nowhere in plaintext.
        assert!(!get("GIT_CONFIG_VALUE_0").contains("ghu_secret"));

        // No auth, no header keys at all.
        assert!(auth_env(None).iter().all(|(k, _)| k == "GIT_TERMINAL_PROMPT"));
    }

    #[test]
    fn local_paths_need_no_forge_connection() {
        let tmp = tempfile::TempDir::new().unwrap();
        let paths = Paths::new(tmp.path().to_path_buf());
        paths.ensure().unwrap();
        let link = RepoLink {
            forge: "github".into(),
            repo: "local/test".into(),
            clone_url: "file:///somewhere/repo.git".into(),
            branch: "main".into(),
            subdir: None,
        };
        // No github account is connected on this fresh Box, and none is needed.
        assert!(auth_for(&paths, &link).unwrap().is_none());
    }
}
