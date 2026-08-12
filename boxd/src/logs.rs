//! Reading a service's logs.
//!
//! Without this, an agent asked to fix a broken deployment had no way to find
//! out what was wrong: it could see that a service existed and what state the
//! platform believed it was in, and nothing else. "Your agent can run your
//! server for you" needs the agent to be able to read the error.
//!
//! Everything a Box runs is a systemd unit, so this is journalctl with the unit
//! name worked out from the service and a hard cap on how much comes back.

use anyhow::{Context, Result};
use serde_json::{json, Value};

use crate::config::BoxConfig;
use crate::paths::Paths;
use crate::templates::{self, Exposure};

/// The systemd units a service might run as, most likely first.
///
/// A container runs as `podman-<name>`, a reverse-proxied app as
/// `box-app-<name>`, and a static site has no unit at all — it is files served
/// by the platform, so its logs are the web server's.
fn candidate_units(config: &BoxConfig, name: &str) -> Vec<String> {
    let Some(service) = config.find(name) else {
        return Vec::new();
    };
    match templates::get(&service.template).map(|t| t.exposure(&service.params)) {
        Some(Exposure::Files) | None => vec!["nginx.service".into()],
        _ => vec![
            format!("podman-{name}.service"),
            format!("box-app-{name}.service"),
        ],
    }
}

fn unit_exists(unit: &str) -> bool {
    std::process::Command::new("systemctl")
        .args(["cat", unit])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

/// Recent log lines for a deployed service, oldest first.
pub fn service_logs(paths: &Paths, name: &str, lines: usize) -> Result<Value> {
    let config = BoxConfig::load(paths)?;
    if config.find(name).is_none() {
        anyhow::bail!("no service named {name:?}");
    }

    let candidates = candidate_units(&config, name);
    let Some(unit) = candidates.iter().find(|u| unit_exists(u)).cloned() else {
        // Being able to say *why* there are no logs is the useful answer here:
        // a container with no unit has not been applied to the system yet.
        return Ok(json!({
            "service": name,
            "unit": Value::Null,
            "lines": [],
            "note": format!(
                "no system unit for {name} yet — it is declared but not running. \
                 Looked for: {}.",
                candidates.join(", ")
            ),
        }));
    };

    let out = std::process::Command::new("journalctl")
        .args([
            "-u",
            &unit,
            "-n",
            &lines.to_string(),
            "--no-pager",
            "-o",
            "short-iso",
        ])
        .output()
        .context("running journalctl (is this a systemd system?)")?;

    if !out.status.success() {
        anyhow::bail!(
            "could not read logs for {name}: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    let text = String::from_utf8_lossy(&out.stdout);
    let lines: Vec<&str> = text.lines().filter(|l| !l.trim().is_empty()).collect();
    Ok(json!({ "service": name, "unit": unit, "lines": lines }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::ServiceConfig;
    use chrono::Utc;

    fn config_with(template: &str, params: serde_json::Value) -> BoxConfig {
        BoxConfig {
            services: vec![ServiceConfig {
                name: "app".into(),
                template: template.into(),
                params,
                domain: None,
                public: false,
                port: Some(8000),
                created_at: Utc::now(),
            }],
            ..Default::default()
        }
    }

    #[test]
    fn units_follow_from_how_a_service_runs() {
        let container = config_with("container", json!({ "image": "nginx" }));
        assert_eq!(
            candidate_units(&container, "app").first().unwrap(),
            "podman-app.service"
        );

        let app = config_with("reverse-proxied-app", json!({ "command": "serve" }));
        assert!(candidate_units(&app, "app")
            .iter()
            .any(|u| u == "box-app-app.service"));

        // A static site is files, not a process: its logs are the web server's.
        let site = config_with("static-site", json!({}));
        assert_eq!(candidate_units(&site, "app"), vec!["nginx.service"]);

        // An unknown service has no units to look at.
        assert!(candidate_units(&site, "nope").is_empty());
    }
}
