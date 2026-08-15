//! Service port allocation and validation.
//!
//! Two exposure modes, one port model:
//!
//!   * **Proxied** (a web site/app): nginx routes the service's domain to it on
//!     `127.0.0.1:<port>`. The port is internal and invisible to the user, so
//!     the platform assigns one and the firewall stays closed.
//!   * **Exposed** (a non-HTTP service — a game server, a database): the service
//!     binds a port that is opened in the firewall. Here the port *is* the
//!     interface, so the user (or their agent) sets it deliberately.
//!
//! Either way every setter — the dashboard, an agent over MCP, the CLI — goes
//! through the same validation here, so an agent can no more set a reserved or
//! colliding port than a person can. Invariants live in the config engine, not
//! the UI.

use std::collections::BTreeMap;
use std::ops::RangeInclusive;

/// Ports the platform itself binds; no service may take them. These are not
/// tracked as services, so they must be listed explicitly.
pub const RESERVED: &[(u16, &str)] = &[
    (22, "SSH"),
    (53, "DNS"),
    (80, "nginx (HTTP reverse proxy)"),
    (443, "nginx (HTTPS)"),
    (2693, "boxd (dashboard, API, MCP)"),
    (5353, "mDNS / avahi"),
    (51820, "WireGuard (Box Connect)"),
    (41641, "Tailscale (Box Connect)"),
];

/// Auto-allocated internal ports come from this high, unprivileged range, clear
/// of the well-known ports and the usual ephemeral range.
pub const AUTO_RANGE: RangeInclusive<u16> = 8000..=8999;

/// Services run unprivileged and can't bind ports below this; 80/443 belong to
/// the proxy, not to a service.
pub const MIN_SERVICE_PORT: u16 = 1024;

#[derive(Debug, PartialEq, Eq)]
pub enum PortError {
    /// Below 1024: privileged, not bindable by an unprivileged service.
    Privileged(u16),
    /// Owned by the platform (see [`RESERVED`]).
    Reserved(u16, &'static str),
    /// Already taken by another service.
    Collision { port: u16, owner: String },
    /// No free port left in [`AUTO_RANGE`].
    Exhausted,
}

impl std::fmt::Display for PortError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            PortError::Privileged(p) => write!(
                f,
                "port {p} is privileged (below {MIN_SERVICE_PORT}); services must use a higher port"
            ),
            PortError::Reserved(p, who) => {
                write!(f, "port {p} is reserved by the platform ({who})")
            }
            PortError::Collision { port, owner } => {
                write!(f, "port {port} is already used by service {owner:?}")
            }
            PortError::Exhausted => write!(
                f,
                "no free port left in {}-{}",
                AUTO_RANGE.start(),
                AUTO_RANGE.end()
            ),
        }
    }
}
impl std::error::Error for PortError {}

fn reserved(port: u16) -> Option<&'static str> {
    RESERVED
        .iter()
        .find(|(p, _)| *p == port)
        .map(|(_, who)| *who)
}

/// Validate an explicit port request. `in_use` maps each taken port to the
/// service that owns it; `self_name` is the service being set, so re-setting a
/// service to the port it already holds is fine.
pub fn validate(
    port: u16,
    in_use: &BTreeMap<u16, String>,
    self_name: &str,
) -> Result<u16, PortError> {
    // Reserved wins over privileged: "reserved by SSH" beats "privileged" for a
    // port that is both (22, 80, 443, 53).
    if let Some(who) = reserved(port) {
        return Err(PortError::Reserved(port, who));
    }
    if port < MIN_SERVICE_PORT {
        return Err(PortError::Privileged(port));
    }
    if let Some(owner) = in_use.get(&port) {
        if owner != self_name {
            return Err(PortError::Collision {
                port,
                owner: owner.clone(),
            });
        }
    }
    Ok(port)
}

/// Allocate the lowest free port in [`AUTO_RANGE`] for a new service.
pub fn allocate(in_use: &BTreeMap<u16, String>) -> Result<u16, PortError> {
    AUTO_RANGE
        .clone()
        .find(|p| !in_use.contains_key(p) && reserved(*p).is_none())
        .ok_or(PortError::Exhausted)
}

/// The single entry point a deploy calls: honor an explicit, validated request,
/// or keep this service's existing allocation (stable across redeploys), or
/// allocate a fresh one.
pub fn resolve(
    requested: Option<u16>,
    in_use: &BTreeMap<u16, String>,
    self_name: &str,
) -> Result<u16, PortError> {
    match requested {
        Some(p) => validate(p, in_use, self_name),
        None => {
            if let Some((&existing, _)) = in_use.iter().find(|(_, owner)| *owner == self_name) {
                return Ok(existing); // don't churn a service's port on redeploy
            }
            allocate(in_use)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn used(pairs: &[(u16, &str)]) -> BTreeMap<u16, String> {
        pairs.iter().map(|(p, s)| (*p, s.to_string())).collect()
    }

    #[test]
    fn rejects_privileged_and_reserved() {
        let none = used(&[]);
        assert_eq!(
            validate(22, &none, "x"),
            Err(PortError::Reserved(22, "SSH"))
        );
        assert_eq!(
            validate(2693, &none, "x"),
            Err(PortError::Reserved(2693, "boxd (dashboard, API, MCP)"))
        );
        assert_eq!(
            validate(80, &none, "x"),
            Err(PortError::Reserved(80, "nginx (HTTP reverse proxy)"))
        );
        assert_eq!(validate(1000, &none, "x"), Err(PortError::Privileged(1000)));
        assert!(validate(8080, &none, "x").is_ok());
    }

    #[test]
    fn rejects_collisions_but_allows_self() {
        let in_use = used(&[(8080, "blog")]);
        assert_eq!(
            validate(8080, &in_use, "shop"),
            Err(PortError::Collision {
                port: 8080,
                owner: "blog".into()
            })
        );
        // A service may keep its own port.
        assert_eq!(validate(8080, &in_use, "blog"), Ok(8080));
    }

    #[test]
    fn allocates_lowest_free() {
        assert_eq!(allocate(&used(&[])).unwrap(), 8000);
        assert_eq!(allocate(&used(&[(8000, "a"), (8001, "b")])).unwrap(), 8002);
    }

    #[test]
    fn resolve_keeps_stable_then_allocates() {
        // Explicit request is honored (and validated).
        assert_eq!(resolve(Some(9000), &used(&[]), "app"), Ok(9000));
        assert!(resolve(Some(80), &used(&[]), "app").is_err());
        // No request + existing allocation -> keep it stable.
        let in_use = used(&[(8000, "app"), (8001, "other")]);
        assert_eq!(resolve(None, &in_use, "app"), Ok(8000));
        // No request + new service -> next free.
        assert_eq!(resolve(None, &in_use, "fresh"), Ok(8002));
    }

    #[test]
    fn exhaustion_is_reported() {
        let full: BTreeMap<u16, String> = AUTO_RANGE.map(|p| (p, format!("s{p}"))).collect();
        assert_eq!(allocate(&full), Err(PortError::Exhausted));
    }
}
