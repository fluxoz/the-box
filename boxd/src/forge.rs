//! Connecting a Box to wherever the person's code already lives.
//!
//! The competing products all do this the same way: the host is reachable from
//! the internet, so the forge calls IT with a webhook. A Box starts life behind
//! a home router with nothing open, which would mean standing up a tunnel
//! before you could deploy at all. So the direction is reversed here — the Box
//! asks the forge what changed. That costs a poll interval and buys a first
//! deploy that needs no ingress whatsoever. Once the ladder in [`crate::ingress`]
//! is up, a webhook is a strict improvement and the Box can register one itself.
//!
//! Authentication is the OAuth **device flow** (RFC 8628), for the same reason:
//! it is the only grant that needs no callback URL, which is the only kind that
//! works on a machine with no public address. It also has no client secret,
//! so nothing confidential ships in the OS image.
//!
//! What the person does is read a code off the screen and type it into a page
//! they are already logged into. What they do NOT do is learn a permission
//! model — on GitHub they tick the repositories they want on GitHub's own
//! screen, and the token the Box ends up holding reaches those and nothing
//! else. That is the whole reason for preferring an app over a personal access
//! token, whose repository scope is all-or-nothing across everything they own.
//!
//! Request construction and response parsing are pure and tested; only `send`
//! touches the network, shelling out to curl as the rest of the daemon does.

use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::config::ForgeConfig;
use crate::paths::Paths;

/// Every forge a Box can pull code from.
pub fn forges() -> &'static [&'static dyn Forge] {
    &[&crate::ghapi::GitHub, &crate::glapi::GitLab]
}

pub fn get(id: &str) -> Option<&'static dyn Forge> {
    forges().iter().copied().find(|f| f.id() == id)
}

/// Where the connected account's token is kept. One connection per forge for
/// now; encrypted at rest like every other operational credential, and never
/// readable back through the API.
pub fn token_secret(id: &str) -> String {
    format!("forge-{id}-token")
}

/// How this Box reaches a forge. A Box with nothing in box.toml still gets a
/// usable config, because a shipped registration may be all it needs.
pub fn config_for(paths: &Paths, id: &str) -> Result<ForgeConfig> {
    let cfg = crate::config::BoxConfig::load(paths)?;
    Ok(cfg
        .forges
        .into_iter()
        .find(|f| f.provider == id)
        .unwrap_or_else(|| ForgeConfig::new(id)))
}

/// Record how to reach a forge, merging into whatever is already there so that
/// supplying one field later does not silently erase another.
pub fn configure(
    paths: &Paths,
    id: &str,
    base_url: Option<String>,
    client_id: Option<String>,
    app_slug: Option<String>,
) -> Result<ForgeConfig> {
    if get(id).is_none() {
        bail!("unknown forge {id:?}");
    }
    let mut cfg = crate::config::BoxConfig::load(paths)?;
    let entry = match cfg.forges.iter_mut().find(|f| f.provider == id) {
        Some(e) => e,
        None => {
            cfg.forges.push(ForgeConfig::new(id));
            cfg.forges.last_mut().expect("just pushed")
        }
    };
    if base_url.is_some() {
        entry.base_url = base_url;
    }
    if client_id.is_some() {
        entry.client_id = client_id;
    }
    if app_slug.is_some() {
        entry.app_slug = app_slug;
    }
    let out = entry.clone();
    cfg.save(paths)?;
    Ok(out)
}

/// The in-flight device authorization, between "we showed them a code" and
/// "they typed it in". The device code is itself a bearer of the pending grant
/// — whoever holds it collects the token — so it goes in the encrypted store
/// rather than a scratch file, short-lived though it is.
fn pending_secret(id: &str) -> String {
    format!("forge-{id}-pending")
}

// ---------------------------------------------------------------------------

/// What a person must do themselves before a Box can talk to this forge, and
/// whether it is a one-off for the whole product or something each operator
/// repeats. The two forges genuinely differ here and the console should say so.
#[derive(Debug, Clone, Copy, Serialize)]
pub struct Requirements {
    /// The operator has to register an OAuth application before anything works.
    /// False where a registration ships with the product.
    pub needs_own_app: bool,
    /// This forge can be self-hosted, so the Box needs to be told where it is.
    pub needs_base_url: bool,
    /// The person picks which repositories to share, on the forge's own screen,
    /// rather than handing over everything they own.
    pub per_repo_consent: bool,
}

