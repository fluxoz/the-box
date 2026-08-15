//! boxd — The Box daemon.
//!
//! Turns a declarative service configuration into immutable, atomically
//! switchable generations (built with Nix when available), and serves the
//! local dashboard, JSON API and deployed static sites.

pub mod agecrypt;
pub mod approvals;
pub mod auth;
pub mod backup;
pub mod board;
pub mod build;
pub mod catalog;
pub mod cfapi;
pub mod channel;
pub mod cloud;
pub mod config;
pub mod connect;
pub mod fleet;
pub mod forge;
pub mod ghapi;
pub mod glapi;
pub mod history;
pub mod hostgen;
pub mod ingress;
pub mod jobs;
pub mod logs;
pub mod manifest;
pub mod nixgen;
pub mod ops;
pub mod ostier;
pub mod paths;
pub mod ports;
pub mod provision;
pub mod pull;
pub mod secrets;
pub mod store;
pub mod templates;
pub mod tunnel;
pub mod util;
pub mod web;
pub mod webauthn;
pub mod words;
