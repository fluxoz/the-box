//! The sandboxed build step: a repository that is not yet a file tree becomes
//! one, inside a container its code cannot climb out of.
//!
//! The security posture, decided before any of this was written (PLAN §1):
//! both current classes of container escape are triggered by
//! attacker-controlled build *contexts*, so the user's repository is data on a
//! bind mount handed to a builder image the platform ships — never
//! `podman build` on something they wrote. Network cannot be dropped part-way
//! through a run, so a build is two phases sharing a cache volume: install
//! with the network, build with `--network=none`. Rootless, capabilities
//! dropped, no-new-privileges, a read-only root filesystem, and hard limits on
//! memory, PIDs and wall clock.
//!
//! Three things that must be right or the limits are decorative:
//!
//! - rootless resource limits silently do nothing without cgroup delegation,
//!   and boxd is a system unit — the OS tier sets `Delegate=` on the boxd
//!   unit and this code pins `--cgroup-manager=cgroupfs`, because a system
//!   unit has no user session bus for podman to ask systemd through;
//! - `--memory` without a matching `NODE_OPTIONS` kills builds that would
//!   otherwise pass, because Node sizes its heap at half the container limit;
//!   the two are set together here, from the same constants;
//! - NixOS has no SELinux and AppArmor is off, so user namespaces and seccomp
//!   are the whole defence rather than a backstop. That is what the flag
//!   assertions in the tests are protecting.
//!
//! The builder image itself is not pulled from any registry: the OS closure
//! ships it as a tarball (built by `dockerTools` in the platform flake) and
//! the boxd unit points at it with `BOX_BUILDER_IMAGE`. It updates the way
//! everything else on a Box updates — through the release channel.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};

/// Memory ceiling for one build phase. A Pi 5 has 4-8 GB; half the small one
/// leaves the Box itself alive while a runaway webpack is killed.
pub const MEMORY_MB: u64 = 2048;
/// Node's heap, sized to fit inside [`MEMORY_MB`] with room for everything
/// that is not the JS heap. Without this, Node picks half the cgroup limit
/// and a build that needs 1.2 GB of heap dies inside a 2 GB container.
pub const NODE_HEAP_MB: u64 = 1536;
pub const PIDS_LIMIT: u64 = 512;
/// Wall clock per phase, enforced by podman itself (`--timeout`), so a hung
/// install cannot wedge the sync loop.
pub const PHASE_TIMEOUT_SECS: u64 = 900;

/// How many lines of the build log ride along in an error. Enough to carry
/// the actual compiler error to the agent; the full log stays on disk.
const LOG_TAIL_LINES: usize = 40;

/// What to run, declared on the service's [`crate::config::RepoLink`] and
/// therefore in box.toml like everything else: the link says where content
/// comes from, this says how it becomes servable.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct BuildSpec {
    /// The build command, run by `sh -c` in the app root.
    pub command: String,
    /// The install command. `None` means detect from the lockfile
    /// (see [`default_install`]).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub install: Option<String>,
    /// Where the built site lands, relative to the app root. `None` means
    /// look in the places builders conventionally write
    /// ([`crate::pull::suggest_subdir`]).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output_dir: Option<String>,
}

/// How this machine can run a build. Detected once at startup and carried in
/// the app state, so every caller agrees.
#[derive(Debug, Clone)]
pub enum BuildExec {
    /// The real thing: rootless podman running the trusted builder image
    /// shipped in the OS closure.
    Podman { image_tar: PathBuf },
    /// Run the phases directly on this host, with no isolation at all. For
    /// tests, where the commands are our own; a dev machine can opt in with
    /// BOX_BUILD_INSECURE_DIRECT=1 and gets exactly what the name says.
    Direct,
    /// No sandbox here. Build-step links fail with an honest explanation
    /// instead of running untrusted code on someone's laptop.
    Unavailable,
}

impl BuildExec {
    pub fn detect() -> Self {
        if let Some(tar) = std::env::var_os("BOX_BUILDER_IMAGE") {
            return Self::Podman {
                image_tar: PathBuf::from(tar),
            };
        }
        if std::env::var_os("BOX_BUILD_INSECURE_DIRECT").is_some_and(|v| v == "1") {
            return Self::Direct;
        }
        Self::Unavailable
    }
}

