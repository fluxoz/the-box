//! Client-side-encrypted DATA backups via restic, to a user-provided backend
//! (bring-your-own free tier) or Box Cloud. What to back up is derived from
//! the service manifest — each service's declared state dirs plus the
//! database dumps — so adding a stateful service protects its data with no
//! path list to maintain. Config, secrets and identity are deliberately NOT
//! here: they travel in the config repo (a git remote the operator owns), and
//! the OS rebuilds from them. Config in git + data in storage = a
//! recreatable Box.
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
    // DATA only. The Box's config, secrets and identity travel in the config
    // repo (the git remote the operator owns) — that is the recreate half.
    // When the data dir and /etc/box rode in here too, "backup" meant
    // everything and nothing, and restoring was archaeology. The contract is
    // now one sentence: config in git + data in storage = a recreatable Box.
    let mut p = vec![crate::dumps::dumps_dir(paths)];
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

/// `--include` list for the config scope — meaningful only against OLD
/// snapshots (made before backups became data-only). Kept so a pre-doctrine
/// snapshot can still be mined; new snapshots simply contain none of this.
pub fn config_includes(paths: &Paths) -> Vec<String> {
    vec![paths.data_dir.display().to_string(), "/etc/box".to_string()]
}

/// Resolve a restore scope into the `--include` filters restic should apply.
///
/// An EMPTY filter list means "restore this entire snapshot", which — written
/// back over a live `/` — is the most destructive thing this daemon can do. So
/// it is reachable only through the explicit `all` scope. A misspelled service
/// name, an empty scope, or a service with no backed-up state is refused, never
/// silently widened into a full-system restore.
pub fn resolve_scope(paths: &Paths, config: &BoxConfig, scope: &str) -> Result<Vec<String>> {
    match scope.trim() {
        "all" => Ok(Vec::new()),
        "" => bail!("a restore scope is required: \"all\", \"config\", or the name of a service"),
        // Config lives in the config repo now (git remote, Recreate); this
        // scope only finds anything in snapshots made before that split.
        "config" => Ok(config_includes(paths)),
        svc => {
            if config.find(svc).is_none() {
                bail!(
                    "unknown restore scope {svc:?} — expected \"all\", \"config\", or a deployed service name"
                );
            }
            let includes = service_includes(config, svc);
            if includes.is_empty() {
                bail!(
                    "service {svc:?} has no backed-up state to restore (it re-materializes from config) — \
                     use the \"config\" scope, or \"all\" to restore the whole snapshot"
                );
            }
            Ok(includes)
        }
    }
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
/// What a curl failure against the endpoint means, in words an operator can
/// act on. Split out pure so the R2 case stays tested.
pub fn endpoint_diagnosis(curl_stderr: &str) -> &'static str {
    if curl_stderr.contains("TLS") || curl_stderr.contains("SSL") {
        // Seen live: a fresh Cloudflare account with R2 keys but R2 never
        // activated has NO working TLS on its S3 endpoint at all.
        " The endpoint refused TLS entirely. For Cloudflare R2 this usually \
         means R2 was never enabled on the account — that is a one-time step \
         in Cloudflare's dashboard, not something a key can do."
    } else if curl_stderr.contains("resolve") {
        " The hostname does not resolve — check the endpoint for typos."
    } else {
        ""
    }
}

/// Prove the S3 endpoint answers HTTPS at all before handing it to restic.
/// restic retries backend errors with exponential backoff and no output, so a
/// dead endpoint looks like a hung backup for fifteen minutes — live, it was
/// an R2 endpoint whose TLS did not exist yet, and the first `backup_now` sat
/// silent past a ten-minute timeout. Any HTTP answer (403 included) passes;
/// only "cannot even talk to it" fails here, in seconds, with a diagnosis.
fn probe_s3_endpoint(b: &BackendConfig) -> Result<()> {
    if b.kind != "s3" {
        return Ok(());
    }
    let Some(ep) = b.endpoint.as_deref().filter(|e| !e.is_empty()) else {
        return Ok(()); // repo_url reports the missing field better
    };
    let url = if ep.contains("://") {
        ep.to_string()
    } else {
        format!("https://{ep}")
    };
    let out = std::process::Command::new("curl")
        .args(["-sS", "-m", "10", "-o", "/dev/null", &url])
        .output()
        .context("running curl to probe the backup endpoint")?;
    if out.status.success() {
        return Ok(());
    }
    let err = String::from_utf8_lossy(&out.stderr).trim().to_string();
    bail!(
        "the backup endpoint {url} is not reachable: {err}.{}",
        endpoint_diagnosis(&err)
    )
}

