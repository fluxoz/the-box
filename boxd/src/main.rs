use std::net::SocketAddr;
use std::path::PathBuf;

use anyhow::{Context, Result};
use clap::{Parser, Subcommand, ValueEnum};

use boxd::ops;
use boxd::paths::{default_data_dir, Paths};
use boxd::store::{self, local::LocalBuilder, nix::NixBuilder, Builder};
use boxd::web::{self, AppState};

#[derive(Parser)]
#[command(
    name = "boxd",
    version,
    about = "The Box daemon — declarative personal server with atomic generations"
)]
struct Cli {
    /// Data directory (default: $BOXD_DATA_DIR or ~/.local/share/boxd)
    #[arg(long, global = true)]
    data_dir: Option<PathBuf>,

    /// Generation build backend
    #[arg(long, global = true, value_enum, default_value_t = BackendArg::Auto)]
    backend: BackendArg,

    #[command(subcommand)]
    command: Command,
}

#[derive(Clone, Copy, ValueEnum)]
enum BackendArg {
    /// Use Nix when available, otherwise the local fallback
    Auto,
    Nix,
    Local,
}

#[derive(Subcommand)]
enum Command {
    /// Run the daemon: dashboard, JSON API and site serving
    Serve {
        #[arg(long, default_value = "127.0.0.1:2693")]
        listen: SocketAddr,
    },
    /// Build and activate a new generation from the current config
    Apply,
    /// Show the current generation and declared services
    Status,
    /// List all generations
    Generations,
    /// Roll back to a previous generation
    Rollback { number: u64 },
    /// Generate the standalone dendritic per-box config repo (the OS-tier
    /// source of truth) from the current config, without building it.
    HostGen {
        /// Host id (becomes the hostname and nixosConfigurations attr name)
        #[arg(long)]
        host_id: String,
        /// Platform flake ref for the `the-box` input (the update channel)
        #[arg(long, default_value = "github:fluxoz/the-box")]
        platform: String,
        /// Nix system double
        #[arg(long, default_value = "x86_64-linux")]
        system: String,
        /// Output directory (default: <data>/os-config)
        #[arg(long)]
        out: Option<PathBuf>,
    },
    /// OS tier: build this box's whole NixOS system from the current config and
    /// switch to it, rolling back the system on a failed health check. Runs
    /// only on a Box (a NixOS host with a system profile).
    OsSwitch {
        #[arg(long)]
        host_id: String,
        #[arg(long, default_value = "github:fluxoz/the-box")]
        platform: String,
        #[arg(long, default_value = "x86_64-linux")]
        system: String,
    },
    /// Platform update channel: inspect and apply platform releases.
    Channel {
        #[command(subcommand)]
        action: ChannelCmd,
    },
    /// Operator access: enroll the first device, list or revoke sessions.
    Auth {
        #[command(subcommand)]
        action: AuthCmd,
    },
    /// Client-side-encrypted backups to your own backend (restic).
    Backup {
        #[command(subcommand)]
        action: BackupCmd,
    },
    /// Managed cloud services (optional, paid): link this Box to a cloud account.
    Cloud {
        #[command(subcommand)]
        action: CloudCmd,
    },
    /// Serve the browser install wizard inside the installer (pre-pairing, no
    /// auth). On commit it writes orders + a disko config for box-install to
    /// act on, and serves install progress read from --progress.
    InstallWizard {
        #[arg(long, default_value = "0.0.0.0:2693")]
        listen: SocketAddr,
        #[arg(long)]
        orders_out: PathBuf,
        #[arg(long)]
        disko_out: PathBuf,
        #[arg(long)]
        commit_flag: PathBuf,
        #[arg(long)]
        progress: PathBuf,
        #[arg(long)]
        done: PathBuf,
        /// Base orders (name/wifi/key/pairing) when only storage is deferred.
        #[arg(long)]
        base_orders: Option<PathBuf>,
        /// Setup PIN shown on the console; when set, guards the API.
        #[arg(long)]
        pin: Option<String>,
    },
}

#[derive(Subcommand)]
enum AuthCmd {
    /// Mint a one-time enrollment code for the first device to pair with.
    Enroll,
    /// List active operator sessions (paired devices).
    List,
    /// Revoke a session by id.
    Revoke { id: String },
    /// Mint a session token directly (e.g. for an agent). Prints the token once.
    Mint {
        #[arg(long, default_value = "agent")]
        label: String,
    },
    /// Install a pre-hashed pairing code (used by firstboot to seed the
    /// enrollment code from the install handoff). Not for interactive use.
    ImportCode {
        #[arg(long)]
        hash: String,
        #[arg(long, default_value = "enrollment")]
        label: String,
    },
}

