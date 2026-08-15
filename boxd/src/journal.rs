//! The ops journal: the Box narrates its own life, in sentences.
//!
//! Everything here is already known somewhere — a deploy in the generations
//! list, a backup in restic, an update in the systemd journal — but scattered
//! across surfaces only an engineer would stitch together. This is the
//! stitched version: one chronological page of plain sentences ("took
//! platform update v0.3.19, healthy", "backed up 1.2 GB in 41 s", "deployed
//! a8d2d5d from a push to main"), because people trust a machine that
//! explains itself, and agents can read the same story the owner does.
//!
//! Deliberately boring storage: one JSON-lines file per day under
//! `<data>/journal/`, append-only, pruned by age. No database, restorable by
//! `cat`, and a torn last line (power loss mid-write) costs one entry, not
//! the page.

use std::fs;
use std::io::Write;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::paths::Paths;

/// Keep roughly this many days of story. Enough to answer "what happened
/// while I was on holiday", small enough to never matter on disk.
const KEEP_DAYS: usize = 90;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Entry {
    pub at_unix: u64,
    /// What kind of thing happened: "deploy" | "update" | "backup" |
    /// "rollback" | "approval" | "preview" | "service". Coarse on purpose —
    /// the sentence carries the detail.
    pub kind: String,
    /// The sentence. Written for the owner, readable by their agent.
    pub what: String,
}

fn dir(paths: &Paths) -> PathBuf {
    paths.data_dir.join("journal")
}

fn day_file(paths: &Paths, at_unix: u64) -> PathBuf {
    let day = chrono::DateTime::from_timestamp(at_unix as i64, 0)
        .map(|t| t.format("%Y-%m-%d").to_string())
        .unwrap_or_else(|| "unknown".into());
    dir(paths).join(format!("{day}.jsonl"))
}

fn now_unix() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or_default()
}

/// Record one sentence. Best-effort by design: the journal must never be the
/// reason an operation fails, so errors are swallowed after a log line.
pub fn record(paths: &Paths, kind: &str, what: impl AsRef<str>) {
    let entry = Entry {
        at_unix: now_unix(),
        kind: kind.to_string(),
        what: what.as_ref().to_string(),
    };
    let write = || -> std::io::Result<()> {
        fs::create_dir_all(dir(paths))?;
        let mut f = fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(day_file(paths, entry.at_unix))?;
        writeln!(f, "{}", serde_json::to_string(&entry).unwrap_or_default())?;
        Ok(())
    };
    if let Err(e) = write() {
        tracing::warn!("journal: could not record entry: {e}");
    }
    prune(paths);
}

/// The most recent `limit` entries, newest first.
pub fn recent(paths: &Paths, limit: usize) -> Vec<Entry> {
    let Ok(entries) = fs::read_dir(dir(paths)) else {
        return Vec::new();
    };
    let mut files: Vec<PathBuf> = entries.flatten().map(|e| e.path()).collect();
    files.sort();
    let mut out: Vec<Entry> = Vec::new();
    for file in files.iter().rev() {
        let Ok(text) = fs::read_to_string(file) else {
            continue;
        };
        let mut day: Vec<Entry> = text
            .lines()
            .filter_map(|l| serde_json::from_str(l).ok())
            .collect();
        day.reverse();
        out.extend(day);
        if out.len() >= limit {
            break;
        }
    }
    out.truncate(limit);
    out
}

fn prune(paths: &Paths) {
    let Ok(entries) = fs::read_dir(dir(paths)) else {
        return;
    };
    let mut files: Vec<PathBuf> = entries.flatten().map(|e| e.path()).collect();
    files.sort();
    while files.len() > KEEP_DAYS {
        let _ = fs::remove_file(files.remove(0));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_story_reads_newest_first_and_survives_torn_lines() {
        let tmp = tempfile::TempDir::new().unwrap();
        let paths = Paths::new(tmp.path().to_path_buf());
        record(&paths, "deploy", "deployed abc123 from a push to main");
        record(&paths, "backup", "backed up 1.2 GB in 41 s");
        // A torn line from a power loss must not poison the page.
        let file = fs::read_dir(dir(&paths))
            .unwrap()
            .next()
            .unwrap()
            .unwrap()
            .path();
        let mut f = fs::OpenOptions::new().append(true).open(&file).unwrap();
        write!(f, "{{\"at_unix\":123,\"ki").unwrap();

        let got = recent(&paths, 10);
        assert_eq!(got.len(), 2);
        assert!(got[0].what.contains("backed up"), "newest first");
        assert!(got[1].what.contains("deployed"));
    }
}