/// The install command a lockfile implies. Explicit beats detected — this is
/// only consulted when the spec does not say. No package.json at all means no
/// install phase: not every build is a Node build.
pub fn default_install(appdir: &Path) -> Option<String> {
    if appdir.join("package-lock.json").is_file() {
        Some("npm ci".into())
    } else if appdir.join("yarn.lock").is_file() {
        Some("yarn install --frozen-lockfile".into())
    } else if appdir.join("pnpm-lock.yaml").is_file() {
        Some("pnpm install --frozen-lockfile".into())
    } else if appdir.join("package.json").is_file() {
        Some("npm install".into())
    } else {
        None
    }
}

/// The image reference the shipped tarball loads as. The tag is the Nix store
/// hash of the tarball, so a channel update that ships a new builder is a new
/// tag and gets loaded on first use — while every build against an unchanged
/// image skips the (slow, on a Pi) `podman load` entirely.
pub fn image_ref_for(image_tar: &Path) -> String {
    let base = image_tar
        .file_name()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_default();
    let tag: String = base
        .chars()
        .take_while(|c| c.is_ascii_alphanumeric())
        .take(12)
        .collect();
    let tag = if tag.is_empty() {
        "local".to_string()
    } else {
        tag.to_lowercase()
    };
    format!("localhost/box-builder:{tag}")
}

/// Global podman flags, needed on every invocation: a system unit has no user
/// session, so podman must not try to reach a session systemd or journald.
fn podman_global_args() -> Vec<String> {
    vec![
        "--cgroup-manager=cgroupfs".into(),
        "--events-backend=file".into(),
    ]
}

fn podman(args: &[String]) -> Result<std::process::Output> {
    let mut all = podman_global_args();
    all.extend(args.iter().cloned());
    Command::new("podman")
        .args(&all)
        .output()
        .context("running podman — is this a managed Box? (dev machines have no build sandbox)")
}

/// Make sure the shipped builder image is loaded, cheaply when it already is.
pub fn ensure_image(image_tar: &Path) -> Result<String> {
    let image = image_ref_for(image_tar);
    let exists = podman(&["image".into(), "exists".into(), image.clone()])?;
    if exists.status.success() {
        return Ok(image);
    }
    let tar = image_tar.to_string_lossy().into_owned();
    let load = podman(&["load".into(), "-i".into(), tar])?;
    if !load.status.success() {
        bail!(
            "loading the builder image failed: {}",
            String::from_utf8_lossy(&load.stderr).trim()
        );
    }
    // "Loaded image: localhost/box-builder:latest" — retag to the store-hash
    // tag this closure answers to.
    let stdout = String::from_utf8_lossy(&load.stdout);
    let loaded = stdout
        .lines()
        .rev()
        .find_map(|l| l.strip_prefix("Loaded image:"))
        .map(str::trim)
        .context("podman load reported no image name")?;
    let tag = podman(&["tag".into(), loaded.into(), image.clone()])?;
    if !tag.status.success() {
        bail!(
            "tagging the builder image failed: {}",
            String::from_utf8_lossy(&tag.stderr).trim()
        );
    }
    Ok(image)
}

/// The full argv (after `podman`) for one phase. Pure, so the tests can hold
/// every security-load-bearing flag up to the light without a container
/// runtime in sight.
/// Knobs a non-build caller (the work runner) turns; builds take the default.
pub struct PhaseOpts {
    pub timeout_secs: u64,
    pub extra_env: Vec<(String, String)>,
}

impl Default for PhaseOpts {
    fn default() -> Self {
        Self {
            timeout_secs: PHASE_TIMEOUT_SECS,
            extra_env: Vec::new(),
        }
    }
}