#[derive(Subcommand)]
enum BackupCmd {
    /// Generate the backup key and print it once. SAVE IT — it's required to
    /// restore, and cannot be recovered.
    Init,
    /// Take a backup now.
    Run {
        /// Only run if enabled and the schedule interval has elapsed. The
        /// systemd timer uses this; a manual `run` always backs up.
        #[arg(long)]
        if_due: bool,
    },
    /// List snapshots.
    Snapshots,
    /// Show backup status (reachability, last snapshot).
    Status,
    /// Restore. Default: everything; scope with --service or --config-only.
    Restore {
        /// Snapshot id (default: latest).
        #[arg(long, default_value = "latest")]
        snapshot: String,
        /// Restore only this service's data.
        #[arg(long)]
        service: Option<String>,
        /// Restore only the Box's config/secrets/auth.
        #[arg(long)]
        config_only: bool,
        /// Where to write the restored files.
        #[arg(long, default_value = "/")]
        target: PathBuf,
    },
    /// Verify repository integrity.
    Check,
}

#[derive(Subcommand)]
enum CloudCmd {
    /// Link this Box to a cloud account and turn on managed backup.
    Enroll {
        #[arg(long)]
        server: String,
        /// One-time enrollment token from your cloud account.
        #[arg(long)]
        token: String,
    },
    /// Re-fetch managed storage credentials (rotation / repair).
    Provision,
    /// Show managed backup usage.
    Status,
}

#[derive(Subcommand)]
enum ChannelCmd {
    /// Show the configured channel and the currently pinned platform.
    Status,
    /// Check whether the channel has a newer platform than the current pin.
    Check,
    /// Apply an update: on a new release (or --force), rebuild the system,
    /// switch, and roll back on a failed health check. A no-op when no channel
    /// is configured or the platform is already current — safe to run on a timer.
    Update {
        #[arg(long)]
        force: bool,
    },
    /// Configure this box's channel binding (writes channel.toml).
    Set {
        #[arg(long)]
        host_id: String,
        #[arg(long, default_value = "github:fluxoz/the-box")]
        platform: String,
        #[arg(long, default_value = "x86_64-linux")]
        system: String,
        #[arg(long)]
        auto_update: bool,
    },
}

fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()),
        )
        // Logs go to stderr so command output (e.g. `auth mint` printing a
        // token) stays clean for capture by scripts and agents.
        .with_writer(std::io::stderr)
        .init();

    let cli = Cli::parse();

    let data_dir = cli.data_dir.unwrap_or_else(default_data_dir);
    std::fs::create_dir_all(&data_dir)
        .with_context(|| format!("creating data dir {}", data_dir.display()))?;
    let data_dir = std::fs::canonicalize(&data_dir)?;
    let paths = Paths::new(data_dir);
    paths.ensure()?;

    let builder = make_builder(cli.backend, &paths);
    tracing::info!(
        backend = builder.name(),
        data_dir = %paths.data_dir.display(),
        "boxd starting"
    );

    match cli.command {
        Command::Serve { listen } => run_server(paths, builder, listen),
        Command::Apply => {
            let info = ops::apply(&paths, builder.as_ref())?;
            println!("activated generation #{}", info.number);
            Ok(())
        }
        Command::Status => {
            let config = boxd::config::BoxConfig::load(&paths)?;
            match store::current(&paths)? {
                Some(g) => println!(
                    "current generation: #{} ({})",
                    g.number,
                    g.store_path.display()
                ),
                None => println!("current generation: none (nothing applied yet)"),
            }
            println!("declared services: {}", config.services.len());
            for s in &config.services {
                println!("  - {} ({})", s.name, s.template.as_str());
            }
            Ok(())
        }
        Command::Generations => {
            for g in store::list(&paths)? {
                println!(
                    "#{}{}\t{}",
                    g.number,
                    if g.current { " *" } else { "" },
                    g.store_path.display()
                );
            }
            Ok(())
        }
        Command::Rollback { number } => {
            let info = ops::rollback(&paths, number)?;
            println!("rolled back to generation #{}", info.number);
            Ok(())
        }
        Command::HostGen {
            host_id,
            platform,
            system,
            out,
        } => {
            let config = boxd::config::BoxConfig::load(&paths)?;
            let spec = boxd::hostgen::HostSpec::new(host_id, platform, system);
            let out = out.unwrap_or_else(|| paths.os_config_dir());
            boxd::hostgen::write_host_repo(&paths, &config, &spec, &out)?;
            println!(
                "generated per-box config for {:?} at {}",
                spec.id,
                out.display()
            );
            Ok(())
        }
        Command::OsSwitch {
            host_id,
            platform,
            system,
        } => {
            let config = boxd::config::BoxConfig::load(&paths)?;
            let spec = boxd::hostgen::HostSpec::new(host_id, platform, system);
            let toplevel = boxd::ostier::reconcile(
                &paths,
                &config,
                &spec,
                &boxd::ostier::default_system_health,
            )?;
            println!("switched to new system generation: {}", toplevel.display());
            Ok(())
        }
        Command::Channel { action } => run_channel(&paths, action),
        Command::Auth { action } => run_auth(&paths, action),
        Command::Backup { action } => run_backup(&paths, action),
        Command::Cloud { action } => match action {
            CloudCmd::Enroll { server, token } => {
                boxd::cloud::enroll(&paths, &server, &token)?;
                println!("enrolled — managed backup is on. Reveal your recovery key with `boxd backup` in the dashboard.");
                Ok(())
            }
            CloudCmd::Provision => {
                boxd::cloud::provision(&paths)?;
                println!("managed storage refreshed");
                Ok(())
            }
            CloudCmd::Status => {
                let u = boxd::cloud::usage(&paths)?;
                let bytes = u.get("bytes").and_then(|v| v.as_u64()).unwrap_or(0);
                println!("managed backup: {:.2} MiB stored", bytes as f64 / 1_048_576.0);
                Ok(())
            }
        },
        Command::InstallWizard {
            listen,
            orders_out,
            disko_out,
            commit_flag,
            progress,
            done,
            base_orders,
            pin,
        } => web::wizard::run(
            web::wizard::WizardCfg {
                orders_out,
                disko_out,
                commit_flag,
                progress,
                done,
                base_orders,
                pin,
            },
            listen,
        ),
    }
}