/// The OAuth endpoints and client identity for one forge on one host. Resolved
/// per-connection because a self-hosted forge has its own address and its own
/// application registration.
#[derive(Debug, Clone, PartialEq)]
pub struct Endpoints {
    pub device_code_url: String,
    pub token_url: String,
    pub client_id: String,
    /// GitHub Apps take their permissions from the app registration and the
    /// installation, so they send none. GitLab requires them on the request.
    pub scope: Option<&'static str>,
}

/// One repository, as much of it as every forge agrees on.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Repo {
    /// `owner/name`, which is what a person recognises and what both forges
    /// will accept back.
    pub full_name: String,
    pub default_branch: String,
    pub private: bool,
    /// Fetchable over HTTPS. The credential is added at clone time and is not
    /// stored here — a URL with a token in it would end up in logs.
    pub clone_url: String,
}

pub trait Forge: Send + Sync {
    fn id(&self) -> &'static str;
    fn title(&self) -> &'static str;
    /// One sentence, for a person choosing between these.
    fn description(&self) -> &'static str;
    fn requirements(&self) -> Requirements;
    /// What is left for a human, in order. Empty means nothing.
    fn steps(&self) -> Vec<&'static str> {
        Vec::new()
    }
    /// Everything needed to start a device flow — or a readable refusal naming
    /// exactly what is missing, because "unauthorized" is not a diagnosis.
    fn endpoints(&self, cfg: &ForgeConfig) -> Result<Endpoints>;
    /// The repositories this token can actually reach.
    fn list_repos(&self, token: &str, cfg: &ForgeConfig) -> Result<Vec<Repo>>;
    /// Where to send someone whose token works but who shared no repositories.
    /// On an app-based forge this is the ordinary case on first connect, not an
    /// error, and the fix is one click on the forge's own screen.
    fn grant_more_url(&self, cfg: &ForgeConfig) -> Option<String>;
}

// ---------------------------------------------------------------------------
// The device flow itself, shared because both forges implement the same RFC.

/// One request, before it is sent. Form-encoded rather than JSON: it is what
/// RFC 8628 specifies and what both forges accept.
#[derive(Debug, Clone, PartialEq)]
pub struct Request {
    pub url: String,
    pub form: Vec<(String, String)>,
}

/// Ask the forge for a code to show the person.
pub fn device_code_request(ep: &Endpoints) -> Request {
    let mut form = vec![("client_id".into(), ep.client_id.clone())];
    if let Some(scope) = ep.scope {
        form.push(("scope".into(), scope.into()));
    }
    Request {
        url: ep.device_code_url.clone(),
        form,
    }
}

/// Ask whether they have typed it in yet.
pub fn token_request(ep: &Endpoints, device_code: &str) -> Request {
    Request {
        url: ep.token_url.clone(),
        form: vec![
            ("client_id".into(), ep.client_id.clone()),
            ("device_code".into(), device_code.into()),
            (
                "grant_type".into(),
                "urn:ietf:params:oauth:grant-type:device_code".into(),
            ),
        ],
    }
}

/// A device authorization waiting on a human.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Pending {
    pub device_code: String,
    /// Short, and the only part the person has to handle.
    pub user_code: String,
    pub verification_uri: String,
    /// Seconds to wait between polls. Politeness here is enforced: poll faster
    /// and the forge starts answering `slow_down`.
    pub interval: u64,
    /// Unix seconds. Both forges expire these in about fifteen minutes, which
    /// is long enough to find your phone and short enough to matter.
    pub expires_at: u64,
}

pub fn parse_pending(v: &Value, now: u64) -> Result<Pending> {
    let s = |k: &str| v.get(k).and_then(Value::as_str).map(str::to_string);
    // GitLab sends expires_in and interval as numbers, GitHub as numbers too,
    // but a JSON body that came back form-decoded would make them strings.
    let n = |k: &str, d: u64| {
        v.get(k)
            .and_then(|x| x.as_u64().or_else(|| x.as_str().and_then(|s| s.parse().ok())))
            .unwrap_or(d)
    };
    if let Some(err) = describe_oauth_error(v) {
        bail!("{err}");
    }
    Ok(Pending {
        device_code: s("device_code").context("no device_code in the response")?,
        user_code: s("user_code").context("no user_code in the response")?,
        verification_uri: s("verification_uri")
            .or_else(|| s("verification_uri_complete"))
            .context("no verification_uri in the response")?,
        // RFC 8628 says 5 seconds when the server does not say.
        interval: n("interval", 5).max(1),
        expires_at: now + n("expires_in", 900),
    })
}

