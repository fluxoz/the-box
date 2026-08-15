//! The Cloudflare calls needed to set up a tunnel from nothing.
//!
//! This exists so that "put my site on my domain" can be one request from an
//! agent instead of six steps in someone else's dashboard. The credential stays
//! HERE, in the Box's encrypted secret store: an agent asks the Box to set
//! ingress up and never sees the token. That matters more than the convenience
//! — a Cloudflare API token can rewrite DNS for every domain in the account, and
//! an agent that also builds code from a repo it did not write is exactly the
//! thing that should not be holding it.
//!
//! Request construction is pure and tested; only `call` touches the network,
//! shelling out to curl as the rest of the daemon does rather than pulling in an
//! HTTP client.

use anyhow::{bail, Context, Result};
use serde_json::{json, Value};

const API: &str = "https://api.cloudflare.com/client/v4";

/// The permissions this needs, as a Cloudflare token-template URL. Handing
/// someone a link with the boxes already ticked is the difference between "make
/// a token" and "understand Cloudflare's permission model first".
pub const TOKEN_TEMPLATE_URL: &str =
    "https://dash.cloudflare.com/profile/api-tokens?permissionGroupKeys=%5B%7B%22key%22%3A%22argo_tunnel%22%2C%22type%22%3A%22edit%22%7D%2C%7B%22key%22%3A%22dns%22%2C%22type%22%3A%22edit%22%7D%2C%7B%22key%22%3A%22zone%22%2C%22type%22%3A%22read%22%7D%5D&name=The+Box";

pub const API_TOKEN_SECRET: &str = "cloudflare-api-token";

/// One request, before it is sent. Split out so the shape can be tested without
/// a Cloudflare account.
#[derive(Debug, Clone, PartialEq)]
pub struct Request {
    pub method: &'static str,
    pub url: String,
    pub body: Option<Value>,
}

pub fn verify_token() -> Request {
    Request {
        method: "GET",
        // NOT /user/tokens/verify: that endpoint only answers for user-owned
        // tokens, and Cloudflare's dashboard now mints ACCOUNT-owned tokens
        // (cfat_…) by default — a working token was refused as "Invalid API
        // Token" on the first real BYO-domain run. Listing zones proves the
        // token works AND that it has the Zone:Read this Box actually needs,
        // for both token kinds.
        url: format!("{API}/zones?per_page=1"),
        body: None,
    }
}

/// Find a zone by name. Also tells us the account it belongs to, so the
/// operator never has to find an account id.
pub fn find_zone(zone: &str) -> Request {
    Request {
        method: "GET",
        url: format!("{API}/zones?name={}", urlencoding::encode(zone)),
        body: None,
    }
}

pub fn create_tunnel(account_id: &str, name: &str) -> Request {
    Request {
        method: "POST",
        url: format!("{API}/accounts/{account_id}/cfd_tunnel"),
        // `cloudflare` means the ingress rules live at the edge and we manage
        // them through this API. The alternative puts them in a local config
        // file that the edge then overrides anyway.
        body: Some(json!({ "name": name, "config_src": "cloudflare" })),
    }
}

pub fn tunnel_run_token(account_id: &str, tunnel_id: &str) -> Request {
    Request {
        method: "GET",
        url: format!("{API}/accounts/{account_id}/cfd_tunnel/{tunnel_id}/token"),
        body: None,
    }
}

/// Point everything at the Box's public listener.
///
/// One wildcard rule, so that adding a service later costs nothing here: the
/// Box already decides which hostnames it answers for, and a service that is
/// not published is simply not served (see nix/module.nix). The trailing
/// catch-all is required — Cloudflare rejects a rule set whose last entry has a
/// hostname.
pub fn put_ingress(account_id: &str, tunnel_id: &str, zone: &str, port: u16) -> Request {
    Request {
        method: "PUT",
        url: format!("{API}/accounts/{account_id}/cfd_tunnel/{tunnel_id}/configurations"),
        body: Some(json!({
            "config": {
                "ingress": [
                    { "hostname": format!("*.{zone}"), "service": format!("http://127.0.0.1:{port}") },
                    { "hostname": zone, "service": format!("http://127.0.0.1:{port}") },
                    { "service": "http_status:404" },
                ]
            }
        })),
    }
}

/// The DNS record that sends a hostname into the tunnel. Proxied, because an
/// unproxied record would publish the Box's own address, which is the thing
/// this whole design avoids.
pub fn dns_record(zone_id: &str, name: &str, tunnel_id: &str) -> Request {
    Request {
        method: "POST",
        url: format!("{API}/zones/{zone_id}/dns_records"),
        body: Some(json!({
            "type": "CNAME",
            "name": name,
            "content": format!("{tunnel_id}.cfargotunnel.com"),
            "proxied": true,
            "comment": "created by The Box",
        })),
    }
}

/// The parent-token ceremony (the self-minting design): the person grants ONE
/// token that can create tokens, and the Box mints the exactly-scoped child it
/// actually uses. Every failure class from the first live run — wrong token
/// kind, a missed checkbox, surprise expiry — becomes impossible, because no
/// human ever assembles the working token again.
pub const PARENT_TOKEN_SECRET: &str = "cloudflare-parent-token";

