//! Render a resolved layout to a self-contained disko configuration. This is
//! the single source of truth for on-disk layout — the old fixed
//! `disko-template.nix` is just the `Single` case emitted here.
//!
//! All layouts label the root filesystem `box-root` and the ESP `BOX-ESP`, so
//! Box OS mounts by label and stays decoupled from device names.

use crate::resolve::{LayoutKind, ResolvedLayout};

/// Emit a complete disko config for `disko --mode destroy,format,mount`.
pub fn render(layout: &ResolvedLayout) -> String {
    let devs: Vec<&str> = layout
        .devices
        .iter()
        .map(|d| d.stable_path.as_str())
        .collect();
    match layout.kind {
        LayoutKind::Single => single(devs[0]),
        LayoutKind::Mirror => mirror(&devs),
        LayoutKind::Pool => pool(&devs),
    }
}

/// A 1 GB ESP mounted at /boot, plus the rest as `rest_content`.
fn esp_and_rest(dev: &str, rest_content: &str) -> String {
    format!(
        r#"    {{
      device = "{dev}";
      type = "disk";
      content = {{
        type = "gpt";
        partitions = {{
          ESP = {{
            size = "1G";
            type = "EF00";
            content = {{
              type = "filesystem";
              format = "vfat";
              mountpoint = "/boot";
              mountOptions = [ "umask=0077" ];
              extraArgs = [ "-n" "BOX-ESP" ];
            }};
          }};
          {rest_content}
        }};
      }};
    }}"#
    )
}

/// Whole-disk, ext4 root by label — identical to the original template.
fn single(dev: &str) -> String {
    let root = r#"root = {
            size = "100%";
            content = {
              type = "filesystem";
              format = "ext4";
              mountpoint = "/";
              extraArgs = [ "-L" "box-root" ];
            };
          };"#;
    format!(
        "{{\n  disko.devices.disk.main = {};\n}}\n",
        esp_and_rest(dev, root)
    )
}

/// RAID1 across the disks (mdadm), ext4 root on the array. The first disk
/// carries the ESP; every disk contributes its remaining space to the array,
/// so a single disk failure never loses data.
fn mirror(devs: &[&str]) -> String {
    let raid_part = r#"raid = {
            size = "100%";
            content = { type = "mdraid"; name = "boxroot"; };
          };"#;

    let mut disk_entries = String::new();
    for (i, dev) in devs.iter().enumerate() {
        let entry = if i == 0 {
            esp_and_rest(dev, raid_part)
        } else {
            // Later mirror members are whole-disk array partitions; mdadm sizes
            // the array to the smallest member, so the missing 1 GB ESP on the
            // first disk is harmless.
            format!(
                r#"    {{
      device = "{dev}";
      type = "disk";
      content = {{
        type = "gpt";
        partitions = {{
          {raid_part}
        }};
      }};
    }}"#
            )
        };
        disk_entries.push_str(&format!("      d{i} = {entry};\n"));
    }

    format!(
        r#"{{
  disko.devices = {{
    disk = {{
{disk_entries}    }};
    mdadm = {{
      boxroot = {{
        type = "mdadm";
        level = 1;
        content = {{
          type = "filesystem";
          format = "ext4";
          mountpoint = "/";
          extraArgs = [ "-L" "box-root" ];
        }};
      }};
    }};
  }};
}}
"#
    )
}

/// LVM linear across every disk — one big volume, no redundancy. First disk
/// carries the ESP.
fn pool(devs: &[&str]) -> String {
    let pv_part = r#"pv = {
            size = "100%";
            content = { type = "lvm_pv"; vg = "boxpool"; };
          };"#;

    let mut disk_entries = String::new();
    for (i, dev) in devs.iter().enumerate() {
        let entry = if i == 0 {
            esp_and_rest(dev, pv_part)
        } else {
            format!(
                r#"    {{
      device = "{dev}";
      type = "disk";
      content = {{
        type = "gpt";
        partitions = {{
          {pv_part}
        }};
      }};
    }}"#
            )
        };
        disk_entries.push_str(&format!("      d{i} = {entry};\n"));
    }

    format!(
        r#"{{
  disko.devices = {{
    disk = {{
{disk_entries}    }};
    lvm_vg = {{
      boxpool = {{
        type = "lvm_vg";
        lvs = {{
          root = {{
            size = "100%FREE";
            content = {{
              type = "filesystem";
              format = "ext4";
              mountpoint = "/";
              extraArgs = [ "-L" "box-root" ];
            }};
          }};
        }};
      }};
    }};
  }};
}}
"#
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::Disk;
    use crate::resolve::{resolve, LayoutKind, ResolveOpts};

    fn disk(name: &str, gb: u64) -> Disk {
        Disk {
            name: name.into(),
            path: format!("/dev/{name}"),
            stable_path: format!("/dev/disk/by-id/nvme-{name}"),
            size_bytes: gb * 1_000_000_000,
            model: None,
            serial: None,
            rotational: false,
            removable: false,
            has_content: false,
            transport: None,
        }
    }

    #[test]
    fn single_matches_the_shape_box_os_expects() {
        let r = resolve(
            &[disk("nvme0n1", 512)],
            LayoutKind::Single,
            &ResolveOpts::default(),
        )
        .unwrap();
        let nix = render(&r);
        assert!(nix.contains(r#"device = "/dev/disk/by-id/nvme-nvme0n1""#));
        assert!(nix.contains(r#""-L" "box-root""#));
        assert!(nix.contains(r#""-n" "BOX-ESP""#));
        assert!(nix.contains(r#"mountpoint = "/boot""#));
    }

    #[test]
    fn mirror_builds_an_mdadm_raid1_across_both() {
        let r = resolve(
            &[disk("a", 1000), disk("b", 1000)],
            LayoutKind::Mirror,
            &ResolveOpts::default(),
        )
        .unwrap();
        let nix = render(&r);
        assert!(nix.contains("type = \"mdadm\""));
        assert!(nix.contains("level = 1"));
        assert!(nix.contains(r#"name = "boxroot""#));
        assert!(nix.contains(r#"device = "/dev/disk/by-id/nvme-a""#));
        assert!(nix.contains(r#"device = "/dev/disk/by-id/nvme-b""#));
        // exactly one ESP (on the first disk)
        assert_eq!(nix.matches("BOX-ESP").count(), 1);
    }

    #[test]
    fn pool_builds_one_vg_over_all_pvs() {
        let r = resolve(
            &[disk("a", 1000), disk("b", 500), disk("c", 250)],
            LayoutKind::Pool,
            &ResolveOpts::default(),
        )
        .unwrap();
        let nix = render(&r);
        assert!(nix.contains(r#"type = "lvm_vg""#));
        assert_eq!(
            nix.matches(r#"vg = "boxpool""#).count(),
            3,
            "one PV per disk"
        );
        assert!(nix.contains("100%FREE"));
    }
}
