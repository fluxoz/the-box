//! OS-tier activation: build a per-box NixOS system from the generated repo
//! and switch to it atomically, exactly as `nixos-rebuild switch` does, with a
//! health-checked automatic rollback to the previous *system* generation.
//!
//! This is the slow path of the two-speed reconciler: structural changes (a
//! new service, OS config, a platform update) rebuild the whole system; pure
//! content edits stay on boxd's fast path. The mechanism is the standard NixOS
//! system profile, so the bootloader keeps every generation as a boot entry —
//! the ultimate backstop if a switch ever breaks boot itself.
//!
//! The command *plans* are pure and unit-tested here; execution runs on a Box
//! (a NixOS host with a system profile), so the daemon guards it with
//! [`available`].

use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{bail, Context, Result};

use crate::config::BoxConfig;
use crate::hostgen::{self, HostSpec};
use crate::paths::Paths;

/// The NixOS system profile every generation is registered against.
pub const SYSTEM_PROFILE: &str = "/nix/var/nix/profiles/system";

/// switch-to-configuration actions. `Switch` activates now and sets the default
/// boot entry; `Boot` stages for next boot; `Test` activates without touching
/// the bootloader.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Action {
    Switch,
    Boot,
    Test,
}

impl Action {
    pub fn as_str(self) -> &'static str {
        match self {
            Action::Switch => "switch",
            Action::Boot => "boot",
            Action::Test => "test",
        }
    }
}

/// This host can run OS-tier switches only if it *is* a NixOS system — i.e. the
/// system profile exists. On a dev machine it doesn't, and the daemon falls
/// back to the content fast path.
pub fn available() -> bool {
    Path::new(SYSTEM_PROFILE).exists()
}

/// The root oneshot that applies the current config to the running system.
/// boxd is unprivileged, so it asks systemd to start this rather than switching
/// the system itself — the same shape as the platform update unit.
pub const APPLY_UNIT: &str = "boxd-os-apply.service";

/// Marker recording that the running system is BEHIND the config: a structural
/// change was deployed but the OS tier could not be applied. Its presence is
/// what lets the console say "deployed, not yet running" instead of "active".
pub fn pending_marker(paths: &Paths) -> PathBuf {
    paths.data_dir.join("os-apply-pending")
}

pub fn is_pending(paths: &Paths) -> bool {
    pending_marker(paths).exists()
}

pub fn mark_pending(paths: &Paths, reason: &str) {
    let _ = std::fs::write(pending_marker(paths), reason);
    crate::util::chown_like(&paths.data_dir, &pending_marker(paths));
}

pub fn clear_pending(paths: &Paths) {
    let _ = std::fs::remove_file(pending_marker(paths));
}

/// Why a structural change is not running yet, if it isn't.
pub fn pending_reason(paths: &Paths) -> Option<String> {
    std::fs::read_to_string(pending_marker(paths)).ok()
}

/// Is the root apply unit installed? A machine can be NixOS without being a
/// Box, and the unit ships with the Box's platform module.
/// Whether this machine is a managed Box, as opposed to merely NixOS.
///
/// `available()` alone is the wrong question for diagnostics: a developer's
/// NixOS desktop has a system profile too, and judging it by Box rules reports
/// a perfectly healthy dev server as a broken system. The apply unit exists
/// only where the platform module installed it.
pub fn managed_system() -> bool {
    available() && apply_unit_available()
}

pub fn apply_unit_available() -> bool {
    Command::new("systemctl")
        .args(["cat", APPLY_UNIT])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

/// What happened when a structural change asked the OS tier to catch up.
pub enum ApplyOutcome {
    /// The system now matches the config.
    Applied,
    /// Nothing to do — this machine has no OS tier (a dev box).
    NotABox,
    /// The apply ran and failed; the system rolled itself back.
    Failed(String),
}

/// Ask the root oneshot to make the system match the current config, and wait
/// for it. Called after a structural deploy: until this runs, a container or
/// app exists in the config and the generation but nothing actually runs it.
pub fn request_apply(paths: &Paths) -> ApplyOutcome {
    if !available() || !apply_unit_available() {
        mark_pending(
            paths,
            "this machine is not a Box (no OS tier), so services that need systemd or podman are declared but not running",
        );
        return ApplyOutcome::NotABox;
    }
    // Blocking start: the caller is a job or a CLI invocation that should
    // report the real outcome, not "started".
    let out = Command::new("systemctl")
        .args(["start", APPLY_UNIT])
        .output();
    match out {
        Ok(o) if o.status.success() => {
            clear_pending(paths);
            ApplyOutcome::Applied
        }
        Ok(o) => {
            let detail = String::from_utf8_lossy(&o.stderr).trim().to_string();
            let detail = if detail.is_empty() {
                last_apply_log()
            } else {
                detail
            };
            mark_pending(paths, &format!("the last system apply failed: {detail}"));
            ApplyOutcome::Failed(detail)
        }
        Err(e) => {
            mark_pending(paths, &format!("could not start {APPLY_UNIT}: {e}"));
            ApplyOutcome::Failed(e.to_string())
        }
    }
}

/// The tail of the apply unit's journal, so a failure says what went wrong
/// rather than just "job failed".
fn last_apply_log() -> String {
    last_unit_log(APPLY_UNIT)
}

fn last_unit_log(unit: &str) -> String {
    Command::new("journalctl")
        .args(["-u", unit, "-n", "20", "--no-pager", "-o", "cat"])
        .output()
        .ok()
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| format!("see `journalctl -u {unit}`"))
}

