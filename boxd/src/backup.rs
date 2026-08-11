//! Client-side-encrypted backups via restic, to a user-provided backend (the
//! bring-your-own free tier). What to back up is derived from the service
//! manifest — the Box's config/secrets/auth plus each service's declared state
//! dirs — so adding a stateful service protects its data with no path list to
//! maintain. The Nix store and generations are deliberately NOT backed up: they
//! rebuild from the (backed-up) config.
//!
//! The repo password is generated on the Box, shown once for the recovery kit,
//! and never leaves. We hand restic the *storage* credentials, never the key —
//! so a backup is unreadable to us, and unrecoverable without the user's key.

use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{anyhow, bail, Context, Result};
use serde::{Deserialize, Serialize};

use crate::config::{BackendConfig, BackupConfig, BoxConfig};
use crate::paths::Paths;
use crate::{secrets, templates};

/// Secret holding the restic repository password (the encryption key).
pub const PW_SECRET: &str = "backup-password";
const S3_KEY: &str = "backup-s3-access-key";
const S3_SECRET: &str = "backup-s3-secret-key";

// ---- key / recovery ------------------------------------------------------

/// Generate the repo password, store it, and return it once for the recovery
/// kit. Refuses to overwrite an existing key (that would orphan old backups).
pub fn init_key(paths: &Paths) -> Result<String> {
    if has_key(paths) {
        bail!("a backup key already exists; refusing to replace it (that would orphan existing backups)");
    }
    let key = random_hex(32)?;
    secrets::set(paths, PW_SECRET, &key)?;
    Ok(key)
}

pub fn has_key(paths: &Paths) -> bool {
    secrets::exists(paths, PW_SECRET)
}

fn random_hex(bytes: usize) -> Result<String> {
    let mut buf = vec![0u8; bytes];
    std::fs::File::open("/dev/urandom")
        .and_then(|mut f| f.read_exact(&mut buf))
        .context("reading /dev/urandom")?;
    Ok(buf.iter().map(|b| format!("{b:02x}")).collect())
}

// ---- what to back up (manifest-derived) ----------------------------------

/// Always the Box's config/secrets/auth, plus every service's declared state
/// dirs, plus explicit extras. Missing paths are dropped (a service may not
/// have created its dir yet).
pub fn backup_paths(paths: &Paths, config: &BoxConfig, bc: &BackupConfig) -> Vec<PathBuf> {
    let mut p = vec![paths.data_dir.clone(), PathBuf::from("/etc/box")];
    p.extend(service_state_dirs(config, None));
    p.extend(bc.extra_paths.iter().map(PathBuf::from));
    p.sort();
    p.dedup();
    p.retain(|d| d.exists());
    p
}

/// State dirs for one service (`Some(name)`) or all services (`None`).
fn service_state_dirs(config: &BoxConfig, only: Option<&str>) -> Vec<PathBuf> {
    config
        .services
        .iter()
        .filter(|s| only.is_none_or(|n| s.name == n))
        .filter_map(|s| templates::get(&s.template).map(|t| t.state_dirs(&s.name, &s.params)))
        .flatten()
        .collect()
}

/// `--include` list to restore just one service's data (its state dirs).
pub fn service_includes(config: &BoxConfig, name: &str) -> Vec<String> {
    service_state_dirs(config, Some(name))
        .iter()
        .map(|p| p.display().to_string())
        .collect()
}

/// `--include` list to restore just the Box's config/secrets/auth.
pub fn config_includes(paths: &Paths) -> Vec<String> {
    vec![paths.data_dir.display().to_string(), "/etc/box".to_string()]
}

// ---- restic plumbing -----------------------------------------------------

fn repo_url(b: &BackendConfig) -> Result<String> {
    let req = |v: &Option<String>, f: &str| {
        v.clone()
            .filter(|s| !s.is_empty())
            .ok_or_else(|| anyhow!("backup backend '{}' is missing '{}'", b.kind, f))
    };
    Ok(match b.kind.as_str() {
        "local" => req(&b.path, "path")?,
        "s3" => {
            // Keep the endpoint verbatim: restic infers TLS from it —
            // "s3.us-west-002.backblazeb2.com" (https) vs "http://host:9000"
            // (a plain-HTTP MinIO). Stripping the scheme breaks the latter.
            let ep = req(&b.endpoint, "endpoint")?;
            let mut u = format!("s3:{ep}/{}", req(&b.bucket, "bucket")?);
            if let Some(prefix) = b.prefix.as_deref().filter(|p| !p.is_empty()) {
                u.push('/');
                u.push_str(prefix);
            }
            u
        }
        "sftp" => format!(
            "sftp:{}@{}:{}",
            req(&b.user, "user")?,
            req(&b.host, "host")?,
            req(&b.path, "path")?
        ),
        "rest" => req(&b.url, "url")?,
        other => bail!("unknown backup backend {other:?} (use local | s3 | sftp | rest)"),
    })
}

