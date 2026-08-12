//! Getting a service onto the internet.
//!
//! There is no single right answer here, so this is a ladder rather than a
//! feature: a way to show someone what you made in ten seconds, a way to put it
//! on a domain you own, and (later) a way to own the whole path yourself. What
//! they have in common is the shape — something on the Box dials OUT to a
//! service that accepts inbound traffic on its behalf, which is what makes any
//! of this work behind a home router with no ports open.
//!
//! Whichever rung you are on, the Box's side is identical: nginx serves the
//! services you published on a loopback-only listener
//! (`services.the-box.publicListenPort`, 2694), and the connector points at
//! that. A provider cannot expose anything you did not publish, because
//! nothing else is there to expose — see nix/module.nix.
//!
//! Each provider is "a preflight, a command to supervise, and a way to learn
//! the public URL". The supervisor below is shared, because keeping a child
//! process alive with backoff is not the interesting part.

use std::process::{Command, Stdio};

use anyhow::{bail, Context, Result};
use serde::Serialize;

use crate::config::{BoxConfig, IngressConfig};
use crate::paths::Paths;

/// What using this provider costs the person, so the console can be honest
/// before they commit to one.
#[derive(Debug, Clone, Copy, Serialize)]
pub struct Capabilities {
    /// They must own a domain and point it here.
    pub needs_domain: bool,
    /// They must create an account somewhere.
    pub needs_account: bool,
    /// HTTPS is handled for us, so the Box needs no certificate.
    pub terminates_tls: bool,
    /// The address survives a restart. A demo link that changes every reboot
    /// is fine for showing a friend and wrong for anything you tell people.
    pub stable_url: bool,
    /// Someone else's infrastructure can read the traffic in the clear,
    /// because they terminate the TLS.
    pub third_party_sees_traffic: bool,
}

pub trait IngressProvider: Send + Sync {
    fn id(&self) -> &'static str;
    fn title(&self) -> &'static str;
    /// One sentence, for a person choosing between these.
    fn description(&self) -> &'static str;
    fn capabilities(&self) -> Capabilities;
    /// What the person has to do themselves, in order. Empty means nothing.
    fn steps(&self) -> Vec<&'static str> {
        Vec::new()
    }
    /// Is the tool this needs actually on the Box?
    fn available(&self) -> bool;
    /// Refuse with a readable reason rather than starting something that
    /// cannot work.
    fn preflight(&self, paths: &Paths, cfg: &IngressConfig) -> Result<()>;
    /// The connector to supervise. `Ok(None)` means this provider needs no
    /// long-lived process.
    fn command(&self, paths: &Paths, cfg: &IngressConfig, port: u16) -> Result<Option<Command>>;
    /// Configure something that outlives us, for providers whose state is kept
    /// by another daemon rather than by a process we hold open.
    fn activate(&self, _paths: &Paths, _cfg: &IngressConfig, _port: u16) -> Result<()> {
        Ok(())
    }
    fn deactivate(&self, _paths: &Paths) -> Result<()> {
        Ok(())
    }
    /// The public address of a service, once this is running. `None` when the
    /// provider hands out an address we can only learn at runtime and have not
    /// learned yet.
    fn url_for(&self, cfg: &IngressConfig, service: &str, runtime: Option<&str>) -> Option<String>;
}

/// Every rung of the ladder. The console renders this list.
pub fn providers() -> &'static [&'static dyn IngressProvider] {
    &[&CloudflareTunnel, &CloudflareQuick, &TailscaleFunnel]
}

pub fn get(id: &str) -> Option<&'static dyn IngressProvider> {
    providers().iter().copied().find(|p| p.id() == id)
}

/// The loopback port the connector points at — the one nginx serves published
/// services on. Kept in step with `services.the-box.publicListenPort`.
pub const PUBLIC_PORT: u16 = 2694;

// ---------------------------------------------------------------------------