pub fn phase_args(
    image: &str,
    tree: &Path,
    cache: &Path,
    subdir: Option<&str>,
    network: bool,
    command: &str,
    opts: &PhaseOpts,
) -> Vec<String> {
    let workdir = match subdir {
        Some(s) => format!("/work/{s}"),
        None => "/work".into(),
    };
    let mut args = podman_global_args();
    args.extend(["run", "--rm"].map(String::from));
    // The whole defence, per the module doc: no capabilities, no privilege
    // escalation, nothing writable but the two mounts and /tmp.
    args.extend(
        [
            "--cap-drop=ALL",
            "--security-opt=no-new-privileges",
            "--read-only",
        ]
        .map(String::from),
    );
    args.extend(["--tmpfs".into(), "/tmp:rw,size=512m,mode=1777".to_string()]);
    // Hard limits: memory (no swap escape hatch), processes, wall clock.
    args.push(format!("--memory={MEMORY_MB}m"));
    args.push(format!("--memory-swap={MEMORY_MB}m"));
    args.push(format!("--pids-limit={PIDS_LIMIT}"));
    args.push(format!("--timeout={}", opts.timeout_secs));
    if !network {
        args.push("--network=none".into());
    }
    // The repository is data on a bind mount; the cache persists across
    // builds so a Pi is not reinstalling the world on every push.
    args.extend(["-v".into(), format!("{}:/work:rw", tree.display())]);
    args.extend(["-v".into(), format!("{}:/cache:rw", cache.display())]);
    args.extend(["-w".into(), workdir]);
    for (k, v) in phase_env() {
        args.extend(["-e".into(), format!("{k}={v}")]);
    }
    for (k, v) in &opts.extra_env {
        args.extend(["-e".into(), format!("{k}={v}")]);
    }
    args.push(image.into());
    args.extend(["sh", "-c", command].map(String::from));
    args
}

/// The environment both execution modes agree on: caches on the cache volume,
/// Node's heap sized to the container, and CI=1 so tools skip prompts.
fn phase_env() -> Vec<(String, String)> {
    vec![
        ("HOME".into(), "/cache/home".into()),
        ("npm_config_cache".into(), "/cache/npm".into()),
        ("XDG_CACHE_HOME".into(), "/cache/xdg".into()),
        ("YARN_CACHE_FOLDER".into(), "/cache/yarn".into()),
        // No pnpm store override: npm warns about the unknown key on EVERY
        // build (seen in the first live log), and pnpm's default store lands
        // under $HOME — which is already on the cache volume.
        ("CI".into(), "1".into()),
        (
            "NODE_OPTIONS".into(),
            format!("--max-old-space-size={NODE_HEAP_MB}"),
        ),
    ]
}

fn append_log(log: &Path, header: &str, out: &std::process::Output) {
    let mut text = format!("=== {header}\n");
    text.push_str(&String::from_utf8_lossy(&out.stdout));
    text.push_str(&String::from_utf8_lossy(&out.stderr));
    if !text.ends_with('\n') {
        text.push('\n');
    }
    if let Ok(existing) = fs::read_to_string(log) {
        let _ = fs::write(log, existing + &text);
    } else {
        let _ = fs::write(log, text);
    }
}

fn log_tail(log: &Path) -> String {
    let Ok(text) = fs::read_to_string(log) else {
        return String::new();
    };
    let lines: Vec<&str> = text.lines().collect();
    let start = lines.len().saturating_sub(LOG_TAIL_LINES);
    lines[start..].join("\n")
}

/// Everything a phase needs to run, resolved once for both phases.
struct PhaseRunner<'a> {
    exec: &'a BuildExec,
    /// Resolved image reference, when `exec` is podman.
    image: Option<String>,
    tree: &'a Path,
    cache: &'a Path,
    appdir: &'a Path,
    subdir: Option<&'a str>,
    log: &'a Path,
}

