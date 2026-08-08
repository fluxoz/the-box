//! Operator authentication — "management answers to you."
//!
//! Coarse health is public and loopback (hence SSH-tunnelled) access is
//! trusted; every other request must carry a valid **session**. Sessions are
//! minted by redeeming a one-time **code**: the first code comes from the
//! install recovery kit (enrollment), the rest are vouched for from an
//! already-paired device ("add device"). The operator's key never enters the
//! browser — the browser only ever holds a session token. Tokens and codes are
//! stored SHA-256-hashed, 0600, and never committed to git.

use std::io::Read;
use std::os::unix::fs::PermissionsExt;

use anyhow::{Context, Result};
use axum::http::HeaderMap;
use chrono::Utc;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::paths::Paths;

/// One-time codes are short-lived: enough to walk to another device and type.
const CODE_TTL_SECS: i64 = 15 * 60;
const COOKIE: &str = "box_session";

#[derive(Serialize, Deserialize, Clone)]
struct StoredSession {
    id: String,
    label: String,
    hash: String,
    created_at: i64,
}

#[derive(Serialize, Deserialize, Clone)]
struct StoredCode {
    hash: String,
    label: String,
    expires_at: i64,
}

#[derive(Serialize, Deserialize, Default)]
struct Store {
    #[serde(default)]
    sessions: Vec<StoredSession>,
    #[serde(default)]
    codes: Vec<StoredCode>,
}

/// A session as shown in the "Paired devices" list — never includes the hash.
#[derive(Serialize, Clone)]
pub struct SessionInfo {
    pub id: String,
    pub label: String,
    pub created_at: i64,
}