/// Bring your own domain, through a Cloudflare tunnel you created.
///
/// The production rung: a name you own, a stable address, TLS at Cloudflare's
/// edge, and it works behind CGNAT because the connection is outbound.
pub struct CloudflareTunnel;

pub const CF_TOKEN_SECRET: &str = "cloudflare-tunnel-token";

impl IngressProvider for CloudflareTunnel {
    fn id(&self) -> &'static str {
        "cloudflare-tunnel"
    }
    fn title(&self) -> &'static str {
        "Your own domain"
    }
    fn description(&self) -> &'static str {
        "Publish on a domain you own, through a Cloudflare tunnel. Stable address, HTTPS, and nothing open on your router."
    }
    fn capabilities(&self) -> Capabilities {
        Capabilities {
            needs_domain: true,
            needs_account: true,
            terminates_tls: true,
            stable_url: true,
            third_party_sees_traffic: true,
        }
    }
    fn steps(&self) -> Vec<&'static str> {
        vec![
            "Add your domain to Cloudflare and change its nameservers at your registrar. (Buying the domain at Cloudflare skips this.)",
            "In Cloudflare Zero Trust, create a tunnel and copy its token.",
            "Add one public hostname on that tunnel pointing at http://localhost:2694.",
            "Paste the token here and turn it on.",
        ]
    }
    fn available(&self) -> bool {
        binary_available("cloudflared")
    }
    fn preflight(&self, paths: &Paths, cfg: &IngressConfig) -> Result<()> {
        if !self.available() {
            bail!("cloudflared is not installed on this Box");
        }
        if !crate::secrets::exists(paths, CF_TOKEN_SECRET) {
            bail!("no Cloudflare tunnel token saved yet — paste one first");
        }
        match cfg.zone.as_deref().map(str::trim) {
            Some(z) if !z.is_empty() => crate::config::validate_domain(z)
                .context("the domain this Box publishes under")?,
            _ => bail!("set the domain this Box publishes under, e.g. example.com"),
        }
        Ok(())
    }
    fn command(&self, paths: &Paths, _cfg: &IngressConfig, _port: u16) -> Result<Option<Command>> {
        let token = crate::secrets::get(paths, CF_TOKEN_SECRET)?
            .context("cloudflare tunnel token missing")?;
        let mut cmd = Command::new("cloudflared");
        // Token in the environment, never argv: argv is world-readable in /proc.
        cmd.args(["tunnel", "run"]).env("TUNNEL_TOKEN", token);
        Ok(Some(cmd))
    }
    fn url_for(&self, cfg: &IngressConfig, service: &str, _runtime: Option<&str>) -> Option<String> {
        let zone = cfg.zone.as_deref()?.trim();
        if zone.is_empty() {
            return None;
        }
        Some(format!("https://{service}.{zone}/"))
    }
}

/// A link to show someone right now: no account, no domain, no setup.
///
/// This is the rung that makes the product feel like it works, because the
/// first thing anyone wants after building something is to show it to a
/// person. The address is temporary and changes when it restarts, which the
/// console says plainly — it is for showing, not for telling people where to
/// find you.
pub struct CloudflareQuick;

