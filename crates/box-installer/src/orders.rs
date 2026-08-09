//! Reading the storage *policy* out of an orders file (box-install.json) and
//! resolving it against the real disks. Orders may express storage three ways:
//!
//!   "storage": { "layout": "single|mirror|pool", "min_disk_gb": N }  (preferred)
//!   "disk": "/dev/disk/by-id/..."   (an explicit single device)
//!   "disk": "auto" or absent        (single, largest internal — the default)

use anyhow::{anyhow, Context, Result};
use box_core::{resolve, Disk, LayoutKind, ResolveOpts, ResolvedLayout};
use serde_json::Value;

pub struct Policy {
    pub kind: Option<LayoutKind>,
    pub explicit_disk: Option<String>,
    pub min_gb: u64,
}

pub fn read_policy(orders: &Value) -> Policy {
    let storage = orders.get("storage");
    let layout = storage
        .and_then(|s| s.get("layout"))
        .and_then(Value::as_str)
        .and_then(LayoutKind::parse);

    let disk = orders.get("disk").and_then(Value::as_str);
    let explicit_disk = match disk {
        Some(d) if d != "auto" && !d.is_empty() => Some(d.to_string()),
        _ => None,
    };

    let min_gb = storage
        .and_then(|s| s.get("min_disk_gb"))
        .or_else(|| orders.get("min_disk_gb"))
        .and_then(Value::as_u64)
        .unwrap_or(8);

    Policy {
        kind: layout,
        explicit_disk,
        min_gb,
    }
}

/// Resolve a policy to a concrete layout against the probed disks.
///
/// Precedence: an explicit device wins (the user named a disk); otherwise the
/// declared layout kind; otherwise Single (largest internal).
pub fn resolve_policy(disks: &[Disk], policy: &Policy) -> Result<ResolvedLayout> {
    let opts = ResolveOpts {
        min_gb: policy.min_gb,
        ..Default::default()
    };

    if let Some(path) = &policy.explicit_disk {
        let dev = find_disk(disks, path).unwrap_or_else(|| synthetic_disk(path));
        return Ok(ResolvedLayout {
            kind: LayoutKind::Single,
            devices: vec![dev],
            filesystem: "ext4",
            raid: None,
            warnings: Vec::new(),
        });
    }

    let kind = policy.kind.unwrap_or(LayoutKind::Single);
    resolve(disks, kind, &opts).map_err(|e| anyhow!("{e}"))
}

fn find_disk(disks: &[Disk], path: &str) -> Option<Disk> {
    disks
        .iter()
        .find(|d| d.path == path || d.stable_path == path || d.name == path)
        .cloned()
}

/// If orders name a device the probe didn't surface (unusual, but honor the
/// operator's explicit choice), build a minimal Disk around the path.
fn synthetic_disk(path: &str) -> Disk {
    Disk {
        name: path.rsplit('/').next().unwrap_or(path).to_string(),
        path: path.to_string(),
        stable_path: path.to_string(),
        size_bytes: 0,
        model: None,
        serial: None,
        rotational: false,
        removable: false,
        has_content: false,
        transport: None,
    }
}

pub fn load(path: &str) -> Result<Value> {
    let text =
        std::fs::read_to_string(path).with_context(|| format!("reading orders from {path}"))?;
    serde_json::from_str(&text).with_context(|| format!("{path} is not valid JSON"))
}

/// Merge a chosen layout into a base orders object and return the effective
/// orders the installer will act on: consent is forced on, and the resolved
/// storage is recorded for provenance. `base` may be `Value::Null` (a blank
/// box configured entirely at the console).
pub fn effective_orders(base: &Value, layout: &ResolvedLayout) -> Value {
    let mut obj = match base {
        Value::Object(m) => m.clone(),
        _ => serde_json::Map::new(),
    };
    obj.insert("erase_disk".into(), Value::Bool(true));
    if !obj.contains_key("hostname") {
        obj.insert("hostname".into(), Value::String("auto".into()));
    }
    let devices: Vec<Value> = layout
        .devices
        .iter()
        .map(|d| Value::String(d.stable_path.clone()))
        .collect();
    obj.insert(
        "storage".into(),
        serde_json::json!({
            "layout": layout.kind.as_str(),
            "devices": devices,
        }),
    );
    Value::Object(obj)
}
