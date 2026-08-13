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
/channel.toml
/.gitconfig
/store/
/profiles/
/generation-src/
/logs/
# Repo-linked services: cloned git dirs and their checkouts are caches of
# someone ELSE'S repository. Committing them here would copy a person's
# source tree into their own config history on every deploy.
/repos/
/repo-trees/
# Job records: operational state, not configuration.
/jobs/
/network.toml
/auth.json
/config-remote
";

fn git(paths: &Paths, args: &[&str]) -> Result<std::process::Output> {
    let out = Command::new("git")
        // The service runs with HOME set to the data dir, so a `.gitconfig`
        // that arrives IN a cloned repo would be read as git's global config —
        // and `core.hooksPath` plus a tracked executable hook is then arbitrary
        // code execution as the boxd user on the next commit. Ignore external
        // config entirely and pin hooks off; everything we need is passed with
        // -c below.
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .arg("-C")
        .arg(&paths.data_dir)
        .args(["-c", "core.hooksPath=/dev/null"])
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

/// The only paths a restore is allowed to take from a config repo. Everything
/// else in the data dir is this Box's own and must survive untouched.
///
/// This list is a security boundary, not tidiness. A full `checkout -f` of a
/// remote tree would let whoever controls that repo overwrite `auth.json`
/// (their session hash becomes a live operator, yours are revoked), rewrite
/// `channel.toml` (they own your update channel, therefore root), drop a
/// `.gitconfig` (HOME is the data dir, so `core.hooksPath` is code execution),
/// replace `secrets/boxd-identity.key` (they own your operational secrets), and
/// delete `.gitignore` so the next push ships your plaintext to them.
const RESTORE_PATHS: &[&str] = &["box.toml", "sources", "secrets"];

/// Populate this Box's config from a remote: fetch its `main` and take only
/// [`RESTORE_PATHS`] out of it. Used by restore — brings back box.toml, the
/// service sources, and the encrypted `.age` secrets, and nothing else.
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

    // Path-limited checkout. A path missing from their tree is not an error —
    // a config repo with no secrets is perfectly normal.
    let mut took_any = false;
    for path in RESTORE_PATHS {
        let co = git(paths, &["checkout", "-f", "origin/main", "--", path])?;
        if co.status.success() {
            took_any = true;
        }
    }
    if !took_any {
        bail!("nothing to restore: {url} has no box.toml, sources or secrets on main");
    }

    // Adopt their history as our parent so later pushes fast-forward rather
    // than force-rewriting the operator's repo, while the working tree stays
    // the filtered one we just built.
    let _ = git(paths, &["reset", "--soft", "origin/main"]);

    // Our ignore rules, never theirs: an incoming tree without .gitignore would
    // otherwise leave the data dir unfiltered, and the next push would ship
    // plaintext secrets and auth.json to that remote.
    std::fs::write(paths.data_dir.join(".gitignore"), GITIGNORE)?;

    // Secrets travel only as ciphertext. Anything else that arrived under
    // secrets/ (a plaintext file, or a substituted box identity key) is dropped.
    prune_non_age_secrets(&paths.data_dir.join("secrets"))?;
    Ok(())
}

/// Remove everything under a restored `secrets/` tree that is not a `.age`
/// file, so a hostile repo cannot install its own box identity or plant
/// plaintext that a later push would leak back.
fn prune_non_age_secrets(dir: &std::path::Path) -> Result<()> {
    if !dir.is_dir() {
        return Ok(());
    }
    for entry in std::fs::read_dir(dir)? {
        let path = entry?.path();
        if path.is_dir() {
            prune_non_age_secrets(&path)?;
        } else if path.extension().and_then(|e| e.to_str()) != Some("age") {
            tracing::warn!("restore: dropping non-encrypted {}", path.display());
            let _ = std::fs::remove_file(&path);
        }
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
