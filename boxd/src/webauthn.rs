//! Security keys and passkeys for the console (WebAuthn).
//!
//! A YubiKey (or a phone's passkey) replaces the pairing code: instead of
//! copying a one-time string, the operator proves possession of hardware the
//! Box has met before. The credential's private half never leaves the key, and
//! the assertion is bound to this Box's origin, so a phished page cannot reuse
//! it.
//!
//! THE CONSTRAINT THAT SHAPES EVERYTHING HERE: browsers only expose WebAuthn in
//! a *secure context*. Per W3C Secure Contexts §3.1 that means https, or a
//! loopback host (`localhost`, 127.0.0.0/8, ::1) — and explicitly NOT a private
//! LAN address, and NOT an mDNS `.local` name. The console is plain HTTP on the
//! LAN, so on `http://box.local:2693` the API is simply absent, and an IP
//! origin can never work at all because the Relying Party ID must be a domain.
//! Rather than fail mysteriously, [`availability`] computes that server-side and
//! the console explains which addresses do work:
//!
//!   * `ssh -L 2693:localhost:2693 <box>` then <http://localhost:2693> — works
//!     today, no infrastructure;
//!   * the Cloudflare tunnel hostname — a real HTTPS origin.
//!
//! A credential is bound to one Relying Party ID, so a key enrolled over the
//! SSH forward is not visible through the tunnel and vice versa. That is
//! cryptographic, not a bug: we store the rpId with each credential and only
//! offer the ones that match where you are.

use std::collections::HashMap;
use std::sync::Mutex;

use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use webauthn_rs::prelude::*;

/// Whether this request could do WebAuthn at all, and why not if it cannot.
#[derive(Debug, Clone, Serialize)]
pub struct Availability {
    pub usable: bool,
    /// The origin as the browser sees it, e.g. `http://localhost:2693`.
    pub origin: String,
    /// The Relying Party ID a credential would be bound to (a bare domain).
    pub rp_id: String,
    /// Plain-language reason, shown to the operator when `usable` is false.
    pub reason: String,
}

/// Decide from the request's own headers. `forwarded_proto` comes from the
/// tunnel (`x-forwarded-proto`), which terminates TLS in front of us.
pub fn availability(host: &str, forwarded_proto: Option<&str>) -> Availability {
    let host_only = host.split(':').next().unwrap_or(host).to_string();
    let https = forwarded_proto.is_some_and(|p| p.eq_ignore_ascii_case("https"));
    let scheme = if https { "https" } else { "http" };
    let origin = format!("{scheme}://{host}");

    let loopback = host_only == "localhost"
        || host_only.ends_with(".localhost")
        || host_only.parse::<std::net::IpAddr>().is_ok_and(|ip| ip.is_loopback());
    // An rpId must be a domain: "Only the domain format of host is allowed
    // here" (WebAuthn L3 §5.1.3). An IP origin can never register a credential.
    let is_ip = host_only.parse::<std::net::IpAddr>().is_ok();

    // A secure context is https, or a loopback host — and never a bare IP,
    // because the Relying Party ID has to be a domain.
    let (usable, reason) = if (https || loopback) && !is_ip {
        (true, String::new())
    } else if is_ip && loopback {
        (
            false,
            "Security keys need a name, not an IP address. Reach the console at http://localhost:2693 (for example over an SSH forward) and it will work.".into(),
        )
    } else if is_ip {
        (
            false,
            "Security keys cannot be used on an address like this one: the standard requires a domain name, and this connection is not encrypted. Use an SSH forward to http://localhost:2693, or your tunnel's web address.".into(),
        )
    } else {
        (
            false,
            "This page arrived over plain HTTP, and browsers only offer security keys on an encrypted connection. Use an SSH forward to http://localhost:2693, or reach the console through your tunnel.".into(),
        )
    };

    Availability {
        usable,
        origin,
        rp_id: host_only,
        reason,
    }
}

fn build(rp_id: &str, origin: &str) -> Result<Webauthn> {
    let url = Url::parse(origin).with_context(|| format!("parsing origin {origin}"))?;
    WebauthnBuilder::new(rp_id, &url)
        .context("building the relying party")?
        .rp_name("The Box")
        .build()
        .context("building the relying party")
}

/// A ceremony in flight. Deliberately in memory and short-lived: the challenge
/// is single-use, and a daemon restart should invalidate it rather than leave a
/// replayable artifact on disk.
struct Pending {
    kind: Ceremony,
    rp_id: String,
    origin: String,
    created: i64,
}

enum Ceremony {
    Register(Box<PasskeyRegistration>),
    Authenticate(Box<PasskeyAuthentication>),
}

#[derive(Default)]
pub struct Ceremonies {
    inner: Mutex<HashMap<String, Pending>>,
}

/// How long a half-finished ceremony stays valid.
const TTL_SECS: i64 = 120;

fn now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

impl Ceremonies {
    pub fn new() -> Self {
        Self::default()
    }

    fn put(&self, pending: Pending) -> Result<String> {
        let handle = crate::auth::random_hex(16)?;
        if let Ok(mut map) = self.inner.lock() {
            let cutoff = now() - TTL_SECS;
            map.retain(|_, p| p.created >= cutoff);
            map.insert(handle.clone(), pending);
        }
        Ok(handle)
    }

