//! boxd — The Box daemon.
//!
//! Turns a declarative service configuration into immutable, atomically
//! switchable generations (built with Nix when available), and serves the
//! local dashboard, JSON API and deployed static sites.

pub mod config;
pub mod history;
pub mod hostgen;
pub mod manifest;
pub mod nixgen;
pub mod ops;
pub mod ostier;
pub mod paths;
pub mod secrets;
pub mod store;
pub mod templates;
pub mod tunnel;
pub mod util;
pub mod web;
