//! The resident: the Box's own agent, on a leash.
//!
//! Version one is deliberately not a free-roaming tool loop. It is the
//! morning report: on a schedule, the resident gathers what the Box knows
//! about itself — services and their sync states, backup freshness, whether
//! an update waits, the journal's recent story — hands it to a language
//! model, and writes the model's plain-sentence summary back into the
//! journal, signed "resident". Concerns are named. If the model suggests a
//! destructive remedy, it is QUEUED as a pending approval like any agent's
//! ask — the leash is the same leash, and the human is still the human.
//!
//! The brain is any OpenAI-compatible endpoint: an API key and base_url
//! (Anthropic's compatibility surface, xAI, a metered provider) — or the
//! Box's OWN /v1 endpoint once a model is pulled, at which point the machine
//! reflects on itself with its own hardware, which is the whole idea taken
//! to its logical end.

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use crate::config::BoxConfig;
use crate::paths::Paths;

/// Where the resident's brain credentials live (encrypted like every other).
pub const API_KEY_SECRET: &str = "resident-api-key";

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ResidentConfig {
    #[serde(default)]
    pub enabled: bool,
    /// An OpenAI-compatible chat completions base, e.g.
    /// "https://api.x.ai/v1" or "http://127.0.0.1:2693/v1" (the Box itself).
    pub base_url: String,
    pub model: String,
    /// "daily" for now; the scheduler reads this.
    #[serde(default = "default_schedule")]
    pub schedule: String,
}

fn default_schedule() -> String {
    "daily".into()
}

/// Everything the resident considers, gathered as plain facts. Pure, so the
/// report's inputs are testable without a model anywhere near.
pub fn gather(paths: &Paths, config: &BoxConfig) -> Value {
    let services: Vec<Value> = config
        .services
        .iter()
        .map(|s| {
            let sync = crate::pull::read_sync_state(paths, &s.name);
            json!({
                "name": s.name,
                "template": s.template,
                "public": s.public,
                "linked_repo": s.repo.as_ref().map(|l| l.repo.clone()),
                "last_sync_ok": sync.as_ref().map(|st| st.ok),
                "last_sync_error": sync.and_then(|st| st.error),
            })
        })
        .collect();
    let backup_stale = config.backup.as_ref().filter(|b| b.enabled).map(|_| {
        crate::backup::last_backup_age(paths)
            .map(|age| age > chrono::Duration::days(1))
            .unwrap_or(true)
    });
    let update_available = crate::channel::ChannelConfig::load(paths)
        .ok()
        .flatten()
        .and_then(|cfg| crate::channel::check(paths, &cfg).ok())
        .map(|st| st.update_available);
    let story: Vec<String> = crate::journal::recent(paths, 20)
        .into_iter()
        .map(|e| format!("[{}] {}", e.kind, e.what))
        .collect();
    json!({
        "services": services,
        "backup_stale": backup_stale,
        "update_available": update_available,
        "recent_journal": story,
    })
}

/// The instruction the model gets. It answers in strict JSON; suggested
/// actions may only name tools the leash already gates.
pub fn prompt(facts: &Value) -> String {
    format!(
        "You are the resident caretaker of a small personal server (\"a Box\"). \
         Below are today's facts about it. Reply with ONLY a JSON object: \
         {{\"summary\": one warm plain sentence for the owner about the state of their Box, \
         \"concerns\": [short strings naming anything that needs attention, empty if none], \
         \"suggested_actions\": [{{\"tool\": one of \"delete_service\"|\"backup_restore\", \
         \"arguments\": {{...}}, \"reason\": why}}] — almost always empty; suggest only for \
         clear, present problems}}. No markdown, no prose outside the JSON.\n\nFACTS:\n{facts}"
    )
}

#[derive(Debug, Deserialize)]
pub struct Report {
    pub summary: String,
    #[serde(default)]
    pub concerns: Vec<String>,
    #[serde(default)]
    pub suggested_actions: Vec<SuggestedAction>,
}

#[derive(Debug, Deserialize)]
pub struct SuggestedAction {
    pub tool: String,
    #[serde(default)]
    pub arguments: Value,
    #[serde(default)]
    pub reason: String,
}

/// Parse the model's reply, tolerating the fences models love to add.
pub fn parse_report(text: &str) -> Result<Report> {
    let trimmed = text
        .trim()
        .trim_start_matches("```json")
        .trim_start_matches("```")
        .trim_end_matches("```")
        .trim();
    serde_json::from_str(trimmed).context("the model's reply was not the JSON report asked for")
}