/// What one poll told us.
#[derive(Debug, Clone, PartialEq)]
pub enum Outcome {
    /// They finished. This is the access token.
    Token(String),
    /// They have not typed the code in yet.
    Waiting,
    /// We polled too fast; wait longer from now on.
    SlowDown,
    /// They pressed cancel.
    Denied,
    /// The code aged out before they used it.
    Expired,
    /// The forge refused in a way we should show verbatim rather than guess at.
    Refused(String),
}

pub fn parse_token(v: &Value) -> Outcome {
    if let Some(token) = v.get("access_token").and_then(Value::as_str) {
        return Outcome::Token(token.to_string());
    }
    match v.get("error").and_then(Value::as_str) {
        Some("authorization_pending") => Outcome::Waiting,
        Some("slow_down") => Outcome::SlowDown,
        Some("access_denied") => Outcome::Denied,
        Some("expired_token") => Outcome::Expired,
        // The one worth translating: it means the app registration is missing a
        // checkbox, and no amount of retrying will fix it.
        Some("device_flow_disabled") => Outcome::Refused(
            "this forge's application does not have the device flow enabled — \
             tick it in the application's settings"
                .into(),
        ),
        _ => Outcome::Refused(
            describe_oauth_error(v).unwrap_or_else(|| "the forge sent no token and no error".into()),
        ),
    }
}

fn describe_oauth_error(v: &Value) -> Option<String> {
    let code = v.get("error").and_then(Value::as_str)?;
    let described = v
        .get("error_description")
        .and_then(Value::as_str)
        .unwrap_or(code);
    Some(described.to_string())
}

