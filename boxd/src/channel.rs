//! The platform update channel. A Box's whole platform — boxd, the base OS,
//! the template library — is the `the-box` flake input of its generated
//! per-box repo, pinned in `flake.lock`. This module owns that pin: it locks
//! it, reports whether the channel has moved ahead, bumps it on request, and
//! runs the safe update — rebuild the system, switch, health-check, and roll
//! back (restoring the previous pin) if the new platform doesn't come up.
//!
//! The channel is configured per-box in `channel.toml`. The default ref points
//! at our platform; a box can override it to a fork, a mirror or a pinned
//! revision. The binary cache that lets a small box download rather than
//! compile is declared on the platform itself (see nix/module.nix).

use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::config::BoxConfig;
use crate::hostgen::{self, HostSpec};
use crate::ostier::{self, SystemHealth};
use crate::paths::Paths;

/// Our platform channel: the `release` branch, which the publish workflow
/// fast-forwards to a tag only after that release's closure is cached — so a
/// box updating from it downloads, never compiles. A box overrides this in
/// channel.toml to track a fork, a mirror, or a pinned revision.
pub const DEFAULT_PLATFORM_REF: &str = "github:fluxoz/the-box/release";

fn default_platform_ref() -> String {
    DEFAULT_PLATFORM_REF.to_string()
}
fn default_system() -> String {
    // Track the platform for THIS box's architecture: a Pi is aarch64, and
    // pinning x86_64-linux there would build (or fail to fetch) the wrong system.
    format!("{}-linux", std::env::consts::ARCH)
}

/// Per-box OS-tier binding: identity, which platform to track, and whether to
/// pick up new platform releases automatically.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChannelConfig {
    pub host_id: String,
    #[serde(default = "default_platform_ref")]
    pub platform_ref: String,
    #[serde(default = "default_system")]
    pub system: String,
    /// Hardware board (`pi3`/`pi4`/`pi5`) this box's systems must be built for;
    /// absent on generic hardware. TOML can't encode `None`, so it's skipped.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub board: Option<String>,
    /// GPU layer ("nvidia") for the generic appliance — the spare-RTX-PC
    /// inference Box. Like `board`, it must ride the channel binding: a
    /// channel update that rebuilt without the GPU layer would boot a machine
    /// whose GPU has vanished.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub gpu: Option<String>,
    #[serde(default)]
    pub auto_update: bool,
}

impl ChannelConfig {
    pub fn new(host_id: impl Into<String>) -> Self {
        Self {
            host_id: host_id.into(),
            platform_ref: default_platform_ref(),
            system: default_system(),
            board: None,
            gpu: None,
            auto_update: false,
        }
    }

    /// The channel binding for this box, or `None` if the box has no OS-tier
    /// channel configured (in which case OS-tier updates are a no-op).
    pub fn load(paths: &Paths) -> Result<Option<Self>> {
        let file = paths.channel_file();
        if !file.exists() {
            return Ok(None);
        }
        let text = std::fs::read_to_string(&file)
            .with_context(|| format!("reading {}", file.display()))?;
        let cfg = toml::from_str(&text).with_context(|| format!("parsing {}", file.display()))?;
        Ok(Some(cfg))
    }

    pub fn save(&self, paths: &Paths) -> Result<()> {
        let body = toml::to_string_pretty(self).context("serializing channel config")?;
        let file = paths.channel_file();
        std::fs::write(&file, body).with_context(|| format!("writing {}", file.display()))?;
        // Root writes this too (first-boot channel-init, SSH operators), and a
        // root-owned channel.toml makes the unprivileged updater fail its very
        // first pin write with a bare Permission denied. Found on the real Pi.
        crate::util::chown_like(&paths.data_dir, &file);
        Ok(())
    }

    pub fn spec(&self) -> HostSpec {
        HostSpec::new(
            self.host_id.clone(),
            self.platform_ref.clone(),
            self.system.clone(),
        )
        .with_board(self.board.clone())
        .with_gpu(self.gpu.clone())
        .with_auto_update(self.auto_update)
    }
}

// --- nix flake plumbing (pure arg builders + thin exec) -------------------

/// `nix flake lock <repo>`: pin any unpinned inputs, keeping existing pins.
pub fn lock_args(repo: &Path) -> Vec<String> {
    vec![
        "--extra-experimental-features".into(),
        "nix-command flakes".into(),
        "flake".into(),
        "lock".into(),
        repo.display().to_string(),
    ]
}

