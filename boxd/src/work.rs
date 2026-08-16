//! The overnight shift: a coding agent runs ON the Box, against a linked
//! repository, inside the same hardened sandbox the build step uses — and
//! the human wakes up to a pull request.
//!
//! The leash is structural, not behavioral. The agent's world is one scratch
//! checkout mounted at /work and a cache volume; the container is read-only
//! elsewhere, capability-dropped, and knows exactly one secret (the model
//! API key the owner stored for this purpose). Box credentials never enter
//! the container: git push and the PR happen on the host afterwards, with
//! the forge plumbing every deploy already uses. If the agent goes feral,
//! the blast radius is a branch named work/... that nobody has to merge.

use anyhow::{bail, Context, Result};
use std::process::Command;

/// Where the owner's model API key lives (encrypted like every secret).
pub const API_KEY_SECRET: &str = "work-api-key";
/// Overnight is the point: one hour of wall clock per phase.
const WORK_TIMEOUT_SECS: u64 = 3600;
/// The agent, installed into the cache volume at run time (the container
/// filesystem is read-only; /cache persists across runs so this is fast
/// after the first).
const AGENT_PKG: &str = "@anthropic-ai/claude-code";

/// Kick off a run as a persistent job; returns the job id to poll.
pub fn start(state: &crate::web::SharedState, service: &str, prompt: &str) -> Result<String> {
    let paths = state.paths.clone();
    crate::secrets::get(&paths, API_KEY_SECRET)?
        .context("no agent credentials stored — work_configure saves the API key first")?;
    if matches!(
        crate::build::BuildExec::detect(),
        crate::build::BuildExec::Unavailable
    ) {
        bail!(
            "this machine cannot run work: it has no sandbox. A managed Box \
             ships one; dev machines don't."
        );
    }
    let config = crate::config::BoxConfig::load(&paths)?;
    let svc = config
        .services
        .iter()
        .find(|s| s.name == service)
        .with_context(|| format!("no service named {service:?}"))?;
    let link = svc
        .repo
        .clone()
        .with_context(|| format!("service {service:?} has no linked repository to work on"))?;

    let label = format!(
        "agent working on {service}: {}",
        prompt.chars().take(60).collect::<String>()
    );
    let service = service.to_string();
    let prompt = prompt.to_string();
    let id = state
        .jobs
        .start("work", label, service.clone(), move |progress| {
            run(&paths, &service, &link, &prompt, progress)
        });
    Ok(id)
}

