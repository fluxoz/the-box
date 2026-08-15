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
    /// May this session run destructive operations (wipe a machine, delete a
    /// service, restore over live data) WITHOUT a human tap? Off by default;
    /// the operator turns it on per-session, eyes open, in the device list.
    #[serde(default)]
    autonomous: bool,
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
    /// Enrolled security keys / passkeys. These stay here rather than in the
    /// config repo: they bind an authenticator to THIS box's origin, so they
    /// are not portable configuration and must not be pushed to a git host.
    #[serde(default)]
    keys: Vec<crate::webauthn::StoredKey>,
    /// Stable WebAuthn user handle for this Box's operator. Generated once so
    /// every key enrolled here belongs to the same user, which is what lets a
    /// browser offer them together.
    #[serde(default)]
    operator_id: Option<String>,
}

/// A session as shown in the "Paired devices" list — never includes the hash.
#[derive(Serialize, Clone)]
pub struct SessionInfo {
    pub id: String,
    pub label: String,
    pub created_at: i64,
    pub autonomous: bool,
}

fn hash(s: &str) -> String {
    Sha256::digest(s.as_bytes())
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

pub(crate) fn random_hex(n: usize) -> Result<String> {
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
    load_checked(paths).unwrap_or_default()
}

/// Load the auth store, distinguishing "no file yet" (a genuinely unclaimed
/// Box — `Ok(default)`) from "a file exists but cannot be read or parsed"
/// (`Err`).
///
/// That distinction is a security boundary, not tidiness: every caller that
/// decides whether this Box is still claimable must treat an unreadable store
/// as claimed. Silently defaulting to an empty store would re-open first-run
/// claim to anyone on the LAN — and let `claim` overwrite the real operator's
/// credentials — on the strength of a truncated write.
fn load_checked(paths: &Paths) -> Result<Store> {
    let file = paths.auth_file();
    match std::fs::read_to_string(&file) {
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Store::default()),
        Err(e) => {
            Err(anyhow::Error::from(e)).with_context(|| format!("reading {}", file.display()))
        }
        Ok(text) => {
            serde_json::from_str(&text).with_context(|| format!("parsing {}", file.display()))
        }
    }
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
        autonomous: false,
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
            autonomous: s.autonomous,
        })
        .collect()
}

/// The session a token belongs to, for callers that need to know WHO is
/// asking, not just that someone authorized is.
pub fn session_for(paths: &Paths, token: &str) -> Option<SessionInfo> {
    let h = hash(token);
    load(paths)
        .sessions
        .into_iter()
        .find(|s| s.hash == h)
        .map(|s| SessionInfo {
            id: s.id,
            label: s.label,
            created_at: s.created_at,
            autonomous: s.autonomous,
        })
}

/// Grant or revoke a session's leave to run destructive operations without a
/// human tap. An operator decision, made in the device list.
pub fn set_autonomous(paths: &Paths, id: &str, on: bool) -> Result<bool> {
    let mut store = load(paths);
    let mut found = false;
    for s in &mut store.sessions {
        if s.id == id {
            s.autonomous = on;
            found = true;
        }
    }
    if found {
        save(paths, &store)?;
    }
    Ok(found)
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

/// Install a pre-hashed, non-expiring single-use code. Used to seed the
/// enrollment code from the install handoff (its hash rides in box-install.json,
/// generated client-side by the Configurator), so a box is pairable from first
/// boot with the code from the user's recovery kit — no SSH.
pub fn import_code(paths: &Paths, code_hash: &str, label: &str) -> Result<()> {
    let mut store = load(paths);
    store.codes.push(StoredCode {
        hash: code_hash.trim().to_ascii_lowercase(),
        label: label.to_string(),
        expires_at: i64::MAX, // valid until first used (first boot may be much later)
    });
    save(paths, &store)?;
    Ok(())
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
        autonomous: false,
    });
    save(paths, &store)?;
    Ok(token)
}

/// A Box no one has claimed yet: no operator sessions and no pending codes. A
/// freshly flashed Box (no Configurator recovery kit) is claimable; a Box set
/// up with orders (which seed an enrollment code) or already paired is not.
pub fn is_claimable(paths: &Paths) -> bool {
    // An enrolled security key is an operator credential just like a session:
    // if one exists, this Box has been claimed. Ignoring them meant revoking
    // the last session re-opened first-run claim to anyone on the LAN, even
    // though the real operator could still sign in with their key.
    // An unreadable store counts as claimed — see `load_checked`.
    match load_checked(paths) {
        Ok(store) => store.sessions.is_empty() && store.codes.is_empty() && store.keys.is_empty(),
        Err(_) => false,
    }
}

/// Whether any security key is enrolled — the sign-in affordance is only
/// offered where it can actually succeed.
pub fn has_security_keys(paths: &Paths) -> bool {
    !load(paths).keys.is_empty()
}