fn run_backup(paths: &Paths, action: BackupCmd) -> Result<()> {
    use boxd::backup;
    if let BackupCmd::Init = action {
        let key = backup::init_key(paths)?;
        eprintln!(
            "Backup recovery key — SAVE THIS. It is required to restore your\n\
             backups and cannot be recovered. It never leaves this Box."
        );
        println!("{key}");
        return Ok(());
    }

    let config = boxd::config::BoxConfig::load(paths)?;

    // The timer's `run --if-due` must be a quiet no-op when backup isn't set up.
    if let BackupCmd::Run { if_due: true } = &action {
        match &config.backup {
            None => {
                eprintln!("backup not configured — nothing to do");
                return Ok(());
            }
            Some(bc) if !bc.enabled => {
                eprintln!("backup disabled — nothing to do");
                return Ok(());
            }
            _ => {}
        }
    }

    let bc = config.backup.clone().ok_or_else(|| {
        anyhow::anyhow!("no [backup] backend configured — add one to box.toml or the dashboard")
    })?;

    match action {
        BackupCmd::Init => unreachable!(),
        BackupCmd::Run { if_due } => {
            if if_due {
                let st = backup::status(paths, &bc);
                if !backup::is_due(&bc, st.last.as_ref()) {
                    println!("backup not due yet");
                    return Ok(());
                }
            }
            backup::run(paths, &config, &bc)?;
            println!("backup complete");
            Ok(())
        }
        BackupCmd::Snapshots => {
            for s in backup::snapshots(paths, &bc)? {
                let id = &s.id[..8.min(s.id.len())];
                println!("{id}\t{}\t{}", s.time, s.paths.join(" "));
            }
            Ok(())
        }
        BackupCmd::Status => {
            let st = backup::status(paths, &bc);
            println!(
                "reachable: {}  snapshots: {}  last: {}",
                st.reachable,
                st.count,
                st.last.map(|l| l.time).as_deref().unwrap_or("never")
            );
            Ok(())
        }
        BackupCmd::Restore {
            snapshot,
            service,
            config_only,
            target,
        } => {
            let includes = if config_only {
                backup::config_includes(paths)
            } else if let Some(name) = service {
                backup::service_includes(&config, &name)
            } else {
                Vec::new() // everything in the snapshot
            };
            backup::restore(paths, &bc, &snapshot, &target, &includes)?;
            println!("restore complete");
            Ok(())
        }
        BackupCmd::Check => {
            backup::check(paths, &bc)?;
            println!("repository OK");
            Ok(())
        }
    }
}

