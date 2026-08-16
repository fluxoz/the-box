//! The memory vault: durable, private memory on the Box that any connected
//! agent can write to and recall from. Notes never leave the machine.
//!
//! Version one is deliberately plain: an append-only JSONL file and keyword
//! recall. That is enough for agents to leave each other context across
//! sessions ("the deploy key lives in...", "the customer prefers...") with
//! zero model dependencies. Semantic recall arrives when the Box has an
//! embedding model to do it with; the storage format already carries what
//! that will need.

use anyhow::Result;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Note {
    pub at_unix: i64,
    /// Who left it: a device label, "resident", "work", a human.
    pub by: String,
    pub text: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tags: Vec<String>,
}

fn path(paths: &crate::paths::Paths) -> std::path::PathBuf {
    paths.data_dir.join("vault.jsonl")
}

pub fn save(paths: &crate::paths::Paths, by: &str, text: &str, tags: Vec<String>) -> Result<()> {
    let note = Note {
        at_unix: chrono::Utc::now().timestamp(),
        by: by.to_string(),
        text: text.to_string(),
        tags,
    };
    let line = serde_json::to_string(&note)?;
    use std::io::Write as _;
    let mut f = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path(paths))?;
    writeln!(f, "{line}")?;
    Ok(())
}

fn all(paths: &crate::paths::Paths) -> Vec<Note> {
    std::fs::read_to_string(path(paths))
        .unwrap_or_default()
        .lines()
        .filter_map(|l| serde_json::from_str(l).ok())
        .collect()
}

/// Keyword recall: every query term must appear (case-insensitive) in the
/// text or tags. Most recent first, capped.
pub fn search(paths: &crate::paths::Paths, query: &str, limit: usize) -> Vec<Note> {
    let terms: Vec<String> = query.split_whitespace().map(|t| t.to_lowercase()).collect();
    let mut hits: Vec<Note> = all(paths)
        .into_iter()
        .filter(|n| {
            let hay = format!("{} {}", n.text, n.tags.join(" ")).to_lowercase();
            terms.iter().all(|t| hay.contains(t))
        })
        .collect();
    hits.sort_by_key(|n| -n.at_unix);
    hits.truncate(limit);
    hits
}

/// Forget every note whose text or tags match the query terms; returns how
/// many were removed. Rewrites the file — the vault is small by design.
pub fn forget(paths: &crate::paths::Paths, query: &str) -> Result<usize> {
    let terms: Vec<String> = query.split_whitespace().map(|t| t.to_lowercase()).collect();
    let notes = all(paths);
    let (gone, kept): (Vec<_>, Vec<_>) = notes.into_iter().partition(|n| {
        let hay = format!("{} {}", n.text, n.tags.join(" ")).to_lowercase();
        terms.iter().all(|t| hay.contains(t))
    });
    let text = kept
        .iter()
        .filter_map(|n| serde_json::to_string(n).ok())
        .collect::<Vec<_>>()
        .join("\n");
    std::fs::write(
        path(paths),
        if text.is_empty() { text } else { text + "\n" },
    )?;
    Ok(gone.len())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn notes_persist_recall_by_every_term_and_forget_on_request() {
        let tmp = tempfile::TempDir::new().unwrap();
        let paths = crate::paths::Paths::new(tmp.path().to_path_buf());
        paths.ensure().unwrap();
        save(
            &paths,
            "test",
            "the deploy key lives in the blue drawer",
            vec!["ops".into()],
        )
        .unwrap();
        save(&paths, "test", "the customer prefers dark mode", vec![]).unwrap();

        let hits = search(&paths, "deploy drawer", 10);
        assert_eq!(hits.len(), 1);
        assert!(hits[0].text.contains("blue drawer"));
        assert!(search(&paths, "deploy customer", 10).is_empty());

        assert_eq!(forget(&paths, "dark mode").unwrap(), 1);
        assert!(search(&paths, "customer", 10).is_empty());
        assert_eq!(search(&paths, "deploy", 10).len(), 1);
    }
}