impl PhaseRunner<'_> {
    /// One phase, through whichever execution the state detected.
    fn run(&self, network: bool, command: &str) -> Result<()> {
        let phase = if network { "install" } else { "build" };
        let out = match self.exec {
            BuildExec::Podman { .. } => {
                let image = self
                    .image
                    .as_deref()
                    .expect("podman exec resolves its image first");
                let args = phase_args(
                    image,
                    self.tree,
                    self.cache,
                    self.subdir,
                    network,
                    command,
                    &PhaseOpts::default(),
                );
                Command::new("podman")
                    .args(&args)
                    .output()
                    .context("running podman")?
            }
            BuildExec::Direct => {
                let mut cmd = Command::new("sh");
                cmd.args(["-c", command]).current_dir(self.appdir);
                for (k, v) in phase_env() {
                    // Directly on the host, the cache paths are the real ones.
                    let v = v.replace("/cache", &self.cache.to_string_lossy());
                    cmd.env(k, v);
                }
                cmd.output().context("running the build command")?
            }
            BuildExec::Unavailable => unreachable!("checked before any phase runs"),
        };
        append_log(self.log, &format!("{phase}: {command}"), &out);
        if !out.status.success() {
            bail!(
                "the {phase} step failed ({}). Last lines of the build log:\n{}\n(full log: {})",
                out.status,
                log_tail(self.log),
                self.log.display()
            );
        }
        Ok(())
    }
}

