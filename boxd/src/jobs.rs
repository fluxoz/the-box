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

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum State {
    Running,
    Done,
    Failed,
}

/// One announced phase and when it began. The console turns consecutive marks
/// into steps with real durations.
#[derive(Debug, Clone, Serialize, serde::Deserialize)]
pub struct PhaseMark {
    pub name: String,
    pub started_unix: i64,
}

#[derive(Debug, Clone, Serialize, serde::Deserialize)]
pub struct Job {
    pub id: String,
    /// Machine-readable kind: deploy, delete, rollback, recreate, backup, update.
    pub kind: String,
    /// Human label, e.g. "Deploying blog".
    pub label: String,
    pub state: State,
    /// The phase running right now ("building generation", "health check").
    pub phase: String,
    /// Every phase so far, in order. `phase` is always the last entry's name;
    /// this is the sequence with timings the progress view steps through.
    #[serde(default)]
    pub phase_history: Vec<PhaseMark>,
    pub log: Vec<String>,
    /// Set when the job ends: the flash message to show.
    pub message: String,
    /// Where the console should go once this finishes.
    pub target: String,
    pub started_unix: i64,
    pub finished_unix: Option<i64>,
    /// What history says this kind of job takes (median of its recent
    /// successes). None on a first run — the bar sweeps instead of guessing.
    #[serde(default)]
    pub expected_secs: Option<i64>,
}

impl Job {
    pub fn elapsed_secs(&self, now: i64) -> i64 {
        self.finished_unix
            .unwrap_or(now)
            .saturating_sub(self.started_unix)
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
            j.phase_history.push(PhaseMark {
                name: text.clone(),
                started_unix: now_unix(),
            });
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
    /// Where finished/started jobs are written, so they survive a restart.
    /// `None` = memory only (tests).
    dir: Option<std::path::PathBuf>,
}

impl Registry {
    pub fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }

    /// A registry whose jobs survive the daemon restarting — which matters
    /// most for the one job that CAUSES a restart: a platform update switches
    /// the system, boxd comes back with empty memory, and the agent polling
    /// job_status was told "no such job" at the exact moment it needed the
    /// answer. Anything found still "running" on disk at startup is re-marked
    /// as interrupted-by-restart, with a message saying where to look —
    /// because for an update, the restart is usually the SUCCESS path.
    pub fn persistent(dir: std::path::PathBuf) -> Arc<Self> {
        let _ = std::fs::create_dir_all(&dir);
        let reg = Self {
            dir: Some(dir.clone()),
            ..Self::default()
        };
        let mut loaded: Vec<Job> = std::fs::read_dir(&dir)
            .map(|entries| {
                entries
                    .filter_map(|e| e.ok())
                    .filter_map(|e| std::fs::read_to_string(e.path()).ok())
                    .filter_map(|s| serde_json::from_str::<Job>(&s).ok())
                    .collect()
            })
            .unwrap_or_default();
        loaded.sort_by_key(|j| j.started_unix);
        {
            let mut map = reg.jobs.lock().unwrap();
            let mut recent = reg.recent.lock().unwrap();
            for mut job in loaded {
                if job.state == State::Running {
                    job.state = State::Failed;
                    job.phase = "interrupted".into();
                    job.message = "boxd restarted while this ran. For a platform update that \
                                   is the expected path — check get_status / channel_status \
                                   to see whether the new version is running."
                        .into();
                    job.finished_unix = Some(now_unix());
                    persist(&dir, &job);
                }
                recent.push(job.id.clone());
                map.insert(job.id.clone(), job);
            }
        }
        Arc::new(reg)
    }