impl IngressProvider for CloudflareQuick {
    fn id(&self) -> &'static str {
        "quick-share"
    }
    fn title(&self) -> &'static str {
        "A link to share right now"
    }
    fn description(&self) -> &'static str {
        "A temporary public address, with no account and no domain. It changes if the Box restarts, so use it to show someone, not to tell people where to find you."
    }
    fn capabilities(&self) -> Capabilities {
        Capabilities {
            needs_domain: false,
            needs_account: false,
            terminates_tls: true,
            stable_url: false,
            third_party_sees_traffic: true,
        }
    }
    fn available(&self) -> bool {
        binary_available("cloudflared")
    }
    fn preflight(&self, _paths: &Paths, _cfg: &IngressConfig) -> Result<()> {
        if !self.available() {
            bail!("cloudflared is not installed on this Box");
        }
        Ok(())
    }
    fn command(&self, paths: &Paths, _cfg: &IngressConfig, port: u16) -> Result<Option<Command>> {
        // Two flags that are not optional, both learned the hard way.
        //
        // `--config`: with none given, cloudflared searches /etc/cloudflared
        // and friends, and if it finds a config with `ingress:` rules those
        // SILENTLY WIN over `--url`. On a Box that also runs a named tunnel,
        // "share this right now" would quietly publish a public address
        // pointing at the other tunnel's rules — no warning, no error. Point it
        // at a file of our own.
        //
        // `--metrics`: the default walks a range, so with two connectors
        // running you cannot tell which port belongs to which. We ask this one
        // for its address by port, so the port has to be ours.
        let config = quick_config_file(paths)?;
        let mut cmd = Command::new("cloudflared");
        cmd.args([
            "tunnel",
            "--no-autoupdate",
            "--config",
            &config.display().to_string(),
            "--metrics",
            &format!("127.0.0.1:{QUICK_METRICS_PORT}"),
            "--url",
            &format!("http://127.0.0.1:{port}"),
        ]);
        Ok(Some(cmd))
    }
    fn url_for(&self, _cfg: &IngressConfig, _service: &str, runtime: Option<&str>) -> Option<String> {
        // The address is assigned to us at runtime, and every published service
        // answers on it by Host header — which a temporary address cannot
        // carry. So this is the address of the Box, not of one service.
        runtime.map(|base| format!("{base}/"))
    }
}

/// Publish on a `*.ts.net` address, using the mesh the Box already speaks.
///
/// No domain to buy and no second account if they already use Tailscale for
/// private access. The address is stable, which makes this the honest middle
/// rung between a throwaway link and a domain of your own.
pub struct TailscaleFunnel;

impl IngressProvider for TailscaleFunnel {
    fn id(&self) -> &'static str {
        "tailscale-funnel"
    }
    fn title(&self) -> &'static str {
        "A Tailscale address"
    }
    fn description(&self) -> &'static str {
        "Publish on your tailnet's own name, with no domain to buy. The address is stable, and HTTPS is handled for you."
    }
    fn capabilities(&self) -> Capabilities {
        Capabilities {
            needs_domain: false,
            needs_account: true,
            terminates_tls: true,
            stable_url: true,
            // The one rung where nobody else can read the traffic: Tailscale's
            // ingress proxies the encrypted stream and tailscaled terminates
            // TLS here, on the Box, with its own certificate. The tunnels
            // terminate at someone else's edge and see plaintext.
            third_party_sees_traffic: false,
        }
    }
    fn steps(&self) -> Vec<&'static str> {
        vec![
            // Funnel is a tailscale.com feature and does not exist on Headscale
            // — by design, on both sides. So this rung needs a Tailscale
            // account even for someone whose private access is self-hosted.
            "Sign this Box in to a Tailscale account (not a self-hosted coordinator — Funnel only works on tailscale.com).",
            "In the Tailscale admin console, turn on MagicDNS and HTTPS certificates.",
            "Allow Funnel for this machine in your tailnet's access policy.",
        ]
    }
    fn available(&self) -> bool {
        binary_available("tailscale")
    }
    fn preflight(&self, _paths: &Paths, _cfg: &IngressConfig) -> Result<()> {
        if !self.available() {
            bail!("tailscale is not installed on this Box");
        }
        let Some(status) = tailscale_status() else {
            bail!(
                "this Box is not signed in to a tailnet yet — sign in first, and note that Funnel \
                 needs a tailscale.com account rather than a self-hosted coordinator"
            );
        };
        if tailnet_address().is_none() {
            bail!("this Box has no tailnet address yet — sign it in to a tailnet first");
        }
        if !funnel_permitted(&status) {
            match funnel_approval_url(&status) {
                Some(url) => bail!(
                    "your tailnet has not allowed Funnel for this Box yet. Turn it on here, then \
                     try again: {url}"
                ),
                None => bail!("your tailnet has not allowed Funnel for this Box yet"),
            }
        }
        Ok(())
    }
    fn command(&self, _paths: &Paths, _cfg: &IngressConfig, _port: u16) -> Result<Option<Command>> {
        // Nothing of ours to supervise: tailscaled keeps the configuration, so
        // this is set once (see `activate`) rather than by a process we hold
        // open. That is also why `--bg` is not optional there.
        Ok(None)
    }
    fn activate(&self, _paths: &Paths, _cfg: &IngressConfig, port: u16) -> Result<()> {
        // `timeout`, because this command waits at a prompt forever when the
        // tailnet has not granted Funnel — it prints an approval link and then
        // blocks. The preflight above catches that case, but a request that
        // never returns is a bad enough failure to guard twice.
        run_ok(&[
            "timeout",
            "30",
            "tailscale",
            "funnel",
            "--bg",
            "--https=443",
            &format!("http://127.0.0.1:{port}"),
        ])
        .map_err(|e| {
            anyhow::anyhow!(
                "{e:#}\n\nIf this timed out, your tailnet has not allowed Funnel for this Box yet."
            )
        })
    }
    fn deactivate(&self, _paths: &Paths) -> Result<()> {
        run_ok(&["tailscale", "funnel", "--https=443", "off"])
    }
    fn url_for(&self, _cfg: &IngressConfig, _service: &str, runtime: Option<&str>) -> Option<String> {
        runtime
            .map(|base| format!("{base}/"))
            .or_else(|| tailnet_address().map(|base| format!("{base}/")))
    }
}

