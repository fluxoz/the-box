//! Generation storage: immutable output trees plus a Nix-style profile of
//! numbered symlinks with an atomically switchable `box` link.
//!
//! The profile layout is identical for both backends:
//! ```text
//! profiles/box            -> box-3-link          (relative; atomic rename swap)
//! profiles/box-1-link     -> /nix/store/...      (or <data>/store/out-1)
//! profiles/box-3-link     -> /nix/store/...
//! ```
//! Builders only produce the immutable tree; profile bookkeeping is shared.

pub mod local;
pub mod nix;

use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};
use chrono::{DateTime, Utc};
use serde::Serialize;

use crate::paths::Paths;

pub const PROFILE: &str = "box";

#[derive(Debug, Clone, Serialize)]
pub struct GenerationInfo {
    pub number: u64,
    pub store_path: PathBuf,
    pub created_at: Option<DateTime<Utc>>,
    pub current: bool,
}

/// Builds a generation source directory into an immutable output tree.
pub trait Builder: Send + Sync {
    fn name(&self) -> &'static str;
    fn build(&self, gensrc: &Path) -> Result<PathBuf>;
}

fn link_name(number: u64) -> String {
    format!("{PROFILE}-{number}-link")
}

fn parse_link_name(name: &str) -> Option<u64> {
    name.strip_prefix("box-")?
        .strip_suffix("-link")?
        .parse()
        .ok()
}

fn next_number(paths: &Paths) -> Result<u64> {
    Ok(list(paths)?.iter().map(|g| g.number).max().unwrap_or(0) + 1)
}

/// Register a built output tree as the next generation and switch to it.
pub fn record_and_switch(paths: &Paths, store_path: &Path) -> Result<GenerationInfo> {
    let profiles = paths.profiles_dir();
    fs::create_dir_all(&profiles)?;
    let number = next_number(paths)?;
    let link = profiles.join(link_name(number));
    std::os::unix::fs::symlink(store_path, &link)
        .with_context(|| format!("creating {}", link.display()))?;
    switch(paths, number)?;
    Ok(GenerationInfo {
        number,
        store_path: store_path.to_path_buf(),
        created_at: Some(Utc::now()),
        current: true,
    })
}

/// Atomically point the profile at an existing generation.
pub fn switch(paths: &Paths, number: u64) -> Result<()> {
    let profiles = paths.profiles_dir();
    let target = link_name(number);
    if !profiles.join(&target).exists() {
        bail!("generation {number} does not exist");
    }
    let tmp = profiles.join(".box-switch-tmp");
    let _ = fs::remove_file(&tmp);
    std::os::unix::fs::symlink(&target, &tmp)?;
    fs::rename(&tmp, profiles.join(PROFILE)).context("switching profile")?;
    Ok(())
}

pub fn current(paths: &Paths) -> Result<Option<GenerationInfo>> {
    let profiles = paths.profiles_dir();
    let Ok(target) = fs::read_link(profiles.join(PROFILE)) else {
        return Ok(None);
    };
    let name = target
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or_default()
        .to_string();
    let Some(number) = parse_link_name(&name) else {
        return Ok(None);
    };
    let link = profiles.join(&name);
    let store_path = fs::read_link(&link).unwrap_or_default();
    let created_at = created_at_of(&link);
    Ok(Some(GenerationInfo {
        number,
        store_path,
        created_at,
        current: true,
    }))
}

/// All generations, ascending by number.
pub fn list(paths: &Paths) -> Result<Vec<GenerationInfo>> {
    let profiles = paths.profiles_dir();
    if !profiles.is_dir() {
        return Ok(Vec::new());
    }
    let current_number = current(paths)?.map(|g| g.number);
    let mut out = Vec::new();
    for entry in fs::read_dir(&profiles)? {
        let entry = entry?;
        let name = entry.file_name().to_string_lossy().to_string();
        let Some(number) = parse_link_name(&name) else {
            continue;
        };
        let link = profiles.join(&name);
        out.push(GenerationInfo {
            number,
            store_path: fs::read_link(&link).unwrap_or_default(),
            created_at: created_at_of(&link),
            current: Some(number) == current_number,
        });
    }
    out.sort_by_key(|g| g.number);
    Ok(out)
}

/// Fully resolved output tree of the current generation, if any.
pub fn current_store_path(paths: &Paths) -> Option<PathBuf> {
    fs::canonicalize(paths.profiles_dir().join(PROFILE)).ok()
}

fn created_at_of(link: &Path) -> Option<DateTime<Utc>> {
    fs::symlink_metadata(link)
        .and_then(|m| m.modified())
        .ok()
        .map(DateTime::<Utc>::from)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn link_name_roundtrip() {
        assert_eq!(parse_link_name(&link_name(7)), Some(7));
        assert_eq!(parse_link_name("box-12-link"), Some(12));
        assert_eq!(parse_link_name("box"), None);
        assert_eq!(parse_link_name("box--link"), None);
        assert_eq!(parse_link_name("other-1-link"), None);
    }
}