    fn persist_by_id(&self, id: &str) {
        let Some(dir) = &self.dir else { return };
        if let Ok(map) = self.jobs.lock() {
            if let Some(job) = map.get(id) {
                persist(dir, job);
            }
        }
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

    /// What history says a `kind` of job takes: the median duration of its
    /// last three successes. This is the number the progress bar is honest
    /// about — measured on THIS box, not invented.
    fn estimate_secs(&self, kind: &str) -> Option<i64> {
        let mut durations: Vec<i64> = self
            .recent(50)
            .into_iter()
            .filter(|j| j.kind == kind && j.state == State::Done)
            .filter_map(|j| j.finished_unix.map(|f| f - j.started_unix))
            .filter(|d| *d >= 1)
            .take(3)
            .collect();
        if durations.is_empty() {
            return None;
        }
        durations.sort_unstable();
        Some(durations[durations.len() / 2])
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
        let expected_secs = self.estimate_secs(kind);
        let job = Job {
            id: id.clone(),
            kind: kind.to_string(),
            label: label.into(),
            state: State::Running,
            phase: "starting".into(),
            phase_history: Vec::new(),
            log: Vec::new(),
            message: String::new(),
            target: target.into(),
            started_unix: now_unix(),
            finished_unix: None,
            expected_secs,
        };
        if let Ok(mut map) = self.jobs.lock() {
            map.insert(id.clone(), job);
        }
        self.persist_by_id(&id);
        if let Ok(mut r) = self.recent.lock() {
            r.push(id.clone());
            // Keep the list bounded; the generation list is the real history.
            if r.len() > 50 {
                let cut = r.len() - 50;
                let drop_ids: Vec<String> = r.drain(..cut).collect();
                if let Ok(mut map) = self.jobs.lock() {
                    for old in drop_ids {
                        map.remove(&old);
                        if let Some(dir) = &self.dir {
                            let _ = std::fs::remove_file(dir.join(format!("{old}.json")));
                        }
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
            let outcome =
                std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| work(&progress)));
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
            registry.persist_by_id(&job_id);
        });
        id
    }
}

/// One file per job. Written at start (so an interrupted job is findable) and
/// at the end (so the outcome is). Log lines in between stay in memory only —
/// the disk record is for "what happened", not a live tail.
fn persist(dir: &std::path::Path, job: &Job) {
    if let Ok(json) = serde_json::to_string(job) {
        let _ = std::fs::write(dir.join(format!("{}.json", job.id)), json);
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
    fn jobs_survive_the_restart_they_cause() {
        let tmp = tempfile::TempDir::new().unwrap();
        let dir = tmp.path().join("jobs");

        // A finished job outlives its registry — the daemon restarting must
        // not turn "it worked" into "no such job".
        let reg = Registry::persistent(dir.clone());
        let id = reg.start("test", "t", "t", |_| Ok("did it".into()));
        wait_for(&reg, &id, State::Done);
        drop(reg);
        let reg2 = Registry::persistent(dir.clone());
        let job = reg2.get(&id).expect("finished job survived the restart");
        assert_eq!(job.state, State::Done);
        assert_eq!(job.message, "did it");

        // A job RUNNING at the moment of restart — the platform-update case,
        // where the job itself causes the restart — is re-marked interrupted
        // and says where to look for the outcome.
        let stuck = Job {
            id: "deadbeef00000000".into(),
            kind: "platform-update".into(),
            label: "platform update".into(),
            state: State::Running,
            phase: "switching".into(),
            phase_history: Vec::new(),
            log: Vec::new(),
            message: String::new(),
            target: "system".into(),
            started_unix: 1,
            finished_unix: None,
            expected_secs: None,
        };
        persist(&dir, &stuck);
        let reg3 = Registry::persistent(dir);
        let j = reg3.get("deadbeef00000000").unwrap();
        assert_eq!(j.state, State::Failed);
        assert!(j.message.contains("restarted"), "{}", j.message);
        assert!(j.message.contains("channel_status"), "{}", j.message);
    }

    #[test]
    fn a_repeat_job_carries_what_history_says_it_takes() {
        let tmp = tempfile::TempDir::new().unwrap();
        let dir = tmp.path().join("jobs");
        std::fs::create_dir_all(&dir).unwrap();
        // Three past deploys: 30 s, 40 s and one 300 s outlier. The median
        // keeps the bar honest against the outlier.
        for (i, (t0, t1)) in [(100, 130), (200, 240), (300, 600)].iter().enumerate() {
            let done = Job {
                id: format!("old{i}"),
                kind: "deploy".into(),
                label: "x".into(),
                state: State::Done,
                phase: "done".into(),
                phase_history: Vec::new(),
                log: Vec::new(),
                message: "ok".into(),
                target: "/".into(),
                started_unix: *t0,
                finished_unix: Some(*t1),
                expected_secs: None,
            };
            persist(&dir, &done);
        }
        let reg = Registry::persistent(dir);
        let id = reg.start("deploy", "Deploying blog", "/", |p| {
            p.phase("building generation");
            p.phase("activating");
            Ok("ok".into())
        });
        assert_eq!(reg.get(&id).unwrap().expected_secs, Some(40));

        // A kind never seen has no estimate: the bar sweeps instead of lying.
        let first = reg.start("rollback", "r", "/", |_| Ok("ok".into()));
        assert_eq!(reg.get(&first).unwrap().expected_secs, None);

        // Phases arrive as an ordered history the console can step through.
        let done = wait_for(&reg, &id, State::Done);
        let names: Vec<&str> = done.phase_history.iter().map(|m| m.name.as_str()).collect();
        assert_eq!(names, ["building generation", "activating"]);
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
        assert!(j
            .log
            .iter()
            .any(|l| l.contains(&format!("line {}", LOG_LIMIT + 49))));
    }
}