/// Ours, so that a config someone else's tunnel dropped on the box cannot
/// redirect a share link. `no-autoupdate` rather than a comment: an
/// effectively-empty YAML makes cloudflared log a spurious error every start.
pub const QUICK_METRICS_PORT: u16 = 20996;

fn quick_config_file(paths: &Paths) -> Result<std::path::PathBuf> {
    let path = paths.data_dir.join("cloudflared-quick.yml");
    std::fs::write(&path, "no-autoupdate: true\n")
        .with_context(|| format!("writing {}", path.display()))?;
    Ok(path)
}

/// Ask a running quick tunnel what address it was given.
///
/// cloudflared serves this as structured data on its metrics port, and has
/// since 2021 — which is a far better contract than the ASCII box it draws on
/// stderr, where the address arrives padded inside box art. A named tunnel
/// answers with an empty hostname, so a non-empty one also tells us this really
/// is a quick tunnel.
pub fn quick_tunnel_address() -> Option<String> {
    let host = curl_json(&format!(
        "http://127.0.0.1:{QUICK_METRICS_PORT}/quicktunnel"
    ))?
    .get("hostname")?
    .as_str()?
    .trim()
    .to_string();
    if host.is_empty() {
        return None;
    }
    // Having an address is not the same as answering on it: the edge connection
    // comes up a beat later, and handing someone a link that 404s for ten
    // seconds is worse than making them wait for it.
    let ready = curl_json(&format!("http://127.0.0.1:{QUICK_METRICS_PORT}/ready"))
        .and_then(|v| v.get("readyConnections")?.as_u64())
        .unwrap_or(0);
    if ready == 0 {
        return None;
    }
    Some(format!("https://{host}"))
}

/// The address this machine has on its tailnet, which is where Funnel
/// publishes it. Derived the same way Tailscale's own CLI derives it.
pub fn tailnet_address() -> Option<String> {
    let status = tailscale_status()?;
    let name = status
        .get("Self")?
        .get("DNSName")?
        .as_str()?
        .trim_end_matches('.');
    if name.is_empty() {
        return None;
    }
    Some(format!("https://{name}"))
}

