use std::fs;
use std::path::Path;

use anyhow::{Context, Result};

/// Recursively copy a directory tree. Follows symlinks (their targets are
/// copied as regular files), which is what we want when snapshotting user
/// sources or materializing content out of the Nix store.
pub fn copy_dir_recursive(src: &Path, dst: &Path) -> Result<()> {
    fs::create_dir_all(dst).with_context(|| format!("creating {}", dst.display()))?;
    let entries = fs::read_dir(src).with_context(|| format!("reading {}", src.display()))?;
    for entry in entries {
        let entry = entry?;
        let from = entry.path();
        let to = dst.join(entry.file_name());
        if from.is_dir() {
            copy_dir_recursive(&from, &to)?;
        } else {
            fs::copy(&from, &to).with_context(|| format!("copying {}", from.display()))?;
        }
    }
    Ok(())
}

/// Remove a directory tree if it exists, first making everything writable so
/// content previously copied out of the read-only Nix store can be deleted.
pub fn remove_dir_all_forced(path: &Path) -> Result<()> {
    if !path.exists() {
        return Ok(());
    }
    make_writable(path)?;
    fs::remove_dir_all(path).with_context(|| format!("removing {}", path.display()))?;
    Ok(())
}

fn make_writable(path: &Path) -> Result<()> {
    let meta = fs::symlink_metadata(path)?;
    let mut perms = meta.permissions();
    if perms.readonly() {
        #[allow(clippy::permissions_set_readonly_false)]
        perms.set_readonly(false);
        let _ = fs::set_permissions(path, perms);
    }
    if meta.is_dir() {
        for entry in fs::read_dir(path)? {
            make_writable(&entry?.path())?;
        }
    }
    Ok(())
}