fn run_auth(paths: &Paths, action: AuthCmd) -> Result<()> {
    match action {
        AuthCmd::Enroll => {
            let code = boxd::auth::mint_code(paths, "enrollment")?;
            println!("Enrollment code (valid 15 min, single use):\n\n    {code}\n");
            println!("Enter it at http://<box>:2693/pair on the device you want to pair.");
            Ok(())
        }
        AuthCmd::List => {
            let sessions = boxd::auth::list(paths);
            if sessions.is_empty() {
                println!("no paired devices");
            }
            for s in sessions {
                println!("{}\t{}", s.id, s.label);
            }
            Ok(())
        }
        AuthCmd::Revoke { id } => {
            if boxd::auth::revoke(paths, &id)? {
                println!("revoked {id}");
            } else {
                println!("no session with id {id}");
            }
            Ok(())
        }
        AuthCmd::Mint { label } => {
            let token = boxd::auth::mint_session(paths, &label)?;
            println!("{token}");
            Ok(())
        }
        AuthCmd::ImportCode { hash, label } => {
            boxd::auth::import_code(paths, &hash, &label)?;
            println!("imported {label} code");
            Ok(())
        }
    }
}

fn run_channel(paths: &Paths, action: ChannelCmd) -> Result<()> {
    use boxd::channel::ChannelConfig;

    match action {
        ChannelCmd::Set {
            host_id,
            platform,
            system,
            auto_update,
        } => {
            let cfg = ChannelConfig {
                host_id,
                platform_ref: platform,
                system,
                auto_update,
            };
            cfg.save(paths)?;
            println!(
                "channel set: {} tracking {} (auto-update: {})",
                cfg.host_id, cfg.platform_ref, cfg.auto_update
            );
            Ok(())
        }
        ChannelCmd::Status => {
            match ChannelConfig::load(paths)? {
                None => println!("no channel configured"),
                Some(cfg) => {
                    let pinned = boxd::channel::locked_platform_id(&paths.os_config_dir())?;
                    println!("host id:      {}", cfg.host_id);
                    println!("platform:     {}", cfg.platform_ref);
                    println!("system:       {}", cfg.system);
                    println!("auto-update:  {}", cfg.auto_update);
                    println!(
                        "pinned to:    {}",
                        pinned.as_deref().unwrap_or("(not yet built)")
                    );
                }
            }
            Ok(())
        }
        ChannelCmd::Check => {
            let Some(cfg) = ChannelConfig::load(paths)? else {
                println!("no channel configured");
                return Ok(());
            };
            let status = boxd::channel::check(paths, &cfg)?;
            println!("current: {}", status.current.as_deref().unwrap_or("(none)"));
            println!("latest:  {}", status.latest);
            println!(
                "status:  {}",
                if status.update_available {
                    "update available"
                } else {
                    "up to date"
                }
            );
            Ok(())
        }
        ChannelCmd::Update { force } => {
            let Some(cfg) = ChannelConfig::load(paths)? else {
                // Unconfigured boxes are a valid state; the timer runs this on
                // every box, so this must be a clean no-op.
                println!("no channel configured; nothing to update");
                return Ok(());
            };
            // Only rebuild/switch when there's actually a new release, unless
            // forced — otherwise the timer would needlessly re-switch each run.
            let bump = if force {
                true
            } else {
                match boxd::channel::check(paths, &cfg) {
                    Ok(status) if !status.update_available => {
                        println!("platform up to date ({})", status.latest);
                        return Ok(());
                    }
                    Ok(_) => true,
                    Err(e) => {
                        // Offline / unreachable channel: don't fail the timer.
                        eprintln!("channel check skipped: {e:#}");
                        return Ok(());
                    }
                }
            };
            let config = boxd::config::BoxConfig::load(paths)?;
            let toplevel = boxd::channel::update_and_switch(
                paths,
                &config,
                &cfg,
                bump,
                &boxd::ostier::default_system_health,
            )?;
            println!("platform updated; switched to {}", toplevel.display());
            Ok(())
        }
    }
}

fn make_builder(choice: BackendArg, paths: &Paths) -> Box<dyn Builder> {
    match choice {
        BackendArg::Nix => Box::new(NixBuilder),
        BackendArg::Local => Box::new(LocalBuilder::new(paths)),
        BackendArg::Auto => {
            if NixBuilder::available() {
                Box::new(NixBuilder)
            } else {
                tracing::warn!("nix not found; using the local (non-Nix) generation backend");
                Box::new(LocalBuilder::new(paths))
            }
        }
    }
}

fn run_server(paths: Paths, builder: Box<dyn Builder>, listen: SocketAddr) -> Result<()> {
    let state = AppState::new(paths, builder);
    state.tunnel.startup();
    let runtime = tokio::runtime::Runtime::new()?;
    runtime.block_on(async move {
        let app = web::router(state);
        let listener = tokio::net::TcpListener::bind(listen)
            .await
            .with_context(|| format!("binding {listen}"))?;
        tracing::info!("The Box dashboard listening on http://{listen}");
        // ConnectInfo carries the peer address so auth can trust loopback.
        axum::serve(
            listener,
            app.into_make_service_with_connect_info::<SocketAddr>(),
        )
        .await?;
        Ok(())
    })
}
