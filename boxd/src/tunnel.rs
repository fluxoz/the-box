//! BYO public exposure via Cloudflare Tunnel: boxd stores the tunnel token
//! as a secret and supervises a `cloudflared tunnel run` child process.
//! Traffic flow: Internet -> Cloudflare -> tunnel -> boxd, which routes
//! requests to services by Host header (see web::sites::host_dispatch).

use std::fs;
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};

use crate::paths::Paths;
use crate::secrets;

pub const TOKEN_SECRET: &str = "cloudflare-tunnel-token";

#[derive(Debug, Clone, Serialize)]
pub struct TunnelStatus {
    pub provider: &'static str,
    pub enabled: bool,
    pub installed: bool,
    pub token_saved: bool,
    /// "disabled" | "running" | "failed: ..."
    pub state: String,
    pub pid: Option<u32>,
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct TunnelConfig {
    #[serde(default)]
    cloudflare_enabled: bool,
}

fn config_file(paths: &Paths) -> std::path::PathBuf {
    paths.data_dir.join("network.toml")
}

fn load_config(paths: &Paths) -> TunnelConfig {
    fs::read_to_string(config_file(paths))
        .ok()
        .and_then(|text| toml::from_str(&text).ok())
        .unwrap_or_default()
}

fn save_config(paths: &Paths, config: &TunnelConfig) -> Result<()> {
    let text = toml::to_string_pretty(config)?;
    fs::write(config_file(paths), text).context("writing network.toml")?;
    Ok(())
}

#[derive(Default)]
struct Runtime {
    child: Option<Child>,
    pid: Option<u32>,
    state: Option<String>,
}

pub struct TunnelManager {
    paths: Paths,
    desired: AtomicBool,
    supervising: AtomicBool,
    runtime: Mutex<Runtime>,
}

impl TunnelManager {
    pub fn new(paths: Paths) -> Arc<Self> {
        Arc::new(Self {
            paths,
            desired: AtomicBool::new(false),
            supervising: AtomicBool::new(false),
            runtime: Mutex::new(Runtime::default()),
        })
    }

    pub fn cloudflared_available() -> bool {
        Command::new("cloudflared")
            .arg("--version")
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .map(|s| s.success())
            .unwrap_or(false)
    }

    /// Resume the tunnel on daemon start if it was previously enabled.
    pub fn startup(self: &Arc<Self>) {
        if load_config(&self.paths).cloudflare_enabled && secrets::exists(&self.paths, TOKEN_SECRET)
        {
            tracing::info!("cloudflare tunnel enabled in config; starting");
            self.start();
        }
    }

    /// Save a token (if given) and enable or disable the tunnel.
    pub fn configure(self: &Arc<Self>, token: Option<&str>, enable: bool) -> Result<TunnelStatus> {
        if let Some(token) = token {
            let token = token.trim();
            if token.is_empty() {
                bail!("tunnel token is empty");
            }
            secrets::set(&self.paths, TOKEN_SECRET, token)?;
        }
        if enable {
            if !secrets::exists(&self.paths, TOKEN_SECRET) {
                bail!("no Cloudflare tunnel token saved yet");
            }
            save_config(
                &self.paths,
                &TunnelConfig {
                    cloudflare_enabled: true,
                },
            )?;
            self.start();
        } else {
            save_config(
                &self.paths,
                &TunnelConfig {
                    cloudflare_enabled: false,
                },
            )?;
            self.stop();
        }
        Ok(self.status())
    }

    pub fn status(&self) -> TunnelStatus {
        let runtime = self.runtime.lock().unwrap();
        let enabled = self.desired.load(Ordering::SeqCst);
        let state = if let Some(state) = &runtime.state {
            state.clone()
        } else if enabled {
            "starting".to_string()
        } else {
            "disabled".to_string()
        };
        TunnelStatus {
            provider: "cloudflare",
            enabled,
            installed: Self::cloudflared_available(),
            token_saved: secrets::exists(&self.paths, TOKEN_SECRET),
            state,
            pid: runtime.pid,
        }
    }

    fn start(self: &Arc<Self>) {
        self.desired.store(true, Ordering::SeqCst);
        if !self.supervising.swap(true, Ordering::SeqCst) {
            let this = Arc::clone(self);
            std::thread::spawn(move || this.supervise());
        }
    }

    pub fn stop(&self) {
        self.desired.store(false, Ordering::SeqCst);
        let mut runtime = self.runtime.lock().unwrap();
        if let Some(child) = runtime.child.as_mut() {
            let _ = child.kill();
            let _ = child.wait();
        }
        runtime.child = None;
        runtime.pid = None;
        runtime.state = Some("disabled".to_string());
    }

    fn supervise(self: Arc<Self>) {
        let mut backoff = 1u64;
        while self.desired.load(Ordering::SeqCst) {
            let needs_spawn = self.runtime.lock().unwrap().child.is_none();
            if needs_spawn {
                match self.spawn_cloudflared() {
                    Ok(child) => {
                        tracing::info!(pid = child.id(), "cloudflared started");
                        let mut runtime = self.runtime.lock().unwrap();
                        runtime.pid = Some(child.id());
                        runtime.child = Some(child);
                        runtime.state = Some("running".to_string());
                        backoff = 1;
                    }
                    Err(err) => {
                        tracing::warn!("cloudflared failed to start: {err:#}");
                        self.runtime.lock().unwrap().state = Some(format!("failed: {err:#}"));
                        std::thread::sleep(Duration::from_secs(backoff));
                        backoff = (backoff * 2).min(30);
                        continue;
                    }
                }
            }
            std::thread::sleep(Duration::from_secs(1));
            let mut runtime = self.runtime.lock().unwrap();
            if let Some(child) = runtime.child.as_mut() {
                if let Ok(Some(status)) = child.try_wait() {
                    tracing::warn!("cloudflared exited: {status}");
                    runtime.child = None;
                    runtime.pid = None;
                    runtime.state = Some(format!("failed: cloudflared exited ({status})"));
                    drop(runtime);
                    std::thread::sleep(Duration::from_secs(backoff));
                    backoff = (backoff * 2).min(30);
                }
            }
        }
        self.supervising.store(false, Ordering::SeqCst);
    }

    fn spawn_cloudflared(&self) -> Result<Child> {
        let token =
            secrets::get(&self.paths, TOKEN_SECRET)?.context("cloudflare tunnel token missing")?;
        let logs_dir = self.paths.data_dir.join("logs");
        fs::create_dir_all(&logs_dir)?;
        let log = fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(logs_dir.join("cloudflared.log"))?;
        // Token via environment, not argv: argv is world-readable in /proc.
        Command::new("cloudflared")
            .args(["tunnel", "run"])
            .env("TUNNEL_TOKEN", token)
            .stdin(Stdio::null())
            .stdout(log.try_clone()?)
            .stderr(log)
            .spawn()
            .context("spawning cloudflared (is it installed and on PATH?)")
    }
}
