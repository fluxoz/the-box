//! The one-liner (`site/connect`) against a real boxd over real HTTP: the
//! script is the funnel's front door, so it gets tested like one. Both doors:
//! a fresh Box (claim, with consent) and a claimed Box (pairing code), each
//! ending with an MCP config on disk whose token actually drives /mcp.

use std::process::{Command, Stdio};
use std::time::Duration;
use tempfile::TempDir;

fn free_port() -> u16 {
    std::net::TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port()
}

fn curl(args: &[&str]) -> (bool, String) {
    let out = Command::new("curl").args(args).output().unwrap();
    (
        out.status.success(),
        String::from_utf8_lossy(&out.stdout).to_string(),
    )
}

struct Server {
    child: std::process::Child,
    addr: String,
    _data: TempDir,
    data_path: std::path::PathBuf,
}

impl Drop for Server {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// The tools the script itself needs. The nix package build runs tests in a
/// sandbox without them; the devshell (and CI's cargo-test job) has both, so
/// the real coverage happens there and the package build skips honestly.
fn script_deps_present() -> bool {
    let have = |c: &str| {
        Command::new(c)
            .arg("--version")
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .is_ok()
    };
    let ok = have("curl") && have("python3");
    if !ok {
        eprintln!("skipping: site/connect needs curl + python3, absent in this build environment");
    }
    ok
}

fn serve() -> Server {
    let data = TempDir::new().unwrap();
    let port = free_port();
    let addr = format!("127.0.0.1:{port}");
    let data_path = data.path().to_path_buf();
    let child = Command::new(env!("CARGO_BIN_EXE_boxd"))
        .args(["--data-dir", data.path().to_str().unwrap(), "serve", "--listen", &addr])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawning boxd serve");
    // Into the struct immediately: every exit from here on — including the
    // panic below — goes through Drop, which kills and reaps the child.
    let server = Server { child, addr, _data: data, data_path };
    for _ in 0..100 {
        let (ok, _) = curl(&["-fsS", "-m", "2", &format!("http://{}/api/v1/health", server.addr)]);
        if ok {
            return server;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    panic!("boxd serve never became healthy on {}", server.addr);
}

fn run_connect(server: &Server, home: &TempDir, extra_env: &[(&str, &str)]) -> (bool, String) {
    let script = concat!(env!("CARGO_MANIFEST_DIR"), "/../site/connect");
    let mut cmd = Command::new("sh");
    cmd.arg(script)
        .env("THEBOX_ADDR", &server.addr)
        .env("THEBOX_YES", "1")
        .env("THEBOX_SKIP_CLAUDE_CLI", "1")
        .env("HOME", home.path());
    for (k, v) in extra_env {
        cmd.env(k, v);
    }
    let out = cmd.output().expect("running site/connect");
    let text = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    (out.status.success(), text)
}

fn cursor_token(home: &TempDir) -> String {
    let cfg: serde_json::Value = serde_json::from_str(
        &std::fs::read_to_string(home.path().join(".cursor/mcp.json")).expect("cursor config"),
    )
    .expect("cursor config parses");
    cfg["mcpServers"]["the-box"]["headers"]["Authorization"]
        .as_str()
        .expect("authorization header")
        .strip_prefix("Bearer ")
        .expect("bearer prefix")
        .to_string()
}

fn mcp_tools_list(addr: &str, token: &str) -> bool {
    let (ok, body) = curl(&[
        "-fsS", "-m", "5", "-X", "POST",
        "-H", &format!("Authorization: Bearer {token}"),
        "-H", "Content-Type: application/json",
        "-d", r#"{"jsonrpc":"2.0","id":1,"method":"tools/list"}"#,
        &format!("http://{addr}/mcp"),
    ]);
    ok && body.contains("link_repo")
}

#[test]
fn the_one_liner_claims_a_fresh_box_and_wires_the_agent() {
    if !script_deps_present() {
        return;
    }
    let server = serve();
    let home = TempDir::new().unwrap();
    // The editors the script should notice.
    std::fs::create_dir_all(home.path().join(".cursor")).unwrap();
    std::fs::create_dir_all(home.path().join(".codeium/windsurf")).unwrap();

    let (ok, out) = run_connect(&server, &home, &[]);
    assert!(ok, "connect failed:\n{out}");
    assert!(out.contains("yours now"), "should claim the fresh box: {out}");

    // Both configs written, same working token.
    let token = cursor_token(&home);
    let ws = std::fs::read_to_string(home.path().join(".codeium/windsurf/mcp_config.json")).unwrap();
    assert!(ws.contains(&token), "windsurf config carries the same session");
    assert!(
        mcp_tools_list(&server.addr, &token),
        "the token from the written config must drive /mcp"
    );

    // And a second run against the now-claimed box, with a pairing code the
    // operator minted — the other door.
    let out = Command::new(env!("CARGO_BIN_EXE_boxd"))
        .args(["--data-dir", server.data_path.to_str().unwrap(), "auth", "enroll"])
        .output()
        .unwrap();
    let text = String::from_utf8_lossy(&out.stdout).to_string();
    let code = text
        .split_whitespace()
        .find(|w| w.len() == 10 && w.chars().all(|c| c.is_ascii_hexdigit()))
        .unwrap_or_else(|| panic!("no code in `auth enroll` output: {text}"))
        .to_string();

    let home2 = TempDir::new().unwrap();
    std::fs::create_dir_all(home2.path().join(".cursor")).unwrap();
    let (ok, out) = run_connect(
        &server,
        &home2,
        &[("THEBOX_CODE", code.as_str()), ("THEBOX_LABEL", "second-laptop")],
    );
    assert!(ok, "connect (claimed box) failed:\n{out}");
    let token2 = cursor_token(&home2);
    assert_ne!(token, token2, "each pairing is its own session");
    assert!(mcp_tools_list(&server.addr, &token2));

    // The label the script sent is what the operator sees in the device list.
    let (ok, devices) = curl(&[
        "-fsS", "-m", "5",
        "-H", &format!("Authorization: Bearer {token}"),
        &format!("http://{}/devices", server.addr),
    ]);
    assert!(ok);
    assert!(devices.contains("second-laptop"), "session label must show up: {devices}");
}

#[test]
fn a_wrong_code_fails_with_the_retry_story() {
    if !script_deps_present() {
        return;
    }
    let server = serve();
    // Claim it first so the code door is the one being tested.
    let (ok, _) = curl(&[
        "-fsS", "-m", "5", "-X", "POST", "-H", "Accept: application/json",
        &format!("http://{}/pair/claim", server.addr),
    ]);
    assert!(ok, "claiming for setup");

    let home = TempDir::new().unwrap();
    let (ok, out) = run_connect(&server, &home, &[("THEBOX_CODE", "not-a-real-code")]);
    assert!(!ok, "a bad code must fail the script");
    assert!(out.contains("expire"), "the failure must carry the retry story: {out}");
    assert!(
        !home.path().join(".cursor/mcp.json").exists(),
        "no config may be written without a session"
    );
}