/// `nix flake update the-box --flake <repo>`: bump just the platform pin.
pub fn update_args(repo: &Path) -> Vec<String> {
    vec![
        "--extra-experimental-features".into(),
        "nix-command flakes".into(),
        "flake".into(),
        "update".into(),
        "the-box".into(),
        "--flake".into(),
        repo.display().to_string(),
    ]
}

/// `nix flake metadata --json <ref>`: resolve a ref to its current lock.
pub fn metadata_args(reference: &str) -> Vec<String> {
    vec![
        "--extra-experimental-features".into(),
        "nix-command flakes".into(),
        "flake".into(),
        "metadata".into(),
        "--json".into(),
        "--refresh".into(),
        reference.into(),
    ]
}

fn nix(args: &[String], what: &str) -> Result<std::process::Output> {
    let out = Command::new("nix")
        .args(args)
        .output()
        .with_context(|| format!("running {what} (is nix installed?)"))?;
    if !out.status.success() {
        bail!("{what} failed:\n{}", String::from_utf8_lossy(&out.stderr));
    }
    Ok(out)
}

/// A platform revision identifier: a git rev for a remote channel, or the
/// content hash for a local path ref. Either way, it changes iff the platform
/// changed, which is all the update logic needs.
fn locked_id(locked: &Value) -> Option<String> {
    locked
        .get("rev")
        .and_then(Value::as_str)
        .or_else(|| locked.get("narHash").and_then(Value::as_str))
        .map(String::from)
}

/// The platform revision the box's repo is currently pinned to.
pub fn locked_platform_id(repo: &Path) -> Result<Option<String>> {
    let lock = repo.join("flake.lock");
    if !lock.exists() {
        return Ok(None);
    }
    let text = std::fs::read_to_string(&lock)?;
    let json: Value = serde_json::from_str(&text).context("parsing flake.lock")?;
    Ok(json
        .get("nodes")
        .and_then(|n| n.get("the-box"))
        .and_then(|t| t.get("locked"))
        .and_then(locked_id))
}

/// The platform revision the channel currently resolves to upstream.
pub fn latest_platform_id(reference: &str) -> Result<String> {
    let out = nix(&metadata_args(reference), "nix flake metadata")?;
    let json: Value =
        serde_json::from_slice(&out.stdout).context("parsing nix flake metadata output")?;
    json.get("locked")
        .and_then(locked_id)
        .context("flake metadata has no locked revision")
}

pub fn ensure_locked(repo: &Path) -> Result<()> {
    nix(&lock_args(repo), "nix flake lock").map(|_| ())
}

pub fn bump_pin(repo: &Path) -> Result<()> {
    nix(&update_args(repo), "nix flake update the-box").map(|_| ())
}

/// Where the channel stands: the pinned revision, the upstream revision, and
/// whether they differ.
#[derive(Debug, Clone, Serialize)]
pub struct UpdateStatus {
    pub current: Option<String>,
    pub latest: String,
    pub update_available: bool,
}

/// Compare the box's pin to the upstream channel without changing anything.
pub fn check(paths: &Paths, channel: &ChannelConfig) -> Result<UpdateStatus> {
    let current = locked_platform_id(&paths.os_config_dir())?;
    let latest = latest_platform_id(&channel.platform_ref)?;
    Ok(UpdateStatus {
        update_available: current.as_deref() != Some(latest.as_str()),
        current,
        latest,
    })
}