/// A restic command with repo + password + backend credentials wired from the
/// secret store.
fn restic(paths: &Paths, bc: &BackupConfig) -> Result<Command> {
    if !has_key(paths) {
        bail!("no backup key set — run `boxd backup init` first");
    }
    let mut c = Command::new("restic");
    c.env("RESTIC_REPOSITORY", repo_url(&bc.backend)?);
    // The password is encrypted at rest; hand restic a command that decrypts it
    // on demand (via the box identity) rather than a plaintext file. No plaintext
    // ever hits disk.
    let exe = std::env::current_exe().context("locating boxd for RESTIC_PASSWORD_COMMAND")?;
    c.env(
        "RESTIC_PASSWORD_COMMAND",
        format!(
            "'{}' --data-dir '{}' secret print {}",
            exe.display(),
            paths.data_dir.display(),
            PW_SECRET
        ),
    );
    if bc.backend.kind == "s3" {
        if let Some(k) = secrets::get(paths, S3_KEY)? {
            c.env("AWS_ACCESS_KEY_ID", k);
        }
        if let Some(s) = secrets::get(paths, S3_SECRET)? {
            c.env("AWS_SECRET_ACCESS_KEY", s);
        }
    }
    Ok(c)
}

// ---- operations ----------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Snapshot {
    pub id: String,
    pub time: String,
    #[serde(default)]
    pub paths: Vec<String>,
    #[serde(default)]
    pub tags: Vec<String>,
}

/// Initialize the repository if it isn't already (idempotent).
pub fn ensure_init(paths: &Paths, bc: &BackupConfig) -> Result<()> {
    let exists = restic(paths, bc)?
        .args(["cat", "config"])
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false);
    if !exists {
        let out = restic(paths, bc)?.arg("init").output()?;
        if !out.status.success() {
            bail!(
                "restic init failed: {}",
                String::from_utf8_lossy(&out.stderr)
            );
        }
    }
    Ok(())
}

/// Take a snapshot, then apply the retention policy.
pub fn run(paths: &Paths, config: &BoxConfig, bc: &BackupConfig) -> Result<()> {
    ensure_init(paths, bc)?;
    let targets = backup_paths(paths, config, bc);
    if targets.is_empty() {
        bail!("nothing to back up (no config/state dirs exist yet)");
    }
    let mut c = restic(paths, bc)?;
    c.args(["backup", "--tag", "box"]);
    for t in &targets {
        c.arg(t);
    }
    if !c.status()?.success() {
        bail!("restic backup failed");
    }
    // Cheap marker so health can report backup freshness without shelling out
    // to restic on every (public, frequent) /health poll.
    let _ = std::fs::write(last_backup_marker(paths), chrono::Utc::now().to_rfc3339());
    prune(paths, bc)
}

/// Path of the "last successful backup" timestamp marker.
pub fn last_backup_marker(paths: &Paths) -> PathBuf {
    paths.data_dir.join("last-backup")
}

/// Age of the last successful backup, read from the marker (cheap). `None` if
/// never backed up.
pub fn last_backup_age(paths: &Paths) -> Option<chrono::Duration> {
    let ts = std::fs::read_to_string(last_backup_marker(paths)).ok()?;
    let t = chrono::DateTime::parse_from_rfc3339(ts.trim()).ok()?;
    Some(chrono::Utc::now().signed_duration_since(t.with_timezone(&chrono::Utc)))
}

pub fn prune(paths: &Paths, bc: &BackupConfig) -> Result<()> {
    let r = &bc.retention;
    let ok = restic(paths, bc)?
        .args([
            "forget",
            "--prune",
            "--keep-daily",
            &r.daily.to_string(),
            "--keep-weekly",
            &r.weekly.to_string(),
            "--keep-monthly",
            &r.monthly.to_string(),
        ])
        .status()?
        .success();
    if !ok {
        bail!("restic forget/prune failed");
    }
    Ok(())
}

