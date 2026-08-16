//! The model router: one endpoint that is yours, wherever the tokens come
//! from. Local model first, always; a cloud fallback (the owner's own API
//! key) only when the Box has no model server to answer with. The stats it
//! keeps are the ownership argument in numbers: how much of your AI ran on
//! your own hardware this month, and what the cloud would have billed for it.

use anyhow::Result;
use serde::{Deserialize, Serialize};
use serde_json::json;

/// Where the fallback's API key lives (encrypted like every secret).
pub const FALLBACK_KEY_SECRET: &str = "router-fallback-key";
/// Blended list-price estimate per million output tokens, for the savings
/// line. Deliberately conservative; the point is scale, not accounting.
const USD_PER_MILLION_TOKENS: f64 = 3.0;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct RouterConfig {
    #[serde(default)]
    pub enabled: bool,
    /// OpenAI-compatible base for the fallback, e.g. "https://api.x.ai/v1".
    pub base_url: String,
}

/// Where a /v1 request should go right now.
pub struct Target {
    /// Base URL up to and including "/v1".
    pub base: String,
    /// Extra Authorization header for a cloud fallback; None for local.
    pub auth: Option<String>,
    pub local: bool,
}

/// Local model server first; the configured fallback only when there is
/// none. Returns None when neither exists (the caller names both fixes).
pub fn resolve(paths: &crate::paths::Paths, local_port: Option<u16>) -> Option<Target> {
    if let Some(port) = local_port {
        return Some(Target {
            base: format!("http://127.0.0.1:{port}/v1"),
            auth: None,
            local: true,
        });
    }
    let config = crate::config::BoxConfig::load(paths).ok()?;
    let rc = config.router.filter(|r| r.enabled)?;
    let key = crate::secrets::get(paths, FALLBACK_KEY_SECRET).ok()??;
    Some(Target {
        base: rc.base_url.trim_end_matches('/').to_string(),
        auth: Some(format!("Authorization: Bearer {key}")),
        local: false,
    })
}

#[derive(Debug, Default, Clone, Serialize, Deserialize)]
pub struct Stats {
    pub local_requests: u64,
    pub cloud_requests: u64,
    pub local_tokens: u64,
    pub cloud_tokens: u64,
}

fn stats_path(paths: &crate::paths::Paths) -> std::path::PathBuf {
    paths.data_dir.join("router-stats.json")
}

pub fn stats(paths: &crate::paths::Paths) -> Stats {
    std::fs::read_to_string(stats_path(paths))
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default()
}

/// Best-effort accounting; a lost count is not worth a failed completion.
pub fn record(paths: &crate::paths::Paths, local: bool, tokens: u64) {
    let mut s = stats(paths);
    if local {
        s.local_requests += 1;
        s.local_tokens += tokens;
    } else {
        s.cloud_requests += 1;
        s.cloud_tokens += tokens;
    }
    if let Ok(text) = serde_json::to_string_pretty(&s) {
        let _ = std::fs::write(stats_path(paths), text);
    }
}

/// The status object router_status returns: raw counts plus the sentence
/// the meter wants to say.
pub fn status_json(paths: &crate::paths::Paths) -> Result<serde_json::Value> {
    let config = crate::config::BoxConfig::load(paths)?;
    let s = stats(paths);
    let saved = s.local_tokens as f64 / 1_000_000.0 * USD_PER_MILLION_TOKENS;
    Ok(json!({
        "fallback": config.router.as_ref().map(|r| json!({
            "enabled": r.enabled,
            "base_url": r.base_url,
        })),
        "stats": s,
        "estimated_saved_usd": (saved * 100.0).round() / 100.0,
        "note": "local tokens priced at a conservative blended cloud rate; \
                 streaming responses count requests but not tokens",
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stats_accumulate_and_savings_stay_conservative() {
        let tmp = tempfile::TempDir::new().unwrap();
        let paths = crate::paths::Paths::new(tmp.path().to_path_buf());
        paths.ensure().unwrap();
        record(&paths, true, 500_000);
        record(&paths, true, 500_000);
        record(&paths, false, 1000);
        let s = stats(&paths);
        assert_eq!(s.local_requests, 2);
        assert_eq!(s.local_tokens, 1_000_000);
        assert_eq!(s.cloud_requests, 1);
        let v = status_json(&paths).unwrap();
        assert_eq!(v["estimated_saved_usd"], 3.0);
    }

    #[test]
    fn local_always_wins_and_fallback_needs_its_key() {
        let tmp = tempfile::TempDir::new().unwrap();
        let paths = crate::paths::Paths::new(tmp.path().to_path_buf());
        paths.ensure().unwrap();
        // A local port wins unconditionally.
        let t = resolve(&paths, Some(9999)).unwrap();
        assert!(t.local);
        assert!(t.auth.is_none());
        // No local, no config: nothing.
        assert!(resolve(&paths, None).is_none());
        // Config without a stored key: still nothing (never half-route).
        let config = crate::config::BoxConfig {
            router: Some(RouterConfig {
                enabled: true,
                base_url: "https://api.example.com/v1".into(),
            }),
            ..Default::default()
        };
        config.save(&paths).unwrap();
        assert!(resolve(&paths, None).is_none());
    }
}
