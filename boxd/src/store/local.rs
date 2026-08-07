use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

use crate::paths::Paths;
use crate::util;

use super::Builder;

/// Pure-Rust fallback backend: "builds" a generation by copying the assembled
/// tree into an append-only directory under the data dir. Used for tests and
/// for machines without Nix; the profile/rollback mechanics are identical to
/// the Nix backend.
pub struct LocalBuilder {
    store_root: PathBuf,
}

impl LocalBuilder {
    pub fn new(paths: &Paths) -> Self {
        Self {
            store_root: paths.local_store_dir(),
        }
    }
}

impl Builder for LocalBuilder {
    fn name(&self) -> &'static str {
        "local"
    }

    fn build(&self, gensrc: &Path) -> Result<PathBuf> {
        fs::create_dir_all(&self.store_root)
            .with_context(|| format!("creating {}", self.store_root.display()))?;
        let next = fs::read_dir(&self.store_root)?
            .filter_map(|e| e.ok())
            .filter_map(|e| {
                e.file_name()
                    .to_string_lossy()
                    .strip_prefix("out-")?
                    .parse::<u64>()
                    .ok()
            })
            .max()
            .unwrap_or(0)
            + 1;
        let dest = self.store_root.join(format!("out-{next}"));
        fs::create_dir_all(&dest)?;
        for entry in fs::read_dir(gensrc)? {
            let entry = entry?;
            let name = entry.file_name();
            // The flake only matters to the Nix backend.
            if name == "flake.nix" || name == "flake.lock" {
                continue;
            }
            let from = entry.path();
            let to = dest.join(&name);
            if from.is_dir() {
                util::copy_dir_recursive(&from, &to)?;
            } else {
                fs::copy(&from, &to)?;
            }
        }
        Ok(dest)
    }
}
