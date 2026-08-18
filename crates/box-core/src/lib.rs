//! The Box shared installer core: probe the machine's disks, resolve a storage
//! *intent* (single / mirror / pool) into a concrete validated layout, and
//! render that layout to a disko configuration.
//!
//! This crate is deliberately front-end-agnostic. The console TUI, the
//! browser wizard, and the non-interactive `plan` path all call the same
//! `probe` → `resolve` → `disko::render` pipeline, so they can never disagree
//! about what a layout means or when it is unsafe.

pub mod disko;
pub mod model;
pub mod orders;
pub mod probe;
pub mod resolve;
pub mod pairing;

pub use model::Disk;
pub use resolve::{eligible, resolve, LayoutKind, ResolveError, ResolveOpts, ResolvedLayout};