/// Dashboard template for the parent: one permission, "API Tokens: Edit".
pub const PARENT_TOKEN_URL: &str =
    "https://dash.cloudflare.com/profile/api-tokens?permissionGroupKeys=%5B%7B%22key%22%3A%22api_tokens%22%2C%22type%22%3A%22edit%22%7D%5D&name=The+Box+%28parent%29";

/// The permission groups a Box needs, by the names Cloudflare lists them
/// under. Tunnel has carried two names across their renames; match either.
pub const NEEDED_GROUPS: [(&str, &[&str]); 3] = [
    ("tunnel", &["Argo Tunnel Write", "Cloudflare Tunnel Write"]),
    ("dns", &["DNS Write"]),
    ("zone", &["Zone Read"]),
];

pub fn list_permission_groups() -> Request {
    Request {
        method: "GET",
        url: format!("{API}/user/tokens/permission_groups"),
        body: None,
    }
}

/// Pick the ids this Box needs out of the permission-group listing. Returns
/// (tunnel, dns, zone) or names what is missing — if Cloudflare renames a
/// group, the error says exactly which one to add to NEEDED_GROUPS.
pub fn needed_group_ids(groups: &Value) -> Result<(String, String, String)> {
    let list = groups
        .get("result")
        .and_then(Value::as_array)
        .context("permission-group listing had no result array")?;
    let find = |names: &[&str]| {
        list.iter().find_map(|g| {
            let name = g.get("name").and_then(Value::as_str)?;
            names
                .iter()
                .any(|n| n.eq_ignore_ascii_case(name))
                .then(|| g.get("id").and_then(Value::as_str).map(str::to_string))?
        })
    };
    let mut found = Vec::new();
    for (key, names) in NEEDED_GROUPS {
        match find(names) {
            Some(id) => found.push(id),
            None => bail!(
                "Cloudflare's permission-group listing has nothing named {names:?} \
                 (needed for {key}) — they may have renamed it"
            ),
        }
    }
    Ok((found[0].clone(), found[1].clone(), found[2].clone()))
}

/// Create the child token: tunnel writes on every account the parent can
/// reach, DNS writes + zone reads on every zone. No expiry — the child lives
/// until the parent's owner revokes it, and nothing silently dies on a date.
pub fn create_child_token(
    name: &str,
    tunnel_group: &str,
    dns_group: &str,
    zone_group: &str,
) -> Request {
    Request {
        method: "POST",
        url: format!("{API}/user/tokens"),
        body: Some(json!({
            "name": name,
            "policies": [
                {
                    "effect": "allow",
                    "resources": { "com.cloudflare.api.account.*": "*" },
                    "permission_groups": [ { "id": tunnel_group } ],
                },
                {
                    "effect": "allow",
                    "resources": { "com.cloudflare.api.account.zone.*": "*" },
                    "permission_groups": [ { "id": dns_group }, { "id": zone_group } ],
                },
            ],
        })),
    }
}

