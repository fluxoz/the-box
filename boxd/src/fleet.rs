//! Fleet discovery over two planes, same peer-federated model on both: there is
//! no central registry — each Box finds the others and reads their *coarse*
//! public health directly. Coarse health is public by design (discovery is not
//! authorization: seeing a Box reveals only a health summary; managing it needs
//! pairing).
//!
//!   * LAN plane — mDNS (`_thebox._tcp`, see nix/module.nix). Zero-config, but
//!     L2-only: dies across subnets/sites.
//!   * Mesh plane — a shared Headscale/Tailscale tailnet (Box Connect). Spans
//!     sites and NATs; peers are the `tag:box`-tagged nodes in `tailscale
//!     status`. This is what makes a *fleet* work off one LAN.
//!
//! `discover()` merges both and de-duplicates a Box reachable on both planes.

use std::collections::HashMap;
use std::process::Command;

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::config::BoxConfig;
use crate::paths::Paths;
use crate::store;

/// The mDNS service type every Box advertises.
pub const SERVICE_TYPE: &str = "_thebox._tcp";

/// The ACL tag every Box carries on the mesh (see the control plane's fleet
/// policy). Tailnet discovery filters to nodes bearing it, so the fleet view
/// shows Boxes, not the operator's laptop or other tailnet devices.
pub const FLEET_TAG: &str = "tag:box";

/// The port boxd listens on — the mesh has no mDNS TXT record to carry it.
const BOXD_PORT: u16 = 2693;

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

/// Peer Boxes on the LAN, via mDNS.
fn discover_lan() -> Vec<Peer> {
    let Some(output) = browse() else {
        return Vec::new();
    };
    parse_avahi(&output)
        .into_iter()
        .map(|(host, address, port)| Peer {
            health: fetch_health(&address, port),
            host,
            address,
            port,
        })
        .collect()
}

/// Extract `tag:box`-tagged peers from `tailscale status --json`: their hostname
/// and first IPv4 tailnet address. Self is excluded (it's under `Self`, not
/// `Peer`), matching mDNS's `-l` "ignore local" behavior.
pub fn parse_tailscale_peers(status: &Value, tag: &str) -> Vec<(String, String)> {
    let Some(peers) = status.get("Peer").and_then(Value::as_object) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for peer in peers.values() {
        let tagged = peer
            .get("Tags")
            .and_then(Value::as_array)
            .is_some_and(|tags| tags.iter().any(|t| t.as_str() == Some(tag)));
        if !tagged {
            continue;
        }
        let host = peer
            .get("HostName")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string();
        // Prefer the IPv4 (100.64.0.0/10 CGNAT) address — a plain HTTP GET to it
        // needs no zone id, unlike the fd7a:: IPv6 the tailnet also assigns.
        let address = peer
            .get("TailscaleIPs")
            .and_then(Value::as_array)
            .and_then(|ips| ips.iter().filter_map(Value::as_str).find(|ip| ip.contains('.')))
            .unwrap_or_default()
            .to_string();
        if host.is_empty() || address.is_empty() {
            continue;
        }
        out.push((host, address));
    }
    out
}

/// Peer Boxes on the mesh, via the tailnet. Empty when tailscale isn't present
/// or this Box hasn't joined a mesh — callers degrade to the LAN plane.
fn discover_tailnet() -> Vec<Peer> {
    let out = Command::new("tailscale")
        .args(["status", "--json"])
        .output()
        .ok();
    let Some(out) = out.filter(|o| o.status.success()) else {
        return Vec::new();
    };
    let Ok(status) = serde_json::from_slice::<Value>(&out.stdout) else {
        return Vec::new();
    };
    parse_tailscale_peers(&status, FLEET_TAG)
        .into_iter()
        .map(|(host, address)| Peer {
            health: fetch_health(&address, BOXD_PORT),
            host,
            address,
            port: BOXD_PORT,
        })
        .collect()
}

/// Discover peer Boxes across both planes (LAN + mesh) and read each one's
/// coarse health. A Box reachable on both appears once — keyed on its stable
/// health id, preferring the reachable sighting.
pub fn discover() -> Vec<Peer> {
    let mut all = discover_lan();
    all.extend(discover_tailnet());

    let mut idx: HashMap<String, usize> = HashMap::new();
    let mut out: Vec<Peer> = Vec::new();
    for p in all {
        // Stable id when we reached it; else the host string (unreachable peers
        // on different planes may not merge, but they're unreachable anyway).
        let key = p
            .health
            .as_ref()
            .map(|h| h.id.clone())
            .unwrap_or_else(|| p.host.clone());
        match idx.get(&key) {
            Some(&i) => {
                if out[i].health.is_none() && p.health.is_some() {
                    out[i] = p; // replace an unreachable sighting with a live one
                }
            }
            None => {
                idx.insert(key, out.len());
                out.push(p);
            }
        }
    }
    out
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

    #[test]
    fn tailnet_peers_keep_tagged_boxes_only() {
        // Two tagged Boxes, plus the operator's untagged laptop (must be ignored)
        // and a Box with only an IPv6 address (no routable IPv4 -> dropped).
        let status = serde_json::json!({
            "Self": { "HostName": "me", "Tags": ["tag:box"], "TailscaleIPs": ["100.64.0.1"] },
            "Peer": {
                "nodekey:aaa": {
                    "HostName": "box-abc",
                    "Tags": ["tag:box"],
                    "TailscaleIPs": ["100.64.0.2", "fd7a:1::2"]
                },
                "nodekey:bbb": {
                    "HostName": "laptop",
                    "TailscaleIPs": ["100.64.0.3"]
                },
                "nodekey:ccc": {
                    "HostName": "box-def",
                    "Tags": ["tag:box"],
                    "TailscaleIPs": ["100.64.0.4"]
                },
                "nodekey:ddd": {
                    "HostName": "box-v6only",
                    "Tags": ["tag:box"],
                    "TailscaleIPs": ["fd7a:1::9"]
                }
            }
        });
        let mut peers = parse_tailscale_peers(&status, FLEET_TAG);
        peers.sort();
        assert_eq!(
            peers,
            vec![
                ("box-abc".to_string(), "100.64.0.2".to_string()),
                ("box-def".to_string(), "100.64.0.4".to_string()),
            ],
            "only tagged Boxes with a routable IPv4 survive; the laptop is excluded"
        );
    }
}
