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
