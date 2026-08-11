//! Background jobs with live progress.
//!
//! Deploying, rolling back, recreating, backing up and updating the platform
//! all run a Nix build, which takes seconds at best and minutes on a small box.
//! Those used to run inside the request, so the browser sat on a dead
//! connection until they finished and the console looked hung. Now the request
//! starts a job and returns immediately; the work continues on a worker thread,
//! reporting phases as it goes, and the page follows along.
//!
//! Deliberately in-memory and per-process: a job is a view of something
//! happening right now, not a record. The durable history of what actually
//! changed is the generation list and the config repo's commit log.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use serde::Serialize;

/// How many log lines a job keeps. Enough to show what happened, bounded so a
/// chatty build cannot grow the daemon's memory.
const LOG_LIMIT: usize = 300;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum State {
    Running,
    Done,
    Failed,
}

#[derive(Debug, Clone, Serialize)]
pub struct Job {
    pub id: String,
    /// Machine-readable kind: deploy, delete, rollback, recreate, backup, update.
    pub kind: String,
    /// Human label, e.g. "Deploying blog".
    pub label: String,
    pub state: State,
    /// The phase running right now ("building generation", "health check").
    pub phase: String,
    pub log: Vec<String>,
    /// Set when the job ends: the flash message to show.
    pub message: String,
    /// Where the console should go once this finishes.
    pub target: String,
    pub started_unix: i64,
    pub finished_unix: Option<i64>,
}

impl Job {
    pub fn elapsed_secs(&self, now: i64) -> i64 {
        self.finished_unix.unwrap_or(now).saturating_sub(self.started_unix)
    }
}

/// Handle given to the running closure so it can report progress.
pub struct Progress {
    id: String,
    jobs: Arc<Registry>,
}

impl Progress {
    /// Announce a new phase. Also appended to the log, so the page shows a
    /// sequence rather than a single flickering line.
    pub fn phase(&self, text: impl Into<String>) {
        let text = text.into();
        self.jobs.with(&self.id, |j| {
            j.phase = text.clone();
            push_log(j, &text);
        });
    }

    pub fn log(&self, text: impl Into<String>) {
        let text = text.into();
        self.jobs.with(&self.id, |j| push_log(j, &text));
    }
}

fn push_log(job: &mut Job, line: &str) {
    job.log.push(line.to_string());
    if job.log.len() > LOG_LIMIT {
        let overflow = job.log.len() - LOG_LIMIT;
        job.log.drain(..overflow);
    }
}

#[derive(Default)]
pub struct Registry {
    jobs: Mutex<HashMap<String, Job>>,
    /// Newest-first ids, so the console can show what is currently happening.
    recent: Mutex<Vec<String>>,
}

