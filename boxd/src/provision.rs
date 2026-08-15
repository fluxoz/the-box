//! Agent-driven provisioning — turn a spare machine on the network into a Box,
//! hands-off.
//!
//! Everything *after* "a Box exists" was already agent-ready: the MCP server at
//! `/mcp`, session tokens carried as `Authorization: Bearer`. The missing seam
//! was getting a bare machine *to be* a Box without a human at the keyboard.
//! `boxd provision <ssh-target>` closes it:
//!
//!   1. mint the machine's identity — hostname, the agent's SSH key, and a
//!      one-time pairing code (only its hash leaves this process);
//!   2. ship it as orders over SSH and run the takeover installer, which wipes
//!      the disk and kexecs into the Box installer;
//!   3. wait for the machine to come back up as a Box;
//!   4. redeem the pairing code for a session token.
//!
//! The agent then drives the new Box over MCP with that token — configure,
//! deploy, maintain — exactly as it would any other Box.

use std::io::Read;
use std::process::Command;
use std::time::{Duration, Instant};

use anyhow::{bail, Context, Result};
use serde_json::{json, Value};

use crate::util::base64_encode;

/// Where the takeover installer lives (overridable for forks/mirrors/tests).
pub const DEFAULT_INSTALL_URL: &str = "https://thebox.build/install.sh";
/// The Box management port (dashboard, API, MCP).
pub const BOX_PORT: u16 = 2693;

/// Printed by the remote shell before the installer runs, so a dropped SSH
/// connection (the installer kexecs) is distinguishable from "never reached".
const SENTINEL: &str = "__BOX_PROVISION_CONNECTED__";

/// Everything `provision` needs to turn one machine into a Box.
pub struct ProvisionOpts {
    /// SSH target for the spare machine, e.g. `root@192.168.1.42`.
    pub target: String,
    /// Hostname for the new Box (`auto` derives a stable per-machine name).
    pub hostname: String,
    /// Public SSH keys authorized on the new Box (the agent's / operator's).
    pub ssh_keys: Vec<String>,
    /// Optional storage layout: `single` | `mirror` | `pool`.
    pub layout: Option<String>,
    /// The takeover installer URL.
    pub install_url: String,
    /// Host/IP to reach the Box on after it boots (default: the target's host).
    pub reach_host: Option<String>,
    /// How long to wait for the Box to come up, in seconds.
    pub boot_timeout_secs: u64,
    /// Extra arguments passed through to `ssh` (e.g. `-p 2222`, `-i key`).
    pub ssh_opts: Vec<String>,
}

/// What an agent needs to take over management of the freshly provisioned Box.
pub struct Provisioned {
    /// `host:port`.
    pub address: String,
    /// Session token — use as `Authorization: Bearer <token>` (or the
    /// `box_session` cookie). Minted by redeeming the one-time pairing code.
    pub token: String,
    /// `http://host:port/mcp`.
    pub mcp_url: String,
    /// `http://host:port/`.
    pub dashboard: String,
}

/// Drive the whole flow. Progress goes to stderr so stdout can carry clean
/// JSON for an agent.
pub fn run(opts: &ProvisionOpts) -> Result<Provisioned> {
    if opts.ssh_keys.is_empty() {
        bail!(
            "no SSH key to authorize on the Box — the agent needs a key on it \
               to connect after provisioning"
        );
    }

    // 1. Mint a one-time pairing code. Only its hash rides in the orders (seeded
    //    into the Box's auth store at first boot); the plaintext stays here.
    let code = random_code()?;
    let code_hash = sha256_hex(&code);

    // 2. Assemble the identity + consent and encode it for the command line.
    let orders = build_orders(
        &opts.hostname,
        &opts.ssh_keys,
        &code_hash,
        opts.layout.as_deref(),
    );
    let orders_b64 = base64_encode(&serde_json::to_vec(&orders)?);

    // 3. Ship the orders over SSH and start the takeover. The installer kexecs,
    //    so the connection drops mid-command — expected, not an error.
    eprintln!(
        "→ shipping orders to {} and starting the takeover …",
        opts.target
    );
    ship_and_install(opts, &orders_b64)?;

    // 4. Wait for the machine to wipe, reinstall, reboot, and come up as a Box.
    let host = opts
        .reach_host
        .clone()
        .unwrap_or_else(|| host_of(&opts.target));
    let address = format!("{host}:{BOX_PORT}");
    eprintln!(
        "→ takeover underway. Waiting up to {}s for {address} to come up as a Box …",
        opts.boot_timeout_secs
    );
    wait_for_box(&address, opts.boot_timeout_secs)?;

    // 5. Redeem the pairing code for a session token the agent can use.
    eprintln!("→ Box is up. Pairing the agent …");
    let token = redeem_over_http(&address, &code)?;

    Ok(Provisioned {
        mcp_url: format!("http://{address}/mcp"),
        dashboard: format!("http://{address}/"),
        address,
        token,
    })
}