fn tailscale_status() -> Option<serde_json::Value> {
    curl_free_json(&["tailscale", "status", "--json"])
}

/// Whether this tailnet has actually granted this machine Funnel.
///
/// Worth checking BEFORE running the command, because `tailscale funnel`
/// answers a missing grant by printing an approval link and then waiting at a
/// prompt forever — which on a Box means a request that never returns. Asked of
/// the capability map instead, which answers immediately.
/// Matched as a PREFIX, because the capability a permitted node actually
/// carries is `…/cap/funnel-ports?ports=443,8443,10000` — the grant and the
/// allowed ports arrive as one key. Looking for an exact `…/cap/funnel` finds
/// nothing and refuses a tailnet that has granted Funnel perfectly well.
const FUNNEL_CAP_PREFIX: &str = "https://tailscale.com/cap/funnel";

fn funnel_permitted(status: &serde_json::Value) -> bool {
    status
        .get("Self")
        .and_then(|s| s.get("CapMap"))
        .and_then(|m| m.as_object())
        .map(|m| m.keys().any(|k| k.starts_with(FUNNEL_CAP_PREFIX)))
        .unwrap_or(false)
}

/// The page that turns Funnel on for this machine, so the refusal can say
/// exactly where to go rather than "ask your administrator".
fn funnel_approval_url(status: &serde_json::Value) -> Option<String> {
    let id = status.get("Self")?.get("ID")?.as_str()?;
    Some(format!("https://login.tailscale.com/f/funnel?node={id}"))
}

fn curl_json(url: &str) -> Option<serde_json::Value> {
    curl_free_json(&["curl", "-fsS", "-m", "3", url])
}

fn curl_free_json(argv: &[&str]) -> Option<serde_json::Value> {
    let out = Command::new(argv[0]).args(&argv[1..]).output().ok()?;
    if !out.status.success() {
        return None;
    }
    serde_json::from_slice(&out.stdout).ok()
}

