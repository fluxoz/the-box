use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

/// Well-known locations inside the boxd data directory.
///
/// Layout:
/// ```text
/// <data>/box.toml            declarative config (desired state)
/// <data>/sources/<name>/     content sources per service
/// <data>/generation-src/     machine-generated Nix flake for the next build
/// <data>/profiles/box        current-generation symlink (atomic switch point)
/// <data>/profiles/box-N-link per-generation symlinks to immutable output trees
/// <data>/store/              output trees for the non-Nix local backend
/// ```
#[derive(Debug, Clone)]
pub struct Paths {
    pub data_dir: PathBuf,
}

impl Paths {
    pub fn new(data_dir: PathBuf) -> Self {
        Self { data_dir }
    }

    pub fn config_file(&self) -> PathBuf {
        self.data_dir.join("box.toml")
    }

    pub fn sources_dir(&self) -> PathBuf {
        self.data_dir.join("sources")
    }

    pub fn source_dir(&self, service: &str) -> PathBuf {
        self.sources_dir().join(service)
    }

    pub fn gensrc_dir(&self) -> PathBuf {
        self.data_dir.join("generation-src")
    }

    pub fn profiles_dir(&self) -> PathBuf {
        self.data_dir.join("profiles")
    }

    pub fn local_store_dir(&self) -> PathBuf {
        self.data_dir.join("store")
    }

    /// The generated standalone per-box config repo the OS tier builds from.
    pub fn os_config_dir(&self) -> PathBuf {
        self.data_dir.join("os-config")
    }

    /// The OS-tier channel binding (host id, platform ref, update policy).
    pub fn channel_file(&self) -> PathBuf {
        self.data_dir.join("channel.toml")
    }

    /// The git remote the config repo (config + encrypted secrets) is pushed to
    /// as an offsite, user-owned backup. Per-box, never committed.
    pub fn config_remote_file(&self) -> PathBuf {
        self.data_dir.join("config-remote")
    }

    /// Operator sessions and one-time pairing codes (hashed, 0600, never in git).
    pub fn auth_file(&self) -> PathBuf {
        self.data_dir.join("auth.json")
    }

    pub fn ensure(&self) -> Result<()> {
        for dir in [
            self.data_dir.clone(),
            self.sources_dir(),
            self.profiles_dir(),
        ] {
            std::fs::create_dir_all(&dir).with_context(|| format!("creating {}", dir.display()))?;
        }
        Ok(())
    }
}

/// The Box's own state directory, created by the platform's systemd unit. Its
/// existence is what marks this machine as a Box.
const PLATFORM_DATA_DIR: &str = "/var/lib/boxd";

/// Default data directory: `$BOXD_DATA_DIR`, else the Box's own state dir if
/// this machine is a Box, else `~/.local/share/boxd`.
///
/// The middle case matters: the platform exports `BOXD_DATA_DIR` for interactive
/// shells, but `sudo` resets the environment, so `sudo boxd channel update` on a
/// real Box used to fall through to `/root/.local/share/boxd` and quietly report
/// "no channel configured" while managing an empty private state dir instead of
/// the Box. Commands that need root now land on the Box's state by default.
pub fn default_data_dir() -> PathBuf {
    if let Ok(dir) = std::env::var("BOXD_DATA_DIR") {
        return PathBuf::from(dir);
    }
    let platform = PathBuf::from(PLATFORM_DATA_DIR);
    if platform.is_dir() {
        return platform;
    }
    let home = std::env::var("HOME").unwrap_or_else(|_| PLATFORM_DATA_DIR.into());
    Path::new(&home).join(".local/share/boxd")
}
