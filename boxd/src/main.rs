use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

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
}

fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()),
        )
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
    let state = Arc::new(AppState {
        paths,
        builder,
        apply_lock: Mutex::new(()),
    });
    let runtime = tokio::runtime::Runtime::new()?;
    runtime.block_on(async move {
        let app = web::router(state);
        let listener = tokio::net::TcpListener::bind(listen)
            .await
            .with_context(|| format!("binding {listen}"))?;
        tracing::info!("The Box dashboard listening on http://{listen}");
        axum::serve(listener, app).await?;
        Ok(())
    })
}