fn run_ok(argv: &[&str]) -> Result<()> {
    let out = Command::new(argv[0])
        .args(&argv[1..])
        .output()
        .with_context(|| format!("running {}", argv.join(" ")))?;
    if !out.status.success() {
        bail!(
            "{} failed: {}",
            argv.join(" "),
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    Ok(())
}

fn binary_available(bin: &str) -> bool {
    Command::new(bin)
        .arg("--version")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

/// What a person or an agent is told about how this Box is reachable.
#[derive(Debug, Clone, Serialize)]
pub struct IngressStatus {
    pub provider: String,
    pub enabled: bool,
    /// "off" | "starting" | "running" | "failed: …"
    pub state: String,
    /// The address the provider gave us, when it assigns one at runtime.
    pub address: Option<String>,
    /// Why this cannot work yet, if it cannot.
    pub problem: Option<String>,
}

/// The published address of each service, for the console and for agents.
pub fn published_urls(paths: &Paths, runtime: Option<&str>) -> Vec<(String, String)> {
    let Ok(config) = BoxConfig::load(paths) else {
        return Vec::new();
    };
    let Some(ingress) = config.ingress.as_ref().filter(|i| i.enabled) else {
        return Vec::new();
    };
    let Some(provider) = get(&ingress.provider) else {
        return Vec::new();
    };
    config
        .services
        .iter()
        .filter(|s| s.public)
        .filter_map(|s| {
            provider
                .url_for(ingress, &s.name, runtime)
                .map(|url| (s.name.clone(), url))
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_rung_describes_itself() {
        for p in providers() {
            assert!(!p.id().is_empty());
            assert!(!p.title().is_empty());
            // A person choosing between these has to be able to tell them
            // apart, so nothing may ship without saying what it costs them.
            assert!(p.description().len() > 30, "{}", p.id());
        }
        assert!(get("cloudflare-tunnel").is_some());
        assert!(get("nope").is_none());
    }

    #[test]
    fn a_domain_provider_addresses_services_by_subdomain() {
        let mut cfg = IngressConfig::new("cloudflare-tunnel");
        cfg.zone = Some("example.com".into());
        assert_eq!(
            CloudflareTunnel.url_for(&cfg, "blog", None).as_deref(),
            Some("https://blog.example.com/")
        );
        // Without a domain there is no address to offer.
        assert!(CloudflareTunnel
            .url_for(&IngressConfig::new("cloudflare-tunnel"), "blog", None)
            .is_none());
    }

    /// A provider that is handed its address cannot know it before it runs.
    /// The rungs differ in ways that decide which one is right for a person,
    /// so the differences have to be stated rather than implied.
    #[test]
    fn the_rungs_are_honestly_distinguished() {
        let quick = CloudflareQuick.capabilities();
        let funnel = TailscaleFunnel.capabilities();
        let domain = CloudflareTunnel.capabilities();

        // Only one rung asks for nothing at all.
        assert!(!quick.needs_domain && !quick.needs_account);
        // Only one rung gives an address that does not survive a restart.
        assert!(!quick.stable_url);
        assert!(funnel.stable_url && domain.stable_url);
        // Only one rung keeps the traffic away from a third party: Tailscale
        // proxies an encrypted stream and the Box terminates TLS itself, where
        // the tunnels terminate it at someone else's edge.
        assert!(!funnel.third_party_sees_traffic);
        assert!(quick.third_party_sees_traffic && domain.third_party_sees_traffic);
        // Every rung that needs the person to do something says what.
        for p in providers() {
            if p.capabilities().needs_account {
                assert!(!p.steps().is_empty(), "{} asks for an account silently", p.id());
            }
        }
    }

    #[test]
    fn runtime_addressed_providers_wait_for_their_address() {
        let cfg = IngressConfig::new("quick-share");
        assert!(CloudflareQuick.url_for(&cfg, "blog", None).is_none());
        assert_eq!(
            CloudflareQuick
                .url_for(&cfg, "blog", Some("https://x-y-z.trycloudflare.com"))
                .as_deref(),
            Some("https://x-y-z.trycloudflare.com/")
        );
    }

    /// A permitted node does not carry a bare `…/cap/funnel`; it carries
    /// `…/cap/funnel-ports?ports=…`. Matching exactly refused tailnets that had
    /// granted Funnel correctly — found by running it against a real one.
    #[test]
    fn funnel_grant_is_recognised_in_the_form_tailscale_actually_sends() {
        let granted = serde_json::json!({ "Self": { "CapMap": {
            "https://tailscale.com/cap/funnel-ports?ports=443,8443,10000": [],
            "https://tailscale.com/cap/ssh": []
        }}});
        assert!(funnel_permitted(&granted));

        let not_granted = serde_json::json!({ "Self": { "CapMap": {
            "https://tailscale.com/cap/ssh": [],
            "https://tailscale.com/cap/file-sharing": []
        }}});
        assert!(!funnel_permitted(&not_granted));
        assert!(!funnel_permitted(&serde_json::json!({})));

        // The refusal points at the page that fixes it, for this node.
        let status = serde_json::json!({ "Self": { "ID": "nABC123" } });
        assert_eq!(
            funnel_approval_url(&status).as_deref(),
            Some("https://login.tailscale.com/f/funnel?node=nABC123")
        );
    }

    #[test]
    fn preflight_refuses_a_domain_provider_with_no_domain() {
        let tmp = tempfile::TempDir::new().unwrap();
        let paths = Paths::new(tmp.path().to_path_buf());
        paths.ensure().unwrap();
        let err = CloudflareTunnel
            .preflight(&paths, &IngressConfig::new("cloudflare-tunnel"))
            .unwrap_err()
            .to_string();
        // Either complaint is correct; both name what to do next.
        assert!(
            err.contains("token") || err.contains("domain") || err.contains("cloudflared"),
            "unhelpful: {err}"
        );
    }
}