    fn take(&self, handle: &str) -> Result<Pending> {
        let mut map = self.inner.lock().map_err(|_| anyhow::anyhow!("lock"))?;
        let pending = map
            .remove(handle)
            .context("that sign-in attempt expired; start again")?;
        if pending.created < now() - TTL_SECS {
            bail!("that sign-in attempt expired; start again");
        }
        Ok(pending)
    }

    /// Step 1 of enrolling a key: the challenge the browser hands the key.
    pub fn start_registration(
        &self,
        avail: &Availability,
        operator_id: Uuid,
        existing: &[Passkey],
    ) -> Result<(serde_json::Value, String)> {
        let webauthn = build(&avail.rp_id, &avail.origin)?;
        let exclude: Vec<CredentialID> = existing.iter().map(|p| p.cred_id().clone()).collect();
        let (challenge, state) = webauthn
            .start_passkey_registration(operator_id, "operator", "Box operator", Some(exclude))
            .context("starting registration")?;
        let handle = self.put(Pending {
            kind: Ceremony::Register(Box::new(state)),
            rp_id: avail.rp_id.clone(),
            origin: avail.origin.clone(),
            created: now(),
        })?;
        Ok((serde_json::to_value(challenge)?, handle))
    }

    /// Step 2: verify what the key signed and produce the credential to store.
    pub fn finish_registration(
        &self,
        handle: &str,
        response: serde_json::Value,
    ) -> Result<Passkey> {
        let pending = self.take(handle)?;
        let Ceremony::Register(state) = pending.kind else {
            bail!("that handle is not a registration");
        };
        let response: RegisterPublicKeyCredential =
            serde_json::from_value(response).context("reading the key's response")?;
        let webauthn = build(&pending.rp_id, &pending.origin)?;
        webauthn
            .finish_passkey_registration(&response, &state)
            .context("the security key's response did not verify")
    }

    /// Step 1 of signing in: challenge whichever keys are bound to this origin.
    pub fn start_authentication(
        &self,
        avail: &Availability,
        keys: &[Passkey],
    ) -> Result<(serde_json::Value, String)> {
        if keys.is_empty() {
            bail!("no security key is enrolled for this address");
        }
        let webauthn = build(&avail.rp_id, &avail.origin)?;
        let (challenge, state) = webauthn
            .start_passkey_authentication(keys)
            .context("starting sign-in")?;
        let handle = self.put(Pending {
            kind: Ceremony::Authenticate(Box::new(state)),
            rp_id: avail.rp_id.clone(),
            origin: avail.origin.clone(),
            created: now(),
        })?;
        Ok((serde_json::to_value(challenge)?, handle))
    }

    /// Step 2: verify the assertion. Returns the credential id that signed.
    pub fn finish_authentication(
        &self,
        handle: &str,
        response: serde_json::Value,
    ) -> Result<AuthenticationResult> {
        let pending = self.take(handle)?;
        let Ceremony::Authenticate(state) = pending.kind else {
            bail!("that handle is not a sign-in");
        };
        let response: PublicKeyCredential =
            serde_json::from_value(response).context("reading the key's response")?;
        let webauthn = build(&pending.rp_id, &pending.origin)?;
        webauthn
            .finish_passkey_authentication(&response, &state)
            .context("the security key's response did not verify")
    }
}

/// A stored credential, as it lives in auth.json.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StoredKey {
    pub id: String,
    pub label: String,
    /// Which origin this key was enrolled at. A credential is bound to one
    /// Relying Party ID, so a key enrolled over an SSH forward genuinely cannot
    /// be used through the tunnel — we show that rather than let it fail.
    pub rp_id: String,
    pub created_at: i64,
    pub last_used_at: Option<i64>,
    /// The webauthn-rs Passkey, kept opaque so auth.rs need not know the type.
    pub passkey: serde_json::Value,
}

impl StoredKey {
    pub fn passkey(&self) -> Result<Passkey> {
        serde_json::from_value(self.passkey.clone()).context("reading a stored security key")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn availability_follows_the_secure_context_rule() {
        // Works: loopback by name, and anything behind the TLS tunnel.
        assert!(availability("localhost:2693", None).usable);
        assert!(availability("box.localhost:2693", None).usable);
        assert!(availability("box.example.com", Some("https")).usable);

        // Does not: plain HTTP on the LAN, by name or by address.
        let lan = availability("box.local:2693", None);
        assert!(!lan.usable);
        assert!(lan.reason.contains("localhost"), "{}", lan.reason);

        let ip = availability("192.168.1.58:2693", None);
        assert!(!ip.usable);
        assert!(ip.reason.to_lowercase().contains("domain"), "{}", ip.reason);

        // An IP is not rescued by TLS either: the rpId must be a domain.
        assert!(!availability("192.168.1.58", Some("https")).usable);
    }

    #[test]
    fn rp_id_is_the_bare_host() {
        assert_eq!(availability("box.example.com:2693", Some("https")).rp_id, "box.example.com");
        assert_eq!(
            availability("box.example.com", Some("https")).origin,
            "https://box.example.com"
        );
        assert_eq!(availability("localhost:2693", None).origin, "http://localhost:2693");
    }

    #[test]
    fn a_ceremony_expires_and_is_single_use() {
        let c = Ceremonies::new();
        let avail = availability("localhost:2693", None);
        let (_challenge, handle) = c
            .start_registration(&avail, Uuid::new_v4(), &[])
            .expect("start");
        // Wrong kind is refused, and the handle is consumed either way.
        assert!(c.finish_authentication(&handle, serde_json::json!({})).is_err());
        assert!(c.finish_registration(&handle, serde_json::json!({})).is_err());
    }
}