fn run(
    paths: &crate::paths::Paths,
    service: &str,
    link: &crate::config::RepoLink,
    prompt: &str,
    progress: &crate::jobs::Progress,
) -> Result<String> {
    // A work checkout gets its own mirror key so it never races the deploy
    // checkout of the same service.
    let wname = format!("{service}--work");
    progress.phase("fetching the repository");
    let commit = crate::pull::fetch(paths, &wname, link)?;
    let tree = crate::pull::checkout(paths, &wname, &commit)?;
    let branch = format!("work/{}", &commit[..commit.len().min(8)]);

    // The sandbox: same image, same walls as a build. Phase 1 puts the agent
    // on the cache volume (network for npm); phase 2 lets it work (network
    // for the model API), with the one secret it is entitled to.
    let exec = crate::build::BuildExec::detect();
    let image = match &exec {
        crate::build::BuildExec::Podman { image_tar } => crate::build::ensure_image(image_tar)?,
        _ => bail!("work needs the podman sandbox"),
    };
    let cache = paths.build_cache_dir(&wname);
    std::fs::create_dir_all(cache.join("home"))?;
    let key =
        crate::secrets::get(paths, API_KEY_SECRET)?.context("the work API key vanished mid-run")?;

    progress.phase("installing the agent");
    let install = crate::build::phase_args(
        &image,
        &tree,
        &cache,
        None,
        true,
        &format!("npm install --prefix /cache/agent {AGENT_PKG}"),
        &crate::build::PhaseOpts {
            timeout_secs: WORK_TIMEOUT_SECS,
            extra_env: Vec::new(),
        },
    );
    podman(&install, "installing the agent")?;

    progress.phase("the agent is working");
    let work = crate::build::phase_args(
        &image,
        &tree,
        &cache,
        None,
        true,
        "/cache/agent/node_modules/.bin/claude -p \"$WORK_PROMPT\" --dangerously-skip-permissions",
        &crate::build::PhaseOpts {
            timeout_secs: WORK_TIMEOUT_SECS,
            extra_env: vec![
                ("ANTHROPIC_API_KEY".into(), key),
                ("WORK_PROMPT".into(), prompt.to_string()),
            ],
        },
    );
    podman(&work, "running the agent")?;

    // Back on the host: did it actually change anything?
    progress.phase("collecting the result");
    let status = git_out(&tree, &["status", "--porcelain"], &[])?;
    if status.trim().is_empty() {
        crate::journal::record(
            paths,
            "work",
            format!("the agent looked at {service} ({prompt}) and changed nothing"),
        );
        return Ok("the agent finished without making changes".into());
    }

    let auth = crate::pull::auth_for(paths, link)?;
    let env = crate::pull::auth_env(auth.as_ref());
    git(&tree, &["checkout", "-B", &branch], &[])?;
    git(&tree, &["add", "-A"], &[])?;
    git(
        &tree,
        &[
            "-c",
            "user.name=the box",
            "-c",
            "user.email=work@thebox.build",
            "commit",
            "-m",
            &format!("work: {prompt}"),
        ],
        &[],
    )?;
    git(
        &tree,
        &["push", "origin", &format!("HEAD:refs/heads/{branch}")],
        &env,
    )?;

    // The handoff. GitHub gets a PR; anything else gets the branch and a
    // journal line saying where to look.
    let outcome = if link.forge == "github" {
        let token = crate::forge::token(paths, &link.forge)?;
        match crate::ghapi::create_pull_request(
            &token,
            &link.repo,
            &branch,
            &link.branch,
            &format!("work: {prompt}"),
            &format!(
                "Overnight run by this repository's Box.\n\nPrompt: {prompt}\n\n\
                 Review like any contribution; nothing merges on the Box's say-so."
            ),
        ) {
            Ok(url) => format!("branch {branch} pushed; PR: {url}"),
            // The App may lack PR permission; the branch still landed.
            Err(e) => format!("branch {branch} pushed; PR not opened ({e})"),
        }
    } else {
        format!("branch {branch} pushed; open the merge request from there")
    };
    crate::journal::record(
        paths,
        "work",
        format!("worked on {service}: {prompt} — {outcome}"),
    );
    Ok(outcome)
}

fn podman(args: &[String], what: &str) -> Result<()> {
    let out = Command::new("podman")
        .args(args)
        .output()
        .with_context(|| format!("{what}: running podman"))?;
    if !out.status.success() {
        bail!(
            "{what} failed ({}): {}",
            out.status,
            String::from_utf8_lossy(&out.stderr)
                .lines()
                .rev()
                .take(6)
                .collect::<Vec<_>>()
                .join(" | ")
        );
    }
    Ok(())
}

fn git_cmd(
    tree: &std::path::Path,
    args: &[&str],
    env: &[(String, String)],
) -> Result<std::process::Output> {
    let mut cmd = Command::new("git");
    cmd.arg("-C").arg(tree).args(args);
    for (k, v) in env {
        cmd.env(k, v);
    }
    cmd.output().context("running git")
}

fn git(tree: &std::path::Path, args: &[&str], env: &[(String, String)]) -> Result<()> {
    let out = git_cmd(tree, args, env)?;
    if !out.status.success() {
        bail!(
            "git {} failed: {}",
            args.first().copied().unwrap_or(""),
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    Ok(())
}

fn git_out(tree: &std::path::Path, args: &[&str], env: &[(String, String)]) -> Result<String> {
    let out = git_cmd(tree, args, env)?;
    if !out.status.success() {
        bail!(
            "git {} failed: {}",
            args.first().copied().unwrap_or(""),
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    Ok(String::from_utf8_lossy(&out.stdout).to_string())
}
