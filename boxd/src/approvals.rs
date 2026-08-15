//! The trust ceremony: agents drive everything, humans hold the veto.
//!
//! By default a paired agent may not run a destructive operation — wipe a
//! machine, delete a service, restore over live data. Asking for one creates
//! a *pending action* the operator approves or denies with a tap in the
//! console. Approval executes the exact call the agent made, through the same
//! code path; the agent polls `approval_status` and picks up the result.
//!
//! The operator can also promote a specific session to full autonomy in the
//! device list, at which point its destructive calls run immediately — a
//! deliberate, per-agent, revocable grant. "Agents run your infrastructure,
//! you hold the veto" is the product's security story; this module is where
//! it is true.

use std::fs;
use std::path::PathBuf;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::paths::Paths;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum State {
    Pending,
    Approved,
    Denied,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PendingAction {
    pub id: String,
    pub created_unix: u64,
    /// The MCP tool this would run, e.g. "delete_service".
    pub tool: String,
    /// The exact arguments the agent sent — approval runs precisely these.
    pub args: Value,
    /// One line for the human: what is being asked, in consequences.
    pub summary: String,
    /// The session label that asked (from the device list).
    pub requested_by: String,
    pub state: State,
    /// The executed call's outcome (JSON on success, error text on failure),
    /// present once approved and run.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub result: Option<String>,
}

fn dir(paths: &Paths) -> PathBuf {
    paths.data_dir.join("approvals")
}

fn file(paths: &Paths, id: &str) -> PathBuf {
    // ids are minted by us (hex); no traversal to guard here.
    dir(paths).join(format!("{id}.json"))
}

fn write(paths: &Paths, action: &PendingAction) -> Result<()> {
    fs::create_dir_all(dir(paths))?;
    let f = file(paths, &action.id);
    fs::write(&f, serde_json::to_vec_pretty(action)?)
        .with_context(|| format!("writing {}", f.display()))?;
    Ok(())
}

/// Queue a destructive call for a human decision. Returns the action.
pub fn request(
    paths: &Paths,
    tool: &str,
    args: Value,
    summary: &str,
    requested_by: &str,
) -> Result<PendingAction> {
    let action = PendingAction {
        id: crate::auth::random_hex(8)?,
        created_unix: std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or_default(),
        tool: tool.to_string(),
        args,
        summary: summary.to_string(),
        requested_by: requested_by.to_string(),
        state: State::Pending,
        result: None,
    };
    write(paths, &action)?;
    Ok(action)
}

pub fn get(paths: &Paths, id: &str) -> Option<PendingAction> {
    serde_json::from_slice(&fs::read(file(paths, id)).ok()?).ok()
}

/// Every action, newest first — the console's approvals page.
pub fn list(paths: &Paths) -> Vec<PendingAction> {
    let Ok(entries) = fs::read_dir(dir(paths)) else {
        return Vec::new();
    };
    let mut out: Vec<PendingAction> = entries
        .flatten()
        .filter_map(|e| serde_json::from_slice(&fs::read(e.path()).ok()?).ok())
        .collect();
    out.sort_by_key(|a| std::cmp::Reverse(a.created_unix));
    out
}

/// Record the human's decision (and, for approvals, what running it did).
pub fn resolve(paths: &Paths, id: &str, state: State, result: Option<String>) -> Result<()> {
    let mut action = get(paths, id).with_context(|| format!("no pending action {id:?}"))?;
    action.state = state;
    action.result = result;
    write(paths, &action)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn the_ledger_round_trips_and_newest_comes_first() {
        let tmp = tempfile::TempDir::new().unwrap();
        let paths = Paths::new(tmp.path().to_path_buf());
        let a = request(
            &paths,
            "delete_service",
            json!({"name":"x"}),
            "delete x",
            "agent",
        )
        .unwrap();
        let b = request(
            &paths,
            "provision_machine",
            json!({"target":"t"}),
            "wipe t",
            "agent",
        )
        .unwrap();
        assert_eq!(get(&paths, &a.id).unwrap().state, State::Pending);

        resolve(
            &paths,
            &a.id,
            State::Approved,
            Some("{\"deleted\":\"x\"}".into()),
        )
        .unwrap();
        resolve(&paths, &b.id, State::Denied, None).unwrap();

        let all = list(&paths);
        assert_eq!(all.len(), 2);
        assert_eq!(get(&paths, &a.id).unwrap().state, State::Approved);
        assert_eq!(get(&paths, &b.id).unwrap().state, State::Denied);
        assert!(get(&paths, &a.id)
            .unwrap()
            .result
            .unwrap()
            .contains("deleted"));
    }
}
