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

/// HMAC-SHA256, by the book (RFC 2104), over the sha2 we already carry — for
/// verifying webhook signatures without another dependency.
pub fn hmac_sha256(key: &[u8], message: &[u8]) -> [u8; 32] {
    use sha2::{Digest, Sha256};
    const BLOCK: usize = 64;
    let mut k = [0u8; BLOCK];
    if key.len() > BLOCK {
        k[..32].copy_from_slice(&Sha256::digest(key));
    } else {
        k[..key.len()].copy_from_slice(key);
    }
    let ipad: Vec<u8> = k.iter().map(|b| b ^ 0x36).collect();
    let opad: Vec<u8> = k.iter().map(|b| b ^ 0x5c).collect();
    let inner = Sha256::digest([ipad.as_slice(), message].concat());
    let outer = Sha256::digest([opad.as_slice(), inner.as_slice()].concat());
    outer.into()
}

/// Compare a received hex signature against an expected MAC without letting
/// the comparison's timing say how close the guess was.
pub fn constant_time_eq_hex(expected: &[u8; 32], received_hex: &str) -> bool {
    let expected_hex: String = expected.iter().map(|b| format!("{b:02x}")).collect();
    let a = expected_hex.as_bytes();
    let b = received_hex.as_bytes();
    if a.len() != b.len() {
        return false;
    }
    let mut diff = 0u8;
    for (x, y) in a.iter().zip(b.iter()) {
        diff |= x ^ y;
    }
    diff == 0
}

#[cfg(test)]
mod hmac_tests {
    use super::*;

    #[test]
    fn rfc_4231_test_case_2() {
        // Key "Jefe", data "what do ya want for nothing?".
        let mac = hmac_sha256(b"Jefe", b"what do ya want for nothing?");
        let hex: String = mac.iter().map(|b| format!("{b:02x}")).collect();
        assert_eq!(
            hex,
            "5bdcc146bf60754e6a042426089575c75a003f089d2739839dec58b964ec3843"
        );
        assert!(constant_time_eq_hex(&mac, &hex));
        assert!(!constant_time_eq_hex(&mac, &hex.replace('5', "6")));
        assert!(!constant_time_eq_hex(&mac, "short"));
    }
}

/// Whether an outbound base URL may point at this machine or its private
/// network. The resident's brain is documented as possibly being the Box's own
/// `/v1`, so that path passes `Loopback::Allow`; a cloud fallback or a control
/// plane has no business there and passes `Loopback::Deny`.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Loopback {
    Allow,
    Deny,
}

