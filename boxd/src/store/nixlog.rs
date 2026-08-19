//! Parse `nix --log-format internal-json` into real progress.
//!
//! Nix's own progress bar is fed by `@nix {...}` lines on stderr: activities
//! start and stop, aggregate activities report (done, expected) counts, and
//! builders' output arrives as result lines. This module turns that stream
//! into the two things a job view can honestly show — the log lines as they
//! happen, and a fraction measured by nix itself rather than a clock.
//!
//! The numeric constants are nix's `ActivityType` / `ResultType` enums
//! (`src/libutil/logging.hh`), stable across releases for years — the
//! external progress-bar protocol every nix UI speaks.

use std::collections::HashMap;

use serde_json::Value;

use super::BuildWatch;

// ActivityType (the ones acted on).
const ACT_COPY_PATH: u64 = 100;
const ACT_COPY_PATHS: u64 = 103;
const ACT_BUILDS: u64 = 104;
const ACT_BUILD: u64 = 105;
const ACT_SUBSTITUTE: u64 = 108;

// ResultType.
const RES_BUILD_LOG_LINE: u64 = 101;
const RES_SET_PHASE: u64 = 104;
const RES_PROGRESS: u64 = 105;
const RES_POST_BUILD_LOG_LINE: u64 = 107;

#[derive(Default)]
struct Aggregate {
    done: u64,
    expected: u64,
}

/// Feed lines in, get log lines and unit counts out.
#[derive(Default)]
pub struct NixLog {
    /// Activity id -> its type, so results can be attributed.
    activities: HashMap<u64, u64>,
    /// The two aggregate activities nix totals its own bar from.
    builds: Aggregate,
    copies: Aggregate,
    /// Rolling tail of everything seen, for the error message when the build
    /// fails: the watch is optional, the diagnosis is not.
    tail: Vec<String>,
}

const TAIL_LIMIT: usize = 40;

impl NixLog {
    pub fn new() -> Self {
        Self::default()
    }

    /// The last lines seen, newline-joined — the failure diagnosis.
    pub fn tail(&self) -> String {
        self.tail.join("\n")
    }

    fn keep(&mut self, line: &str, watch: Option<&dyn BuildWatch>) {
        let line = &strip_ansi(line);
        self.tail.push(line.to_string());
        if self.tail.len() > TAIL_LIMIT {
            let cut = self.tail.len() - TAIL_LIMIT;
            self.tail.drain(..cut);
        }
        if let Some(w) = watch {
            w.line(line);
        }
    }

    fn push_units(&self, watch: Option<&dyn BuildWatch>) {
        let done = self.builds.done + self.copies.done;
        let expected = self.builds.expected + self.copies.expected;
        if expected > 0 {
            if let Some(w) = watch {
                w.units(done, expected);
            }
        }
    }

    /// One stderr line from nix. Non-`@nix` lines pass through as plain log.
    pub fn line(&mut self, raw: &str, watch: Option<&dyn BuildWatch>) {
        let Some(json) = raw.strip_prefix("@nix ") else {
            if !raw.trim().is_empty() {
                self.keep(raw, watch);
            }
            return;
        };
        let Ok(msg) = serde_json::from_str::<Value>(json) else {
            return;
        };
        let field_u64 = |k: &str| msg.get(k).and_then(Value::as_u64);
        let field_str = |k: &str| msg.get(k).and_then(Value::as_str);
        match field_str("action") {
            Some("start") => {
                let (Some(id), Some(typ)) = (field_u64("id"), field_u64("type")) else {
                    return;
                };
                self.activities.insert(id, typ);
                // The per-item activities read like a build log on their own:
                // "building /nix/store/…", "fetching … from https://cache…".
                if matches!(typ, ACT_BUILD | ACT_SUBSTITUTE | ACT_COPY_PATH) {
                    if let Some(text) = field_str("text").filter(|t| !t.is_empty()) {
                        self.keep(text, watch);
                    }
                }
            }
            Some("stop") => {
                if let Some(id) = field_u64("id") {
                    self.activities.remove(&id);
                }
            }
            Some("result") => {
                let (Some(id), Some(typ)) = (field_u64("id"), field_u64("type")) else {
                    return;
                };
                let fields = msg.get("fields").and_then(Value::as_array);
                match typ {
                    RES_BUILD_LOG_LINE | RES_POST_BUILD_LOG_LINE => {
                        if let Some(line) =
                            fields.and_then(|f| f.first()).and_then(Value::as_str)
                        {
                            self.keep(line, watch);
                        }
                    }
                    // A builder moving to its next phase (unpackPhase,
                    // buildPhase…) — the signal quiet builds still give.
                    RES_SET_PHASE => {
                        if let Some(phase) =
                            fields.and_then(|f| f.first()).and_then(Value::as_str)
                        {
                            self.keep(&format!("── {phase}"), watch);
                        }
                    }
                    RES_PROGRESS => {
                        let Some(f) = fields else { return };
                        let val = |i: usize| f.get(i).and_then(Value::as_u64).unwrap_or(0);
                        match self.activities.get(&id) {
                            Some(&ACT_BUILDS) => {
                                self.builds.done = val(0);
                                self.builds.expected = val(1);
                                self.push_units(watch);
                            }
                            Some(&ACT_COPY_PATHS) => {
                                self.copies.done = val(0);
                                self.copies.expected = val(1);
                                self.push_units(watch);
                            }
                            _ => {}
                        }
                    }
                    _ => {}
                }
            }
            // Errors and warnings; the chatty levels stay in nix's world.
            Some("msg") if field_u64("level").unwrap_or(99) <= 2 => {
                if let Some(m) = field_str("msg").filter(|m| !m.is_empty()) {
                    self.keep(m, watch);
                }
            }
            _ => {}
        }
    }
}

