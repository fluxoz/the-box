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
// Loopback is NOT authority any more: every service the Box runs can also reach
// 127.0.0.1:2693, so a deployed app could otherwise mint a pairing code and
// take the console over. Authority comes from holding a session. Alongside
// that: a cross-site write is refused, a source tree cannot publish the Box's
// secrets, a container cannot mount the host, and a service the operator did
// not publish is not served through the tunnel.
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
async fn loopback_does_not_grant_authority() {
    let (_tmp, paths, app) = app();

    // Precondition: no operator has ever paired. Zero sessions, zero codes.
    assert!(boxd::auth::is_claimable(&paths));
    assert!(boxd::auth::list(&paths).is_empty(), "no credentials exist");

    // Reaching the loopback interface used to BE the credential. It is not any
    // more, and it must not become one again: every service the Box runs can
    // reach 127.0.0.1:2693 too, so this was a path from "deploy an app" to
    // "own the console" (proven: /devices/add minted a pairing code and handed
    // it back in the redirect).
    for (method, path, body) in [
        ("GET", "/api/v1/status", None),
        ("GET", "/", None),
        (
            "POST",
            "/api/v1/services",
            Some(json!({"name":"pwned","index_html":"<h1>owned</h1>"}).to_string()),
        ),
        ("POST", "/devices/add", Some(String::new())),
        ("POST", "/system/channel", Some(String::new())),
    ] {
        let req = Request::builder()
            .method(method)
            .uri(path)
            .header("content-type", "application/json")
            .extension(loopback());
        let (s, b, _) = send(
            &app,
            req.body(body.map(Body::from).unwrap_or(Body::empty()))
                .unwrap(),
        )
        .await;
        println!("[loopback] {method} {path} -> {s}");
        assert_ne!(
            s,
            StatusCode::OK,
            "REGRESSION: loopback carried authority for {method} {path}: {b}"
        );
    }

    // Nothing was created by any of that.
    assert!(boxd::auth::list(&paths).is_empty(), "a session appeared");
    assert!(boxd::auth::is_claimable(&paths), "the box was claimed");

    // A real session still works, from anywhere.
    let token = boxd::auth::mint_session(&paths, "test").unwrap();
    let (s, _, _) = send(
        &app,
        Request::get("/api/v1/status")
            .header("authorization", format!("Bearer {token}"))
            .extension(lan())
            .body(Body::empty())
            .unwrap(),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "a valid session must still be honoured");
}

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

/// The console offers, per service, "Let people outside your home reach it".
/// Nothing consulted that flag when serving: a site left unpublished was served
/// to anyone who reached the Box through its tunnel, both by its own domain and
/// at `/sites/<name>/`. On your own network these paths stay credential-free —
/// that is what the console promises — but tunnel traffic only gets what the
/// operator actually published.
#[tokio::test]
async fn unpublished_sites_are_not_served_through_the_tunnel() {
    let (_tmp, paths, app) = app();
    let token = boxd::auth::mint_session(&paths, "test").unwrap();

    for (name, public) in [("secret", false), ("shared", true)] {
        let (status, body, _) = send(
            &app,
            Request::post("/api/v1/services")
                .header("authorization", format!("Bearer {token}"))
                .header("content-type", "application/json")
                .extension(lan())
                .body(Body::from(
                    json!({
                        "name": name,
                        "template": "static-site",
                        "params": { "index_html": format!("<h1>{name}</h1>") },
                        "domain": format!("{name}.example.com"),
                        "public": public,
                    })
                    .to_string(),
                ))
                .unwrap(),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "deploying {name} failed: {body}");
    }

    // --- through the tunnel (a proxy header is what marks that traffic) ---
    for (path, host) in [("/sites/secret/", "box.local"), ("/", "secret.example.com")] {
        let (status, _, _) = send(
            &app,
            Request::get(path)
                .header("host", host)
                .header("x-forwarded-for", "203.0.113.9")
                .extension(lan())
                .body(Body::empty())
                .unwrap(),
        )
        .await;
        assert_eq!(
            status,
            StatusCode::NOT_FOUND,
            "REGRESSION: unpublished site served through the tunnel via {path} (host {host})"
        );
    }

    // What the operator DID publish still answers.
    let (status, body, _) = send(
        &app,
        Request::get("/")
            .header("host", "shared.example.com")
            .header("x-forwarded-for", "203.0.113.9")
            .extension(lan())
            .body(Body::empty())
            .unwrap(),
    )
    .await;
    assert_eq!(
        status,
        StatusCode::OK,
        "published site must still be served"
    );
    assert!(body.contains("shared"), "{body}");

    // On your own network, both are reachable with no credential — unchanged.
    for name in ["secret", "shared"] {
        let (status, body, _) = send(
            &app,
            Request::get(format!("/sites/{name}/"))
                .header("host", "box.local")
                .extension(lan())
                .body(Body::empty())
                .unwrap(),
        )
        .await;
        assert_eq!(
            status,
            StatusCode::OK,
            "LAN access to {name} must be unchanged"
        );
        assert!(body.contains(name), "{body}");
    }
}