pub fn snapshots(paths: &Paths, bc: &BackupConfig) -> Result<Vec<Snapshot>> {
    let out = restic(paths, bc)?.args(["snapshots", "--json"]).output()?;
    if !out.status.success() {
        bail!(
            "restic snapshots failed: {}",
            String::from_utf8_lossy(&out.stderr)
        );
    }
    Ok(serde_json::from_slice(&out.stdout).unwrap_or_default())
}

/// Restore a snapshot (default `latest`) to `target`, optionally limited to
/// `includes` (a service's or the config's paths).
pub fn restore(
    paths: &Paths,
    bc: &BackupConfig,
    snapshot: &str,
    target: &Path,
    includes: &[String],
) -> Result<()> {
    let mut c = restic(paths, bc)?;
    c.arg("restore").arg(snapshot).arg("--target").arg(target);
    for inc in includes {
        c.arg("--include").arg(inc);
    }
    if !c.status()?.success() {
        bail!("restic restore failed");
    }
    Ok(())
}

pub fn check(paths: &Paths, bc: &BackupConfig) -> Result<()> {
    if !restic(paths, bc)?.arg("check").status()?.success() {
        bail!("restic check failed");
    }
    Ok(())
}

#[derive(Debug, Clone, Serialize)]
pub struct Status {
    pub reachable: bool,
    pub count: usize,
    pub last: Option<Snapshot>,
}

/// Whether a scheduled backup should run now: enabled, and either no snapshot
/// yet or the schedule interval has elapsed since the last one. Unknown/cron
/// schedules fall back to daily (the heartbeat timer runs hourly regardless).
pub fn is_due(bc: &BackupConfig, last: Option<&Snapshot>) -> bool {
    if !bc.enabled {
        return false;
    }
    let interval = match bc.schedule.as_str() {
        "hourly" => chrono::Duration::hours(1),
        "weekly" => chrono::Duration::weeks(1),
        _ => chrono::Duration::days(1),
    };
    match last.and_then(|s| chrono::DateTime::parse_from_rfc3339(&s.time).ok()) {
        None => true,
        Some(t) => chrono::Utc::now().signed_duration_since(t.with_timezone(&chrono::Utc)) >= interval,
    }
}

/// Best-effort status — reachability + newest snapshot. Never errors (the
/// dashboard/health want a value even when the backend is down).
pub fn status(paths: &Paths, bc: &BackupConfig) -> Status {
    match snapshots(paths, bc) {
        Ok(mut snaps) => {
            snaps.sort_by(|a, b| a.time.cmp(&b.time));
            Status {
                reachable: true,
                count: snaps.len(),
                last: snaps.pop(),
            }
        }
        Err(_) => Status {
            reachable: false,
            count: 0,
            last: None,
        },
    }
}

#[cfg(test)]
mod tests {
    //! Pure-logic tests for the two things that decide *where* bytes land and
    //! *when* a backup runs — no restic, no network. A wrong repo URL silently
    //! writes to the wrong place (or a plain-HTTP endpoint fails), and a wrong
    //! due-check either skips backups or hammers the backend.
    use super::*;
    use crate::config::Retention;

    fn backend(kind: &str) -> BackendConfig {
        BackendConfig {
            kind: kind.into(),
            ..Default::default()
        }
    }

    #[test]
    fn repo_url_local() {
        let mut b = backend("local");
        b.path = Some("/srv/backups".into());
        assert_eq!(repo_url(&b).unwrap(), "/srv/backups");
    }

    #[test]
    fn repo_url_s3_keeps_endpoint_scheme_verbatim() {
        // Backblaze-style HTTPS host: no scheme -> restic uses TLS.
        let mut b = backend("s3");
        b.endpoint = Some("s3.us-west-002.backblazeb2.com".into());
        b.bucket = Some("mybucket".into());
        assert_eq!(
            repo_url(&b).unwrap(),
            "s3:s3.us-west-002.backblazeb2.com/mybucket"
        );

        // Plain-HTTP MinIO/rclone: the http:// scheme MUST survive, or restic
        // tries TLS against a plaintext port. This is the regression we fixed.
        let mut b = backend("s3");
        b.endpoint = Some("http://127.0.0.1:9000".into());
        b.bucket = Some("box-backups".into());
        b.prefix = Some("acct-alice".into());
        assert_eq!(
            repo_url(&b).unwrap(),
            "s3:http://127.0.0.1:9000/box-backups/acct-alice"
        );
    }