// --- pure helpers (unit-tested) --------------------------------------------

/// Build the orders (box-install.json) that identify the new Box. `code_hash`
/// is the SHA-256 of the one-time pairing code; it rides in as
/// `enrollment_code_hash` so the Box is pairable from first boot without SSH.
pub fn build_orders(
    hostname: &str,
    ssh_keys: &[String],
    code_hash: &str,
    layout: Option<&str>,
) -> Value {
    let mut orders = json!({
        // Consent is explicit and machine-checked: install.sh refuses to touch
        // any disk unless the orders say erase_disk:true.
        "erase_disk": true,
        "hostname": hostname,
        "ssh_authorized_keys": ssh_keys,
        "enrollment_code_hash": code_hash,
    });
    if let Some(l) = layout {
        orders["storage"] = json!({ "layout": l });
    }
    orders
}

/// The one-liner the target runs over SSH: fetch the takeover installer and run
/// it with the orders embedded (base64) so nothing secret touches the disk. It
/// runs as root directly if the SSH user is root, else via `sudo -n` (fail fast
/// rather than hang on a password prompt).
/// `BOX_YES=1` carries the consent that a person would otherwise type: this runs
/// over SSH with no terminal to prompt on, and the operator already consented by
/// running `boxd provision` against a named machine — the orders themselves say
/// `erase_disk: true`, which the installer checks independently.
pub fn remote_install_command(install_url: &str, orders_b64: &str) -> String {
    format!(
        "curl -fsSL {url} | if [ \"$(id -u)\" = 0 ]; then \
BOX_YES=1 BOX_ORDERS_B64={orders} sh; else \
sudo -n BOX_YES=1 BOX_ORDERS_B64={orders} sh; fi",
        url = install_url,
        orders = orders_b64,
    )
}

/// Extract the host from an ssh target (`user@host` → `host`).
pub fn host_of(target: &str) -> String {
    let after_user = target.rsplit('@').next().unwrap_or(target);
    // ssh targets don't carry a `:port` (that's `-p`), but be defensive.
    after_user
        .split(['/', ':'])
        .next()
        .unwrap_or(after_user)
        .to_string()
}

/// Resolve `--ssh-key` inputs (literal keys or paths to `.pub` files) into
/// public-key lines. With no inputs, autodetect `~/.ssh/id_*.pub`.
pub fn resolve_ssh_keys(inputs: Vec<String>) -> Result<Vec<String>> {
    let mut keys = Vec::new();
    if inputs.is_empty() {
        if let Some(home) = std::env::var_os("HOME") {
            for name in ["id_ed25519.pub", "id_ecdsa.pub", "id_rsa.pub"] {
                let p = std::path::Path::new(&home).join(".ssh").join(name);
                if let Ok(text) = std::fs::read_to_string(&p) {
                    push_keys(&mut keys, &text);
                }
            }
        }
        if keys.is_empty() {
            bail!(
                "no --ssh-key given and no ~/.ssh/id_*.pub found. The agent needs a \
                 public key authorized on the Box to manage it after provisioning."
            );
        }
    } else {
        for inp in inputs {
            if std::path::Path::new(&inp).is_file() {
                let text =
                    std::fs::read_to_string(&inp).with_context(|| format!("reading {inp}"))?;
                push_keys(&mut keys, &text);
            } else {
                push_keys(&mut keys, &inp);
            }
        }
        if keys.is_empty() {
            bail!("no valid SSH public keys in the given --ssh-key input(s)");
        }
    }
    Ok(keys)
}

