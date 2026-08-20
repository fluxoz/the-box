//! Database-aware pre-backup dumps.
//!
//! File-copying a RUNNING database is not a backup — the files are mid-write
//! and restore to a corrupt engine, permissions aside. So before restic runs,
//! every service whose image says "database" gets dumped through its own
//! tooling (podman exec into the container), and the dump is what rides in
//! the snapshot alongside the volume files.
//!
//! The rules that make this trustworthy:
//!   * a dump that fails, times out, or writes zero bytes FAILS THE BACKUP —
//!     a database backup without the database is not a backup;
//!   * a database whose container is not running is skipped (its files are
//!     closed and consistent; there is nothing to exec into) and that skip is
//!     reported, never silent;
//!   * unknown images are not guessed at: they get file-level backup only,
//!     which is correct for everything that is not a database.

use std::path::PathBuf;
use std::process::Command;

use anyhow::{bail, Context, Result};

use crate::config::BoxConfig;
use crate::paths::Paths;

/// Hard ceiling per dump. A wedged database must fail the backup loudly, not
/// hold the nightly hostage.
const DUMP_TIMEOUT_SECS: u32 = 900;

/// Where dumps land; included in the backup set. Root-written during the
/// backup, handed back to the data dir's owner like the freshness marker.
pub fn dumps_dir(paths: &Paths) -> PathBuf {
    paths.data_dir.join("dumps")
}

/// What one service needs dumped: the command run INSIDE the container and
/// the file its stdout becomes.
#[derive(Debug, PartialEq)]
pub struct DumpPlan {
    pub engine: &'static str,
    /// Shell command executed inside the container; stdout is the dump.
    pub container_cmd: &'static str,
    pub file: String,
}

/// The last path segment of an image ref, without the tag — "postgres" from
/// "docker.io/library/postgres:16".
pub fn image_base(image: &str) -> &str {
    image
        .rsplit('/')
        .next()
        .unwrap_or(image)
        .split(':')
        .next()
        .unwrap_or(image)
}

/// (engine, the port it actually listens on) for unmistakable database
/// images — what the deploy form's "80 is typical" default must not be
/// allowed to sabotage. Matching is deliberately narrow, like [`plan_for`].
pub fn db_default_port(image: &str) -> Option<(&'static str, u16)> {
    Some(match image_base(image) {
        "postgres" | "postgresql" | "pgvector" | "timescaledb" => ("postgres", 5432),
        "mysql" | "mariadb" => ("mysql", 3306),
        "redis" | "valkey" => ("redis", 6379),
        "mongo" | "mongodb" => ("mongodb", 27017),
        "clickhouse" | "clickhouse-server" => ("clickhouse", 8123),
        "memcached" => ("memcached", 11211),
        "etcd" => ("etcd", 2379),
        _ => return None,
    })
}

/// The engine table. Matching is on the image name, deliberately narrow:
/// a false positive would exec nonsense inside someone's container, a false
/// negative just means file-level backup — so only unmistakable names match.
pub fn plan_for(name: &str, image: &str) -> Option<DumpPlan> {
    let (engine, container_cmd, ext) = match image_base(image) {
        "postgres" | "postgresql" | "pgvector" | "timescaledb" => (
            "postgres",
            // pg_dumpall carries roles + all databases; the image's own env
            // names the superuser.
            r#"pg_dumpall -U "${POSTGRES_USER:-postgres}""#,
            "pg_dumpall.sql",
        ),
        "mysql" | "mariadb" => (
            "mysql",
            r#"exec mysqldump --all-databases --single-transaction -uroot -p"${MYSQL_ROOT_PASSWORD:-$MARIADB_ROOT_PASSWORD}""#,
            "mysqldump.sql",
        ),
        "redis" | "valkey" => (
            "redis",
            // --rdb - streams a consistent point-in-time RDB to stdout.
            "redis-cli --rdb -",
            "rdb",
        ),
        _ => return None,
    };
    Some(DumpPlan {
        engine,
        container_cmd,
        file: format!("{name}.{ext}"),
    })
}

/// Dump every running database service. Returns human lines describing what
/// happened (dumped / skipped-not-running); any real failure aborts.
pub fn run_dumps(paths: &Paths, config: &BoxConfig) -> Result<Vec<String>> {
    let dir = dumps_dir(paths);
    std::fs::create_dir_all(&dir)?;
    crate::util::chown_like(&paths.data_dir, &dir);
    let mut report = Vec::new();
    for svc in &config.services {
        if svc.template != "container" {
            continue;
        }
        let image = svc
            .params
            .get("image")
            .and_then(|v| v.as_str())
            .unwrap_or("");
        let Some(plan) = plan_for(&svc.name, image) else {
            continue;
        };
        let unit_active = Command::new("systemctl")
            .args(["is-active", "--quiet", &format!("podman-{}", svc.name)])
            .status()
            .map(|s| s.success())
            .unwrap_or(false);
        if !unit_active {
            report.push(format!(
                "{}: {} container not running — files are closed, backing them up as-is",
                svc.name, plan.engine
            ));
            continue;
        }
        let out_path = dir.join(&plan.file);
        let tmp = dir.join(format!("{}.partial", plan.file));
        let shell = format!(
            "timeout {DUMP_TIMEOUT_SECS} podman exec {} sh -c '{}' > '{}'",
            svc.name,
            plan.container_cmd,
            tmp.display()
        );
        let status = Command::new("sh")
            .arg("-c")
            .arg(&shell)
            .status()
            .with_context(|| format!("running the {} dump for {}", plan.engine, svc.name))?;
        let size = std::fs::metadata(&tmp).map(|m| m.len()).unwrap_or(0);
        if !status.success() || size == 0 {
            let _ = std::fs::remove_file(&tmp);
            bail!(
                "the {} dump for {:?} {} — refusing to call this a backup. \
                 A snapshot without the database is not one; fix the dump and run again.",
                plan.engine,
                svc.name,
                if status.success() {
                    "wrote zero bytes".to_string()
                } else {
                    format!("failed (exit {:?})", status.code())
                }
            );
        }
        // Atomic into place: a crashed run never leaves a truncated dump
        // where a good one used to be.
        std::fs::rename(&tmp, &out_path)?;
        crate::util::chown_like(&paths.data_dir, &out_path);
        report.push(format!(
            "{}: {} dumped ({} bytes)",
            svc.name, plan.engine, size
        ));
    }
    Ok(report)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_unmistakable_databases_match() {
        let p = plan_for("db", "docker.io/library/postgres:16").unwrap();
        assert_eq!(p.engine, "postgres");
        assert_eq!(p.file, "db.pg_dumpall.sql");
        assert!(p.container_cmd.contains("pg_dumpall"));

        assert_eq!(plan_for("m", "mariadb:11").unwrap().engine, "mysql");
        assert_eq!(plan_for("r", "valkey:8").unwrap().engine, "redis");

        // The near-misses that must NOT get exec'd into.
        for not_db in [
            "ghcr.io/me/postgres-exporter:1",
            "docker.io/library/nginx:latest",
            "redisinsight:2",
            "mysql-backup-sidecar:1",
            "",
        ] {
            assert!(plan_for("x", not_db).is_none(), "{not_db} must not match");
        }
    }
}