/// Run a service's build against a fresh checkout and return the directory to
/// serve. The tree is scratch — the deploy copies out of it — so the build
/// writing all over it is fine; nothing outside the tree and the cache is
/// writable at all.
pub fn run(
    paths: &crate::paths::Paths,
    exec: &BuildExec,
    service: &str,
    tree: &Path,
    subdir: Option<&str>,
    spec: &BuildSpec,
) -> Result<PathBuf> {
    if matches!(exec, BuildExec::Unavailable) {
        bail!(
            "this machine cannot run build steps: it has no build sandbox. A managed \
             Box ships one (and builds run there automatically); on a dev machine, \
             repositories that commit their built site deploy fine via subdir."
        );
    }

    let appdir = match subdir {
        None => tree.to_path_buf(),
        Some(s) => {
            crate::pull::validate_subdir(s)?;
            let dir = crate::web::sites::resolve_safe(tree, s)
                .with_context(|| format!("subdir {s:?} escapes the repository"))?;
            if !dir.is_dir() {
                bail!("subdir {s:?} does not exist in the repository at this commit");
            }
            dir
        }
    };

    let cache = paths.build_cache_dir(service);
    for sub in ["home", "npm", "xdg", "yarn", "pnpm-store"] {
        fs::create_dir_all(cache.join(sub))?;
    }
    let log = paths.build_log(service);
    let _ = fs::remove_file(&log);
    fs::create_dir_all(paths.repos_dir())?;

    let runner = PhaseRunner {
        exec,
        image: match exec {
            BuildExec::Podman { image_tar } => Some(ensure_image(image_tar)?),
            _ => None,
        },
        tree,
        cache: &cache,
        appdir: &appdir,
        subdir,
        log: &log,
    };

    let install = spec.install.clone().or_else(|| default_install(&appdir));
    if let Some(cmd) = &install {
        runner.run(true, cmd)?;
    }
    runner.run(false, &spec.command)?;

    match &spec.output_dir {
        Some(d) => {
            crate::pull::validate_subdir(d)?;
            let out = crate::web::sites::resolve_safe(&appdir, d)
                .with_context(|| format!("output_dir {d:?} escapes the repository"))?;
            if !out.is_dir() {
                bail!(
                    "the build finished but produced no {d:?} directory. Last lines of \
                     the build log:\n{}\n(full log: {})",
                    log_tail(&log),
                    log.display()
                );
            }
            Ok(out)
        }
        None => match crate::pull::suggest_subdir(&appdir) {
            Some(d) => Ok(appdir.join(d)),
            None => bail!(
                "the build finished, but no output directory with an index.html \
                 appeared in the usual places (dist/, build/, out/, public/, …). \
                 Pass output_dir to say where this build writes its site."
            ),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_build_phase_has_no_network_and_the_install_phase_does() {
        let tree = Path::new("/data/repo-trees/app");
        let cache = Path::new("/data/repos/app.cache");
        let install = phase_args(
            "localhost/box-builder:t",
            tree,
            cache,
            None,
            true,
            "npm ci",
            &PhaseOpts::default(),
        );
        let build = phase_args(
            "localhost/box-builder:t",
            tree,
            cache,
            None,
            false,
            "npm run build",
            &PhaseOpts::default(),
        );
        assert!(
            !install.contains(&"--network=none".to_string()),
            "install needs the registry: {install:?}"
        );
        assert!(
            build.contains(&"--network=none".to_string()),
            "the build must not see the network: {build:?}"
        );
    }

    #[test]
    fn every_security_flag_is_present_in_both_phases() {
        let tree = Path::new("/t");
        let cache = Path::new("/c");
        for network in [true, false] {
            let args = phase_args(
                "img",
                tree,
                cache,
                None,
                network,
                "x",
                &PhaseOpts::default(),
            );
            for flag in [
                "--cap-drop=ALL",
                "--security-opt=no-new-privileges",
                "--read-only",
                "--rm",
                // A system unit has no session bus; without these podman's
                // limits (or podman itself) silently fail.
                "--cgroup-manager=cgroupfs",
                "--events-backend=file",
            ] {
                assert!(args.contains(&flag.to_string()), "{flag} missing: {args:?}");
            }
            assert!(args.contains(&format!("--memory={MEMORY_MB}m")));
            assert!(
                args.contains(&format!("--memory-swap={MEMORY_MB}m")),
                "no swap escape hatch"
            );
            assert!(args.contains(&format!("--pids-limit={PIDS_LIMIT}")));
            assert!(args.contains(&format!("--timeout={PHASE_TIMEOUT_SECS}")));
            // Only the repo tree and the cache are writable mounts.
            assert!(args.contains(&"/t:/work:rw".to_string()));
            assert!(args.contains(&"/c:/cache:rw".to_string()));
        }
    }

    #[test]
    fn nodes_heap_fits_inside_the_memory_limit() {
        // Node sizes its heap at half the cgroup limit unless told otherwise;
        // told otherwise must still fit.
        let (heap, memory) = (NODE_HEAP_MB, MEMORY_MB);
        assert!(
            heap < memory,
            "heap must leave room for the rest of the process"
        );
        let env = phase_env();
        let node_options = env
            .iter()
            .find(|(k, _)| k == "NODE_OPTIONS")
            .map(|(_, v)| v.clone())
            .unwrap();
        assert_eq!(node_options, format!("--max-old-space-size={NODE_HEAP_MB}"));
    }

    #[test]
    fn a_monorepo_subdir_becomes_the_working_directory() {
        let args = phase_args(
            "img",
            Path::new("/t"),
            Path::new("/c"),
            Some("apps/web"),
            false,
            "x",
            &PhaseOpts::default(),
        );
        let w = args.iter().position(|a| a == "-w").unwrap();
        assert_eq!(args[w + 1], "/work/apps/web");
    }

    #[test]
    fn the_lockfile_picks_the_install_command() {
        let tmp = tempfile::TempDir::new().unwrap();
        let dir = tmp.path();
        assert_eq!(
            default_install(dir),
            None,
            "no package.json, no install phase"
        );
        fs::write(dir.join("package.json"), "{}").unwrap();
        assert_eq!(default_install(dir).as_deref(), Some("npm install"));
        fs::write(dir.join("pnpm-lock.yaml"), "").unwrap();
        assert_eq!(
            default_install(dir).as_deref(),
            Some("pnpm install --frozen-lockfile")
        );
        fs::write(dir.join("yarn.lock"), "").unwrap();
        assert_eq!(
            default_install(dir).as_deref(),
            Some("yarn install --frozen-lockfile")
        );
        fs::write(dir.join("package-lock.json"), "{}").unwrap();
        assert_eq!(default_install(dir).as_deref(), Some("npm ci"));
    }

    #[test]
    fn the_image_tag_is_the_store_hash_so_updates_reload() {
        let a = image_ref_for(Path::new(
            "/nix/store/9y3a8xk2b7qmv0c1d4e5f6g7h8i9j0k1-box-builder.tar.gz",
        ));
        assert_eq!(a, "localhost/box-builder:9y3a8xk2b7qm");
        let b = image_ref_for(Path::new(
            "/nix/store/aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa-box-builder.tar.gz",
        ));
        assert_ne!(a, b, "a new closure is a new tag");
    }
}
