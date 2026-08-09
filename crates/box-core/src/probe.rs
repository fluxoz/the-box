//! Discover the machine's disks by parsing `lsblk` JSON. The parse is pure and
//! unit-tested against fixtures; only `probe()` itself shells out.

use crate::model::Disk;
use serde::de::{self, Deserializer};
use serde::Deserialize;
use std::io;
use std::process::Command;

/// Run `lsblk` and return the whole disks (never partitions, never removable-
/// filtered here — the resolver decides eligibility).
pub fn probe() -> io::Result<Vec<Disk>> {
    let out = Command::new("lsblk").args(["-J", "-b", "-O"]).output()?;
    if !out.status.success() {
        return Err(io::Error::other(format!(
            "lsblk failed: {}",
            String::from_utf8_lossy(&out.stderr)
        )));
    }
    parse_lsblk(&out.stdout)
}

/// Parse `lsblk -J -b -O` output into whole disks. Pure — testable offline.
pub fn parse_lsblk(json: &[u8]) -> io::Result<Vec<Disk>> {
    let parsed: LsblkOut = serde_json::from_slice(json)
        .map_err(|e| io::Error::other(format!("could not parse lsblk output: {e}")))?;

    let mut disks = Vec::new();
    for d in parsed.blockdevices {
        if d.dtype.as_deref() != Some("disk") {
            continue; // skip partitions, loop, rom, lvm, etc.
        }
        let name = d.name.clone();
        let path = d.path.clone().unwrap_or_else(|| format!("/dev/{name}"));
        let has_content = !d.children.is_empty() || d.fstype.is_some();
        disks.push(Disk {
            stable_path: stable_path(&name).unwrap_or_else(|| path.clone()),
            name,
            path,
            size_bytes: d.size.map(|n| n.0).unwrap_or(0),
            model: clean(d.model),
            serial: clean(d.serial),
            rotational: d.rota.map(|b| b.0).unwrap_or(true),
            removable: d.rm.map(|b| b.0).unwrap_or(false),
            has_content,
            transport: clean(d.tran),
        });
    }
    Ok(disks)
}

fn clean(s: Option<String>) -> Option<String> {
    s.map(|s| s.trim().to_string()).filter(|s| !s.is_empty())
}

/// Find a stable `/dev/disk/by-id/...` name for a kernel device, preferring the
/// most durable identifier and never a `-part` alias. Best-effort: `None` if
/// the directory is absent (some VMs) or nothing points at the device.
fn stable_path(name: &str) -> Option<String> {
    let dir = std::fs::read_dir("/dev/disk/by-id").ok()?;
    let mut best: Option<(u8, String)> = None;
    for entry in dir.flatten() {
        let fname = entry.file_name().to_string_lossy().into_owned();
        if fname.contains("-part") {
            continue;
        }
        let target = match std::fs::read_link(entry.path()) {
            Ok(t) => t,
            Err(_) => continue,
        };
        let points_at = target
            .file_name()
            .map(|n| n.to_string_lossy() == name)
            .unwrap_or(false);
        if !points_at {
            continue;
        }
        let rank = if fname.starts_with("nvme-") {
            3
        } else if fname.starts_with("ata-") || fname.starts_with("scsi-") {
            2
        } else if fname.starts_with("wwn-") {
            1
        } else {
            0
        };
        if best.as_ref().map(|(r, _)| rank > *r).unwrap_or(true) {
            best = Some((rank, format!("/dev/disk/by-id/{fname}")));
        }
    }
    best.map(|(_, p)| p)
}

// ---- lsblk JSON shape (only the fields we use) --------------------------

#[derive(Deserialize)]
struct LsblkOut {
    blockdevices: Vec<LsblkDev>,
}

#[derive(Deserialize)]
struct LsblkDev {
    name: String,
    #[serde(default)]
    path: Option<String>,
    #[serde(default)]
    size: Option<Numish>,
    #[serde(default)]
    model: Option<String>,
    #[serde(default)]
    serial: Option<String>,
    #[serde(default)]
    rota: Option<Boolish>,
    #[serde(default)]
    rm: Option<Boolish>,
    #[serde(rename = "type", default)]
    dtype: Option<String>,
    #[serde(default)]
    tran: Option<String>,
    #[serde(default)]
    fstype: Option<String>,
    #[serde(default)]
    children: Vec<LsblkDev>,
}