fn push_keys(keys: &mut Vec<String>, text: &str) {
    for line in text.lines() {
        let line = line.trim();
        if is_pubkey(line) && !keys.iter().any(|k| k == line) {
            keys.push(line.to_string());
        }
    }
}

fn is_pubkey(line: &str) -> bool {
    [
        "ssh-ed25519",
        "ssh-rsa",
        "ecdsa-sha2-",
        "ssh-dss",
        "sk-ssh-ed25519@openssh.com",
        "sk-ecdsa-sha2-nistp256@openssh.com",
    ]
    .iter()
    .any(|p| line.starts_with(p))
}

/// The other direction, for content that arrives base64-encoded (a binary file
/// uploaded through the JSON API). Accepts standard base64 with or without
/// padding, and ignores whitespace, which is what a client that wrapped long
/// lines will send.
pub fn base64_decode(s: &str) -> Result<Vec<u8>> {
    fn value(c: u8) -> Option<u32> {
        match c {
            b'A'..=b'Z' => Some((c - b'A') as u32),
            b'a'..=b'z' => Some((c - b'a') as u32 + 26),
            b'0'..=b'9' => Some((c - b'0') as u32 + 52),
            b'+' => Some(62),
            b'/' => Some(63),
            _ => None,
        }
    }

    let mut out = Vec::with_capacity(s.len() / 4 * 3);
    let mut acc: u32 = 0;
    let mut bits = 0u32;
    for c in s.bytes() {
        if c.is_ascii_whitespace() || c == b'=' {
            continue;
        }
        let v = value(c).ok_or_else(|| anyhow::anyhow!("not valid base64"))?;
        acc = (acc << 6) | v;
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push((acc >> bits) as u8);
            acc &= (1 << bits) - 1;
        }
    }
    Ok(out)
}

