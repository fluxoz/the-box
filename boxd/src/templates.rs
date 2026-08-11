//! Template registry. A service is "a template + params": the template knows
//! how to validate its params, materialize the service's content, and emit a
//! dendritic Nix module for the OS tier. Static-site is template #1; the GUI
//! catalog and the agent tools install any registered template the same way.

use std::path::Path;

use anyhow::{bail, Context, Result};
use serde_json::Value;

use crate::util;

/// How a service is reached, which decides whether it gets a port and whether
/// the firewall opens it. (A future `Exposed` mode — a raw port opened through
/// the firewall — extends this for non-HTTP services; see [`crate::ports`].)
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Exposure {
    /// No port of its own; the platform proxy serves it from files (static-site).
    Files,
    /// Runs on `127.0.0.1:<port>`, reverse-proxied to by domain. The port is
    /// internal and the firewall stays closed.
    Proxied,
}

impl Exposure {
    /// Does a service of this kind get a port allocated?
    pub fn needs_port(self) -> bool {
        matches!(self, Exposure::Proxied)
    }
    pub fn as_str(self) -> &'static str {
        match self {
            Exposure::Files => "files",
            Exposure::Proxied => "proxied",
        }
    }
}

pub trait Template: Send + Sync {
    fn id(&self) -> &'static str;
    fn title(&self) -> &'static str;
    fn description(&self) -> &'static str;

    /// How this template's service is reached. Decides whether it is assigned a
    /// port and whether the firewall opens it. Default: served from files by the
    /// platform proxy (no port, firewall closed), like a static site.
    fn exposure(&self) -> Exposure {
        Exposure::Files
    }

    /// Reject params this template can't honor. Runs before anything is written.
    fn validate(&self, params: &Value) -> Result<()>;

    /// Write the service's content into `source_dir` (already emptied).
    fn materialize(&self, params: &Value, source_dir: &Path) -> Result<()>;

    /// Emit a dendritic host-module snippet describing this service. Not yet
    /// the build driver (boxd's fast path builds the content directly); this
    /// is the compile-to-module artifact the OS tier and introspection use.
    fn nix_module(&self, name: &str, params: &Value) -> String;

    /// A generation is healthy if every service's expected outputs exist.
    /// Checked against the built generation tree before the switch is kept.
    fn health(&self, name: &str, www_dir: &Path) -> Result<()> {
        let _ = (name, www_dir);
        Ok(())
    }

    /// Absolute paths of this service's *persistent mutable state* — the data
    /// that must be backed up (a database's data dir, uploaded files…). Empty
    /// for stateless services whose content re-materializes from config (like
    /// static-site). Backups derive their path set from this, so adding a
    /// stateful service protects its data automatically. See [`crate::backup`].
    fn state_dirs(&self, name: &str, params: &Value) -> Vec<std::path::PathBuf> {
        let _ = (name, params);
        Vec::new()
    }
}

/// All templates known to this Box. The GUI catalog renders from this list.
pub fn all() -> &'static [&'static dyn Template] {
    &[&StaticSite, &ReverseProxiedApp]
}

/// Escape a string for splicing into a Nix double-quoted string. User/agent
/// input (like an app's run command) reaches the OS-tier module through here,
/// so it must never be able to inject Nix.
fn nix_escape(s: &str) -> String {
    s.replace('\\', "\\\\")
        .replace('"', "\\\"")
        .replace("${", "\\${")
}

pub fn get(id: &str) -> Option<&'static dyn Template> {
    all().iter().copied().find(|t| t.id() == id)
}

// ---------------------------------------------------------------------------

pub const DEFAULT_INDEX: &str = r#"<!doctype html>
<html lang="en">
<head>
  <meta charset="utf-8">
  <meta name="viewport" content="width=device-width, initial-scale=1">
  <title>Hello from The Box</title>
</head>
<body style="font-family: system-ui, sans-serif; display: grid; place-items: center; min-height: 100vh; margin: 0;">
  <div style="text-align: center;">
    <h1>&#128230; It works!</h1>
    <p>This site is served by The Box.</p>
  </div>
</body>
</html>
"#;

/// static-site: serves a directory of files. Params: `index_html` (inline
/// single page) and/or `source_path` (a directory on the Box to copy;
/// wins over `index_html`).
pub struct StaticSite;

