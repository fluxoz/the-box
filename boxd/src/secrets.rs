//! Minimal file-based secrets store: one secret per file under
//! `<data>/secrets/`, directory 0700 and files 0600. Secrets never enter the
//! declarative config, the generation source, or the Nix store.

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;

use anyhow::{bail, Context, Result};

use crate::paths::Paths;

fn secrets_dir(paths: &Paths) -> PathBuf {
    paths.data_dir.join("secrets")
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

pub fn set(paths: &Paths, name: &str, value: &str) -> Result<()> {
    validate_name(name)?;
    let dir = secrets_dir(paths);
    fs::create_dir_all(&dir)?;
    fs::set_permissions(&dir, fs::Permissions::from_mode(0o700))?;
    let file = dir.join(name);
    fs::write(&file, value).with_context(|| format!("writing secret {name}"))?;
    fs::set_permissions(&file, fs::Permissions::from_mode(0o600))?;
    Ok(())
}

pub fn get(paths: &Paths, name: &str) -> Result<Option<String>> {
    validate_name(name)?;
    let file = secrets_dir(paths).join(name);
    if !file.exists() {
        return Ok(None);
    }
    let value = fs::read_to_string(&file).with_context(|| format!("reading secret {name}"))?;
    Ok(Some(value.trim().to_string()))
}

pub fn exists(paths: &Paths, name: &str) -> bool {
    matches!(get(paths, name), Ok(Some(_)))
}

pub fn delete(paths: &Paths, name: &str) -> Result<()> {
    validate_name(name)?;
    let file = secrets_dir(paths).join(name);
    if file.exists() {
        fs::remove_file(&file)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip_and_permissions() {
        let tmp = tempfile::TempDir::new().unwrap();
        let paths = Paths::new(tmp.path().to_path_buf());
        set(&paths, "my-token", "s3cret\n").unwrap();
        assert_eq!(get(&paths, "my-token").unwrap().as_deref(), Some("s3cret"));
        assert!(exists(&paths, "my-token"));

        let mode = fs::metadata(secrets_dir(&paths).join("my-token"))
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o600);

        delete(&paths, "my-token").unwrap();
        assert!(!exists(&paths, "my-token"));
        assert!(set(&paths, "Bad Name", "x").is_err());
    }
}
