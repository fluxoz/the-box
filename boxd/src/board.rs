//! Which physical board this box runs on. The OS tier must compose board
//! support — vendor kernel, firmware, boot method — into any system it
//! rebuilds: a Raspberry Pi switched onto a generic aarch64 build comes back
//! from its next reboot a brick. Detection reads the device tree; the guard
//! refuses an OS-tier build whose board binding doesn't match the hardware.

use anyhow::{bail, Result};

/// Boards the platform can build a bootable system for (`lib.boxSystem`).
pub const SUPPORTED: &[&str] = &["pi3", "pi4", "pi5"];

/// The hardware model string from the device tree, if this machine has one
/// (x86 machines don't — they are never a board in this sense).
fn model() -> Option<String> {
    std::fs::read_to_string("/proc/device-tree/model")
        .ok()
        .map(|m| m.trim_end_matches('\0').trim().to_string())
        .filter(|m| !m.is_empty())
}

/// Map a device-tree model string to a supported board id.
pub fn from_model(model: &str) -> Option<&'static str> {
    let m = model.to_ascii_lowercase();
    if m.contains("raspberry pi 5") {
        Some("pi5")
    } else if m.contains("raspberry pi 4") || m.contains("compute module 4") {
        Some("pi4")
    } else if m.contains("raspberry pi 3") {
        Some("pi3")
    } else {
        None
    }
}

/// Detect this machine's board: `None` on generic hardware, `Some(board)` on a
/// supported Pi — and an error on a Raspberry Pi model the platform can't
/// build a bootable system for, so no caller ever falls back to a generic
/// build on Pi hardware.
pub fn detect() -> Result<Option<String>> {
    let Some(model) = model() else {
        return Ok(None);
    };
    match from_model(&model) {
        Some(b) => Ok(Some(b.to_string())),
        None if model.to_ascii_lowercase().contains("raspberry pi") => bail!(
            "this machine is a {model:?}, a Raspberry Pi model the platform can't build a bootable system for yet"
        ),
        None => Ok(None),
    }
}

/// Resolve a user-supplied board argument: `auto` detects, `none` forces the
/// generic appliance, anything else must be a supported board id.
pub fn resolve_arg(arg: &str) -> Result<Option<String>> {
    match arg {
        "auto" => detect(),
        "none" => Ok(None),
        b if SUPPORTED.contains(&b) => Ok(Some(b.to_string())),
        other => bail!(
            "unknown board {other:?} (use auto, none, or one of: {})",
            SUPPORTED.join(", ")
        ),
    }
}

/// Refuse an OS-tier build/switch whose board binding doesn't match this
/// machine. Run before every reconcile: this is the line between a Pi and an
/// unbootable generic system.
pub fn assert_compatible(bound: Option<&str>) -> Result<()> {
    let detected = detect()?;
    match (detected.as_deref(), bound) {
        (Some(d), Some(b)) if d == b => Ok(()),
        (Some(d), Some(b)) => bail!(
            "the channel is bound to board {b:?} but this machine is a {d:?} — fix with `boxd channel set --board auto` (plus your host id/platform)"
        ),
        (Some(d), None) => bail!(
            "this machine is a Raspberry Pi ({d}) but its channel binding has no board — a generic rebuild would not boot it. Re-run `boxd channel set` (board is auto-detected) before updating"
        ),
        (None, Some(b)) => bail!(
            "the channel is bound to board {b:?} but this machine doesn't look like one — if the config moved to new hardware, re-run `boxd channel set`"
        ),
        (None, None) => Ok(()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn maps_pi_models() {
        assert_eq!(from_model("Raspberry Pi 5 Model B Rev 1.0"), Some("pi5"));
        assert_eq!(from_model("Raspberry Pi 4 Model B Rev 1.4"), Some("pi4"));
        assert_eq!(from_model("Raspberry Pi Compute Module 4"), Some("pi4"));
        assert_eq!(
            from_model("Raspberry Pi 3 Model B Plus Rev 1.3"),
            Some("pi3")
        );
        // Unsupported Pis map to None (detect() turns that into an error).
        assert_eq!(from_model("Raspberry Pi Zero 2 W"), None);
        // Non-Pi hardware is simply not a board.
        assert_eq!(from_model("Jetson Nano"), None);
    }

    #[test]
    fn resolve_arg_validates() {
        assert_eq!(resolve_arg("none").unwrap(), None);
        assert_eq!(resolve_arg("pi5").unwrap().as_deref(), Some("pi5"));
        assert!(resolve_arg("pi6").is_err());
    }
}
