//! Git-backed config history. The declarative config (box.toml + generated
//! modules + service sources) lives in a Git repo in the data dir, and every
//! generation-producing operation commits it — so generation history is
//! literally the commit log, and it's the source of truth a fleet's shared
//! repo will sync. Secrets and heavy runtime state are never tracked.
//!
//! All operations are best-effort: a Box without git still works, it just has
//! no history. Callers log and continue rather than failing an apply.

use std::process::Command;

use anyhow::{bail, Context, Result};
use serde::Serialize;

use crate::paths::Paths;

const GITIGNORE: &str = "\
# boxd: track declarative config only — never secrets or heavy runtime state
/secrets/
/store/
/profiles/
/generation-src/
/logs/
/network.toml
/auth.json
";

fn git(paths: &Paths, args: &[&str]) -> Result<std::process::Output> {
    let out = Command::new("git")
        .arg("-C")
        .arg(&paths.data_dir)
        // Don't depend on a global git identity being configured.
        .args(["-c", "user.name=boxd", "-c", "user.email=boxd@localhost"])
        .args(args)
        .output()
        .context("running git (is it installed?)")?;
    Ok(out)
}

pub fn available() -> bool {
    Command::new("git")
        .arg("--version")
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

/// Initialize the config repo if absent (idempotent).
pub fn ensure_repo(paths: &Paths) -> Result<()> {
    if paths.data_dir.join(".git").exists() {
        return Ok(());
    }
    let out = git(paths, &["init", "-q", "-b", "main"])?;
    if !out.status.success() {
        bail!("git init failed: {}", String::from_utf8_lossy(&out.stderr));
    }
    std::fs::write(paths.data_dir.join(".gitignore"), GITIGNORE)?;
    Ok(())
}

/// Stage the tracked config and commit it. Empty commits are allowed so every
/// generation maps to exactly one commit.
pub fn commit(paths: &Paths, message: &str) -> Result<()> {
    ensure_repo(paths)?;
    let add = git(paths, &["add", "-A"])?;
    if !add.status.success() {
        bail!("git add failed: {}", String::from_utf8_lossy(&add.stderr));
    }
    let out = git(paths, &["commit", "-q", "--allow-empty", "-m", message])?;
    if !out.status.success() {
        bail!(
            "git commit failed: {}",
            String::from_utf8_lossy(&out.stderr)
        );
    }
    Ok(())
}

/// Best-effort commit: never fails a caller; logs and moves on.
pub fn commit_soft(paths: &Paths, message: &str) {
    if let Err(e) = commit(paths, message) {
        tracing::warn!("config history commit skipped: {e:#}");
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct HistoryEntry {
    pub hash: String,
    pub timestamp: i64,
    pub message: String,
}

/// The commit log, newest first (up to `limit`).
pub fn log(paths: &Paths, limit: usize) -> Result<Vec<HistoryEntry>> {
    if !paths.data_dir.join(".git").exists() {
        return Ok(Vec::new());
    }
    let out = git(
        paths,
        &["log", &format!("-n{limit}"), "--format=%H%x1f%ct%x1f%s"],
    )?;
    if !out.status.success() {
        // Fresh repo with no commits yet.
        return Ok(Vec::new());
    }
    let text = String::from_utf8_lossy(&out.stdout);
    let mut entries = Vec::new();
    for line in text.lines() {
        let mut parts = line.split('\u{1f}');
        let (Some(hash), Some(ts), Some(msg)) = (parts.next(), parts.next(), parts.next()) else {
            continue;
        };
        entries.push(HistoryEntry {
            hash: hash.to_string(),
            timestamp: ts.parse().unwrap_or(0),
            message: msg.to_string(),
        });
    }
    Ok(entries)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn commit_and_log() {
        if !available() {
            return; // git not on PATH in this environment; skip
        }
        let tmp = tempfile::TempDir::new().unwrap();
        let paths = Paths::new(tmp.path().to_path_buf());
        paths.ensure().unwrap();
        std::fs::write(paths.config_file(), "services = []\n").unwrap();

        commit(&paths, "generation #1: first").unwrap();
        std::fs::write(paths.config_file(), "services = []\n# changed\n").unwrap();
        commit(&paths, "generation #2: second").unwrap();

        let entries = log(&paths, 10).unwrap();
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].message, "generation #2: second");
        assert_eq!(entries[1].message, "generation #1: first");

        // secrets must never be tracked
        std::fs::create_dir_all(paths.data_dir.join("secrets")).unwrap();
        std::fs::write(paths.data_dir.join("secrets/token"), "s3cret").unwrap();
        commit(&paths, "generation #3").unwrap();
        let ls = git(&paths, &["ls-files"]).unwrap();
        let tracked = String::from_utf8_lossy(&ls.stdout);
        assert!(
            !tracked.contains("secrets"),
            "secrets leaked into git: {tracked}"
        );
    }
}
