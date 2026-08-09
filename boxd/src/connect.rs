//! Box Connect: private remote access to this Box from anywhere, over a
//! WireGuard mesh coordinated by a Headscale/Tailscale control server. WireGuard
//! gives mutual-authenticated, encrypted L3 reachability (any service, not just
//! HTTP) with NAT traversal + relay fallback via the coordinator.
//!
//! Free tier: point at your own Headscale (`connect enroll`). Paid: the control
//! plane mints a key and runs the coordinator + DERP relays (`cloud connect`).

use anyhow::{bail, Context, Result};
use serde_json::Value;

/// Join the mesh: `tailscale up` against the given coordinator with a pre-auth
/// key. Idempotent — re-running updates the registration.
pub fn enroll(login_server: &str, authkey: &str, hostname: &str) -> Result<()> {
    let status = std::process::Command::new("tailscale")
        .args([
            "up",
            &format!("--login-server={login_server}"),
            &format!("--authkey={authkey}"),
            &format!("--hostname={hostname}"),
        ])
        .status()
        .context("running `tailscale up` (is tailscaled enabled on this Box?)")?;
    if !status.success() {
        bail!("tailscale up failed");
    }
    Ok(())
}

pub fn status() -> Result<Value> {
    let out = std::process::Command::new("tailscale")
        .args(["status", "--json"])
        .output()
        .context("running `tailscale status`")?;
    if !out.status.success() {
        bail!(
            "tailscale status failed: {}",
            String::from_utf8_lossy(&out.stderr)
        );
    }
    Ok(serde_json::from_slice(&out.stdout).unwrap_or(Value::Null))
}

pub fn down() -> Result<()> {
    let _ = std::process::Command::new("tailscale").arg("down").status();
    Ok(())
}

/// This Box's hostname, used as the mesh node name when none is given.
pub fn default_hostname() -> String {
    std::fs::read_to_string("/proc/sys/kernel/hostname")
        .map(|s| s.trim().to_string())
        .ok()
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "box".to_string())
}
