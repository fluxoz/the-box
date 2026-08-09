//! The physical picture of a machine's disks — what the probe produces and the
//! resolver reasons about. Nothing here talks to hardware; see `probe`.

use serde::{Deserialize, Serialize};

/// A whole disk on the machine (never a partition).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Disk {
    /// Kernel name, e.g. `nvme0n1`.
    pub name: String,
    /// Kernel path, e.g. `/dev/nvme0n1`.
    pub path: String,
    /// A stable `/dev/disk/by-id/...` path if one resolves, else equal to
    /// `path`. This is what we write into disko/orders — it survives recabling.
    pub stable_path: String,
    pub size_bytes: u64,
    pub model: Option<String>,
    pub serial: Option<String>,
    /// `true` = spinning HDD, `false` = SSD/NVMe.
    pub rotational: bool,
    /// USB sticks and other removable media — never install targets.
    pub removable: bool,
    /// Already partitioned or holds a filesystem — a hint that it's not blank.
    pub has_content: bool,
    /// `nvme` | `sata` | `usb` | ...
    pub transport: Option<String>,
}

impl Disk {
    /// Size in whole GB (decimal, the way drives are sold and users think).
    pub fn size_gb(&self) -> u64 {
        self.size_bytes / 1_000_000_000
    }

    pub fn kind(&self) -> &'static str {
        if self.rotational {
            "HDD"
        } else {
            "SSD"
        }
    }

    /// A compact human label for the TUI list and the plan output.
    pub fn describe(&self) -> String {
        let model = self
            .model
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .unwrap_or("disk");
        let used = if self.has_content { " · in use" } else { "" };
        format!(
            "{}  {} GB  {}  [{}]{}",
            self.path,
            self.size_gb(),
            model,
            self.kind(),
            used
        )
    }
}
