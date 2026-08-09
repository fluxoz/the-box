//! Turn an *intent* (single / mirror / pool) plus the real disks into a
//! concrete, validated layout — or a clear refusal. The cardinal rule: never
//! silently do something destructive or degraded. Ambiguity is an error the
//! caller must surface, not a guess we make.

use crate::model::Disk;
use serde::{Deserialize, Serialize};
use std::fmt;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum LayoutKind {
    /// One disk, whole-disk ext4. The default; works everywhere.
    Single,
    /// RAID1 (mdadm) across two matched disks — survives one disk's death.
    Mirror,
    /// LVM linear across all disks — one big volume, NO redundancy.
    Pool,
}

impl LayoutKind {
    pub fn parse(s: &str) -> Option<Self> {
        match s.trim().to_ascii_lowercase().as_str() {
            "single" => Some(Self::Single),
            "mirror" => Some(Self::Mirror),
            "pool" => Some(Self::Pool),
            _ => None,
        }
    }

    pub fn label(&self) -> &'static str {
        match self {
            Self::Single => "Single disk",
            Self::Mirror => "Mirror (RAID1)",
            Self::Pool => "Pool (spanned)",
        }
    }

    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Single => "single",
            Self::Mirror => "mirror",
            Self::Pool => "pool",
        }
    }
}

/// Eligibility knobs. `size_tolerance` is the fractional size difference two
/// disks may have and still count as "matched" for a mirror.
#[derive(Debug, Clone, Copy)]
pub struct ResolveOpts {
    pub min_gb: u64,
    pub size_tolerance: f64,
}

impl Default for ResolveOpts {
    fn default() -> Self {
        Self {
            min_gb: 8,
            size_tolerance: 0.10,
        }
    }
}

/// A concrete plan: exactly which disks get wiped, how, and what to warn about.
#[derive(Debug, Clone)]
pub struct ResolvedLayout {
    pub kind: LayoutKind,
    /// The disks that will be ERASED and used, in order.
    pub devices: Vec<Disk>,
    pub filesystem: &'static str,
    /// The redundancy mechanism, for display: `mdadm-raid1`, `lvm-linear`, or
    /// `None` for a plain single disk.
    pub raid: Option<&'static str>,
    /// Non-fatal things the human should know before confirming (excluded
    /// disks, the pool's no-redundancy danger).
    pub warnings: Vec<String>,
}

impl ResolvedLayout {
    /// Total usable-ish capacity for display (mirror halves it).
    pub fn usable_gb(&self) -> u64 {
        match self.kind {
            LayoutKind::Single => self.devices.first().map(Disk::size_gb).unwrap_or(0),
            LayoutKind::Mirror => self.devices.iter().map(Disk::size_gb).min().unwrap_or(0),
            LayoutKind::Pool => self.devices.iter().map(Disk::size_gb).sum(),
        }
    }
}

#[derive(Debug, Clone)]
pub enum ResolveError {
    /// Nothing internal and large enough to install onto.
    NoEligibleDisk { min_gb: u64, seen: usize },
    /// Mirror/pool asked for, but fewer than two eligible disks.
    NeedTwoDisks { kind: LayoutKind, have: usize },
    /// Mirror asked for, but the two candidates are too different in size.
    MismatchedSizes {
        larger_gb: u64,
        smaller_gb: u64,
        tol_pct: u64,
    },
}

impl fmt::Display for ResolveError {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        match self {
            Self::NoEligibleDisk { min_gb, seen } => write!(
                f,
                "no eligible disk: need an internal disk of at least {min_gb} GB \
                 (saw {seen} disk(s), all removable or too small)."
            ),
            Self::NeedTwoDisks { kind, have } => write!(
                f,
                "{} needs at least two internal disks; this machine has {have}.",
                kind.label()
            ),
            Self::MismatchedSizes {
                larger_gb,
                smaller_gb,
                tol_pct,
            } => write!(
                f,
                "mirror needs two similarly-sized disks: {larger_gb} GB vs {smaller_gb} GB \
                 differ by more than {tol_pct}%. Use single, or match the disks."
            ),
        }
    }
}

impl std::error::Error for ResolveError {}

/// Internal (non-removable) disks of at least `min_gb`, largest first.
pub fn eligible(disks: &[Disk], opts: &ResolveOpts) -> Vec<Disk> {
    let min = opts.min_gb.saturating_mul(1_000_000_000);
    let mut v: Vec<Disk> = disks
        .iter()
        .filter(|d| !d.removable && d.size_bytes >= min)
        .cloned()
        .collect();
    v.sort_by_key(|d| std::cmp::Reverse(d.size_bytes));
    v
}

