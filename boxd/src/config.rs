use std::fs;

use anyhow::{bail, Context, Result};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

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

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ServiceConfig {
    pub name: String,
    pub template: Template,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub domain: Option<String>,
    #[serde(default)]
    pub public: bool,
    pub created_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Template {
    StaticSite,
}

impl Template {
    pub fn as_str(self) -> &'static str {
        match self {
            Template::StaticSite => "static-site",
        }
    }
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

#[cfg(test)]
mod tests {
    use super::*;

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
                template: Template::StaticSite,
                domain: Some("hello.example.com".into()),
                public: true,
                created_at: Utc::now(),
            }],
        };
        let text = config.to_toml().unwrap();
        let parsed: BoxConfig = toml::from_str(&text).unwrap();
        assert_eq!(parsed.services.len(), 1);
        assert_eq!(parsed.services[0].name, "hello");
        assert_eq!(parsed.services[0].template, Template::StaticSite);
        assert!(parsed.services[0].public);
    }
}
