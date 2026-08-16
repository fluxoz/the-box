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
    /// Backup destination + policy (bring-your-own backend). Absent = off.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub backup: Option<BackupConfig>,
    /// How this Box is reachable from the internet, if at all. Absent = not.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ingress: Option<IngressConfig>,
    /// The owner's electricity rate (USD per kWh) for the meter card; the
    /// stated US-average default applies when unset.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub electricity_rate_per_kwh: Option<f64>,
    /// The resident caretaker agent's configuration. Absent = no resident.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resident: Option<crate::resident::ResidentConfig>,
    /// The model router's cloud fallback (local models always win).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub router: Option<crate::router::RouterConfig>,
    /// Where this Box pulls code from. Empty = nowhere yet.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub forges: Vec<ForgeConfig>,
}

/// How the Box is reached from the internet.
///
/// It lives here, in box.toml, rather than in the old `network.toml`: this is
/// declarative configuration of the box, so it belongs to the thing that gets
/// pushed to the operator's config repo and comes back on destroy-and-recreate.
/// Credentials do not live here — they go to the secret store, encrypted.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct IngressConfig {
    /// Which way in. See `crate::ingress::providers()`.
    pub provider: String,
    #[serde(default)]
    pub enabled: bool,
    /// The domain this Box publishes under, for providers that use one
    /// (`app.example.com` for a service named `app`). Providers that hand out
    /// their own hostname leave it unset.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub zone: Option<String>,
}

impl IngressConfig {
    pub fn new(provider: impl Into<String>) -> Self {
        Self {
            provider: provider.into(),
            enabled: false,
            zone: None,
        }
    }
}

/// A source of code this Box can deploy from.
///
/// Like [`IngressConfig`], this is declarative state and belongs in box.toml so
/// it survives destroy-and-recreate. The access token does not live here — it
/// goes to the encrypted secret store, keyed by provider (see
/// [`crate::forge::token_secret`]).
///
/// The identifiers below are public by design. A device-flow client has no
/// secret, which is exactly why it is the right grant for a machine whose
/// configuration is pushed to a git repository.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ForgeConfig {
    /// Which forge. See `crate::forge::forges()`.
    pub provider: String,
    /// For a self-hosted forge, where it is. Absent = the public instance.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub base_url: Option<String>,
    /// The OAuth client id to authenticate with. Required where the product
    /// ships no registration that could work (any self-hosted instance), and
    /// otherwise an override for operators who would rather not depend on one
    /// administered by someone else.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub client_id: Option<String>,
    /// A GitHub App's public slug, used to build the link that shares more
    /// repositories with this Box.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub app_slug: Option<String>,
}

impl ForgeConfig {
    pub fn new(provider: impl Into<String>) -> Self {
        Self {
            provider: provider.into(),
            base_url: None,
            client_id: None,
            app_slug: None,
        }
    }
}

/// Client-side-encrypted backups (restic) to a user-provided backend. What to
/// back up is derived from the service manifest, not listed here — see
/// [`crate::backup`]. Secrets (repo password, backend creds) live in the secret
/// store, never in this config.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BackupConfig {
    #[serde(default)]
    pub enabled: bool,
    /// "daily" | "hourly" | a cron expression — consumed by the systemd timer.
    #[serde(default = "default_schedule")]
    pub schedule: String,
    #[serde(default)]
    pub retention: Retention,
    pub backend: BackendConfig,
    /// Extra paths to include beyond the manifest-derived set (rarely needed).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub extra_paths: Vec<String>,
}

/// restic `forget --keep-*` policy.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Retention {
    pub daily: u32,
    pub weekly: u32,
    pub monthly: u32,
}

impl Default for Retention {
    fn default() -> Self {
        Self {
            daily: 7,
            weekly: 4,
            monthly: 6,
        }
    }
}

/// A restic backend. Flat (kind + optional fields) to stay TOML-clean; the
/// fields required for each kind are checked when the repository URL is built.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct BackendConfig {
    /// "local" | "s3" | "sftp" | "rest"
    pub kind: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub path: Option<String>, // local, sftp remote path
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub endpoint: Option<String>, // s3
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bucket: Option<String>, // s3
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub prefix: Option<String>, // s3
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub host: Option<String>, // sftp
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub port: Option<u16>, // sftp
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub user: Option<String>, // sftp
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub url: Option<String>, // rest
}

fn default_schedule() -> String {
    "daily".to_string()
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
    /// The port this service runs on, for templates that run a process
    /// (reverse-proxied apps). Assigned and validated by [`crate::ports`];
    /// `None` for file-served services like static-site.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub port: Option<u16>,
    /// The repository this service is kept in step with, if it came from one.
    /// Content flows one way — the Box pulls; it never writes back.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub repo: Option<RepoLink>,
    pub created_at: DateTime<Utc>,
}

/// A service's upstream repository. Declarative like the rest of box.toml: it
/// says where the content comes from, and [`crate::pull`] makes it true. No
/// credential lives here — the forge token is in the secret store, and the
/// clone URL is credential-free so it can sit in a config repo history.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct RepoLink {
    /// Which forge holds it. See `crate::forge::forges()`.
    pub forge: String,
    /// `owner/name`, as the forge lists it.
    pub repo: String,
    /// HTTPS clone URL. Auth is supplied per-invocation, never embedded.
    pub clone_url: String,
    pub branch: String,
    /// Deploy this subdirectory of the repository rather than its root — or,
    /// when a build step is set, run the build there (the monorepo case).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub subdir: Option<String>,
    /// How a checkout becomes servable, for repositories that are not yet a
    /// file tree. Runs in the sandboxed builder (see [`crate::build`]). Last
    /// field on purpose: TOML wants the sub-table after the scalars.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub build: Option<crate::build::BuildSpec>,
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
        // An operator running a boxd command as root must not leave a config
        // the daemon can no longer write.
        crate::util::chown_like(&paths.data_dir, &file);
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
                port: None,
                repo: None,
                created_at: Utc::now(),
            }],
            ..Default::default()
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