fn sha256_hex(s: &str) -> String {
    use sha2::{Digest, Sha256};
    Sha256::digest(s.as_bytes())
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

fn random_code() -> Result<String> {
    let mut buf = [0u8; 5]; // 10 hex chars, ~40 bits — single-use, short-lived
    std::fs::File::open("/dev/urandom")
        .context("opening /dev/urandom")?
        .read_exact(&mut buf)
        .context("reading /dev/urandom")?;
    Ok(buf.iter().map(|b| format!("{b:02x}")).collect())
}

// --- side-effecting steps (exercised end-to-end in real use) ---------------

fn ship_and_install(opts: &ProvisionOpts, orders_b64: &str) -> Result<()> {
    let remote = remote_install_command(&opts.install_url, orders_b64);
    let mut cmd = Command::new("ssh");
    cmd.args(["-o", "BatchMode=yes"])
        .args(["-o", "StrictHostKeyChecking=accept-new"])
        .args(["-o", "ConnectTimeout=15"]);
    for o in &opts.ssh_opts {
        cmd.arg(o);
    }
    cmd.arg(&opts.target);
    // Announce the connection before the installer runs, so a dropped SSH
    // session (the installer kexecs) reads as success rather than failure.
    cmd.arg(format!("echo {SENTINEL}; {remote}"));

    let out = cmd
        .output()
        .context("running ssh (is ssh installed and the target reachable?)")?;
    let combined = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    if !combined.contains(SENTINEL) {
        bail!(
            "could not reach {} over SSH:\n{}",
            opts.target,
            combined.trim()
        );
    }
    // Past the sentinel the installer runs and kexecs; the connection dropping
    // is expected. The Box coming up (next step) is the real success signal.
    Ok(())
}

fn wait_for_box(address: &str, timeout_secs: u64) -> Result<()> {
    let url = format!("http://{address}/api/v1/health");
    let deadline = Duration::from_secs(timeout_secs);
    let start = Instant::now();
    let mut last = String::new();
    while start.elapsed() < deadline {
        std::thread::sleep(Duration::from_secs(5));
        match Command::new("curl")
            .args(["-fsS", "-m", "5", &url])
            .output()
        {
            Ok(o) if o.status.success() => {
                if serde_json::from_slice::<Value>(&o.stdout)
                    .map(|v| v.is_object())
                    .unwrap_or(false)
                {
                    return Ok(());
                }
            }
            Ok(o) => last = String::from_utf8_lossy(&o.stderr).trim().to_string(),
            Err(e) => last = e.to_string(),
        }
    }
    bail!(
        "timed out after {timeout_secs}s waiting for {address} to come up as a Box. \
         Last probe: {}",
        if last.is_empty() {
            "no response".into()
        } else {
            last
        }
    )
}

fn redeem_over_http(address: &str, code: &str) -> Result<String> {
    let url = format!("http://{address}/pair/redeem");
    let out = Command::new("curl")
        .args(["-fsS", "-X", "POST", &url])
        .args(["-H", "accept: application/json"])
        .args(["--data-urlencode", &format!("code={code}")])
        .output()
        .context("running curl to redeem the pairing code")?;
    if !out.status.success() {
        bail!(
            "pairing the agent failed: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    let v: Value = serde_json::from_slice(&out.stdout).context("pair/redeem returned non-JSON")?;
    v.get("token")
        .and_then(Value::as_str)
        .map(String::from)
        .context("pair/redeem returned no token")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn base64_matches_known_vectors() {
        assert_eq!(base64_encode(b""), "");
        assert_eq!(base64_encode(b"box"), "Ym94");
        assert_eq!(base64_encode(b"Man"), "TWFu");
        assert_eq!(base64_encode(b"hello"), "aGVsbG8=");
        assert_eq!(
            base64_encode(b"any carnal pleasure."),
            "YW55IGNhcm5hbCBwbGVhc3VyZS4="
        );
    }

    #[test]
    fn orders_carry_consent_identity_and_pairing() {
        let keys = vec!["ssh-ed25519 AAAA... agent".to_string()];
        let o = build_orders("kitchen", &keys, "deadbeef", None);
        assert_eq!(o["erase_disk"], json!(true));
        assert_eq!(o["hostname"], json!("kitchen"));
        assert_eq!(o["ssh_authorized_keys"], json!(keys));
        assert_eq!(o["enrollment_code_hash"], json!("deadbeef"));
        assert!(o.get("storage").is_none());
    }

    #[test]
    fn orders_include_layout_when_requested() {
        let o = build_orders("auto", &[], "h", Some("mirror"));
        assert_eq!(o["storage"]["layout"], json!("mirror"));
    }

    #[test]
    fn remote_command_embeds_orders_and_falls_back_to_sudo() {
        let cmd = remote_install_command("https://ex/install.sh", "T3JkZXJz");
        assert!(cmd.contains("https://ex/install.sh"));
        assert!(cmd.contains("BOX_ORDERS_B64=T3JkZXJz"));
        assert!(cmd.contains("id -u")); // runs as root directly, else sudo -n
        assert!(cmd.contains("sudo -n"));
        // Non-interactive over SSH: without carried consent the installer has
        // no terminal to ask on and correctly refuses to wipe anything.
        assert!(cmd.contains("BOX_YES=1"));
    }

    #[test]
    fn host_of_strips_user() {
        assert_eq!(host_of("root@192.168.1.42"), "192.168.1.42");
        assert_eq!(host_of("192.168.1.42"), "192.168.1.42");
        assert_eq!(host_of("box.local"), "box.local");
    }

    #[test]
    fn resolve_keys_accepts_literals_and_rejects_junk() {
        let keys = resolve_ssh_keys(vec!["ssh-ed25519 AAAAC3Nz agent@host".to_string()]).unwrap();
        assert_eq!(keys.len(), 1);
        assert!(resolve_ssh_keys(vec!["not-a-key".to_string()]).is_err());
    }

    #[test]
    fn sha256_matches_known_vector() {
        assert_eq!(
            sha256_hex("abc"),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
    }
}