/// First-run claim: on a still-unclaimed Box, mint the first operator session
/// without a code, so a freshly flashed appliance is reachable from a browser
/// with no SSH and no recovery kit. Returns `None` if the Box was already
/// claimed or has a pending code (the caller then falls back to code entry).
/// The check and the mint share one load/save so the claim window stays small.
pub fn claim(paths: &Paths, label: &str) -> Result<Option<String>> {
    // `load_checked`, not `load`: claiming REPLACES the store, so a store that
    // exists but won't parse must abort the claim rather than overwrite the
    // real operator's sessions and keys with a fresh one.
    let mut store = load_checked(paths)?;
    if !store.sessions.is_empty() || !store.codes.is_empty() || !store.keys.is_empty() {
        return Ok(None);
    }
    let token = random_hex(32)?;
    store.sessions.push(StoredSession {
        id: random_hex(4)?,
        label: label.to_string(),
        hash: hash(&token),
        created_at: now(),
        autonomous: false,
    });
    save(paths, &store)?;
    Ok(Some(token))
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

/// True when the request arrived through a proxy/tunnel — cloudflared and the
/// like forward these headers; a direct LAN or loopback connection does not.
pub fn is_proxied(headers: &HeaderMap) -> bool {
    headers.contains_key("x-forwarded-for")
        || headers.contains_key("cf-connecting-ip")
        || headers.contains_key("x-forwarded-host")
}

/// Reject a state-changing request that a browser initiated from another site.
///
/// This is the CSRF boundary, and it matters far more here than in a typical
/// app: until recently [`is_trusted_local`] authorized a loopback peer with NO
/// credential at all, and even now a browser holds a session cookie that a
/// cross-site form POST would otherwise carry. Any page in a browser that can reach the console —
/// one opened on the Box itself, or on a laptop with `ssh -L 2693:...` open,
/// or via DNS rebinding — could otherwise POST a form cross-origin (a CORS
/// "simple request", no preflight) and mint a pairing code, repoint the update
/// channel, or recreate the Box from an attacker's repo. The response is
/// unreadable to the attacker, but the ACTION still happens.
///
/// Browsers always attach `Origin` to a cross-origin POST, so verifying it when
/// present closes that door while leaving credentialed API/agent clients and
/// local scripting (which send no `Origin`) working.
pub fn cross_site_write(method: &str, headers: &HeaderMap) -> bool {
    let changes_state = matches!(method, "POST" | "PUT" | "PATCH" | "DELETE");
    if !changes_state {
        return false;
    }
    let host = headers.get("host").and_then(|v| v.to_str().ok());
    let stated = headers
        .get("origin")
        .and_then(|v| v.to_str().ok())
        .filter(|o| *o != "null")
        .or_else(|| headers.get("referer").and_then(|v| v.to_str().ok()));
    let Some(stated) = stated else {
        return false; // no browser context claimed; a token still gates it
    };
    // Compare authorities: scheme://host[:port]/... -> host[:port]
    let authority = stated
        .split_once("://")
        .map(|(_, rest)| rest.split(['/', '?', '#']).next().unwrap_or(""))
        .unwrap_or("");
    match host {
        Some(h) => !authority.eq_ignore_ascii_case(h),
        // No Host to compare against: treat a stated origin as untrusted.
        None => true,
    }
}

/// A direct connection from this machine (loopback, not through the tunnel).
///
/// This is no longer an authorization decision — every request needs a session.
/// It survives because it answers a different question: is this connection safe
/// to put a SECRET on the wire? The Recreate page uses it to decide whether to
/// accept the operator's private key, since loopback and the TLS tunnel are the
/// only paths where that key is not travelling in the clear.
/// The BYO Cloudflare tunnel also connects from loopback, but cloudflared
/// forwards proxy headers while direct/SSH-tunnel access does not — so tunnel
/// traffic is never mistaken for trusted local access.
pub fn is_trusted_local(peer_is_loopback: bool, headers: &HeaderMap) -> bool {
    peer_is_loopback && !is_proxied(headers)
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

// ---- security keys (WebAuthn) --------------------------------------------

/// The stable user handle every key on this Box is enrolled against.
pub fn operator_id(paths: &Paths) -> Result<uuid::Uuid> {
    let mut store = load(paths);
    if let Some(id) = store.operator_id.as_deref().and_then(|s| s.parse().ok()) {
        return Ok(id);
    }
    let id = uuid::Uuid::new_v4();
    store.operator_id = Some(id.to_string());
    save(paths, &store)?;
    Ok(id)
}

/// Keys usable at this Relying Party ID. A credential is bound to one rpId, so
/// a key enrolled over an SSH forward genuinely cannot answer through the
/// tunnel — filtering here is what keeps that from looking like a failure.
pub fn keys_for(paths: &Paths, rp_id: &str) -> Vec<crate::webauthn::StoredKey> {
    load(paths)
        .keys
        .into_iter()
        .filter(|k| k.rp_id == rp_id)
        .collect()
}

pub fn list_keys(paths: &Paths) -> Vec<crate::webauthn::StoredKey> {
    load(paths).keys
}

pub fn add_key(paths: &Paths, label: &str, rp_id: &str, passkey: serde_json::Value) -> Result<()> {
    let mut store = load(paths);
    store.keys.push(crate::webauthn::StoredKey {
        id: random_hex(4)?,
        label: label.trim().to_string(),
        rp_id: rp_id.to_string(),
        created_at: now(),
        last_used_at: None,
        passkey,
    });
    save(paths, &store)
}

pub fn revoke_key(paths: &Paths, id: &str) -> Result<bool> {
    let mut store = load(paths);
    let before = store.keys.len();
    store.keys.retain(|k| k.id != id);
    let removed = store.keys.len() != before;
    if removed {
        save(paths, &store)?;
    }
    Ok(removed)
}

/// Record a successful assertion (updates the signature counter) and mint a
/// session, so signing in with a key lands in exactly the same place as
/// redeeming a pairing code — nothing downstream needs to know the difference.
pub fn session_from_key(paths: &Paths, cred_id: &[u8], label: &str) -> Result<String> {
    let mut store = load(paths);
    let hex: String = cred_id.iter().map(|b| format!("{b:02x}")).collect();
    if let Some(key) = store.keys.iter_mut().find(|k| {
        k.passkey()
            .map(|p| {
                let id: &[u8] = p.cred_id().as_ref();
                id.iter().map(|b| format!("{b:02x}")).collect::<String>() == hex
            })
            .unwrap_or(false)
    }) {
        key.last_used_at = Some(now());
    }
    let token = random_hex(32)?;
    store.sessions.push(StoredSession {
        id: random_hex(4)?,
        label: label.to_string(),
        hash: hash(&token),
        created_at: now(),
        autonomous: false,
    });
    save(paths, &store)?;
    Ok(token)
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

    /// First-run claim is the one door that opens without a credential, so
    /// everything that means "this Box already has an operator" must close it.
    #[test]
    fn claim_closes_for_every_kind_of_credential() {
        // A fresh Box is claimable exactly once.
        let (_t, p) = paths();
        assert!(is_claimable(&p));
        assert!(claim(&p, "first").unwrap().is_some());
        assert!(!is_claimable(&p));
        assert!(claim(&p, "second").unwrap().is_none());

        // A security key alone keeps it claimed: revoking the last session
        // must not re-open first-run claim to the LAN.
        let (_t2, p2) = paths();
        add_key(&p2, "yubikey", "localhost", serde_json::json!({})).unwrap();
        assert!(
            !is_claimable(&p2),
            "an enrolled security key means this Box has an operator"
        );
        assert!(claim(&p2, "attacker").unwrap().is_none());
        assert!(has_security_keys(&p2));

        // A store that exists but cannot be parsed fails CLOSED: a truncated
        // write must never look like a factory-fresh Box.
        let (_t3, p3) = paths();
        std::fs::write(p3.auth_file(), "{ this is not json").unwrap();
        assert!(
            !is_claimable(&p3),
            "an unreadable auth store is not claimable"
        );
        assert!(
            claim(&p3, "attacker").is_err(),
            "claim must refuse to overwrite it"
        );
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

    #[test]
    fn first_run_claim_mints_then_locks() {
        let (_t, p) = paths();
        // Fresh box: claimable, and the claim mints the first operator session.
        assert!(is_claimable(&p));
        let token = claim(&p, "first device")
            .unwrap()
            .expect("a fresh box is claimable");
        assert!(verify(&p, &token));
        // Now claimed: not claimable, and a second claim is refused.
        assert!(!is_claimable(&p));
        assert!(claim(&p, "again").unwrap().is_none());
    }

    #[test]
    fn a_pending_code_blocks_claim() {
        let (_t, p) = paths();
        // A Box provisioned with orders has an enrollment code but no session; it
        // must NOT be claimable, since the operator already holds a code.
        mint_code(&p, "enrollment").unwrap();
        assert!(!is_claimable(&p));
        assert!(claim(&p, "x").unwrap().is_none());
    }

    #[test]
    fn proxied_requests_are_detected() {
        assert!(!is_proxied(&HeaderMap::new()));
        let mut h = HeaderMap::new();
        h.insert("x-forwarded-for", "1.2.3.4".parse().unwrap());
        assert!(is_proxied(&h)); // tunnel traffic: never claimable
    }
}