pub fn ensure_init(paths: &Paths, bc: &BackupConfig) -> Result<()> {
    probe_s3_endpoint(&bc.backend)?;
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
    // Databases first: dump through their own tooling, fail loudly if a dump
    // fails. The dumps dir then rides in the backup set.
    for line in crate::dumps::run_dumps(paths, config)? {
        println!("{line}");
    }
    let targets = backup_paths(paths, config, bc);
    if targets.is_empty() {
        bail!("nothing to back up (no service data exists yet)");
    }
    // --json so every consumer up the chain gets the truth as it happens:
    // status lines carry real (bytes_done, total_bytes) — the console's
    // progress bar — and the summary carries what a snapshot actually holds.
    // stdout passes through (the root unit's stdout is the journal, which the
    // console job tails); stderr is kept for the failure message.
    let mut c = restic(paths, bc)?;
    c.args(["backup", "--json", "--tag", "box"]);
    for t in &targets {
        c.arg(t);
    }
    c.stdout(std::process::Stdio::inherit());
    c.stderr(std::process::Stdio::piped());
    let mut child = c.spawn().context("starting restic")?;
    let mut stderr_tail = String::new();
    if let Some(mut e) = child.stderr.take() {
        use std::io::Read;
        let _ = e.read_to_string(&mut stderr_tail);
        // restic reports unreadable files here; surface them in the journal
        // even on success-adjacent paths.
        for l in stderr_tail.lines() {
            eprintln!("{l}");
        }
    }
    let status = child.wait().context("waiting for restic")?;
    if !status.success() {
        // Exit 3 is restic's "snapshot saved but some files were unreadable".
        // That USED to pass silently as a saved snapshot; a backup that is
        // missing files is a failed backup, full stop.
        let tail: String = stderr_tail
            .lines()
            .rev()
            .take(8)
            .collect::<Vec<_>>()
            .into_iter()
            .rev()
            .collect::<Vec<_>>()
            .join("\n");
        bail!(
            "restic backup failed (exit {:?}) — nothing was marked as backed up.\n{}",
            status.code(),
            tail
        );
    }
    // Cheap marker so health can report backup freshness without shelling out
    // to restic on every (public, frequent) /health poll. The scheduled backup
    // runs as root (it must read root-owned state), so hand the marker back to
    // the data dir's owner or the boxd service can't refresh it later.
    let marker = last_backup_marker(paths);
    let _ = std::fs::write(&marker, chrono::Utc::now().to_rfc3339());
    crate::util::chown_like(&paths.data_dir, &marker);
    crate::journal::record(
        paths,
        "backup",
        format!(
            "backed up {} paths to the {} repository",
            targets.len(),
            bc.backend.kind
        ),
    );
    prune(paths, bc)
}

/// One line of the backup runner's output, reduced to what a job view needs.
/// restic's `--json` stream is chatty; only three things matter to a person:
/// how far along it is, what it did, and what went wrong.
pub enum ResticEvent {
    /// Real progress: (bytes_done, total_bytes) — the measured bar.
    Progress(u64, u64),
    /// A line worth keeping in the job log.
    Line(String),
    /// Chatter (per-second repeats, empty lines) — drop it.
    Noise,
}

pub fn restic_json_event(line: &str) -> ResticEvent {
    let Ok(v) = serde_json::from_str::<serde_json::Value>(line) else {
        let t = line.trim();
        return if t.is_empty() {
            ResticEvent::Noise
        } else {
            ResticEvent::Line(t.to_string())
        };
    };
    match v.get("message_type").and_then(|m| m.as_str()) {
        Some("status") => ResticEvent::Progress(
            v["bytes_done"].as_u64().unwrap_or(0),
            v["total_bytes"].as_u64().unwrap_or(0),
        ),
        Some("summary") => ResticEvent::Line(format!(
            "backed up {} files, {:.1} MiB processed, {:.1} MiB added",
            v["total_files_processed"].as_u64().unwrap_or(0),
            v["total_bytes_processed"].as_u64().unwrap_or(0) as f64 / 1048576.0,
            v["data_added"].as_u64().unwrap_or(0) as f64 / 1048576.0,
        )),
        Some("error") => ResticEvent::Line(format!(
            "error: {}",
            v["error"]["message"].as_str().unwrap_or("unknown")
        )),
        _ => ResticEvent::Noise,
    }
}

