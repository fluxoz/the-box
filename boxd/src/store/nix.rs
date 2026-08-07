use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{bail, Context, Result};

use super::Builder;

/// Builds generations with `nix build` from the machine-generated flake in
/// the generation source directory, producing an immutable store path.
pub struct NixBuilder;

impl NixBuilder {
    pub fn available() -> bool {
        Command::new("nix")
            .arg("--version")
            .output()
            .map(|o| o.status.success())
            .unwrap_or(false)
    }
}

impl Builder for NixBuilder {
    fn name(&self) -> &'static str {
        "nix"
    }

    fn build(&self, gensrc: &Path) -> Result<PathBuf> {
        // TODO: register the profile as a GC root so `nix-collect-garbage`
        // on the host cannot reap live generations (profiles live in the
        // boxd data dir, outside /nix/var/nix/profiles).
        // `path:` forces the plain path fetcher: without it, a data dir that
        // happens to live inside a git repository is treated as a git flake
        // and untracked generation files become invisible to the build.
        let output = Command::new("nix")
            .args([
                "--extra-experimental-features",
                "nix-command flakes",
                "build",
                "--no-link",
                "--print-out-paths",
            ])
            .arg(format!("path:{}", gensrc.display()))
            .output()
            .context("running nix build (is nix installed?)")?;
        if !output.status.success() {
            bail!(
                "nix build failed:\n{}",
                String::from_utf8_lossy(&output.stderr)
            );
        }
        let stdout = String::from_utf8_lossy(&output.stdout);
        let path = stdout
            .lines()
            .map(str::trim)
            .rfind(|l| !l.is_empty())
            .context("nix build produced no output path")?;
        Ok(PathBuf::from(path))
    }
}