    #[test]
    fn repo_url_s3_empty_prefix_omitted() {
        let mut b = backend("s3");
        b.endpoint = Some("host".into());
        b.bucket = Some("bkt".into());
        b.prefix = Some(String::new()); // empty -> no trailing slash
        assert_eq!(repo_url(&b).unwrap(), "s3:host/bkt");
    }

    #[test]
    fn repo_url_sftp_and_rest() {
        let mut b = backend("sftp");
        b.user = Some("box".into());
        b.host = Some("nas.local".into());
        b.path = Some("/backups/box".into());
        assert_eq!(repo_url(&b).unwrap(), "sftp:box@nas.local:/backups/box");

        let mut b = backend("rest");
        b.url = Some("rest:https://user:pass@rest.example/box".into());
        assert_eq!(repo_url(&b).unwrap(), "rest:https://user:pass@rest.example/box");
    }

    #[test]
    fn repo_url_reports_missing_fields_and_unknown_kind() {
        // s3 without an endpoint names the missing field and the backend.
        let e = repo_url(&backend("s3")).unwrap_err().to_string();
        assert!(e.contains("s3") && e.contains("endpoint"), "got: {e}");
        // Unknown kind is rejected, not silently treated as local.
        let e = repo_url(&backend("dropbox")).unwrap_err().to_string();
        assert!(e.contains("dropbox"), "got: {e}");
    }

    fn bc(enabled: bool, schedule: &str) -> BackupConfig {
        BackupConfig {
            enabled,
            schedule: schedule.into(),
            retention: Retention::default(),
            backend: backend("local"),
            extra_paths: vec![],
        }
    }

    fn snap_at(time: &str) -> Snapshot {
        Snapshot {
            id: "x".into(),
            time: time.into(),
            paths: vec![],
            tags: vec![],
        }
    }

    #[test]
    fn is_due_respects_enabled_flag() {
        assert!(!is_due(&bc(false, "daily"), None), "disabled -> never due");
    }

    #[test]
    fn is_due_true_when_never_backed_up() {
        assert!(is_due(&bc(true, "daily"), None));
    }

    #[test]
    fn is_due_by_schedule_interval() {
        let long_ago = "2000-01-01T00:00:00Z";
        let now = chrono::Utc::now().to_rfc3339();
        // Daily: an old snapshot is due, a fresh one is not.
        assert!(is_due(&bc(true, "daily"), Some(&snap_at(long_ago))));
        assert!(!is_due(&bc(true, "daily"), Some(&snap_at(&now))));

        // Hourly is due after >1h; use a snapshot ~2h old.
        let two_h_ago = (chrono::Utc::now() - chrono::Duration::hours(2)).to_rfc3339();
        assert!(is_due(&bc(true, "hourly"), Some(&snap_at(&two_h_ago))));
        // ...but the same 2h-old snapshot is NOT yet due on a weekly schedule.
        assert!(!is_due(&bc(true, "weekly"), Some(&snap_at(&two_h_ago))));
    }

    #[test]
    fn backup_paths_are_manifest_derived_and_pruned_to_existing() {
        let dir = tempfile::tempdir().unwrap();
        let data = dir.path().join("data");
        std::fs::create_dir_all(&data).unwrap();
        let extra_present = dir.path().join("present");
        std::fs::create_dir_all(&extra_present).unwrap();

        let paths = Paths::new(data.clone());
        let config = BoxConfig::default(); // no services
        let mut b = bc(true, "daily");
        b.extra_paths = vec![
            extra_present.display().to_string(),
            "/nonexistent/definitely/not/here".into(), // dropped
        ];

        let got = backup_paths(&paths, &config, &b);
        assert!(got.contains(&data), "always backs up the data dir");
        assert!(got.contains(&extra_present), "includes existing extras");
        assert!(
            !got.iter().any(|p| p.to_string_lossy().contains("nonexistent")),
            "drops paths that don't exist yet"
        );
    }
}
