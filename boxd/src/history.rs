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
# boxd: track declarative config + encrypted (.age) secrets only — never
# plaintext secrets, the box identity key, or heavy runtime state.
/secrets/*
!/secrets/*.age
!/secrets/op/
/secrets/op/*
!/secrets/op/*.age
/store/
/profiles/
/generation-src/
/logs/
/network.toml
/auth.json
/config-remote
";

fn git(paths: &Paths, args: &[&str]) -> Result<std::process::Output> {
    let out = Command::new("git")
        .arg("-C")
        .arg(&paths.data_dir)
        // Don't depend on a global git identity being configured.
        .args(["-c", "user.name=boxd", "-c", "user.email=boxd@localhost"])
        // The data dir may be owned by the boxd service user while git runs as
        // another (e.g. root during a restore); don't let git's ownership guard
        // refuse to operate on our own repo.
        .arg("-c")
        .arg(format!("safe.directory={}", paths.data_dir.display()))
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

/// Best-effort commit: never fails a caller; logs and moves on. Also pushes the
/// config repo to its remote (offsite backup) if one is set.
pub fn commit_soft(paths: &Paths, message: &str) {
    if let Err(e) = commit(paths, message) {
        tracing::warn!("config history commit skipped: {e:#}");
        return;
    }
    push_soft(paths);
}

/// Set (or clear, with `None`) the git remote the config repo is pushed to as an
/// offsite, user-owned backup.
pub fn set_remote(paths: &Paths, url: Option<&str>) -> Result<()> {
    match url {
        Some(u) => std::fs::write(paths.config_remote_file(), u.trim())?,
        None => {
            let _ = std::fs::remove_file(paths.config_remote_file());
        }
    }
    Ok(())
}

/// The configured config-repo remote, if any.
pub fn remote(paths: &Paths) -> Option<String> {
    std::fs::read_to_string(paths.config_remote_file())
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
}

/// Push the config repo (config + encrypted `.age` secrets) to its remote.
/// No-op when no remote is set.
pub fn push(paths: &Paths) -> Result<()> {
    let Some(url) = remote(paths) else {
        return Ok(());
    };
    ensure_repo(paths)?;
    // Point origin at the current URL (idempotent) and push main.
    let _ = git(paths, &["remote", "remove", "origin"]);
    let add = git(paths, &["remote", "add", "origin", &url])?;
    if !add.status.success() {
        bail!(
            "git remote add failed: {}",
            String::from_utf8_lossy(&add.stderr)
        );
    }
    let out = git(paths, &["push", "-q", "--force", "origin", "main"])?;
    if !out.status.success() {
        bail!("git push failed: {}", String::from_utf8_lossy(&out.stderr));
    }
    Ok(())
}

/// Populate a fresh config repo from a remote: fetch its `main` and check it out
/// into the data dir. Used by restore — brings back box.toml, sources, and the
/// encrypted `.age` secrets. The data dir must not already hold a tracked config.
pub fn fetch_checkout(paths: &Paths, url: &str) -> Result<()> {
    ensure_repo(paths)?;
    let _ = git(paths, &["remote", "remove", "origin"]);
    let add = git(paths, &["remote", "add", "origin", url])?;
    if !add.status.success() {
        bail!(
            "git remote add failed: {}",
            String::from_utf8_lossy(&add.stderr)
        );
    }
    let fetch = git(paths, &["fetch", "-q", "origin", "main"])?;
    if !fetch.status.success() {
        bail!(
            "git fetch from {url} failed: {}",
            String::from_utf8_lossy(&fetch.stderr)
        );
    }
    let co = git(paths, &["checkout", "-f", "-B", "main", "origin/main"])?;
    if !co.status.success() {
        bail!(
            "git checkout failed: {}",
            String::from_utf8_lossy(&co.stderr)
        );
    }
    Ok(())
}

/// Best-effort push: never fails a caller.
pub fn push_soft(paths: &Paths) {
    if remote(paths).is_none() {
        return;
    }
    if let Err(e) = push(paths) {
        tracing::warn!("config push skipped: {e:#}");
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

        // Encrypted (.age) secrets travel with the config; plaintext never does.
        std::fs::create_dir_all(paths.data_dir.join("secrets")).unwrap();
        std::fs::write(paths.data_dir.join("secrets/token"), "s3cret").unwrap();
        std::fs::write(paths.data_dir.join("secrets/db-env.age"), "ciphertext").unwrap();
        commit(&paths, "generation #3").unwrap();
        let ls = git(&paths, &["ls-files"]).unwrap();
        let tracked = String::from_utf8_lossy(&ls.stdout);
        assert!(
            tracked.contains("secrets/db-env.age"),
            "encrypted secret must be tracked: {tracked}"
        );
        assert!(
            !tracked.contains("secrets/token"),
            "plaintext secret leaked into git: {tracked}"
        );
    }

    #[test]
    fn push_ships_config_and_encrypted_secrets() {
        if !available() {
            return;
        }
        let tmp = tempfile::TempDir::new().unwrap();
        let paths = Paths::new(tmp.path().join("box"));
        paths.ensure().unwrap();
        let remote = tmp.path().join("remote.git");
        Command::new("git")
            .args(["init", "-q", "--bare"])
            .arg(&remote)
            .output()
            .unwrap();

        std::fs::write(paths.config_file(), "services = []\n").unwrap();
        std::fs::create_dir_all(paths.data_dir.join("secrets")).unwrap();
        std::fs::write(paths.data_dir.join("secrets/db-env.age"), "ciphertext").unwrap();
        std::fs::write(paths.data_dir.join("secrets/backup-pw"), "PLAINTEXT").unwrap();
        commit(&paths, "gen 1").unwrap();

        set_remote(&paths, Some(&format!("file://{}", remote.display()))).unwrap();
        assert_eq!(
            remote_of(&paths).as_deref(),
            Some(format!("file://{}", remote.display()).as_str())
        );
        push(&paths).unwrap();

        // The remote has the config + the .age, and NOT the plaintext secret.
        let out = Command::new("git")
            .arg("--git-dir")
            .arg(&remote)
            .args(["ls-tree", "-r", "--name-only", "main"])
            .output()
            .unwrap();
        let listing = String::from_utf8_lossy(&out.stdout);
        assert!(listing.contains("box.toml"), "{listing}");
        assert!(listing.contains("secrets/db-env.age"), "{listing}");
        assert!(!listing.contains("backup-pw"), "plaintext pushed: {listing}");
    }

    // Alias so the test reads clearly next to set_remote.
    fn remote_of(paths: &Paths) -> Option<String> {
        remote(paths)
    }
}
