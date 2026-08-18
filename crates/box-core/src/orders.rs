//! The effective orders an install acts on — a chosen storage layout merged
//! into whatever base configuration (name / wifi / keys / pairing hash) the
//! operator supplied. Shared so the console wizard, the browser wizard, and the
//! non-interactive path all produce identical orders.

use crate::ResolvedLayout;
use serde_json::Value;

/// Merge a resolved layout into base orders and return the effective orders:
/// consent is forced on, a hostname defaults to `auto`, and the resolved
/// storage is recorded for provenance. `base` may be `Value::Null` (a blank box
/// configured entirely at the wizard, with no Configurator handoff).
pub fn effective_orders(base: &Value, layout: &ResolvedLayout) -> Value {
    let mut obj = match base {
        Value::Object(m) => m.clone(),
        _ => serde_json::Map::new(),
    };
    obj.insert("erase_disk".into(), Value::Bool(true));
    // The setup PIN guards only the live install wizard; it must not persist
    // into the installed box's config.
    obj.remove("setup_pin");
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

/// Guarantee the orders carry an owner, minting one if the operator did not
/// bring their own. Returns the plaintext code **only when it had to create
/// one**, so the caller can put it in front of the person standing there.
///
/// Every install path must call this. A Box that boots with no enrollment hash
/// has no owner, and the only remaining way to claim it is to ask over the
/// network — which is a race that a script announcing itself on mDNS wins
/// against a human every time. See docs/claim-flow-spec.md.
pub fn ensure_enrollment(orders: &mut Value) -> std::io::Result<Option<String>> {
    let Value::Object(obj) = orders else {
        return Ok(None);
    };
    let present = obj
        .get("enrollment_code_hash")
        .and_then(Value::as_str)
        .is_some_and(|h| !h.trim().is_empty());
    if present {
        return Ok(None);
    }
    let code = crate::pairing::generate()?;
    obj.insert(
        "enrollment_code_hash".into(),
        Value::String(crate::pairing::hash(&code)),
    );
    Ok(Some(code))
}


/// Would an installer accept these orders and run unattended?
///
/// The single source of truth for that question. It exists because the
/// Configurator and the installer are separate programs on separate machines,
/// and the only thing connecting them is this JSON: the person fills in a
/// detailed setup in a browser, and minutes later a machine with nobody in
/// front of it either acts on it or refuses. When those two disagree the
/// failure lands after the disk is already gone, or on a Box that boots and
/// cannot be paired with.
///
/// So the installer calls this, and the browser test drives the real page and
/// feeds what it produces through the same function. Neither can drift without
/// the other noticing.
pub fn validate_for_install(orders: &Value) -> Result<(), String> {
    let Value::Object(o) = orders else {
        return Err("orders must be a JSON object".into());
    };

    // Consent. Without it nothing may touch a disk.
    if o.get("erase_disk").and_then(Value::as_bool) != Some(true) {
        return Err("orders do not set \"erase_disk\": true".into());
    }

    // Ownership. A Box that boots without this has no owner, and the only way
    // left to claim it is over the network, first-come.
    let hash = o
        .get("enrollment_code_hash")
        .and_then(Value::as_str)
        .unwrap_or_default();
    if hash.len() != 64 || !hash.bytes().all(|b| b.is_ascii_hexdigit()) {
        return Err(format!(
            "enrollment_code_hash must be a 64-character hex digest (got {} characters)",
            hash.len()
        ));
    }

    match o.get("hostname").and_then(Value::as_str) {
        None | Some("auto") => {}
        Some(h) => {
            let ok = !h.is_empty()
                && h.len() <= 63
                && !h.starts_with('-')
                && !h.ends_with('-')
                && h.bytes()
                    .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-');
            if !ok {
                return Err(format!("hostname {h:?} is not a usable DNS label"));
            }
        }
    }

    // These land in authorized_keys, one per line. A newline in a value would
    // turn one entry into two.
    if let Some(keys) = o.get("ssh_authorized_keys") {
        let Some(keys) = keys.as_array() else {
            return Err("ssh_authorized_keys must be an array".into());
        };
        for k in keys {
            let Some(k) = k.as_str() else {
                return Err("each SSH key must be a string".into());
            };
            if k.trim().is_empty() || k.contains(['\n', '\r', '\0']) {
                return Err("each SSH key must be a single non-empty line".into());
            }
            if !(k.starts_with("ssh-") || k.starts_with("ecdsa-") || k.starts_with("sk-")) {
                return Err(format!("{:?} does not look like an OpenSSH public key", &k[..k.len().min(24)]));
            }
        }
    }

    // Wi-Fi is written into a NetworkManager connection file, key=value per
    // line. A newline in either field rewrites that file's meaning.
    if let Some(w) = o.get("wifi") {
        let Some(w) = w.as_object() else {
            return Err("wifi must be an object".into());
        };
        let ssid = w.get("ssid").and_then(Value::as_str).unwrap_or_default();
        if ssid.trim().is_empty() {
            return Err("wifi.ssid is required when wifi is set".into());
        }
        for (k, v) in w {
            if v.as_str().is_some_and(|s| s.contains(['\n', '\r', '\0'])) {
                return Err(format!("wifi.{k} must be a single line"));
            }
        }
    }

    if let Some(st) = o.get("storage").and_then(Value::as_object) {
        let layout = st.get("layout").and_then(Value::as_str).unwrap_or_default();
        if !matches!(layout, "single" | "mirror" | "pool" | "ask") {
            return Err(format!("storage.layout {layout:?} is not one this installer knows"));
        }
    }

    Ok(())
}

#[cfg(test)]
mod install_validation_tests {
    use super::*;
    use serde_json::json;

    fn good() -> Value {
        json!({
            "erase_disk": true,
            "hostname": "kitchen",
            "enrollment_code_hash": "a".repeat(64),
            "ssh_authorized_keys": ["ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAI me@laptop"],
            "wifi": { "ssid": "HomeNet", "password": "hunter2" },
            "storage": { "layout": "single", "devices": ["/dev/disk/by-id/x"] }
        })
    }

    #[test]
    fn a_full_configurator_setup_is_installable() {
        validate_for_install(&good()).expect("a complete setup must be accepted");
        let mut auto = good();
        auto["hostname"] = json!("auto");
        validate_for_install(&auto).unwrap();
    }

    #[test]
    fn the_things_that_would_strand_a_machine_are_refused() {
        // No consent: never touch a disk.
        let mut o = good();
        o["erase_disk"] = json!(false);
        assert!(validate_for_install(&o).is_err());

        // No owner: the Box would boot claimable by whoever asks first.
        for bad in [json!(null), json!(""), json!("deadbeef"), json!("g".repeat(64))] {
            let mut o = good();
            o["enrollment_code_hash"] = bad;
            assert!(validate_for_install(&o).is_err(), "must refuse a bad hash");
        }

        // A newline turns one authorized key into two.
        let mut o = good();
        o["ssh_authorized_keys"] = json!(["ssh-rsa AAA\nssh-rsa INJECTED"]);
        assert!(validate_for_install(&o).is_err());

        // ...and would rewrite the Wi-Fi connection file.
        let mut o = good();
        o["wifi"] = json!({ "ssid": "a\nkey-mgmt=none" });
        assert!(validate_for_install(&o).is_err());

        let mut o = good();
        o["hostname"] = json!("Not A Hostname");
        assert!(validate_for_install(&o).is_err());

        let mut o = good();
        o["storage"] = json!({ "layout": "raid9" });
        assert!(validate_for_install(&o).is_err());
    }
}