fn hash(s: &str) -> String {
    Sha256::digest(s.as_bytes())
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

fn random_hex(n: usize) -> Result<String> {
    let mut buf = vec![0u8; n];
    std::fs::File::open("/dev/urandom")
        .context("opening /dev/urandom")?
        .read_exact(&mut buf)
        .context("reading /dev/urandom")?;
    Ok(buf.iter().map(|b| format!("{b:02x}")).collect())
}

fn now() -> i64 {
    Utc::now().timestamp()
}

fn load(paths: &Paths) -> Store {
    std::fs::read_to_string(paths.auth_file())
        .ok()
        .and_then(|t| serde_json::from_str(&t).ok())
        .unwrap_or_default()
}

fn save(paths: &Paths, store: &Store) -> Result<()> {
    let file = paths.auth_file();
    std::fs::write(&file, serde_json::to_string_pretty(store)?)
        .with_context(|| format!("writing {}", file.display()))?;
    std::fs::set_permissions(&file, std::fs::Permissions::from_mode(0o600))?;
    // Match the data dir's owner so a code minted by an operator running
    // `boxd auth enroll` as root over SSH is still readable by the boxd
    // service user. Best-effort: a no-op when we already own it.
    if let Ok(meta) = std::fs::metadata(&paths.data_dir) {
        use std::os::unix::fs::MetadataExt;
        let _ = std::os::unix::fs::chown(&file, Some(meta.uid()), Some(meta.gid()));
    }
    Ok(())
}

/// Mint a session directly (used for the loopback path and for agents via the
/// CLI). Returns the plaintext token — only its hash is stored.
pub fn mint_session(paths: &Paths, label: &str) -> Result<String> {
    let token = random_hex(32)?;
    let mut store = load(paths);
    store.sessions.push(StoredSession {
        id: random_hex(4)?,
        label: label.to_string(),
        hash: hash(&token),
        created_at: now(),
    });
    save(paths, &store)?;
    Ok(token)
}

/// Is this token a live session?
pub fn verify(paths: &Paths, token: &str) -> bool {
    let h = hash(token);
    load(paths).sessions.iter().any(|s| s.hash == h)
}

pub fn list(paths: &Paths) -> Vec<SessionInfo> {
    load(paths)
        .sessions
        .into_iter()
        .map(|s| SessionInfo {
            id: s.id,
            label: s.label,
            created_at: s.created_at,
        })
        .collect()
}

pub fn revoke(paths: &Paths, id: &str) -> Result<bool> {
    let mut store = load(paths);
    let before = store.sessions.len();
    store.sessions.retain(|s| s.id != id);
    let removed = store.sessions.len() != before;
    if removed {
        save(paths, &store)?;
    }
    Ok(removed)
}

/// Mint a one-time pairing/enrollment code (returns plaintext; stores hash).
pub fn mint_code(paths: &Paths, label: &str) -> Result<String> {
    let code = random_hex(5)?; // 10 hex chars, ~40 bits, single-use, short-lived
    let mut store = load(paths);
    let t = now();
    store.codes.retain(|c| c.expires_at > t); // prune expired
    store.codes.push(StoredCode {
        hash: hash(&code),
        label: label.to_string(),
        expires_at: t + CODE_TTL_SECS,
    });
    save(paths, &store)?;
    Ok(code)
}

/// Redeem a one-time code for a new session. The code is consumed whether or
/// not it was still valid, so it can't be replayed.
pub fn redeem_code(paths: &Paths, code: &str, session_label: &str) -> Result<String> {
    let h = hash(code.trim().to_ascii_lowercase().as_str());
    let mut store = load(paths);
    let t = now();
    let pos = store
        .codes
        .iter()
        .position(|c| c.hash == h && c.expires_at > t)
        .context("invalid or expired code")?;
    store.codes.remove(pos);
    let token = random_hex(32)?;
    store.sessions.push(StoredSession {
        id: random_hex(4)?,
        label: session_label.to_string(),
        hash: hash(&token),
        created_at: t,
    });
    save(paths, &store)?;
    Ok(token)
}

// --- request classification (used by the middleware) ----------------------

/// Paths reachable without a session: coarse health, deployed public sites, the
/// pairing flow itself, and the favicon.
pub fn is_public_path(path: &str) -> bool {
    path == "/api/v1/health"
        || path == "/sites"
        || path.starts_with("/sites/")
        || path.starts_with("/pair")
        || path == "/favicon.ico"
}

/// Trusted local access = a loopback peer that is NOT a proxied request. The
/// BYO Cloudflare tunnel also connects from loopback, but cloudflared forwards
/// these headers while direct/SSH-tunnel access does not — so tunnel traffic is
/// never mistaken for trusted local access.
pub fn is_trusted_local(peer_is_loopback: bool, headers: &HeaderMap) -> bool {
    peer_is_loopback
        && !headers.contains_key("x-forwarded-for")
        && !headers.contains_key("cf-connecting-ip")
        && !headers.contains_key("x-forwarded-host")
}

/// Pull a session token from a Bearer header (agents) or the session cookie
/// (browsers).
pub fn extract_token(headers: &HeaderMap) -> Option<String> {
    if let Some(auth) = headers.get("authorization").and_then(|v| v.to_str().ok()) {
        if let Some(t) = auth.strip_prefix("Bearer ") {
            return Some(t.trim().to_string());
        }
    }
    let cookie = headers.get("cookie").and_then(|v| v.to_str().ok())?;
    cookie
        .split(';')
        .filter_map(|p| p.trim().strip_prefix(&format!("{COOKIE}=")))
        .map(str::to_string)
        .next()
}

/// The `Set-Cookie` value that binds a browser session.
pub fn session_cookie(token: &str) -> String {
    format!("{COOKIE}={token}; HttpOnly; Path=/; SameSite=Lax; Max-Age=31536000")
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn paths() -> (TempDir, Paths) {
        let tmp = TempDir::new().unwrap();
        let p = Paths::new(tmp.path().to_path_buf());
        p.ensure().unwrap();
        (tmp, p)
    }

    #[test]
    fn session_mint_verify_revoke() {
        let (_t, p) = paths();
        assert!(!verify(&p, "nope"));
        let token = mint_session(&p, "laptop").unwrap();
        assert!(verify(&p, &token));
        assert!(!verify(&p, "wrong"));
        let id = list(&p)[0].id.clone();
        assert!(revoke(&p, &id).unwrap());
        assert!(!verify(&p, &token));
    }

    #[test]
    fn code_is_single_use() {
        let (_t, p) = paths();
        let code = mint_code(&p, "enrollment").unwrap();
        let token = redeem_code(&p, &code, "first device").unwrap();
        assert!(verify(&p, &token));
        // second redemption of the same code fails
        assert!(redeem_code(&p, &code, "again").is_err());
    }

    #[test]
    fn public_paths_and_trust() {
        assert!(is_public_path("/api/v1/health"));
        assert!(is_public_path("/sites/blog/"));
        assert!(is_public_path("/pair"));
        assert!(!is_public_path("/"));
        assert!(!is_public_path("/api/v1/services"));

        let empty = HeaderMap::new();
        assert!(is_trusted_local(true, &empty));
        assert!(!is_trusted_local(false, &empty));
        let mut proxied = HeaderMap::new();
        proxied.insert("x-forwarded-for", "1.2.3.4".parse().unwrap());
        assert!(!is_trusted_local(true, &proxied)); // tunnel traffic, not trusted
    }

    #[test]
    fn token_extraction() {
        let mut h = HeaderMap::new();
        h.insert("authorization", "Bearer abc123".parse().unwrap());
        assert_eq!(extract_token(&h).as_deref(), Some("abc123"));
        let mut c = HeaderMap::new();
        c.insert("cookie", "foo=1; box_session=tok; bar=2".parse().unwrap());
        assert_eq!(extract_token(&c).as_deref(), Some("tok"));
        assert_eq!(extract_token(&HeaderMap::new()), None);
    }
}