/// Run a backup with the privileges it actually needs. On a Box, that is the
/// root on-demand unit (container volumes belong to container uids; dumps
/// exec into root-run containers) with its journal tailed into the job —
/// progress lines become the measured bar, everything else becomes the log.
/// Off-Box (dev, tests, a root shell) it runs in-process.
pub fn run_for_job(
    paths: &Paths,
    config: &BoxConfig,
    bc: &BackupConfig,
    progress: &crate::jobs::Progress,
) -> Result<()> {
    if crate::ostier::unit_available(crate::ostier::BACKUP_NOW_UNIT) {
        progress.phase("handing the backup to the system runner");
        let p = progress.clone();
        return crate::ostier::run_unit_streaming(crate::ostier::BACKUP_NOW_UNIT, move |l| {
            match restic_json_event(l) {
                ResticEvent::Progress(done, total) if total > 0 => {
                    crate::store::BuildWatch::units(&p, done, total)
                }
                ResticEvent::Line(text) => p.log(text),
                _ => {}
            }
        });
    }
    run(paths, config, bc)
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
        Some(t) => {
            chrono::Utc::now().signed_duration_since(t.with_timezone(&chrono::Utc)) >= interval
        }
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

    #[test]
    fn restic_json_lines_become_progress_log_or_silence() {
        match restic_json_event(r#"{"message_type":"status","bytes_done":5,"total_bytes":10}"#) {
            ResticEvent::Progress(5, 10) => {}
            _ => panic!("status must become progress"),
        }
        match restic_json_event(
            r#"{"message_type":"summary","total_files_processed":9,"total_bytes_processed":2097152,"data_added":1048576}"#,
        ) {
            ResticEvent::Line(l) => assert!(l.contains("9 files") && l.contains("2.0 MiB"), "{l}"),
            _ => panic!("summary must become a log line"),
        }
        match restic_json_event("postgres: postgres dumped (1234 bytes)") {
            ResticEvent::Line(l) => assert!(l.contains("dumped")),
            _ => panic!("plain lines pass through"),
        }
        assert!(matches!(restic_json_event(""), ResticEvent::Noise));
        assert!(matches!(
            restic_json_event(r#"{"message_type":"verbose_status"}"#),
            ResticEvent::Noise
        ));
    }

    #[test]
    fn a_tls_failure_names_the_r2_activation_step() {
        // The live failure: curl exit 35 against an R2 endpoint on an account
        // where R2 was never enabled. The words must point at the dashboard
        // step, because no key or retry can fix it.
        let d = endpoint_diagnosis("curl: (35) TLS connect error: error:0A000410:SSL routines::ssl/tls alert handshake failure");
        assert!(d.contains("R2 was never enabled"), "{d}");
        let d = endpoint_diagnosis("curl: (6) Could not resolve host: nope.example");
        assert!(d.contains("does not resolve"), "{d}");
        assert_eq!(endpoint_diagnosis("curl: (7) Failed to connect"), "");
    }

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
        assert_eq!(
            repo_url(&b).unwrap(),
            "rest:https://user:pass@rest.example/box"
        );
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
        // The doctrine split: config/secrets/identity live in the config
        // repo, NOT the storage backup. The data dir must never ride along.
        assert!(
            !got.contains(&data),
            "the data dir is config-repo territory"
        );
        assert!(got.contains(&extra_present), "includes existing extras");
        assert!(
            !got.iter()
                .any(|p| p.to_string_lossy().contains("nonexistent")),
            "drops paths that don't exist yet"
        );

        // Dumps ride along once they exist.
        let dumps = crate::dumps::dumps_dir(&paths);
        std::fs::create_dir_all(&dumps).unwrap();
        let got = backup_paths(&paths, &config, &b);
        assert!(got.contains(&dumps), "database dumps are part of the data");
    }
}