/// Send it, and unwrap Cloudflare's envelope.
///
/// Their errors are the useful part — "this token cannot edit DNS for that
/// zone" is what the operator needs to read, so it is passed through rather
/// than flattened into "request failed".
pub fn call(token: &str, req: &Request) -> Result<Value> {
    let mut cmd = std::process::Command::new("curl");
    cmd.args(["-sS", "-m", "30", "-X", req.method])
        .arg("-H")
        .arg(format!("Authorization: Bearer {token}"))
        .arg("-H")
        .arg("Content-Type: application/json");
    if let Some(body) = &req.body {
        cmd.arg("-d").arg(body.to_string());
    }
    let out = cmd
        .arg(&req.url)
        .output()
        .context("running curl against the Cloudflare API")?;
    if !out.status.success() {
        bail!(
            "could not reach the Cloudflare API: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    let value: Value = serde_json::from_slice(&out.stdout)
        .context("the Cloudflare API returned something that is not JSON")?;
    if value.get("success").and_then(Value::as_bool) != Some(true) {
        bail!("Cloudflare refused: {}", describe_errors(&value));
    }
    Ok(value)
}

fn describe_errors(value: &Value) -> String {
    let errors = value
        .get("errors")
        .and_then(Value::as_array)
        .map(|es| {
            es.iter()
                .filter_map(|e| e.get("message").and_then(Value::as_str))
                .collect::<Vec<_>>()
                .join("; ")
        })
        .unwrap_or_default();
    if errors.is_empty() {
        value.to_string()
    } else {
        errors
    }
}

/// What a zone lookup told us.
pub struct Zone {
    pub id: String,
    pub account_id: String,
    /// Cloudflare's own word: only an `active` zone actually serves.
    pub status: String,
}

pub fn lookup_zone(token: &str, zone: &str) -> Result<Zone> {
    let value = call(token, &find_zone(zone))?;
    let first = value
        .get("result")
        .and_then(Value::as_array)
        .and_then(|r| r.first())
        .with_context(|| {
            format!(
                "{zone:?} is not on this Cloudflare account. Add it there first \
                 (Websites → Add a site), which means changing its nameservers at \
                 the registrar you bought it from — that step is the one nobody can \
                 do for you."
            )
        })?;
    Ok(Zone {
        id: first
            .get("id")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string(),
        account_id: first
            .get("account")
            .and_then(|a| a.get("id"))
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string(),
        status: first
            .get("status")
            .and_then(Value::as_str)
            .unwrap_or("unknown")
            .to_string(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_child_token_is_scoped_and_never_expires() {
        let r = create_child_token("The Box (box)", "tid", "did", "zid");
        assert_eq!(r.method, "POST");
        assert!(r.url.ends_with("/user/tokens"));
        let body = r.body.unwrap();
        // No expiry: nothing may silently die on a date (the first live token
        // did, eight days out, and nobody had chosen that).
        assert!(body.get("expires_on").is_none());
        let policies = body["policies"].as_array().unwrap();
        assert_eq!(policies.len(), 2);
        // Tunnel writes are account-scoped; DNS write + zone read are
        // zone-scoped. Wrong resource kinds make Cloudflare refuse the mint.
        assert!(policies[0]["resources"]
            .get("com.cloudflare.api.account.*")
            .is_some());
        assert_eq!(policies[0]["permission_groups"][0]["id"], "tid");
        assert!(policies[1]["resources"]
            .get("com.cloudflare.api.account.zone.*")
            .is_some());
        let zone_groups = policies[1]["permission_groups"].as_array().unwrap();
        assert_eq!(zone_groups.len(), 2);
    }

    #[test]
    fn permission_groups_are_found_by_any_of_their_names() {
        let listing = json!({ "result": [
            { "id": "g1", "name": "Cloudflare Tunnel Write" },
            { "id": "g2", "name": "DNS Write" },
            { "id": "g3", "name": "Zone Read" },
            { "id": "g4", "name": "Workers R2 Storage Write" },
        ]});
        let (t, d, z) = needed_group_ids(&listing).unwrap();
        assert_eq!((t.as_str(), d.as_str(), z.as_str()), ("g1", "g2", "g3"));

        // The tunnel group's older name still matches.
        let listing = json!({ "result": [
            { "id": "old", "name": "Argo Tunnel Write" },
            { "id": "g2", "name": "DNS Write" },
            { "id": "g3", "name": "Zone Read" },
        ]});
        assert_eq!(needed_group_ids(&listing).unwrap().0, "old");

        // A missing group names itself rather than failing generically.
        let listing = json!({ "result": [ { "id": "g2", "name": "DNS Write" } ]});
        let err = format!("{:#}", needed_group_ids(&listing).unwrap_err());
        assert!(err.contains("Tunnel"), "{err}");
    }

    #[test]
    fn requests_target_the_documented_endpoints() {
        assert_eq!(
            find_zone("example.com").url,
            format!("{API}/zones?name=example.com")
        );
        // A zone name with a dot is fine; anything needing escaping is escaped.
        assert!(find_zone("a b.com").url.ends_with("a%20b.com"));

        let t = create_tunnel("acct", "box");
        assert_eq!(t.method, "POST");
        assert!(t.url.ends_with("/accounts/acct/cfd_tunnel"));
        // Edge-managed, so the ingress we PUT below is authoritative.
        assert_eq!(t.body.unwrap()["config_src"], "cloudflare");

        assert!(tunnel_run_token("acct", "tid")
            .url
            .ends_with("/cfd_tunnel/tid/token"));
    }

    #[test]
    fn ingress_is_a_wildcard_plus_a_required_catch_all() {
        let r = put_ingress("acct", "tid", "example.com", 2694);
        assert_eq!(r.method, "PUT");
        let rules = r.body.unwrap()["config"]["ingress"].clone();
        let rules = rules.as_array().unwrap().clone();

        // Everything under the domain, and the bare domain, reach the Box's
        // PUBLIC listener — never the console's port.
        assert_eq!(rules[0]["hostname"], "*.example.com");
        assert_eq!(rules[0]["service"], "http://127.0.0.1:2694");
        assert_eq!(rules[1]["hostname"], "example.com");

        // Cloudflare rejects a rule set whose last entry names a hostname.
        let last = rules.last().unwrap();
        assert!(
            last.get("hostname").is_none(),
            "last rule must be the catch-all"
        );
        assert_eq!(last["service"], "http_status:404");
    }

    #[test]
    fn dns_points_into_the_tunnel_and_stays_proxied() {
        let r = dns_record("zid", "*", "tid");
        let b = r.body.unwrap();
        assert_eq!(b["type"], "CNAME");
        assert_eq!(b["content"], "tid.cfargotunnel.com");
        // Unproxied would publish the Box's own address, which is the whole
        // thing a tunnel exists to avoid.
        assert_eq!(b["proxied"], true);
    }

    #[test]
    fn cloudflares_own_error_text_survives() {
        let refused = json!({
            "success": false,
            "errors": [{ "code": 10000, "message": "Authentication error" }]
        });
        assert_eq!(describe_errors(&refused), "Authentication error");
        // And an envelope with no message still says something.
        assert!(!describe_errors(&json!({ "success": false })).is_empty());
    }
}
