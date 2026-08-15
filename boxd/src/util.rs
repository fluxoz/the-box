use std::fs;

/// Standard base64 (with padding) — small enough to carry ourselves rather than
/// add a dependency. Used for the provision handoff (where it must match what
/// `base64 -d` in install.sh expects) and for git's basic-auth header.
pub fn base64_encode(data: &[u8]) -> String {
    const B64: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(data.len().div_ceil(3) * 4);
    for chunk in data.chunks(3) {
        let b0 = chunk[0] as u32;
        let b1 = *chunk.get(1).unwrap_or(&0) as u32;
        let b2 = *chunk.get(2).unwrap_or(&0) as u32;
        let n = (b0 << 16) | (b1 << 8) | b2;
        out.push(B64[((n >> 18) & 63) as usize] as char);
        out.push(B64[((n >> 12) & 63) as usize] as char);
        out.push(if chunk.len() > 1 {
            B64[((n >> 6) & 63) as usize] as char
        } else {
            '='
        });
        out.push(if chunk.len() > 2 {
            B64[(n & 63) as usize] as char
        } else {
            '='
        });
    }
    out
}
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

/// A secret that exists as a file only while a command needs it — the operator
/// identity a restore re-keys with, for instance, which arrives in a request
/// body and must not be persisted.
///
/// Prefers the service's `RuntimeDirectory` (a 0700 tmpfs), so the bytes never
/// reach a disk; off-box it falls back to the system temp dir. The file is
/// 0600, overwritten and unlinked on drop, and deliberately never placed under
/// the data dir — that is a git repo.
pub struct TransientSecret {
    path: PathBuf,
}

impl TransientSecret {
    pub fn new(name: &str, contents: &str) -> Result<Self> {
        use std::io::Write;
        use std::os::unix::fs::OpenOptionsExt;

        let runtime = Path::new("/run/boxd");
        let dir = if runtime.is_dir() {
            runtime.to_path_buf()
        } else {
            std::env::temp_dir()
        };
        let path = dir.join(format!("{name}-{}", std::process::id()));
        let _ = fs::remove_file(&path);
        let mut file = fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&path)
            .with_context(|| format!("creating {}", path.display()))?;
        file.write_all(contents.as_bytes())?;
        file.flush()?;
        Ok(Self { path })
    }

    pub fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for TransientSecret {
    fn drop(&mut self) {
        // Overwrite before unlinking: on tmpfs the pages are freed either way,
        // but the off-box fallback may be a real disk.
        if let Ok(meta) = fs::metadata(&self.path) {
            let _ = fs::write(&self.path, vec![0u8; meta.len() as usize]);
        }
        let _ = fs::remove_file(&self.path);
    }
}

/// Give `target` the same owner as `reference`. Best-effort: a no-op when we
/// already own it or lack privilege. Used so files created by an operator
/// running `boxd …` as root over SSH stay readable by the boxd service user
/// (whose data dir is the reference).
pub fn chown_like(reference: &Path, target: &Path) {
    if let Ok(meta) = fs::metadata(reference) {
        use std::os::unix::fs::MetadataExt;
        let _ = std::os::unix::fs::chown(target, Some(meta.uid()), Some(meta.gid()));
    }
}

/// The same, for a whole tree.
///
/// An operator running `boxd …` as root over SSH — which the docs tell them to
/// do — writes files the unprivileged daemon then cannot touch. A rollback run
/// that way used to leave the config and every service's source tree owned by
/// root, and the next deploy from the console failed with a bare "Permission
/// denied" that named nothing.
pub fn chown_tree_like(reference: &Path, target: &Path) {
    chown_like(reference, target);
    let Ok(entries) = fs::read_dir(target) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            chown_tree_like(reference, &path);
        } else {
            chown_like(reference, &path);
        }
    }
}

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
        // Do NOT follow symlinks. `is_dir()`/`fs::copy` both dereference, so a
        // link inside an otherwise innocent source tree used to pull arbitrary
        // files (the Box's own secrets, /etc) into a tree that gets committed,
        // pushed and served without credentials.
        let kind = entry.file_type()?;
        if kind.is_symlink() {
            tracing::warn!(
                "skipping symlink {} while copying a source tree",
                from.display()
            );
            continue;
        }
        if kind.is_dir() {
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
