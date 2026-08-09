//! Non-interactive paths: `probe` (JSON inventory) and `plan` (resolve the
//! storage policy in an orders file to a disko config). These are the seam the
//! shell installer calls for the pre-baked-orders door.

use crate::orders;
use anyhow::{Context, Result};
use box_core::{disko, probe, ResolvedLayout};

pub fn probe_cmd() -> Result<()> {
    let disks = probe::probe().context("probing disks")?;
    println!("{}", serde_json::to_string_pretty(&disks)?);
    Ok(())
}

pub fn plan_cmd(orders_path: &str, out: &str) -> Result<()> {
    let ord = orders::load(orders_path)?;
    let policy = orders::read_policy(&ord);
    let disks = probe::probe().context("probing disks")?;
    let layout =
        orders::resolve_policy(&disks, &policy).context("resolving storage layout from orders")?;
    let nix = disko::render(&layout);
    std::fs::write(out, &nix).with_context(|| format!("writing disko config to {out}"))?;
    // Human plan on stderr; stdout stays clean for any machine consumer.
    eprintln!("{}", plan_summary(&layout));
    Ok(())
}

/// A plain-text summary of what a layout will do — shared by `plan` output and
/// the wizard's confirmation screen.
pub fn plan_summary(layout: &ResolvedLayout) -> String {
    let mut s = String::new();
    s.push_str(&format!(
        "Layout: {} ({}",
        layout.kind.label(),
        layout.filesystem
    ));
    if let Some(raid) = layout.raid {
        s.push_str(&format!(", {raid}"));
    }
    s.push_str(&format!(
        ")\nUsable: ~{} GB\nWill ERASE:\n",
        layout.usable_gb()
    ));
    for d in &layout.devices {
        s.push_str(&format!("  - {}\n", d.describe()));
    }
    for w in &layout.warnings {
        s.push_str(&format!("  ! {w}\n"));
    }
    s
}
