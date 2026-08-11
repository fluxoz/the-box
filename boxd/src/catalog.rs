//! The service catalog: named presets over the code primitives (static-site,
//! reverse-proxied-app, container). A preset is DATA — a small declarative file
//! — so the catalog grows without a boxd rebuild, and community catalogs are
//! just more source directories (pinned like flake inputs).
//!
//! Sovereignty holds: the catalog ships in the platform closure (or your repo),
//! never fetched live from us. Sources are merged most-authoritative-last:
//! platform catalog, then any community catalogs, then the box's own user
//! catalog. A preset can only parametrize a vetted primitive, so a community or
//! agent-authored entry can't inject arbitrary config — the same central
//! validation (ports, domains) still applies at deploy.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// A catalog entry: a preset over a primitive template.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CatalogEntry {
    pub id: String,
    pub title: String,
    #[serde(default)]
    pub description: String,
    #[serde(default)]
    pub category: String,
    /// The primitive this preset configures, e.g. "container".
    pub base: String,
    /// Default params, merged under the user's params at deploy time.
    #[serde(default)]
    pub params: Value,
    #[serde(default)]
    pub icon: String,
}

/// The source directories, most-authoritative last: the platform catalog (from
/// `$BOX_CATALOG_DIR`, set by the platform module; a repo-relative `catalog/`
/// in dev), then the box's own user catalog under its data dir.
pub fn sources(data_dir: &Path) -> Vec<PathBuf> {
    let mut v = Vec::new();
    match std::env::var_os("BOX_CATALOG_DIR") {
        Some(p) => v.push(PathBuf::from(p)),
        None => {
            let dev = PathBuf::from("catalog");
            if dev.is_dir() {
                v.push(dev);
            }
        }
    }
    v.push(data_dir.join("catalog")); // the box's own presets
    v
}

/// Load + merge entries from the given source directories (a later source wins
/// on id). Unparseable files are skipped, never fatal.
pub fn load(dirs: &[PathBuf]) -> BTreeMap<String, CatalogEntry> {
    let mut out = BTreeMap::new();
    for dir in dirs {
        let Ok(rd) = std::fs::read_dir(dir) else {
            continue;
        };
        for entry in rd.flatten() {
            let path = entry.path();
            if path.extension().and_then(|e| e.to_str()) != Some("toml") {
                continue;
            }
            match std::fs::read_to_string(&path).map(|t| toml::from_str::<CatalogEntry>(&t)) {
                Ok(Ok(e)) => {
                    out.insert(e.id.clone(), e);
                }
                Ok(Err(err)) => tracing::warn!("skipping catalog entry {}: {err}", path.display()),
                Err(_) => {}
            }
        }
    }
    out
}

/// Load the catalog for a box's data dir.
pub fn for_data_dir(data_dir: &Path) -> BTreeMap<String, CatalogEntry> {
    load(&sources(data_dir))
}

/// Resolve a preset to `(base primitive id, params)`, with the user's params
/// merged over the preset's defaults (objects deep-merge, scalars replace).
pub fn resolve(entry: &CatalogEntry, user_params: &Value) -> (String, Value) {
    let mut params = entry.params.clone();
    merge(&mut params, user_params);
    (entry.base.clone(), params)
}

fn merge(base: &mut Value, over: &Value) {
    match (base, over) {
        (Value::Object(b), Value::Object(o)) => {
            for (k, v) in o {
                merge(b.entry(k.clone()).or_insert(Value::Null), v);
            }
        }
        (b, o) => *b = o.clone(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use tempfile::TempDir;

    #[test]
    fn loads_merges_and_resolves() {
        let tmp = TempDir::new().unwrap();
        let platform = tmp.path().join("platform");
        let user = tmp.path().join("user");
        std::fs::create_dir_all(&platform).unwrap();
        std::fs::create_dir_all(&user).unwrap();
        std::fs::write(
            platform.join("postgres.toml"),
            r#"
id = "postgres"
title = "PostgreSQL"
base = "container"
[params]
image = "postgres:16"
expose = "internal"
"#,
        )
        .unwrap();
        // The user catalog overrides the platform entry by id.
        std::fs::write(
            user.join("postgres.toml"),
            "id = \"postgres\"\ntitle = \"My Postgres\"\nbase = \"container\"\n[params]\nimage = \"postgres:17\"\n",
        )
        .unwrap();

        let cat = load(&[platform, user]);
        let pg = cat.get("postgres").unwrap();
        assert_eq!(pg.title, "My Postgres"); // user won
        assert_eq!(pg.params["image"], json!("postgres:17"));

        // Resolve merges the user's deploy params over the preset defaults.
        let (base, params) = resolve(pg, &json!({ "env": { "POSTGRES_DB": "app" } }));
        assert_eq!(base, "container");
        assert_eq!(params["image"], json!("postgres:17"));
        assert_eq!(params["env"]["POSTGRES_DB"], json!("app"));
    }
}
