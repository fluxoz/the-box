//! boxd's own operational secret store (backup password, storage credentials,
//! cloud/tunnel tokens) — distinct from *service* secrets, which agenix decrypts
//! at boot for containers (see [`crate::agecrypt`]).
//!
//! These are encrypted at rest: one `<name>.age` per secret under
//! `<data>/secrets/op/`, encrypted to `[SSH host key + operator + box identity]`
//! (see [`agecrypt::local_recipients`]). boxd reads them on demand with the box
//! identity — it runs unprivileged and can't use the root-only host key the way
//! agenix does. Because they're `.age`, they travel in the config repo and
//! survive destroy-and-recreate (the operator re-keys them onto a fresh box);
//! plaintext never touches disk or the repo. The box identity itself stays on
//! the box (gitignored) — the one piece that can't be encrypted, exactly like
//! the SSH host key.

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;

use anyhow::{bail, Context, Result};

use crate::agecrypt;
use crate::paths::Paths;

fn secrets_dir(paths: &Paths) -> PathBuf {
    paths.data_dir.join("secrets")
}

/// Where encrypted operational secrets live. A subdir so restore can re-key them
/// to `[host + operator + box identity]` (boxd must read them) separately from
/// service secrets in the parent dir, which stay host-key-only.
fn op_dir(paths: &Paths) -> PathBuf {
    secrets_dir(paths).join("op")
}

fn validate_name(name: &str) -> Result<()> {
    let ok = !name.is_empty()
        && name.len() <= 64
        && name
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-');
    if !ok {
        bail!("invalid secret name {name:?}");
    }
    Ok(())
}

/// Store a secret, encrypted at rest. Replaces any prior value (and drops a
/// legacy plaintext copy from before encryption-at-rest).
pub fn set(paths: &Paths, name: &str, value: &str) -> Result<()> {
    validate_name(name)?;
    let dir = op_dir(paths);
    fs::create_dir_all(&dir)?;
    fs::set_permissions(secrets_dir(paths), fs::Permissions::from_mode(0o700))?;
    fs::set_permissions(&dir, fs::Permissions::from_mode(0o700))?;

    let recipients = agecrypt::local_recipients(paths)?;
    let file = dir.join(format!("{name}.age"));
    agecrypt::encrypt(value, &recipients, &file)
        .with_context(|| format!("encrypting secret {name}"))?;
    fs::set_permissions(&file, fs::Permissions::from_mode(0o600))?;

    // Remove any pre-encryption plaintext so it doesn't linger.
    let _ = fs::remove_file(secrets_dir(paths).join(name));
    Ok(())
}

/// Read a secret, decrypting with the box identity. Falls back to (and migrates)
/// a legacy plaintext secret from before encryption-at-rest.
pub fn get(paths: &Paths, name: &str) -> Result<Option<String>> {
    validate_name(name)?;
    let enc = op_dir(paths).join(format!("{name}.age"));
    if enc.exists() {
        let identity = agecrypt::ensure_box_identity(paths)?;
        let plain = agecrypt::decrypt(&enc, &identity)
            .with_context(|| format!("decrypting secret {name}"))?;
        return Ok(Some(String::from_utf8_lossy(&plain).trim().to_string()));
    }
    // Legacy plaintext (pre-encryption): read it, then migrate it in place.
    let legacy = secrets_dir(paths).join(name);
    if legacy.exists() {
        let value = fs::read_to_string(&legacy)
            .with_context(|| format!("reading legacy secret {name}"))?
            .trim()
            .to_string();
        if let Err(e) = set(paths, name, &value) {
            tracing::warn!("could not encrypt legacy secret {name}: {e:#}");
        }
        return Ok(Some(value));
    }
    Ok(None)
}

pub fn exists(paths: &Paths, name: &str) -> bool {
    matches!(get(paths, name), Ok(Some(_)))
}

pub fn delete(paths: &Paths, name: &str) -> Result<()> {
    validate_name(name)?;
    let _ = fs::remove_file(op_dir(paths).join(format!("{name}.age")));
    let _ = fs::remove_file(secrets_dir(paths).join(name));
    Ok(())
}

/// Encrypt any legacy plaintext operational secrets in place. Best-effort;
/// called once at startup so an upgraded box moves its existing secrets to
/// encryption-at-rest without operator action.
pub fn migrate_plaintext(paths: &Paths) {
    let dir = secrets_dir(paths);
    let Ok(entries) = fs::read_dir(&dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if !path.is_file() {
            continue; // skips the op/ subdir
        }
        let Some(name) = path.file_name().and_then(|s| s.to_str()) else {
            continue;
        };
        // Encrypted secrets, the box identity, and service `.age` files are not
        // legacy plaintext; only bare valid secret names are.
        if name.ends_with(".age") || name == "boxd-identity.key" || validate_name(name).is_err() {
            continue;
        }
        // get() reads the plaintext and re-stores it encrypted.
        match get(paths, name) {
            Ok(Some(_)) => tracing::info!("encrypted legacy operational secret {name} at rest"),
            Ok(None) => {}
            Err(e) => tracing::warn!("skipped legacy secret {name}: {e:#}"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn age_available() -> bool {
        std::process::Command::new("age")
            .arg("--version")
            .output()
            .map(|o| o.status.success())
            .unwrap_or(false)
            && std::process::Command::new("age-keygen")
                .arg("--version")
                .output()
                .map(|o| o.status.success())
                .unwrap_or(false)
    }

    #[test]
    fn roundtrip_is_encrypted_at_rest() {
        if !age_available() {
            return;
        }
        let tmp = tempfile::TempDir::new().unwrap();
        let paths = Paths::new(tmp.path().to_path_buf());
        set(&paths, "my-token", "s3cret\n").unwrap();
        assert_eq!(get(&paths, "my-token").unwrap().as_deref(), Some("s3cret"));
        assert!(exists(&paths, "my-token"));

        // On disk it's an .age ciphertext, 0600 — not the plaintext value.
        let file = op_dir(&paths).join("my-token.age");
        let mode = fs::metadata(&file).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o600);
        let bytes = fs::read(&file).unwrap();
        assert!(!bytes.windows(6).any(|w| w == b"s3cret"), "plaintext leaked");
        assert!(bytes.starts_with(b"age-encryption.org/"), "not an age file");

        delete(&paths, "my-token").unwrap();
        assert!(!exists(&paths, "my-token"));
        assert!(set(&paths, "Bad Name", "x").is_err());
    }

    #[test]
    fn migrates_legacy_plaintext() {
        if !age_available() {
            return;
        }
        let tmp = tempfile::TempDir::new().unwrap();
        let paths = Paths::new(tmp.path().to_path_buf());
        // Simulate a pre-encryption box: a plaintext secret file.
        let dir = secrets_dir(&paths);
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join("cloud-api-token"), "legacy-value\n").unwrap();

        migrate_plaintext(&paths);

        // The plaintext is gone; the value is now readable from the .age.
        assert!(!dir.join("cloud-api-token").exists(), "plaintext not removed");
        assert!(op_dir(&paths).join("cloud-api-token.age").exists());
        assert_eq!(
            get(&paths, "cloud-api-token").unwrap().as_deref(),
            Some("legacy-value")
        );
    }
}