/// Gate a URL that boxd will hand to `curl`.
///
/// Two distinct problems, both real:
///
/// 1. **Argument injection.** `curl` treats a leading `-` as an option, not a
///    URL, and `-K<path>` makes it read that path as a config file — which can
///    set `url`, `output` or `upload-file`. boxd runs as the user that owns the
///    0700 secret store, so a stored base URL of `-K/some/file` was arbitrary
///    file read and write. Call sites also pass `--` before positionals now;
///    this is the other half of that fix.
/// 2. **SSRF.** boxd sits on a home network and returns upstream bodies to the
///    caller, so an unvalidated base URL turns it into a probe for the router's
///    admin page, a cloud metadata endpoint, or its own API on loopback.
///
/// Deliberately strict: an explicit `http`/`https` scheme, no credentials in
/// the authority, no control characters, and — unless loopback is allowed — no
/// address literal pointing at this machine or a private network. Name-based
/// hosts that resolve to a private address are NOT caught here; that needs
/// resolve-then-pin at connect time, which curl cannot express.
pub fn validate_outbound_url(url: &str, loopback: Loopback) -> anyhow::Result<()> {
    use anyhow::bail;

    if url.chars().any(|c| c.is_control() || c.is_whitespace()) {
        bail!("URL must not contain spaces or control characters");
    }
    let rest = match url.split_once("://") {
        Some((scheme, rest)) if scheme.eq_ignore_ascii_case("http") => rest,
        Some((scheme, rest)) if scheme.eq_ignore_ascii_case("https") => rest,
        _ => bail!("URL must start with http:// or https:// (got {url:?})"),
    };
    // Authority ends at the first path/query/fragment delimiter.
    let authority = rest
        .split(['/', '?', '#'])
        .next()
        .unwrap_or_default()
        .to_string();
    if authority.is_empty() {
        bail!("URL has no host");
    }
    // `https://good.example@evil.example/` reads as good.example to a human and
    // resolves to evil.example. Refuse the ambiguity outright.
    if authority.contains('@') {
        bail!("URL must not embed credentials");
    }
    // Strip the port. IPv6 literals are bracketed, so find the closing bracket
    // first and only then look for a colon.
    let host = match authority.rfind(']') {
        Some(end) => &authority[..=end],
        None => authority.split(':').next().unwrap_or_default(),
    };
    let host = host.trim_start_matches('[').trim_end_matches(']');
    if host.is_empty() {
        bail!("URL has no host");
    }
    if loopback == Loopback::Allow {
        return Ok(());
    }
    if host.eq_ignore_ascii_case("localhost") || host.to_ascii_lowercase().ends_with(".localhost") {
        bail!("{host} points at this machine; use a reachable endpoint");
    }
    // NOTE ON SCOPE: private LAN ranges are deliberately allowed. This is a
    // self-hosting product — pointing the Box at a beefier GPU machine or a
    // self-hosted GitLab on the same network is a first-class use case, not an
    // attack. What is refused is what is never legitimate: this machine itself,
    // and the link-local range that carries cloud metadata services.
    if let Ok(ip) = host.parse::<std::net::IpAddr>() {
        let forbidden = match ip {
            std::net::IpAddr::V4(v4) => {
                v4.is_loopback() || v4.is_link_local() || v4.is_unspecified()
            }
            std::net::IpAddr::V6(v6) => {
                v6.is_loopback()
                    || v6.is_unspecified()
                    // fe80::/10 link-local.
                    || (v6.segments()[0] & 0xffc0) == 0xfe80
                    || v6
                        .to_ipv4_mapped()
                        .is_some_and(|v4| v4.is_loopback() || v4.is_link_local())
            }
        };
        if forbidden {
            bail!(
                "{ip} is this machine or a link-local address; point this at a reachable endpoint"
            );
        }
    }
    Ok(())
}

#[cfg(test)]
mod url_tests {
    use super::*;

    #[test]
    fn outbound_urls_are_gated() {
        // The verified exploit: curl reads -K<path> as a config file.
        for bad in [
            "-K/var/lib/boxd/x",
            "--config /tmp/x",
            "file:///etc/shadow",
            "gopher://x/",
            "ftp://x/",
            "api.openai.com/v1",
            "",
            "http://",
            "https://good.example@evil.example/v1",
            "https://evil.example/\nx",
            "https://evil.example/ x",
        ] {
            assert!(
                validate_outbound_url(bad, Loopback::Deny).is_err(),
                "must refuse {bad:?}"
            );
        }

        // SSRF targets: this machine, the LAN, the metadata endpoint.
        for bad in [
            "http://127.0.0.1:2693/v1",
            "http://localhost/v1",
            "http://[::1]/v1",
            "http://169.254.169.254/latest/meta-data",
            "http://0.0.0.0/",
        ] {
            assert!(
                validate_outbound_url(bad, Loopback::Deny).is_err(),
                "must refuse SSRF target {bad:?}"
            );
        }

        // Real endpoints still work.
        for good in [
            "https://api.anthropic.com/v1",
            "https://api.x.ai/v1",
            "http://api.example.com:8080/v1",
            "https://gitlab.example.com",
            "https://[2606:4700::1111]/v1",
            // Self-hosting is the point of this product: a model server or a
            // forge on your own network is legitimate, not an attack.
            "http://192.168.1.50:8000/v1",
            "https://gitlab.lan:8443",
        ] {
            validate_outbound_url(good, Loopback::Deny)
                .unwrap_or_else(|e| panic!("must accept {good:?}: {e}"));
        }

        // The resident may legitimately use the Box's own endpoint.
        validate_outbound_url("http://127.0.0.1:2693/v1", Loopback::Allow).unwrap();
        // ...but never a scheme curl could turn into a file operation.
        assert!(validate_outbound_url("-K/tmp/x", Loopback::Allow).is_err());
    }
}
