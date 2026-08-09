//! LAN fleet discovery. Every Box advertises `_thebox._tcp` over mDNS (see
//! nix/module.nix); boxd browses for peers and reads each one's *coarse* public
//! health. Coarse health is public by design — discovery is not authorization,
//! so seeing a Box on the network reveals only a health summary; managing it
//! requires pairing (a later increment). Peer-federated read: there is no
//! central registry, each Box discovers and queries the others directly.

use std::process::Command;

use serde::{Deserialize, Serialize};

use crate::config::BoxConfig;
use crate::paths::Paths;
use crate::store;

/// The mDNS service type every Box advertises.
pub const SERVICE_TYPE: &str = "_thebox._tcp";

/// The public summary a Box exposes at `/api/v1/health`. Deliberately coarse:
/// enough to render a fleet map, nothing that needs authorization to see.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CoarseHealth {
    pub id: String,
    pub name: String,
    pub version: String,
    pub generation: Option<u64>,
    pub services: usize,
    /// Coarse status color: "ok" (running a generation) or "new" (nothing
    /// applied yet). Unreachable peers are represented by `Peer.health = None`.
    pub health: String,
    /// Backup freshness: `None` when backup isn't configured, else whether the
    /// last successful backup is older than a day (stale). Lets the fleet view
    /// flag a Box whose backups have quietly stopped.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub backup_stale: Option<bool>,
}

/// The Box's runtime hostname (identity on the LAN and in mDNS).
pub fn hostname() -> String {
    std::fs::read_to_string("/proc/sys/kernel/hostname")
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "box".to_string())
}

/// This Box's own coarse health.
pub fn self_health(paths: &Paths) -> CoarseHealth {
    let name = hostname();
    let generation = store::current(paths).ok().flatten().map(|g| g.number);
    let config = BoxConfig::load(paths).unwrap_or_default();
    let services = config.services.len();
    // Configured-but-stale: no backup yet, or the last one is over a day old.
    let backup_stale = config
        .backup
        .as_ref()
        .filter(|b| b.enabled)
        .map(|_| {
            crate::backup::last_backup_age(paths)
                .map(|age| age > chrono::Duration::days(1))
                .unwrap_or(true)
        });
    CoarseHealth {
        id: name.clone(),
        name,
        version: env!("CARGO_PKG_VERSION").to_string(),
        health: (if generation.is_some() { "ok" } else { "new" }).to_string(),
        generation,
        services,
        backup_stale,
    }
}

/// A discovered peer Box: where it is, plus its coarse health if reachable.
#[derive(Debug, Clone, Serialize)]
pub struct Peer {
    pub host: String,
    pub address: String,
    pub port: u16,
    pub health: Option<CoarseHealth>,
}

/// Parse `avahi-browse -rpt` output. Resolved records begin with `=` and are
/// `;`-separated: `=;iface;proto;name;type;domain;host;address;port;txt`.
pub fn parse_avahi(output: &str) -> Vec<(String, String, u16)> {
    let mut out = Vec::new();
    for line in output.lines() {
        if !line.starts_with('=') {
            continue;
        }
        let f: Vec<&str> = line.split(';').collect();
        if f.len() < 9 {
            continue;
        }
        let host = f[6].trim_end_matches('.').to_string();
        let address = f[7].to_string();
        let Ok(port) = f[8].parse::<u16>() else {
            continue;
        };
        // Skip IPv6 link-local: avahi advertises both A and AAAA, but a plain
        // HTTP GET to fe80:: needs a zone id, so those records are unreachable
        // noise (the same peer shows up again via its routable address).
        if host.is_empty() || address.is_empty() || address.starts_with("fe80") {
            continue;
        }
        out.push((host, address, port));
    }
    out
}

/// Browse the LAN for peers. `-l` ignores services published locally, so this
/// returns other Boxes, never ourselves. Returns empty when avahi isn't present
/// (e.g. a dev host), so callers degrade to "just this Box".
fn browse() -> Option<String> {
    let out = Command::new("avahi-browse")
        .args(["-r", "-p", "-t", "-l", SERVICE_TYPE])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    Some(String::from_utf8_lossy(&out.stdout).into_owned())
}

/// Read a peer's coarse health over HTTP. Best-effort with a short timeout;
/// `None` means unreachable, which the fleet view renders as such.
fn fetch_health(address: &str, port: u16) -> Option<CoarseHealth> {
    let url = format!("http://{address}:{port}/api/v1/health");
    let out = Command::new("curl")
        .args(["-s", "--max-time", "2", &url])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    serde_json::from_slice(&out.stdout).ok()
}

/// Discover peer Boxes on the LAN and read each one's coarse health.
pub fn discover() -> Vec<Peer> {
    let Some(output) = browse() else {
        return Vec::new();
    };
    parse_avahi(&output)
        .into_iter()
        .map(|(host, address, port)| {
            let health = fetch_health(&address, port);
            Peer {
                host,
                address,
                port,
                health,
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_resolved_records_only() {
        // A '+' line (found, unresolved) and a '=' line (resolved) for one host,
        // plus a malformed line that must be skipped.
        let sample = "\
+;eth0;IPv4;box-abc;_thebox._tcp;local
=;eth0;IPv4;box-abc;_thebox._tcp;local;box-abc.local;192.168.1.20;2693;\"vendor=thebox\"
=;eth0;IPv4;broken;_thebox._tcp;local;box-x.local
=;wlan0;IPv6;box-def;_thebox._tcp;local;box-def.local;fe80::2;2693;\"vendor=thebox\"
";
        // Only the resolved IPv4 record survives: the malformed line and the
        // fe80 (IPv6 link-local) record are both dropped.
        let peers = parse_avahi(sample);
        assert_eq!(peers.len(), 1);
        assert_eq!(
            peers[0],
            (
                "box-abc.local".to_string(),
                "192.168.1.20".to_string(),
                2693
            )
        );
    }
}