/// lsblk has emitted booleans as `true`/`false`, `"0"`/`"1"`, and `0`/`1`
/// across versions — accept all three.
struct Boolish(bool);
impl<'de> Deserialize<'de> for Boolish {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        struct V;
        impl de::Visitor<'_> for V {
            type Value = bool;
            fn expecting(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
                f.write_str("a boolean, 0/1, or \"0\"/\"1\"")
            }
            fn visit_bool<E>(self, v: bool) -> Result<bool, E> {
                Ok(v)
            }
            fn visit_i64<E>(self, v: i64) -> Result<bool, E> {
                Ok(v != 0)
            }
            fn visit_u64<E>(self, v: u64) -> Result<bool, E> {
                Ok(v != 0)
            }
            fn visit_str<E>(self, v: &str) -> Result<bool, E> {
                Ok(matches!(v.trim(), "1" | "true" | "yes"))
            }
        }
        d.deserialize_any(V).map(Boolish)
    }
}

/// Size comes as a JSON number under `-b`, but be forgiving of string forms.
struct Numish(u64);
impl<'de> Deserialize<'de> for Numish {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        struct V;
        impl de::Visitor<'_> for V {
            type Value = u64;
            fn expecting(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
                f.write_str("a byte count")
            }
            fn visit_i64<E>(self, v: i64) -> Result<u64, E> {
                Ok(v.max(0) as u64)
            }
            fn visit_u64<E>(self, v: u64) -> Result<u64, E> {
                Ok(v)
            }
            fn visit_str<E: de::Error>(self, v: &str) -> Result<u64, E> {
                v.trim().parse().map_err(de::Error::custom)
            }
        }
        d.deserialize_any(V).map(Numish)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // A laptop-ish machine: one NVMe (partitioned, in use) + one blank SATA
    // HDD + a USB stick. Mixes boolean encodings on purpose.
    const FIXTURE: &[u8] = br#"{
      "blockdevices": [
        {"name":"nvme0n1","path":"/dev/nvme0n1","size":512110190592,"model":"Samsung SSD 980","serial":"S1","rota":false,"rm":false,"type":"disk","tran":"nvme",
          "children":[{"name":"nvme0n1p1","type":"part","size":1073741824}]},
        {"name":"sda","path":"/dev/sda","size":2000398934016,"model":"WDC WD20","serial":"S2","rota":"1","rm":0,"type":"disk","tran":"sata"},
        {"name":"sdb","path":"/dev/sdb","size":15678432000,"model":"Cruzer","serial":"S3","rota":1,"rm":1,"type":"disk","tran":"usb"}
      ]
    }"#;

    #[test]
    fn parses_disks_and_skips_partitions() {
        let disks = parse_lsblk(FIXTURE).unwrap();
        assert_eq!(disks.len(), 3, "three whole disks, no partitions");
        assert!(disks.iter().all(|d| d.name != "nvme0n1p1"));
    }

    #[test]
    fn decodes_mixed_boolean_encodings() {
        let disks = parse_lsblk(FIXTURE).unwrap();
        let nvme = disks.iter().find(|d| d.name == "nvme0n1").unwrap();
        assert!(!nvme.rotational && !nvme.removable);
        assert!(nvme.has_content, "has a child partition");
        let sda = disks.iter().find(|d| d.name == "sda").unwrap();
        assert!(sda.rotational && !sda.removable);
        assert!(!sda.has_content, "blank disk");
        let usb = disks.iter().find(|d| d.name == "sdb").unwrap();
        assert!(usb.removable, "the USB stick is removable");
    }

    #[test]
    fn sizes_are_decimal_gb() {
        let disks = parse_lsblk(FIXTURE).unwrap();
        assert_eq!(disks[0].size_gb(), 512);
        assert_eq!(disks[1].size_gb(), 2000);
    }
}
