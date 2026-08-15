//! AI keys: the credentials for the Box's OpenAI-compatible endpoint.
//!
//! The adoption lever for self-hosted models is not a better API — it is THE
//! API every SDK already speaks, with a key that behaves like the keys apps
//! already expect. These are those keys: minted per app, listed with their
//! labels, revocable one at a time, stored hashed like sessions. A `boxai_`
//! prefix so a leaked one is recognizable in a config file for what it is —
//! the key to your own hardware, not someone's metered account.

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::paths::Paths;

#[derive(Serialize, Deserialize, Default)]
struct Store {
    #[serde(default)]
    keys: Vec<StoredKey>,
}

#[derive(Serialize, Deserialize, Clone)]
struct StoredKey {
    id: String,
    label: String,
    hash: String,
    created_at: i64,
}

/// A key as listed — never the secret, which is shown once at mint.
#[derive(Serialize, Clone)]
pub struct KeyInfo {
    pub id: String,
    pub label: String,
    pub created_at: i64,
}

fn file(paths: &Paths) -> std::path::PathBuf {
    paths.data_dir.join("ai-keys.json")
}

fn hash(s: &str) -> String {
    Sha256::digest(s.as_bytes())
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

fn load(paths: &Paths) -> Store {
    std::fs::read_to_string(file(paths))
        .ok()
        .and_then(|t| serde_json::from_str(&t).ok())
        .unwrap_or_default()
}

fn save(paths: &Paths, store: &Store) -> Result<()> {
    let f = file(paths);
    std::fs::write(&f, serde_json::to_string_pretty(store)?)
        .with_context(|| format!("writing {}", f.display()))?;
    use std::os::unix::fs::PermissionsExt;
    let _ = std::fs::set_permissions(&f, std::fs::Permissions::from_mode(0o600));
    crate::util::chown_like(&paths.data_dir, &f);
    Ok(())
}

/// Mint a key. The plaintext is returned exactly once.
pub fn mint(paths: &Paths, label: &str) -> Result<String> {
    let key = format!("boxai_{}", crate::auth::random_hex(24)?);
    let mut store = load(paths);
    store.keys.push(StoredKey {
        id: crate::auth::random_hex(4)?,
        label: label.trim().to_string(),
        hash: hash(&key),
        created_at: chrono::Utc::now().timestamp(),
    });
    save(paths, &store)?;
    Ok(key)
}

pub fn verify(paths: &Paths, key: &str) -> bool {
    let h = hash(key);
    load(paths).keys.iter().any(|k| k.hash == h)
}

pub fn list(paths: &Paths) -> Vec<KeyInfo> {
    load(paths)
        .keys
        .into_iter()
        .map(|k| KeyInfo {
            id: k.id,
            label: k.label,
            created_at: k.created_at,
        })
        .collect()
}

pub fn revoke(paths: &Paths, id: &str) -> Result<bool> {
    let mut store = load(paths);
    let before = store.keys.len();
    store.keys.retain(|k| k.id != id);
    let removed = store.keys.len() != before;
    if removed {
        save(paths, &store)?;
    }
    Ok(removed)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keys_mint_verify_list_and_revoke() {
        let tmp = tempfile::TempDir::new().unwrap();
        let paths = Paths::new(tmp.path().to_path_buf());
        let key = mint(&paths, "my-app").unwrap();
        assert!(key.starts_with("boxai_"), "recognizable in a config file");
        assert!(verify(&paths, &key));
        assert!(!verify(&paths, "boxai_wrong"));
        let listed = list(&paths);
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].label, "my-app");
        assert!(revoke(&paths, &listed[0].id).unwrap());
        assert!(!verify(&paths, &key), "revoked means revoked");
    }
}