/// Resolve an intent against the disks, or refuse with a reason.
pub fn resolve(
    disks: &[Disk],
    kind: LayoutKind,
    opts: &ResolveOpts,
) -> Result<ResolvedLayout, ResolveError> {
    let elig = eligible(disks, opts);
    match kind {
        LayoutKind::Single => {
            let d = elig.first().ok_or(ResolveError::NoEligibleDisk {
                min_gb: opts.min_gb,
                seen: disks.len(),
            })?;
            let mut warnings = Vec::new();
            if elig.len() > 1 {
                warnings.push(format!(
                    "{} other eligible disk(s) will be left untouched.",
                    elig.len() - 1
                ));
            }
            Ok(ResolvedLayout {
                kind,
                devices: vec![d.clone()],
                filesystem: "ext4",
                raid: None,
                warnings,
            })
        }
        LayoutKind::Mirror => {
            if elig.len() < 2 {
                return Err(ResolveError::NeedTwoDisks {
                    kind,
                    have: elig.len(),
                });
            }
            let (a, b) = (&elig[0], &elig[1]);
            let diff = a.size_bytes.abs_diff(b.size_bytes) as f64 / a.size_bytes.max(1) as f64;
            if diff > opts.size_tolerance {
                return Err(ResolveError::MismatchedSizes {
                    larger_gb: a.size_gb(),
                    smaller_gb: b.size_gb(),
                    tol_pct: (opts.size_tolerance * 100.0) as u64,
                });
            }
            let mut warnings = Vec::new();
            if elig.len() > 2 {
                warnings.push(format!(
                    "Mirroring the two largest disks; {} other eligible disk(s) left untouched.",
                    elig.len() - 2
                ));
            }
            Ok(ResolvedLayout {
                kind,
                devices: vec![a.clone(), b.clone()],
                filesystem: "ext4",
                raid: Some("mdadm-raid1"),
                warnings,
            })
        }
        LayoutKind::Pool => {
            if elig.len() < 2 {
                return Err(ResolveError::NeedTwoDisks {
                    kind,
                    have: elig.len(),
                });
            }
            Ok(ResolvedLayout {
                kind,
                devices: elig,
                filesystem: "ext4",
                raid: Some("lvm-linear"),
                warnings: vec![
                    "Pool has NO redundancy: if any one disk fails, ALL data is lost.".into(),
                ],
            })
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn disk(name: &str, gb: u64, removable: bool) -> Disk {
        Disk {
            name: name.into(),
            path: format!("/dev/{name}"),
            stable_path: format!("/dev/disk/by-id/x-{name}"),
            size_bytes: gb * 1_000_000_000,
            model: Some("Test".into()),
            serial: None,
            rotational: false,
            removable,
            has_content: false,
            transport: None,
        }
    }

    #[test]
    fn single_picks_largest_internal_and_warns_about_the_rest() {
        let disks = vec![disk("sda", 500, false), disk("nvme0n1", 1000, false)];
        let r = resolve(&disks, LayoutKind::Single, &ResolveOpts::default()).unwrap();
        assert_eq!(r.devices.len(), 1);
        assert_eq!(r.devices[0].name, "nvme0n1");
        assert_eq!(r.warnings.len(), 1);
    }

    #[test]
    fn single_ignores_removable_and_can_fail() {
        let disks = vec![disk("sdb", 64, true)];
        let err = resolve(&disks, LayoutKind::Single, &ResolveOpts::default()).unwrap_err();
        assert!(matches!(err, ResolveError::NoEligibleDisk { .. }));
    }

    #[test]
    fn mirror_needs_two() {
        let disks = vec![disk("nvme0n1", 1000, false)];
        let err = resolve(&disks, LayoutKind::Mirror, &ResolveOpts::default()).unwrap_err();
        assert!(matches!(err, ResolveError::NeedTwoDisks { have: 1, .. }));
    }

    #[test]
    fn mirror_refuses_mismatched_sizes() {
        let disks = vec![disk("a", 1000, false), disk("b", 500, false)];
        let err = resolve(&disks, LayoutKind::Mirror, &ResolveOpts::default()).unwrap_err();
        assert!(matches!(err, ResolveError::MismatchedSizes { .. }));
    }

    #[test]
    fn mirror_accepts_matched_sizes() {
        let disks = vec![disk("a", 1000, false), disk("b", 1005, false)];
        let r = resolve(&disks, LayoutKind::Mirror, &ResolveOpts::default()).unwrap();
        assert_eq!(r.devices.len(), 2);
        assert_eq!(r.raid, Some("mdadm-raid1"));
        assert_eq!(r.usable_gb(), 1000, "mirror capacity is the smaller disk");
    }

    #[test]
    fn pool_spans_all_and_always_warns() {
        let disks = vec![
            disk("a", 1000, false),
            disk("b", 500, false),
            disk("c", 250, false),
        ];
        let r = resolve(&disks, LayoutKind::Pool, &ResolveOpts::default()).unwrap();
        assert_eq!(r.devices.len(), 3);
        assert_eq!(r.usable_gb(), 1750);
        assert!(r.warnings.iter().any(|w| w.contains("NO redundancy")));
    }
}
