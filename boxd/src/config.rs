use std::fs;

use anyhow::{bail, Context, Result};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::paths::Paths;

const CONFIG_HEADER: &str =
    "# The Box — declarative service configuration (managed by boxd; edits are applied atomically)\n\n";

/// The declarative desired state of this Box. Everything the dashboard, API
/// and agents do ultimately mutates this and then applies it as a new
/// generation.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct BoxConfig {
    #[serde(default)]
    pub services: Vec<ServiceConfig>,
}

/// A service is a template id plus that template's params. This is the
/// declarative record the GUI, MCP and CLI all write; boxd compiles it into a
/// generation (and a dendritic module) via the [`crate::templates`] registry.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ServiceConfig {
    pub name: String,
    /// Template id, e.g. "static-site". Resolved against the template registry.
    pub template: String,
    /// Template-specific parameters. Shape is defined and validated by the
    /// template; opaque to the config layer.
    #[serde(default)]
    pub params: Value,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub domain: Option<String>,
    #[serde(default)]
    pub public: bool,
    pub created_at: DateTime<Utc>,
}

impl BoxConfig {
    pub fn load(paths: &Paths) -> Result<Self> {
        let file = paths.config_file();
        if !file.exists() {
            return Ok(Self::default());
        }
        let text =
            fs::read_to_string(&file).with_context(|| format!("reading {}", file.display()))?;
        toml::from_str(&text).with_context(|| format!("parsing {}", file.display()))
    }

    pub fn save(&self, paths: &Paths) -> Result<()> {
        let file = paths.config_file();
        let body = self.to_toml()?;
        fs::write(&file, format!("{CONFIG_HEADER}{body}"))
            .with_context(|| format!("writing {}", file.display()))?;
        Ok(())
    }

    pub fn to_toml(&self) -> Result<String> {
        toml::to_string_pretty(self).context("serializing config")
    }

    pub fn find(&self, name: &str) -> Option<&ServiceConfig> {
        self.services.iter().find(|s| s.name == name)
    }
}

/// Service names become URL segments, directory names and Nix identifiers, so
/// keep them boring: lowercase alphanumerics and dashes.
pub fn validate_service_name(name: &str) -> Result<()> {
    let ok = !name.is_empty()
        && name.len() <= 32
        && name
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
        && !name.starts_with('-')
        && !name.ends_with('-');
    if !ok {
        bail!("invalid service name {name:?}: use 1-32 characters of a-z, 0-9 and '-' (not at the ends)");
    }
    Ok(())
}

/// Domains are matched against incoming Host headers for tunnel traffic, so
/// they must be real lowercase hostnames — and must never shadow localhost
/// or a bare IP, which would expose the dashboard's address space.
pub fn validate_domain(domain: &str) -> Result<()> {
    let ok = !domain.is_empty()
        && domain.len() <= 253
        && domain.contains('.')
        && domain
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '.' || c == '-')
        && !domain
            .split('.')
            .any(|label| label.is_empty() || label.starts_with('-') || label.ends_with('-'))
        && !domain.chars().all(|c| c.is_ascii_digit() || c == '.');
    if !ok {
        bail!("invalid domain {domain:?}: use a lowercase hostname like site.example.com");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn domain_validation() {
        for good in ["example.com", "my-site.example.co.uk", "a.b"] {
            assert!(validate_domain(good).is_ok(), "{good} should be valid");
        }
        for bad in [
            "",
            "localhost",
            "no-dots",
            "127.0.0.1",
            "Example.com",
            "-a.com",
            "a-.com",
            "a..com",
            ".a.com",
            "a b.com",
        ] {
            assert!(validate_domain(bad).is_err(), "{bad:?} should be invalid");
        }
    }

    #[test]
    fn name_validation() {
        for good in ["a", "my-site", "site2", "a-b-c", "0hello"] {
            assert!(
                validate_service_name(good).is_ok(),
                "{good} should be valid"
            );
        }
        for bad in [
            "",
            "-a",
            "a-",
            "My-Site",
            "a b",
            "a/b",
            "../etc",
            "a".repeat(33).as_str(),
        ] {
            assert!(
                validate_service_name(bad).is_err(),
                "{bad:?} should be invalid"
            );
        }
    }

    #[test]
    fn toml_roundtrip() {
        let config = BoxConfig {
            services: vec![ServiceConfig {
                name: "hello".into(),
                template: "static-site".into(),
                params: serde_json::json!({ "index_html": "<h1>hi</h1>" }),
                domain: Some("hello.example.com".into()),
                public: true,
                created_at: Utc::now(),
            }],
        };
        let text = config.to_toml().unwrap();
        let parsed: BoxConfig = toml::from_str(&text).unwrap();
        assert_eq!(parsed.services.len(), 1);
        assert_eq!(parsed.services[0].name, "hello");
        assert_eq!(parsed.services[0].template, "static-site");
        assert_eq!(parsed.services[0].params["index_html"], "<h1>hi</h1>");
        assert!(parsed.services[0].public);
    }
}