impl Template for StaticSite {
    fn id(&self) -> &'static str {
        "static-site"
    }
    fn title(&self) -> &'static str {
        "Static site"
    }
    fn description(&self) -> &'static str {
        "Serve a folder of static files (HTML, CSS, JS, images)."
    }

    fn validate(&self, params: &Value) -> Result<()> {
        if let Some(v) = params.get("index_html") {
            if !v.is_string() {
                bail!("static-site: index_html must be a string");
            }
        }
        if let Some(v) = params.get("source_path") {
            if !v.is_string() {
                bail!("static-site: source_path must be a string");
            }
        }
        Ok(())
    }

    fn materialize(&self, params: &Value, source_dir: &Path) -> Result<()> {
        util::remove_dir_all_forced(source_dir)?;
        std::fs::create_dir_all(source_dir)?;
        if let Some(src) = params.get("source_path").and_then(Value::as_str) {
            let from = std::fs::canonicalize(src)
                .with_context(|| format!("resolving source_path {src}"))?;
            if !from.is_dir() {
                bail!("source_path {} is not a directory", from.display());
            }
            util::copy_dir_recursive(&from, source_dir)?;
        } else {
            let html = params
                .get("index_html")
                .and_then(Value::as_str)
                .unwrap_or(DEFAULT_INDEX);
            std::fs::write(source_dir.join("index.html"), html)?;
        }
        Ok(())
    }

    fn nix_module(&self, name: &str, params: &Value) -> String {
        // Names are validated to [a-z0-9-], safe to splice. This is the OS-tier
        // host-service module: it lives at services/<name>.nix with content
        // alongside at services/<name>/www, and the platform turns it into a
        // real nginx vhost. The same module backs dashboard introspection.
        let domain = params
            .get("domain")
            .and_then(Value::as_str)
            .map(|d| format!("\n    domain = \"{d}\";"))
            .unwrap_or_default();
        format!(
            "# generated by boxd — service {name} (static-site)\n\
             {{ ... }}:\n\
             {{\n  services.the-box.sites.\"{name}\" = {{\n    root = ./{name}/www;{domain}\n  }};\n}}\n"
        )
    }

    fn health(&self, name: &str, www_dir: &Path) -> Result<()> {
        if !www_dir.join("index.html").exists() {
            bail!("service {name:?}: static-site has no index.html to serve");
        }
        Ok(())
    }
}

/// reverse-proxied-app: runs a command as a service on `127.0.0.1:$PORT` and
/// routes its domain to it through nginx. Params: `command` (required — starts
/// the app, which must listen on `$PORT`). The port is assigned by the platform
/// (see [`crate::ports`]) and passed in as the `PORT` env var + the `port` param.
struct ReverseProxiedApp;

impl Template for ReverseProxiedApp {
    fn id(&self) -> &'static str {
        "reverse-proxied-app"
    }
    fn title(&self) -> &'static str {
        "Reverse-proxied app"
    }
    fn description(&self) -> &'static str {
        "Run a web app. The platform assigns it a port, runs it as a service, and routes your domain to it."
    }
    fn exposure(&self) -> Exposure {
        Exposure::Proxied
    }

    fn validate(&self, params: &Value) -> Result<()> {
        match params.get("command").and_then(Value::as_str) {
            Some(c) if !c.trim().is_empty() => Ok(()),
            _ => bail!(
                "reverse-proxied-app: 'command' is required — the command that starts your app, \
                 listening on 127.0.0.1:$PORT"
            ),
        }
    }

    fn materialize(&self, _params: &Value, _source_dir: &Path) -> Result<()> {
        // Nothing to write: the app runs as a service, not from a file tree.
        Ok(())
    }

    fn nix_module(&self, name: &str, params: &Value) -> String {
        // `port` is injected by nixgen from the service's resolved allocation.
        let command = nix_escape(params.get("command").and_then(Value::as_str).unwrap_or("true"));
        let port = params.get("port").and_then(Value::as_u64).unwrap_or(0);
        let domain = params
            .get("domain")
            .and_then(Value::as_str)
            .map(|d| format!("\n    domain = \"{}\";", nix_escape(d)))
            .unwrap_or_default();
        format!(
            "# generated by boxd — service {name} (reverse-proxied-app)\n\
             {{ ... }}:\n\
             {{\n  services.the-box.apps.\"{name}\" = {{\n    \
             command = \"{command}\";\n    port = {port};{domain}\n  }};\n}}\n"
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use tempfile::TempDir;

    #[test]
    fn registry_lookup() {
        assert!(get("static-site").is_some());
        assert!(get("reverse-proxied-app").is_some());
        assert!(get("nope").is_none());
        assert_eq!(all().len(), 2);
        // Exposure drives port allocation + firewall.
        assert_eq!(get("static-site").unwrap().exposure(), Exposure::Files);
        assert!(!get("static-site").unwrap().exposure().needs_port());
        assert_eq!(
            get("reverse-proxied-app").unwrap().exposure(),
            Exposure::Proxied
        );
        assert!(get("reverse-proxied-app").unwrap().exposure().needs_port());
    }

    #[test]
    fn static_site_materializes_and_health() {
        let tmp = TempDir::new().unwrap();
        let dir = tmp.path().join("www");
        StaticSite
            .materialize(&json!({"index_html": "<h1>hi</h1>"}), &dir)
            .unwrap();
        assert_eq!(
            std::fs::read_to_string(dir.join("index.html")).unwrap(),
            "<h1>hi</h1>"
        );
        StaticSite.health("s", &dir).unwrap();

        // empty (no index) fails health
        let empty = tmp.path().join("empty");
        std::fs::create_dir_all(&empty).unwrap();
        assert!(StaticSite.health("s", &empty).is_err());
    }

    #[test]
    fn rejects_bad_params() {
        assert!(StaticSite.validate(&json!({"index_html": 5})).is_err());
        assert!(StaticSite.validate(&json!({"source_path": true})).is_err());
        assert!(StaticSite.validate(&json!({"index_html": "ok"})).is_ok());
    }
}