impl Registry {
    pub fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }

    fn with<F: FnOnce(&mut Job)>(&self, id: &str, f: F) {
        if let Ok(mut map) = self.jobs.lock() {
            if let Some(job) = map.get_mut(id) {
                f(job);
            }
        }
    }

    pub fn get(&self, id: &str) -> Option<Job> {
        self.jobs.lock().ok()?.get(id).cloned()
    }

    /// The most recent jobs, newest first.
    pub fn recent(&self, limit: usize) -> Vec<Job> {
        let ids = match self.recent.lock() {
            Ok(r) => r.clone(),
            Err(_) => return Vec::new(),
        };
        ids.iter()
            .rev()
            .filter_map(|id| self.get(id))
            .take(limit)
            .collect()
    }

    /// Is something that changes the system running right now? The console uses
    /// this to keep polling rather than settle.
    pub fn any_running(&self) -> bool {
        self.jobs
            .lock()
            .map(|m| m.values().any(|j| j.state == State::Running))
            .unwrap_or(false)
    }

    /// Start `work` on a worker thread and return the job id immediately.
    ///
    /// The closure returns the message to show on success; an error becomes the
    /// failure message. Panics are caught and reported as a failed job instead
    /// of poisoning the registry, so a bug in one operation cannot wedge the
    /// console's progress display.
    pub fn start<F>(
        self: &Arc<Self>,
        kind: &str,
        label: impl Into<String>,
        target: impl Into<String>,
        work: F,
    ) -> String
    where
        F: FnOnce(&Progress) -> anyhow::Result<String> + Send + 'static,
    {
        let id = new_id();
        let job = Job {
            id: id.clone(),
            kind: kind.to_string(),
            label: label.into(),
            state: State::Running,
            phase: "starting".into(),
            log: Vec::new(),
            message: String::new(),
            target: target.into(),
            started_unix: now_unix(),
            finished_unix: None,
        };
        if let Ok(mut map) = self.jobs.lock() {
            map.insert(id.clone(), job);
        }
        if let Ok(mut r) = self.recent.lock() {
            r.push(id.clone());
            // Keep the list bounded; the generation list is the real history.
            if r.len() > 50 {
                let cut = r.len() - 50;
                let drop_ids: Vec<String> = r.drain(..cut).collect();
                if let Ok(mut map) = self.jobs.lock() {
                    for old in drop_ids {
                        map.remove(&old);
                    }
                }
            }
        }

        let registry = Arc::clone(self);
        let job_id = id.clone();
        std::thread::spawn(move || {
            let progress = Progress {
                id: job_id.clone(),
                jobs: Arc::clone(&registry),
            };
            let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| work(&progress)));
            let (state, message) = match outcome {
                Ok(Ok(msg)) => (State::Done, msg),
                Ok(Err(e)) => (State::Failed, format!("{e:#}")),
                Err(_) => (State::Failed, "the operation panicked".to_string()),
            };
            registry.with(&job_id, |j| {
                j.state = state;
                j.message = message.clone();
                j.phase = match state {
                    State::Done => "done".into(),
                    _ => "failed".into(),
                };
                push_log(j, &message);
                j.finished_unix = Some(now_unix());
            });
        });
        id
    }
}

fn now_unix() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// Short, unguessable-enough id. Jobs are already behind operator auth; this
/// only needs to avoid collisions between concurrent operations.
fn new_id() -> String {
    let mut buf = [0u8; 8];
    if let Ok(mut f) = std::fs::File::open("/dev/urandom") {
        use std::io::Read;
        let _ = f.read_exact(&mut buf);
    }
    buf.iter().map(|b| format!("{b:02x}")).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn wait_for(reg: &Arc<Registry>, id: &str, want: State) -> Job {
        for _ in 0..200 {
            if let Some(j) = reg.get(id) {
                if j.state == want {
                    return j;
                }
            }
            std::thread::sleep(std::time::Duration::from_millis(25));
        }
        panic!("job {id} never reached {want:?}");
    }

    #[test]
    fn reports_phases_and_completes() {
        let reg = Registry::new();
        let id = reg.start("deploy", "Deploying blog", "/", |p| {
            p.phase("building generation");
            p.phase("activating");
            Ok("Deployed blog".into())
        });
        // Visible immediately — that is the whole point.
        assert!(reg.get(&id).is_some());
        let done = wait_for(&reg, &id, State::Done);
        assert_eq!(done.message, "Deployed blog");
        assert!(done.log.iter().any(|l| l == "building generation"));
        assert!(done.log.iter().any(|l| l == "activating"));
        assert!(done.finished_unix.is_some());
        assert!(!reg.any_running());
    }

    #[test]
    fn failure_and_panic_both_become_failed_jobs() {
        let reg = Registry::new();
        let bad = reg.start("deploy", "x", "/", |_| anyhow::bail!("build failed"));
        assert_eq!(wait_for(&reg, &bad, State::Failed).message, "build failed");

        let boom = reg.start("deploy", "x", "/", |_| panic!("bug"));
        let j = wait_for(&reg, &boom, State::Failed);
        assert!(j.message.contains("panicked"), "got {}", j.message);
    }

    #[test]
    fn log_is_bounded() {
        let reg = Registry::new();
        let id = reg.start("x", "x", "/", |p| {
            for i in 0..(LOG_LIMIT + 50) {
                p.log(format!("line {i}"));
            }
            Ok("done".into())
        });
        let j = wait_for(&reg, &id, State::Done);
        assert!(j.log.len() <= LOG_LIMIT + 1, "log grew to {}", j.log.len());
        assert!(j.log.iter().any(|l| l.contains(&format!("line {}", LOG_LIMIT + 49))));
    }
}
