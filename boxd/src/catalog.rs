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

#[cfg(test)]
mod lint {
    //! The catalog's quality gate: every preset a stranger can click must
    //! deploy something that can actually run. These rules are the distilled
    //! findings of the 2026-08-19 fresh-eyes QA (wrong ports, unqualified
    //! images, secrets in plain env, dead exposure defaults).
    use super::*;

    const CATEGORIES: &[&str] = &[
        "AI",
        "Databases",
        "Dev Tools",
        "Automation",
        "Home",
        "Media",
        "Files & Sync",
        "Web & Publishing",
        "Monitoring",
        "Security",
        "Productivity",
        "Storage",
        "Networking",
    ];
    const REGISTRIES: &[&str] = &[
        "docker.io/",
        "ghcr.io/",
        "quay.io/",
        "lscr.io/",
        "registry.gitlab.com/",
        "codeberg.org/",
        "public.ecr.aws/",
        "gcr.io/",
        "cgr.dev/",
    ];

    #[test]
    fn every_shipped_preset_can_actually_run() {
        let dir = std::path::Path::new("../catalog");
        if !dir.is_dir() {
            eprintln!("catalog/ not present in this build; lint runs in the repo checkout");
            return;
        }
        let entries = load(&[dir.to_path_buf()]);
        assert!(entries.len() >= 8, "catalog went missing?");
        // A preset that does not parse is SKIPPED by the loader with a log
        // line, so a lint that only inspects what loaded would pass while a
        // broken file shipped. Count the files instead: every .toml here must
        // have become an entry.
        let files = std::fs::read_dir(dir)
            .unwrap()
            .flatten()
            .filter(|e| e.path().extension().and_then(|x| x.to_str()) == Some("toml"))
            .count();
        assert_eq!(
            files,
            entries.len(),
            "{} preset file(s) failed to parse and were silently skipped",
            files - entries.len()
        );
        for (id, e) in &entries {
            let ctx = format!("catalog/{id}.toml");
            assert_eq!(&e.id, id, "{ctx}: id must match the filename");
            assert!(!e.title.trim().is_empty(), "{ctx}: title required");
            assert!(
                e.description.len() >= 40,
                "{ctx}: description must actually describe (got {:?})",
                e.description
            );
            assert!(
                !e.description.contains('\u{2014}'),
                "{ctx}: no em-dashes in product copy"
            );
            assert!(
                CATEGORIES.contains(&e.category.as_str()),
                "{ctx}: unknown category {:?}",
                e.category
            );
            assert_eq!(
                e.base, "container",
                "{ctx}: presets configure the container primitive"
            );

            let p = &e.params;
            let image = p.get("image").and_then(Value::as_str).unwrap_or("");
            assert!(
                REGISTRIES.iter().any(|r| image.starts_with(r)),
                "{ctx}: image {image:?} must be fully qualified (a registry prefix)"
            );
            assert!(
                !image.ends_with(":latest") || image.contains("latest"),
                "{ctx}: image tag"
            );

            let port = p.get("container_port").and_then(Value::as_u64).unwrap_or(0);
            // container_port is inside the container's own namespace; the
            // host port is allocated by the Box, so no collision rules apply.
            assert!(
                (1..=65535).contains(&port),
                "{ctx}: container_port {port} out of range"
            );

            let expose = p.get("expose").and_then(Value::as_str).unwrap_or("");
            assert!(
                matches!(expose, "proxied" | "internal" | "exposed"),
                "{ctx}: expose {expose:?} must be proxied | internal | exposed"
            );

            // A known database image must carry its real port and stay internal:
            // the exact traps a stranger walked into on 8/19.
            if let Some((engine, dbport)) = crate::dumps::db_default_port(image) {
                assert_eq!(
                    port,
                    u64::from(dbport),
                    "{ctx}: {engine} listens on {dbport}, not {port}"
                );
                assert_eq!(
                    expose, "internal",
                    "{ctx}: databases have no login page; expose must be internal"
                );
            }

            if let Some(vols) = p.get("volumes").and_then(Value::as_array) {
                for v in vols {
                    let v = v.as_str().unwrap_or("");
                    // The service's own root, including subdirectories and
                    // suffixed siblings; mount options (:U, :ro) are fine.
                    assert!(
                        v.starts_with(&format!("/var/lib/box/{id}")),
                        "{ctx}: volume {v:?} must live under /var/lib/box/{id}"
                    );
                }
            }

            // Secrets never ride plain env.
            if let Some(env) = p.get("env").and_then(Value::as_object) {
                for k in env.keys() {
                    let upper = k.to_uppercase();
                    assert!(
                        !(upper.contains("PASSWORD")
                            || upper.contains("SECRET")
                            || upper.ends_with("_TOKEN")
                            || upper.ends_with("_API_KEY")),
                        "{ctx}: {k} belongs in [params.secret_env], not plain env"
                    );
                }
            }
        }
    }
}