/// Send a form-encoded request and read JSON back.
///
/// The `Accept` header is load-bearing on GitHub: without it the token endpoint
/// answers with a URL-encoded body, which parses as nothing and looks exactly
/// like an outage.
pub fn send(req: &Request) -> Result<Value> {
    let body = req
        .form
        .iter()
        .map(|(k, v)| format!("{}={}", urlencoding::encode(k), urlencoding::encode(v)))
        .collect::<Vec<_>>()
        .join("&");
    let out = std::process::Command::new("curl")
        .args(["-sS", "-m", "30", "-X", "POST"])
        .args(["-H", "Accept: application/json"])
        .args(["-H", "Content-Type: application/x-www-form-urlencoded"])
        .arg("-d")
        .arg(body)
        .arg(&req.url)
        .output()
        .context("running curl against the forge")?;
    if !out.status.success() {
        bail!(
            "could not reach {}: {}",
            req.url,
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    serde_json::from_slice(&out.stdout).with_context(|| {
        format!(
            "{} returned something that is not JSON: {}",
            req.url,
            String::from_utf8_lossy(&out.stdout).chars().take(200).collect::<String>()
        )
    })
}

/// An authenticated GET against a forge's REST API, returning parsed JSON and
/// the `Link` header, which is how both of them paginate.
pub fn get_json(url: &str, token: &str) -> Result<(Value, Option<String>)> {
    let out = std::process::Command::new("curl")
        .args(["-sS", "-m", "30", "-D", "-"])
        .args(["-H", "Accept: application/json"])
        // GitHub answers 403 to an API request with no User-Agent, and curl's
        // default one is not something to depend on.
        .args(["-H", "User-Agent: the-box"])
        .arg("-H")
        .arg(format!("Authorization: Bearer {token}"))
        .arg(url)
        .output()
        .context("running curl against the forge API")?;
    if !out.status.success() {
        bail!(
            "could not reach {url}: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    let raw = String::from_utf8_lossy(&out.stdout);
    let (headers, body) = split_headers(&raw);
    let value: Value = serde_json::from_str(body.trim())
        .with_context(|| format!("{url} returned something that is not JSON"))?;
    if let Some(message) = api_error(&value) {
        bail!("the forge refused: {message}");
    }
    Ok((value, next_link(&headers)))
}

/// curl with `-D -` writes headers then the body. Response headers can repeat
/// (a redirect, or a proxy), so the LAST block is the one that describes the
/// body we are about to parse.
fn split_headers(raw: &str) -> (String, &str) {
    let mut headers = String::new();
    let mut rest = raw;
    while let Some(idx) = rest.find("\r\n\r\n").or_else(|| rest.find("\n\n")) {
        let sep = if rest[idx..].starts_with("\r\n\r\n") { 4 } else { 2 };
        let (block, after) = rest.split_at(idx);
        if !block.starts_with("HTTP/") {
            break;
        }
        headers = block.to_string();
        rest = &after[sep..];
    }
    (headers, rest)
}

/// `Link: <url>; rel="next", <url>; rel="last"` — the same header on both.
fn next_link(headers: &str) -> Option<String> {
    let line = headers
        .lines()
        .find(|l| l.to_ascii_lowercase().starts_with("link:"))?;
    line.split(',').find_map(|part| {
        if !part.contains("rel=\"next\"") {
            return None;
        }
        let start = part.find('<')? + 1;
        let end = part.find('>')?;
        Some(part[start..end].to_string())
    })
}

fn api_error(v: &Value) -> Option<String> {
    // GitHub says {"message": ...}; GitLab says {"message": ...} or {"error": ...}.
    // An array is a successful list response and never an error.
    if v.is_array() {
        return None;
    }
    v.get("message")
        .and_then(Value::as_str)
        .or_else(|| v.get("error").and_then(Value::as_str))
        .map(str::to_string)
}

// ---------------------------------------------------------------------------
// Orchestration: start, poll, forget. Deliberately non-blocking.

fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or_default()
}

/// Begin a connection and return what to show the person.
///
/// This returns in one round trip and never waits for them. An agent that
/// blocks for fifteen minutes holding a tool call open is useless to whoever is
/// talking to it — hand back the code, let them go and type it, and ask again.
pub fn start(paths: &Paths, cfg: &ForgeConfig) -> Result<Pending> {
    let forge = get(&cfg.provider).with_context(|| format!("unknown forge {:?}", cfg.provider))?;
    let ep = forge.endpoints(cfg)?;
    let value = send(&device_code_request(&ep))?;
    let pending = parse_pending(&value, now())?;
    crate::secrets::set(
        paths,
        &pending_secret(forge.id()),
        &serde_json::to_string(&pending)?,
    )?;
    Ok(pending)
}

/// Where a connection has got to.
#[derive(Debug, Clone, Serialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum Status {
    /// No device flow has been started and no token is held.
    Disconnected,
    /// Waiting on the person. Repeated verbatim so an agent can re-show it
    /// without having to have kept it.
    Waiting {
        user_code: String,
        verification_uri: String,
        seconds_left: u64,
    },
    Connected,
    /// Something ended the attempt. The flow is cleared; starting again is fine.
    Failed { reason: String },
}

pub fn status(paths: &Paths, id: &str) -> Result<Status> {
    if crate::secrets::exists(paths, &token_secret(id)) {
        return Ok(Status::Connected);
    }
    let Some(raw) = crate::secrets::get(paths, &pending_secret(id))? else {
        return Ok(Status::Disconnected);
    };
    let p: Pending = serde_json::from_str(&raw).context("stored device authorization is corrupt")?;
    Ok(Status::Waiting {
        user_code: p.user_code,
        verification_uri: p.verification_uri,
        seconds_left: p.expires_at.saturating_sub(now()),
    })
}

/// Poll once. Returns the new status; does not sleep and does not loop.
pub fn poll(paths: &Paths, cfg: &ForgeConfig) -> Result<Status> {
    let forge = get(&cfg.provider).with_context(|| format!("unknown forge {:?}", cfg.provider))?;
    let id = forge.id();
    if crate::secrets::exists(paths, &token_secret(id)) {
        return Ok(Status::Connected);
    }
    let Some(raw) = crate::secrets::get(paths, &pending_secret(id))? else {
        return Ok(Status::Disconnected);
    };
    let mut p: Pending =
        serde_json::from_str(&raw).context("stored device authorization is corrupt")?;

    if now() >= p.expires_at {
        let _ = crate::secrets::delete(paths, &pending_secret(id));
        return Ok(Status::Failed {
            reason: "the code expired before it was used — start again".into(),
        });
    }

    let ep = forge.endpoints(cfg)?;
    let outcome = parse_token(&send(&token_request(&ep, &p.device_code))?);
    match outcome {
        Outcome::Token(token) => {
            crate::secrets::set(paths, &token_secret(id), &token)?;
            let _ = crate::secrets::delete(paths, &pending_secret(id));
            Ok(Status::Connected)
        }
        Outcome::Waiting => Ok(Status::Waiting {
            user_code: p.user_code,
            verification_uri: p.verification_uri,
            seconds_left: p.expires_at.saturating_sub(now()),
        }),
        Outcome::SlowDown => {
            // The forge is telling us the interval it will actually tolerate.
            // Remember it, or every later poll earns the same rebuke.
            p.interval += 5;
            let user_code = p.user_code.clone();
            let verification_uri = p.verification_uri.clone();
            let seconds_left = p.expires_at.saturating_sub(now());
            crate::secrets::set(paths, &pending_secret(id), &serde_json::to_string(&p)?)?;
            Ok(Status::Waiting {
                user_code,
                verification_uri,
                seconds_left,
            })
        }
        Outcome::Denied | Outcome::Expired | Outcome::Refused(_) => {
            let _ = crate::secrets::delete(paths, &pending_secret(id));
            Ok(Status::Failed {
                reason: match outcome {
                    Outcome::Denied => "they declined the request on the forge".into(),
                    Outcome::Expired => "the code expired before it was used — start again".into(),
                    Outcome::Refused(r) => r,
                    _ => unreachable!(),
                },
            })
        }
    }
}

/// Forget a connection. The token is deleted here; it is NOT revoked at the
/// forge, which only the person can do, so say so rather than implying the
/// grant is gone.
pub fn disconnect(paths: &Paths, id: &str) -> Result<()> {
    let _ = crate::secrets::delete(paths, &pending_secret(id));
    let _ = crate::secrets::delete(paths, &token_secret(id));
    Ok(())
}

/// The stored token for a connected forge.
pub fn token(paths: &Paths, id: &str) -> Result<String> {
    crate::secrets::get(paths, &token_secret(id))?
        .with_context(|| format!("no {id} account is connected to this Box yet"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn ep() -> Endpoints {
        Endpoints {
            device_code_url: "https://example.test/device".into(),
            token_url: "https://example.test/token".into(),
            client_id: "cid".into(),
            scope: Some("read_api"),
        }
    }

    #[test]
    fn the_token_request_names_the_grant_the_rfc_requires() {
        let r = token_request(&ep(), "dc");
        assert_eq!(r.url, "https://example.test/token");
        let form: std::collections::HashMap<_, _> = r.form.into_iter().collect();
        assert_eq!(form["client_id"], "cid");
        assert_eq!(form["device_code"], "dc");
        assert_eq!(
            form["grant_type"],
            "urn:ietf:params:oauth:grant-type:device_code"
        );
    }

    #[test]
    fn scope_is_sent_only_when_the_forge_wants_one() {
        // GitLab requires a scope...
        assert!(device_code_request(&ep()).form.iter().any(|(k, _)| k == "scope"));
        // ...and a GitHub App takes its permissions from the installation, so
        // sending one would be describing access we do not decide.
        let mut none = ep();
        none.scope = None;
        assert!(!device_code_request(&none).form.iter().any(|(k, _)| k == "scope"));
    }

    #[test]
    fn pending_defaults_match_the_rfc_when_the_forge_is_terse() {
        let p = parse_pending(
            &json!({
                "device_code": "dc", "user_code": "ABCD-1234",
                "verification_uri": "https://example.test/activate"
            }),
            1_000,
        )
        .unwrap();
        assert_eq!(p.interval, 5);
        assert_eq!(p.expires_at, 1_900);

        // And numbers that arrived as strings still parse.
        let p = parse_pending(
            &json!({
                "device_code": "dc", "user_code": "x", "verification_uri": "u",
                "interval": "10", "expires_in": "60"
            }),
            0,
        )
        .unwrap();
        assert_eq!((p.interval, p.expires_at), (10, 60));
    }

    #[test]
    fn every_documented_poll_error_maps_to_something_actionable() {
        assert_eq!(
            parse_token(&json!({"access_token": "t"})),
            Outcome::Token("t".into())
        );
        assert_eq!(parse_token(&json!({"error": "authorization_pending"})), Outcome::Waiting);
        assert_eq!(parse_token(&json!({"error": "slow_down"})), Outcome::SlowDown);
        assert_eq!(parse_token(&json!({"error": "access_denied"})), Outcome::Denied);
        assert_eq!(parse_token(&json!({"error": "expired_token"})), Outcome::Expired);

        // The one that means "you forgot a checkbox" must not read as a
        // transient failure, or it will be retried forever.
        let Outcome::Refused(why) = parse_token(&json!({"error": "device_flow_disabled"})) else {
            panic!("device_flow_disabled must be terminal");
        };
        assert!(why.contains("device flow"));

        // An unrecognised error still carries the forge's own words.
        let Outcome::Refused(why) = parse_token(&json!({
            "error": "invalid_client", "error_description": "no such application"
        })) else {
            panic!("unknown errors must be terminal, not silently retried");
        };
        assert_eq!(why, "no such application");
    }

    #[test]
    fn the_forgotten_checkbox_is_reported_in_githubs_own_words() {
        // Verbatim from github.com/login/device/code against a real App whose
        // registration was missing the checkbox. Kept because it is the first
        // thing that goes wrong when someone registers an App, and because a
        // guessed shape has been wrong more than once on this project.
        let observed = json!({
            "error": "device_flow_disabled",
            "error_description": "Device Flow must be explicitly enabled for this App",
            "error_uri": "https://docs.github.com"
        });
        let err = parse_pending(&observed, 0).unwrap_err().to_string();
        assert_eq!(err, "Device Flow must be explicitly enabled for this App");
    }

    #[test]
    fn configuring_one_field_does_not_erase_the_others() {
        let tmp = tempfile::TempDir::new().unwrap();
        let paths = Paths::new(tmp.path().to_path_buf());
        paths.ensure().unwrap();

        crate::forge::configure(
            &paths,
            "github",
            None,
            Some("Iv23liABC".into()),
            Some("the-box".into()),
        )
        .unwrap();
        // Supplying only a slug later must not drop the client id — that would
        // silently break a Box that was working a moment ago.
        let merged = crate::forge::configure(&paths, "github", None, None, Some("renamed".into()))
            .unwrap();
        assert_eq!(merged.client_id.as_deref(), Some("Iv23liABC"));
        assert_eq!(merged.app_slug.as_deref(), Some("renamed"));

        // And it survives a reload, because it lives in box.toml rather than
        // in memory — which is what makes it come back after a rebuild.
        let loaded = config_for(&paths, "github").unwrap();
        assert_eq!(loaded.client_id.as_deref(), Some("Iv23liABC"));

        // A forge nobody implements must not be written into the config.
        assert!(configure(&paths, "bitbucket", None, None, None).is_err());
        assert!(crate::config::BoxConfig::load(&paths)
            .unwrap()
            .forges
            .iter()
            .all(|f| f.provider != "bitbucket"));
    }

    #[test]
    fn pagination_follows_only_the_next_link() {
        let headers = "HTTP/2 200\r\nlink: <https://a/2>; rel=\"next\", <https://a/9>; rel=\"last\"";
        assert_eq!(next_link(headers), Some("https://a/2".into()));
        // The last page says prev/first and no next, which is how a loop ends.
        assert_eq!(
            next_link("HTTP/2 200\r\nlink: <https://a/1>; rel=\"prev\""),
            None
        );
        assert_eq!(next_link("HTTP/2 200"), None);
    }

    #[test]
    fn the_body_is_found_past_every_header_block() {
        // A 100-continue or a redirect leaves more than one block in front of
        // the body, and parsing the first one would lose the response.
        let raw = "HTTP/1.1 100 Continue\r\n\r\nHTTP/2 200\r\nlink: <https://a/2>; rel=\"next\"\r\n\r\n[{\"x\":1}]";
        let (headers, body) = split_headers(raw);
        assert_eq!(body, "[{\"x\":1}]");
        assert_eq!(next_link(&headers), Some("https://a/2".into()));
    }

    #[test]
    fn a_list_response_is_never_read_as_an_error() {
        // GitLab returns a bare array of projects; a project could plausibly
        // have a "message" field, and treating the array as an envelope would
        // turn a good response into a refusal.
        assert_eq!(api_error(&json!([{"message": "not an error"}])), None);
        assert_eq!(
            api_error(&json!({"message": "Bad credentials"})).as_deref(),
            Some("Bad credentials")
        );
    }
}
