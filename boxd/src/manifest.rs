use std::path::Path;

use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

pub const MANIFEST_FILE: &str = "manifest.json";

/// Machine-readable description of one generation, baked into its output
/// tree. Carries a full snapshot of the declarative config so that rolling
/// back a generation also rolls back the desired state.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Manifest {
    pub schema: u32,
    pub created_at: DateTime<Utc>,
    pub services: Vec<ManifestService>,
    /// Verbatim box.toml at the time this generation was built.
    pub config_toml: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ManifestService {
    pub name: String,
    pub template: String,
    #[serde(default)]
    pub params: serde_json::Value,
    #[serde(default)]
    pub domain: Option<String>,
    #[serde(default)]
    pub public: bool,
    /// Assigned port for process-backed services (reverse-proxied apps); None
    /// for file-served services.
    #[serde(default)]
    pub port: Option<u16>,
    /// How the service is reached: "files" | "proxied". Drives the nginx vhost
    /// and firewall rendering.
    #[serde(default)]
    pub exposure: String,
}

pub fn read_manifest(generation_path: &Path) -> Result<Manifest> {
    let file = generation_path.join(MANIFEST_FILE);
    let text =
        std::fs::read_to_string(&file).with_context(|| format!("reading {}", file.display()))?;
    serde_json::from_str(&text).with_context(|| format!("parsing {}", file.display()))
}
