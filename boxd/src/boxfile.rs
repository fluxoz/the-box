//! The Boxfile: deploy configuration that lives in the repository.
//!
//! A `box.toml` at the repo root (or the linked subdir) declares how the
//! project becomes a site, next to the code it describes:
//!
//! ```toml
//! [build]
//! command = "npm run build"
//! install = "npm ci"          # optional; detected from the lockfile if absent
//! output_dir = "dist"         # optional; common places are detected
//! subdir = "apps/web"         # optional; the app's root within the repo
//! ```
//!
//! Precedence is one rule long: what the operator said at link time wins;
//! the Boxfile fills what they left unsaid. It is re-read from the fresh
//! checkout on every sync, so changing it in a commit changes the next
//! deploy — configuration as code, with the repo as the single source.
//!
//! A malformed Boxfile fails the sync loudly with the parse error. Silently
//! ignoring a file someone deliberately wrote would deploy the wrong thing
//! while looking healthy, which is this project's least favorite failure.

use std::path::Path;

use anyhow::{Context, Result};
use serde::Deserialize;

pub const FILE_NAME: &str = "box.toml";

#[derive(Debug, Clone, Default, Deserialize)]
pub struct Boxfile {
    #[serde(default)]
    pub build: Option<BuildSection>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct BuildSection {
    pub command: String,
    #[serde(default)]
    pub install: Option<String>,
    #[serde(default)]
    pub output_dir: Option<String>,
    #[serde(default)]
    pub subdir: Option<String>,
}

/// Read the Boxfile from a checkout. `Ok(None)` when there is none — most
/// repos won't have one and that is fine; `Err` when one exists but cannot
/// be parsed, because a deliberate file deserves a loud failure.
pub fn read(tree: &Path) -> Result<Option<Boxfile>> {
    let path = tree.join(FILE_NAME);
    if !path.is_file() {
        return Ok(None);
    }
    let text = std::fs::read_to_string(&path)
        .with_context(|| format!("reading {FILE_NAME} from the repository"))?;
    let parsed: Boxfile = toml::from_str(&text)
        .with_context(|| format!("{FILE_NAME} in the repository is invalid"))?;
    if let Some(b) = &parsed.build {
        crate::pull::validate_subdir_opt(b.subdir.as_deref())?;
        crate::pull::validate_subdir_opt(b.output_dir.as_deref())?;
    }
    Ok(Some(parsed))
}

/// Merge: the link's explicit choices win, the Boxfile fills the gaps.
/// Returns (build, subdir) as they should apply to THIS checkout.
pub fn effective(
    link_build: Option<crate::build::BuildSpec>,
    link_subdir: Option<String>,
    boxfile: Option<&Boxfile>,
) -> (Option<crate::build::BuildSpec>, Option<String>) {
    let file_build = boxfile.and_then(|b| b.build.as_ref());
    let build = link_build.or_else(|| {
        file_build.map(|b| crate::build::BuildSpec {
            command: b.command.clone(),
            install: b.install.clone(),
            output_dir: b.output_dir.clone(),
        })
    });
    let subdir = link_subdir.or_else(|| file_build.and_then(|b| b.subdir.clone()));
    (build, subdir)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn absent_is_fine_malformed_is_loud_and_link_wins() {
        let tmp = tempfile::TempDir::new().unwrap();
        assert!(read(tmp.path()).unwrap().is_none());

        std::fs::write(
            tmp.path().join(FILE_NAME),
            "[build]\ncommand = \"npm run build\"\noutput_dir = \"dist\"\n",
        )
        .unwrap();
        let bf = read(tmp.path()).unwrap().unwrap();
        assert_eq!(bf.build.as_ref().unwrap().command, "npm run build");

        // The operator's explicit build wins over the repo's.
        let explicit = crate::build::BuildSpec {
            command: "make site".into(),
            install: None,
            output_dir: None,
        };
        let (build, _) = effective(Some(explicit), None, Some(&bf));
        assert_eq!(build.unwrap().command, "make site");
        // The Boxfile fills when the link is silent.
        let (build, _) = effective(None, None, Some(&bf));
        assert_eq!(build.unwrap().command, "npm run build");

        std::fs::write(tmp.path().join(FILE_NAME), "[build\nnot toml").unwrap();
        let err = format!("{:#}", read(tmp.path()).unwrap_err());
        assert!(err.contains("box.toml"), "{err}");

        // A traversal in the repo's own config is refused like any other.
        std::fs::write(
            tmp.path().join(FILE_NAME),
            "[build]\ncommand = \"x\"\nsubdir = \"../escape\"\n",
        )
        .unwrap();
        assert!(read(tmp.path()).is_err());
    }
}
