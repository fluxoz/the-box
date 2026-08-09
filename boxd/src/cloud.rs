//! Client for the optional cloud control plane (managed backup — the paid tier).
//! The free tier never touches this: `boxd cloud enroll` links the Box to a
//! cloud account and points Box Backup at managed, metered storage in one step.
//! Everything remains client-side encrypted; the cloud only ever holds
//! ciphertext + scoped storage credentials, never the backup key.

use anyhow::{bail, Context, Result};
use serde_json::Value;

use crate::config::{BackendConfig, BackupConfig, BoxConfig};
use crate::paths::Paths;
use crate::{backup, secrets};

const API_TOKEN: &str = "cloud-api-token";
const SERVER: &str = "cloud-server";

fn curl_json(args: &[&str]) -> Result<Value> {
    let out = std::process::Command::new("curl")
        .arg("-fsS")
        .args(args)
        .output()
        .context("running curl")?;
    if !out.status.success() {
        bail!(
            "cloud request failed: {}",
            String::from_utf8_lossy(&out.stderr)
        );
    }
    Ok(serde_json::from_slice(&out.stdout).unwrap_or(Value::Null))
}

/// Link this Box to a cloud account with a one-time enrollment token, then
/// provision managed backup storage.
pub fn enroll(paths: &Paths, server: &str, enroll_token: &str) -> Result<()> {
    let server = server.trim_end_matches('/');
    let body = serde_json::json!({ "enroll_token": enroll_token }).to_string();
    let resp = curl_json(&[
        "-X",
        "POST",
        &format!("{server}/v1/enroll"),
        "-H",
        "content-type: application/json",
        "-d",
        &body,
    ])?;
    let api_token = resp
        .get("api_token")
        .and_then(Value::as_str)
        .context("enrollment failed (no api_token returned — token invalid or used?)")?;
    secrets::set(paths, API_TOKEN, api_token)?;
    secrets::set(paths, SERVER, server)?;
    provision(paths)
}

/// Fetch scoped storage from the cloud and point Box Backup at it.
pub fn provision(paths: &Paths) -> Result<()> {
    let (server, token) = creds(paths)?;
    let resp = curl_json(&[
        "-X",
        "POST",
        &format!("{server}/v1/backup/provision"),
        "-H",
        &format!("authorization: Bearer {token}"),
    ])?;
    let s = |k: &str| resp.get(k).and_then(Value::as_str).map(String::from);
    let endpoint = s("endpoint").context("provision: no endpoint")?;
    let bucket = s("bucket").context("provision: no bucket")?;
    let access_key = s("access_key").context("provision: no access_key")?;
    let secret_key = s("secret_key").context("provision: no secret_key")?;

    let mut config = BoxConfig::load(paths)?;
    let (schedule, retention) = config
        .backup
        .as_ref()
        .map(|b| (b.schedule.clone(), b.retention.clone()))
        .unwrap_or_else(|| ("daily".to_string(), Default::default()));
    config.backup = Some(BackupConfig {
        enabled: true,
        schedule,
        retention,
        backend: BackendConfig {
            kind: "s3".to_string(),
            endpoint: Some(endpoint),
            bucket: Some(bucket),
            prefix: s("prefix"),
            ..Default::default()
        },
        extra_paths: Vec::new(),
    });
    config.save(paths)?;
    secrets::set(paths, "backup-s3-access-key", &access_key)?;
    secrets::set(paths, "backup-s3-secret-key", &secret_key)?;
    if !backup::has_key(paths) {
        backup::init_key(paths)?;
    }
    Ok(())
}

pub fn usage(paths: &Paths) -> Result<Value> {
    let (server, token) = creds(paths)?;
    curl_json(&[
        "-X",
        "GET",
        &format!("{server}/v1/backup/usage"),
        "-H",
        &format!("authorization: Bearer {token}"),
    ])
}

/// Provision a Box Connect key from the control plane and join the mesh.
pub fn connect(paths: &Paths, hostname: &str) -> Result<()> {
    let (server, token) = creds(paths)?;
    let resp = curl_json(&[
        "-X",
        "POST",
        &format!("{server}/v1/connect/provision"),
        "-H",
        &format!("authorization: Bearer {token}"),
    ])?;
    let login_server = resp
        .get("login_server")
        .and_then(Value::as_str)
        .context("connect: no login_server")?;
    let authkey = resp
        .get("authkey")
        .and_then(Value::as_str)
        .context("connect: no authkey")?;
    crate::connect::enroll(login_server, authkey, hostname)
}

fn creds(paths: &Paths) -> Result<(String, String)> {
    let server = secrets::get(paths, SERVER)?
        .context("not enrolled — run `boxd cloud enroll --server <url> --token <token>`")?;
    let token = secrets::get(paths, API_TOKEN)?.context("not enrolled")?;
    Ok((server, token))
}