/// Nix colors its messages; a job log is plain text. Drop CSI sequences
/// (ESC '[' … final byte) and lone escapes.
fn strip_ansi(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars();
    while let Some(c) = chars.next() {
        if c != '\u{1b}' {
            out.push(c);
            continue;
        }
        if chars.next() == Some('[') {
            for f in chars.by_ref() {
                if ('\u{40}'..='\u{7e}').contains(&f) {
                    break;
                }
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    #[derive(Default)]
    struct Sink {
        lines: Mutex<Vec<String>>,
        units: Mutex<Vec<(u64, u64)>>,
    }
    impl BuildWatch for Sink {
        fn line(&self, line: &str) {
            self.lines.lock().unwrap().push(line.into());
        }
        fn units(&self, done: u64, expected: u64) {
            self.units.lock().unwrap().push((done, expected));
        }
    }

    #[test]
    fn counts_and_log_lines_come_from_the_stream() {
        let sink = Sink::default();
        let mut log = NixLog::new();
        for raw in [
            // the aggregate nix totals its bar from
            r#"@nix {"action":"start","id":1,"level":3,"type":104,"text":"builds"}"#,
            // one derivation starts building
            r#"@nix {"action":"start","id":7,"level":3,"type":105,"text":"building /nix/store/abc-hello.drv"}"#,
            // its build output
            r#"@nix {"action":"result","id":7,"type":101,"fields":["configuring"]}"#,
            // aggregate progress: 1 of 4 done
            r#"@nix {"action":"result","id":1,"type":105,"fields":[1,4,1,0]}"#,
            // a copy aggregate joins: 2 of 10 paths fetched
            r#"@nix {"action":"start","id":2,"level":3,"type":103,"text":"copying"}"#,
            r#"@nix {"action":"result","id":2,"type":105,"fields":[2,10,0,0]}"#,
            // an error-level message
            r#"@nix {"action":"msg","level":0,"msg":"builder failed"}"#,
            // a non-json passthrough line, wearing nix's colors
            "\u{1b}[35;1mwarning:\u{1b}[0m dirty tree",
        ] {
            log.line(raw, Some(&sink));
        }
        let lines = sink.lines.lock().unwrap();
        assert!(lines.iter().any(|l| l.contains("building /nix/store/abc-hello.drv")));
        assert!(lines.iter().any(|l| l == "configuring"));
        assert!(lines.iter().any(|l| l == "builder failed"));
        assert!(lines.iter().any(|l| l == "warning: dirty tree"), "ANSI stripped: {lines:?}");
        let units = sink.units.lock().unwrap();
        assert_eq!(units.first(), Some(&(1, 4)), "builds aggregate alone");
        assert_eq!(units.last(), Some(&(3, 14)), "builds + copies combined");
        // The rolling tail carries the story for the failure message.
        assert!(log.tail().contains("builder failed"));
    }

    #[test]
    fn silence_and_junk_do_not_panic_or_emit() {
        let sink = Sink::default();
        let mut log = NixLog::new();
        log.line("@nix {not json", Some(&sink));
        log.line("", Some(&sink));
        log.line(r#"@nix {"action":"result","id":9,"type":105,"fields":[1,2]}"#, Some(&sink));
        assert!(sink.units.lock().unwrap().is_empty(), "unknown activity id counts nothing");
        assert!(sink.lines.lock().unwrap().is_empty());
    }
}