/// The root oneshot that applies a platform update — same shape as
/// [`APPLY_UNIT`], for the same reason: registering and switching the system
/// profile needs root, and boxd is not root. The polkit rule that lets boxd
/// start it ships with the platform module.
pub const UPDATE_UNIT: &str = "boxd-channel-update.service";
pub const BACKUP_NOW_UNIT: &str = "boxd-backup-now.service";

pub fn unit_available(unit: &str) -> bool {
    Command::new("systemctl")
        .args(["cat", unit])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

pub fn update_unit_available() -> bool {
    unit_available(UPDATE_UNIT)
}

/// Run the platform update through the root oneshot, blocking until it
/// finishes. The update's switch restarts boxd, so the caller may well die
/// mid-call — that is the persistent-job "interrupted, and for an update the
/// restart usually IS the success path" story, and it is the truth.
///
/// The oneshot's story goes to the journal, not to us — so tail it for the
/// duration and hand each line to `line`, which is how the console's job view
/// shows a platform update doing something rather than a bar and silence.
pub fn run_update_unit(line: impl Fn(&str) + Send + 'static) -> Result<()> {
    run_unit_streaming(UPDATE_UNIT, line)
}

/// Run any root oneshot while tailing its journal into `line` — how a job
/// view shows privileged work (updates, backups) happening rather than a bar
/// and silence.
pub fn run_unit_streaming(unit: &str, line: impl Fn(&str) + Send + 'static) -> Result<()> {
    let mut tail = Command::new("journalctl")
        .args(["-f", "-n", "0", "-o", "cat", "-u", unit])
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .spawn()
        .ok();
    // The reader owns the callback and pumps lines AS THEY ARRIVE; killing
    // the tail below ends it via EOF.
    let pump = tail.as_mut().and_then(|t| t.stdout.take()).map(|out| {
        std::thread::spawn(move || {
            use std::io::BufRead;
            for l in std::io::BufReader::new(out).lines() {
                let Ok(l) = l else { break };
                line(&l);
            }
        })
    });

    let out = Command::new("systemctl")
        .args(["start", unit])
        .output()
        .with_context(|| format!("starting {unit}"));

    if let Some(mut t) = tail {
        let _ = t.kill();
        let _ = t.wait();
    }
    if let Some(h) = pump {
        let _ = h.join();
    }

    let out = out?;
    if out.status.success() {
        return Ok(());
    }
    let detail = String::from_utf8_lossy(&out.stderr).trim().to_string();
    let detail = if detail.is_empty() {
        last_unit_log(unit)
    } else {
        detail
    };
    bail!("{unit} failed: {detail}")
}

/// `nix build` args for a per-box system's toplevel from its generated repo.
pub fn build_toplevel_args(repo: &Path, host_id: &str) -> Vec<String> {
    vec![
        "--extra-experimental-features".into(),
        "nix-command flakes".into(),
        "build".into(),
        "--no-link".into(),
        "--print-out-paths".into(),
        // `path:` forces the plain-path fetcher so an uncommitted repo builds.
        format!(
            "path:{}#nixosConfigurations.{}.config.system.build.toplevel",
            repo.display(),
            host_id
        ),
    ]
}

/// `nix-env` args to register a built toplevel as the next system generation.
pub fn set_profile_args(profile: &str, toplevel: &Path) -> Vec<String> {
    vec![
        "-p".into(),
        profile.into(),
        "--set".into(),
        toplevel.display().to_string(),
    ]
}

/// `nix-env` args to roll the system profile back one generation.
pub fn rollback_args(profile: &str) -> Vec<String> {
    vec!["-p".into(), profile.into(), "--rollback".into()]
}

/// The switch-to-configuration invocation for a given toplevel.
pub fn switch_command(toplevel: &Path, action: Action) -> (PathBuf, Vec<String>) {
    (
        toplevel.join("bin/switch-to-configuration"),
        vec![action.as_str().into()],
    )
}

fn run(program: &str, args: &[String], what: &str) -> Result<()> {
    let out = Command::new(program)
        .args(args)
        .output()
        .with_context(|| format!("running {what}"))?;
    if !out.status.success() {
        bail!("{what} failed:\n{}", String::from_utf8_lossy(&out.stderr));
    }
    Ok(())
}

/// A system health check runs against the freshly-switched system; `Err`
/// triggers a rollback. Default: boxd must be back up.
pub type SystemHealth = dyn Fn() -> Result<()>;

/// boxd being active is the minimum bar for a healthy switch — if the new
/// system can't run the daemon, roll back rather than strand the operator.
///
/// A `Type=simple` unit counts as "active" the instant systemd forks it, so a
/// single instantaneous check also passes a daemon that dies a second later and
/// is restarted forever by `Restart=on-failure`. Require it to be active and to
/// STAY active for a few consecutive polls before we keep the switch.
pub fn default_system_health() -> Result<()> {
    const POLLS: u32 = 15;
    const SETTLED: u32 = 3;
    const INTERVAL: std::time::Duration = std::time::Duration::from_secs(2);

    let mut consecutive = 0;
    let mut settled = false;
    for poll in 0..POLLS {
        if unit_is_active("boxd.service")? {
            consecutive += 1;
            if consecutive >= SETTLED {
                settled = true;
                break;
            }
        } else {
            consecutive = 0;
        }
        if poll + 1 < POLLS {
            std::thread::sleep(INTERVAL);
        }
    }
    if !settled {
        bail!(
            "boxd.service did not stay active after the switch (crash-looping or failed to start)"
        );
    }

    // boxd being up is necessary, not sufficient. nginx is how every service
    // on this Box is actually served, and a switch can leave it dead while
    // everything else looks fine — it happened: an invalid nginx config took
    // every site offline and the switch still reported success, because only
    // boxd was checked. Any unit the new system *enables* has to actually run
    // before we keep the switch; a Box with no services has no nginx, and the
    // is-enabled probe skips it there rather than failing.
    for unit in ["nginx.service"] {
        if !unit_is_enabled(unit)? {
            continue;
        }
        let mut up = false;
        for _ in 0..POLLS {
            if unit_is_active(unit)? {
                up = true;
                break;
            }
            std::thread::sleep(INTERVAL);
        }
        if !up {
            bail!("{unit} is enabled in the new system but did not come up after the switch — rolling back rather than serving nothing");
        }
    }
    Ok(())
}

fn unit_is_active(unit: &str) -> Result<bool> {
    Ok(Command::new("systemctl")
        .args(["is-active", "--quiet", unit])
        .status()
        .with_context(|| format!("running systemctl is-active {unit}"))?
        .success())
}

/// Whether the current system includes this unit at all. `is-enabled` exits
/// non-zero both for "disabled" and "no such unit", which collapses exactly
/// the cases where demanding the unit be active would be wrong.
fn unit_is_enabled(unit: &str) -> Result<bool> {
    Ok(Command::new("systemctl")
        .args(["is-enabled", "--quiet", unit])
        .status()
        .with_context(|| format!("running systemctl is-enabled {unit}"))?
        .success())
}

/// Build the per-box system's toplevel from a generated repo.
pub fn build(repo: &Path, host_id: &str) -> Result<PathBuf> {
    let out = Command::new("nix")
        .args(build_toplevel_args(repo, host_id))
        .output()
        .context("running nix build for the system toplevel")?;
    if !out.status.success() {
        bail!(
            "building system toplevel failed:\n{}",
            String::from_utf8_lossy(&out.stderr)
        );
    }
    let stdout = String::from_utf8_lossy(&out.stdout);
    let path = stdout
        .lines()
        .map(str::trim)
        .rfind(|l| !l.is_empty())
        .context("nix build produced no output path")?;
    Ok(PathBuf::from(path))
}

/// Register `toplevel` as the next system generation and activate it.
pub fn activate(profile: &str, toplevel: &Path, action: Action) -> Result<()> {
    run(
        "nix-env",
        &set_profile_args(profile, toplevel),
        "registering the system generation",
    )?;
    let (program, args) = switch_command(toplevel, action);
    let out = Command::new(program.display().to_string())
        .args(&args)
        .output()
        .context("running switch-to-configuration")?;
    switch_outcome(out.status.code(), &String::from_utf8_lossy(&out.stderr))
}

/// What a switch-to-configuration exit code MEANS, kept pure so the one
/// judgement call in the OS tier is testable.
///
/// Exit 4 is "the system activated, but some units failed to (re)start". That
/// is a SERVICE being unhealthy, not the system failing to switch - and
/// treating it as a failed switch is how one crash-looping container wedged an
/// entire Box: every later deploy rolled back and platform updates refused
/// until someone found and deleted the offender. A broken service is reported
/// and left broken; the system keeps the configuration the operator asked for.
pub fn switch_outcome(code: Option<i32>, stderr: &str) -> Result<()> {
    match code {
        Some(0) => Ok(()),
        Some(4) => {
            let units: Vec<&str> = stderr
                .lines()
                .map(str::trim)
                .filter(|l| l.contains("units failed"))
                .collect();
            let detail = if units.is_empty() {
                "(see systemctl --failed)".to_string()
            } else {
                units.join("; ")
            };
            tracing::warn!("the system switched, but some units are unhealthy: {detail}");
            Ok(())
        }
        _ => bail!("switch-to-configuration failed:\n{stderr}"),
    }
}

/// Roll the system profile back one generation and activate the prior system.
pub fn rollback(profile: &str) -> Result<()> {
    run(
        "nix-env",
        &rollback_args(profile),
        "rolling back the system profile",
    )?;
    // After --rollback the profile points at the previous generation; activate
    // it so the running system actually matches.
    let toplevel =
        std::fs::canonicalize(profile).context("resolving the system profile after rollback")?;
    let (program, args) = switch_command(&toplevel, Action::Switch);
    run(
        &program.display().to_string(),
        &args,
        "switch-to-configuration (rollback)",
    )
}

/// The full OS-tier reconcile: regenerate the per-box repo, build its system,
/// switch to it, health-check, and on failure roll back to the previous system
/// generation. The slow-path analogue of [`crate::ops::apply_checked`].
pub fn reconcile(
    paths: &Paths,
    config: &BoxConfig,
    spec: &HostSpec,
    health: &SystemHealth,
) -> Result<PathBuf> {
    if !available() {
        bail!("OS-tier switch unavailable: {SYSTEM_PROFILE} not found (not a NixOS system)");
    }
    let repo = paths.os_config_dir();
    hostgen::write_host_repo(paths, config, spec, &repo)?;
    let toplevel = build(&repo, &spec.id)?;

    activate(SYSTEM_PROFILE, &toplevel, Action::Switch)?;
    match health() {
        Ok(()) => Ok(toplevel),
        Err(e) => {
            rollback(SYSTEM_PROFILE)?;
            bail!("system health check failed — rolled back: {e:#}");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn build_args_target_the_host_toplevel() {
        let args = build_toplevel_args(Path::new("/var/lib/boxd/os-config"), "box-1");
        assert!(args.contains(&"build".to_string()));
        assert!(args.iter().any(|a| {
            a
            == "path:/var/lib/boxd/os-config#nixosConfigurations.box-1.config.system.build.toplevel"
        }));
    }

    #[test]
    fn profile_and_rollback_args() {
        let set = set_profile_args(SYSTEM_PROFILE, Path::new("/nix/store/abc-system"));
        assert_eq!(
            set,
            vec![
                "-p",
                "/nix/var/nix/profiles/system",
                "--set",
                "/nix/store/abc-system"
            ]
        );
        let rb = rollback_args(SYSTEM_PROFILE);
        assert_eq!(rb, vec!["-p", "/nix/var/nix/profiles/system", "--rollback"]);
    }

    #[test]
    fn a_broken_service_does_not_wedge_the_system() {
        // Exit 4: activated, some units unhealthy. The deploy stands.
        ostier_switch_ok(switch_outcome(
            Some(4),
            "warning: the following units failed: podman-n8n.service\n",
        ));
        // A real activation failure still fails.
        assert!(switch_outcome(Some(1), "boom").is_err());
        assert!(switch_outcome(None, "killed").is_err());
        ostier_switch_ok(switch_outcome(Some(0), ""));
    }

    fn ostier_switch_ok(r: Result<()>) {
        assert!(r.is_ok(), "{r:?}");
    }

    #[test]
    fn switch_command_points_into_the_toplevel() {
        let (prog, args) = switch_command(Path::new("/nix/store/abc-system"), Action::Switch);
        assert_eq!(
            prog,
            Path::new("/nix/store/abc-system/bin/switch-to-configuration")
        );
        assert_eq!(args, vec!["switch"]);
        assert_eq!(Action::Boot.as_str(), "boot");
        assert_eq!(Action::Test.as_str(), "test");
    }
}