fn chat(base_url: &str, key: &str, model: &str, prompt_text: &str) -> Result<String> {
    let body = json!({
        "model": model,
        "messages": [{ "role": "user", "content": prompt_text }],
    });
    let out = std::process::Command::new("curl")
        .args(["-sS", "-m", "120", "-X", "POST"])
        .args(["-H", &format!("Authorization: Bearer {key}")])
        .args(["-H", "Content-Type: application/json"])
        .args(["-d", &body.to_string()])
        .arg(format!(
            "{}/chat/completions",
            base_url.trim_end_matches('/')
        ))
        .output()
        .context("reaching the resident's model")?;
    anyhow::ensure!(out.status.success(), "could not reach {base_url}");
    let v: Value =
        serde_json::from_slice(&out.stdout).context("model endpoint returned non-JSON")?;
    v.get("choices")
        .and_then(|c| c.get(0))
        .and_then(|c| c.get("message"))
        .and_then(|m| m.get("content"))
        .and_then(Value::as_str)
        .map(str::to_string)
        .with_context(|| format!("no completion in the reply: {v}"))
}

/// One report, end to end: gather, think, journal, queue. Returns the report.
pub fn run_report(paths: &Paths) -> Result<Report> {
    let config = BoxConfig::load(paths)?;
    let rc = resident_config(&config)
        .context("the resident is not configured — resident_configure sets its model and key")?;
    let key = crate::secrets::get(paths, API_KEY_SECRET)?
        .context("the resident has no API key stored")?;
    let facts = gather(paths, &config);
    let reply = chat(&rc.base_url, &key, &rc.model, &prompt(&facts))?;
    let report = parse_report(&reply)?;

    crate::journal::record(paths, "resident", &report.summary);
    for c in &report.concerns {
        crate::journal::record(paths, "resident", format!("concern: {c}"));
    }
    for a in &report.suggested_actions {
        // The same leash as any agent: destructive suggestions queue for the
        // human; nothing the resident thinks of runs on its own say-so.
        if matches!(a.tool.as_str(), "delete_service" | "backup_restore") {
            let summary = format!(
                "the resident suggests: {} ({})",
                a.tool,
                if a.reason.is_empty() {
                    "no reason given"
                } else {
                    &a.reason
                }
            );
            let _ = crate::approvals::request(
                paths,
                &a.tool,
                a.arguments.clone(),
                &summary,
                "resident",
                // Not a session id — the caretaker holds no session, so no
                // signed-in device can ever collide with it and approve as
                // "itself". Any operator can approve what the resident asks.
                "resident",
            );
        }
    }
    Ok(report)
}

/// The resident's config rides box.toml under [resident].
pub fn resident_config(config: &BoxConfig) -> Option<ResidentConfig> {
    config.resident.clone().filter(|r| r.enabled)
}

/// The scheduler: one thread, one daily check. Deliberately humble — it
/// wakes hourly, and runs the report when a day has passed since the last.
pub fn spawn(state: crate::web::SharedState) {
    std::thread::Builder::new()
        .name("resident".into())
        .spawn(move || loop {
            std::thread::sleep(std::time::Duration::from_secs(3600));
            let due = {
                let marker = state.paths.data_dir.join("last-resident-report");
                let age = std::fs::metadata(&marker)
                    .and_then(|m| m.modified())
                    .ok()
                    .and_then(|t| t.elapsed().ok());
                !matches!(age, Some(a) if a.as_secs() < 23 * 3600)
            };
            let configured = BoxConfig::load(&state.paths)
                .ok()
                .and_then(|c| resident_config(&c))
                .is_some();
            if due && configured {
                match run_report(&state.paths) {
                    Ok(_) => {
                        let _ =
                            std::fs::write(state.paths.data_dir.join("last-resident-report"), "");
                    }
                    Err(e) => tracing::warn!("resident: report failed: {e:#}"),
                }
            }
        })
        .expect("spawning the resident");
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_report_parses_with_and_without_fences_and_actions() {
        let plain = r#"{"summary": "All quiet.", "concerns": []}"#;
        let r = parse_report(plain).unwrap();
        assert_eq!(r.summary, "All quiet.");
        assert!(r.suggested_actions.is_empty());

        let fenced = "```json\n{\"summary\": \"One worry.\", \"concerns\": [\"backups are stale\"], \"suggested_actions\": [{\"tool\": \"delete_service\", \"arguments\": {\"name\": \"old\"}, \"reason\": \"it has been failing for a week\"}]}\n```";
        let r = parse_report(fenced).unwrap();
        assert_eq!(r.concerns.len(), 1);
        assert_eq!(r.suggested_actions[0].tool, "delete_service");

        assert!(parse_report("I think everything is fine!").is_err());
    }

    #[test]
    fn gather_reports_facts_not_opinions() {
        let tmp = tempfile::TempDir::new().unwrap();
        let paths = Paths::new(tmp.path().to_path_buf());
        paths.ensure().unwrap();
        let facts = gather(&paths, &BoxConfig::default());
        assert!(facts
            .get("services")
            .unwrap()
            .as_array()
            .unwrap()
            .is_empty());
        assert!(facts.get("backup_stale").unwrap().is_null());
    }
}