/// Regenerate the per-box repo, optionally bump the platform pin, then build
/// the system, switch to it, health-check, and roll back on failure —
/// restoring the previous pin so a bad release doesn't get rebuilt next time.
/// This is the OS tier's self-reconcile, run on a schedule or on demand.
pub fn update_and_switch(
    paths: &Paths,
    config: &BoxConfig,
    channel: &ChannelConfig,
    bump: bool,
    health: &SystemHealth,
) -> Result<PathBuf> {
    if !ostier::available() {
        bail!(
            "OS-tier update unavailable: {} not found (not a NixOS system)",
            ostier::SYSTEM_PROFILE
        );
    }
    // The brick guard: never rebuild/switch a system whose board binding
    // doesn't match this hardware (a Pi on a generic build won't boot again).
    crate::board::assert_compatible(channel.board.as_deref())?;
    let repo = paths.os_config_dir();
    hostgen::write_host_repo(paths, config, &channel.spec(), &repo)?;

    // Snapshot the pin so we can restore it if ANY step after this fails.
    //
    // Restoring it only when the health check failed was not enough, and the
    // gap was self-concealing: a build that died on a transient fetch error
    // left the bumped pin on disk, `check` compares that pin against upstream,
    // so the box then reported itself up to date — while still running the old
    // platform — until some later release moved upstream again.
    let lock_path = repo.join("flake.lock");
    let saved_lock = std::fs::read(&lock_path).ok();
    let restore_pin = || match &saved_lock {
        Some(lock) => {
            let _ = std::fs::write(&lock_path, lock);
        }
        None => {
            let _ = std::fs::remove_file(&lock_path);
        }
    };

    // The bump itself is a step that can fail — and it failed in the wild in
    // precisely the self-concealing way described above: `nix flake update`
    // rewrote the platform input, then died on a LATER input, and the `?`
    // here walked out without restoring. The box then answered "up to date"
    // to every check while running the old platform. No step between the
    // snapshot and the health check gets to skip the restore.
    if bump {
        if let Err(e) = bump_pin(&repo) {
            restore_pin();
            return Err(e.context("advancing the platform pin (pin restored)"));
        }
    } else if let Err(e) = ensure_locked(&repo) {
        restore_pin();
        return Err(e.context("locking the platform pin (pin restored)"));
    }

    let toplevel = match ostier::build(&repo, &channel.host_id) {
        Ok(t) => t,
        Err(e) => {
            restore_pin();
            return Err(e.context("building the new platform (pin restored)"));
        }
    };

    if let Err(e) = ostier::activate(ostier::SYSTEM_PROFILE, &toplevel, ostier::Action::Switch) {
        // `activate` registers the generation before switching, so a failure
        // part-way leaves the system profile — and the default boot entry —
        // pointing at a system that was never health-checked. Put both back.
        let _ = ostier::rollback(ostier::SYSTEM_PROFILE);
        restore_pin();
        return Err(e.context("activating the new platform (rolled back, pin restored)"));
    }

    match health() {
        Ok(()) => {
            crate::journal::record(
                paths,
                "update",
                "took a platform update and passed the health check",
            );
            Ok(toplevel)
        }
        Err(e) => {
            ostier::rollback(ostier::SYSTEM_PROFILE)?;
            restore_pin();
            crate::journal::record(
                paths,
                "update",
                format!("REFUSED a platform update: health check failed, rolled back ({e:#})"),
            );
            bail!("platform update health check failed — rolled back (pin restored): {e:#}");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn channel_config_roundtrips_and_defaults() {
        let tmp = TempDir::new().unwrap();
        let paths = Paths::new(tmp.path().to_path_buf());
        paths.ensure().unwrap();

        assert!(ChannelConfig::load(&paths).unwrap().is_none());

        let cfg = ChannelConfig::new("box-abc");
        cfg.save(&paths).unwrap();
        let loaded = ChannelConfig::load(&paths).unwrap().unwrap();
        assert_eq!(loaded.host_id, "box-abc");
        assert_eq!(loaded.platform_ref, DEFAULT_PLATFORM_REF);
        assert_eq!(loaded.system, format!("{}-linux", std::env::consts::ARCH));
        assert!(loaded.board.is_none(), "no board on generic hardware");
        assert!(!loaded.auto_update);

        // A Pi's binding round-trips its board (TOML has no null, so None is
        // simply absent — which is also what pre-board channel.toml files have).
        let mut pi = ChannelConfig::new("pibox");
        pi.board = Some("pi5".into());
        pi.save(&paths).unwrap();
        let loaded = ChannelConfig::load(&paths).unwrap().unwrap();
        assert_eq!(loaded.board.as_deref(), Some("pi5"));
        assert_eq!(loaded.spec().board.as_deref(), Some("pi5"));
    }

    #[test]
    fn arg_builders() {
        let repo = Path::new("/var/lib/boxd/os-config");
        assert!(lock_args(repo).contains(&"lock".to_string()));
        let up = update_args(repo);
        assert!(up.windows(2).any(|w| w == ["update", "the-box"]));
        assert!(up.iter().any(|a| a == "--flake"));
        assert!(metadata_args("github:x/y").contains(&"metadata".to_string()));
    }

    #[test]
    fn parses_locked_ids() {
        assert_eq!(
            locked_id(&serde_json::json!({"rev": "abc123"})),
            Some("abc123".into())
        );
        assert_eq!(
            locked_id(&serde_json::json!({"narHash": "sha256-xyz"})),
            Some("sha256-xyz".into())
        );
        assert_eq!(locked_id(&serde_json::json!({})), None);
    }
}
