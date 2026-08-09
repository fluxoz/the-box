//! The Box installer core binary. One tool, three entry points over the shared
//! `box-core` pipeline (probe → resolve → disko::render):
//!
//!   box-installer probe                 — list this machine's disks (JSON)
//!   box-installer plan   --orders F     — resolve a storage policy to disko
//!   box-installer wizard                — pick a layout on the box (TUI)

mod orders;
mod plan;
mod wizard;

use anyhow::Result;
use clap::{Parser, Subcommand};

#[derive(Parser)]
#[command(
    name = "box-installer",
    version,
    about = "The Box installer: probe disks, resolve storage, run the setup wizard"
)]
struct Cli {
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Print the machine's disks as JSON.
    Probe,
    /// Resolve the storage policy in an orders file into a disko config.
    Plan {
        /// Path to box-install.json (the orders / handoff).
        #[arg(long)]
        orders: String,
        /// Where to write the generated disko config.
        #[arg(long, default_value = "/tmp/box-disko.nix")]
        out: String,
    },
    /// Interactive console wizard: pick a disk layout on the box itself.
    Wizard {
        /// Optional base orders (name/wifi/keys) to merge the disk choice into.
        #[arg(long)]
        base_orders: Option<String>,
        /// Where to write the effective orders the installer will act on.
        #[arg(long, default_value = "/tmp/box-install.json")]
        orders_out: String,
        /// Where to write the generated disko config.
        #[arg(long, default_value = "/tmp/box-disko.nix")]
        disko_out: String,
        /// If this file appears, another door (the browser wizard) committed —
        /// exit without writing, so the two never both drive an install.
        #[arg(long)]
        watch_commit: Option<String>,
        /// Shown in the TUI so a person at the screen can finish in a browser.
        #[arg(long)]
        browser_url: Option<String>,
        /// The setup PIN, shown alongside the browser URL.
        #[arg(long)]
        pin: Option<String>,
    },
}

fn main() -> Result<()> {
    match Cli::parse().cmd {
        Cmd::Probe => plan::probe_cmd(),
        Cmd::Plan { orders, out } => plan::plan_cmd(&orders, &out),
        Cmd::Wizard {
            base_orders,
            orders_out,
            disko_out,
            watch_commit,
            browser_url,
            pin,
        } => wizard::run(
            base_orders.as_deref(),
            &orders_out,
            &disko_out,
            watch_commit.as_deref(),
            wizard::browser_hint(browser_url.as_deref(), pin.as_deref()),
        ),
    }
}
