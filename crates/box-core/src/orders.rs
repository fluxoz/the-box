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
