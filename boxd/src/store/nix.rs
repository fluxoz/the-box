use std::io::{BufRead, BufReader, Read};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use anyhow::{bail, Context, Result};

use super::{nixlog::NixLog, Builder};

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
        // `--no-link` is safe here: the generation's profile link is what keeps
        // the output alive, and `store::create_generation_link` registers that
        // as an indirect GC root.
        // `path:` forces the plain path fetcher: without it, a data dir that
        // happens to live inside a git repository is treated as a git flake
        // and untracked generation files become invisible to the build.
        //
        // internal-json is nix's own progress-bar feed: per-path and per-drv
        // activities, aggregate (done, expected) counts, and every builder log
        // line. Streamed as it happens into this thread's build watch (a job's
        // Progress installs itself as that), so the console shows the build
        // the way `nix build` shows it to a person — measured, not guessed.
        let mut child = Command::new("nix")
            .args([
                "--extra-experimental-features",
                "nix-command flakes",
                "--log-format",
                "internal-json",
                "build",
                "--no-link",
                "--print-out-paths",
            ])
            .arg(format!("path:{}", gensrc.display()))
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .context("running nix build (is nix installed?)")?;

        let watch = super::thread_watch();
        let mut log = NixLog::new();
        let stderr = child.stderr.take().expect("stderr was piped");
        for line in BufReader::new(stderr).lines() {
            let Ok(line) = line else { break };
            log.line(&line, watch.as_deref());
        }

        let mut stdout = String::new();
        if let Some(mut out) = child.stdout.take() {
            let _ = out.read_to_string(&mut stdout);
        }
        let status = child.wait().context("waiting for nix build")?;
        if !status.success() {
            bail!("nix build failed:\n{}", log.tail());
        }
        let path = stdout
            .lines()
            .map(str::trim)
            .rfind(|l| !l.is_empty())
            .context("nix build produced no output path")?;
        Ok(PathBuf::from(path))
    }
}
