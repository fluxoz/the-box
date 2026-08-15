//! The installer wizard's discovery surface: `/api/hello` is the unauthenticated
//! identity an agent reads after finding an unclaimed installer on the LAN (the
//! mDNS beacon's HTTP counterpart), while the destructive endpoints stay behind
//! the setup PIN.

use axum::body::Body;
use axum::http::{Request, StatusCode};
use axum::Router;
use boxd::web::wizard::{router, WizardCfg};
use http_body_util::BodyExt;
use serde_json::Value;
use tempfile::TempDir;
use tower::ServiceExt;

fn app(pin: Option<&str>) -> (TempDir, Router) {
    let tmp = TempDir::new().unwrap();
    let p = |n: &str| tmp.path().join(n);
    let cfg = WizardCfg {
        orders_out: p("orders.json"),
        disko_out: p("disko.nix"),
        commit_flag: p("commit"),
        progress: p("progress.log"),
        done: p("done"),
        base_orders: None,
        pin: pin.map(String::from),
    };
    (tmp, router(cfg))
}

async fn get(app: &Router, path: &str, pin: Option<&str>) -> (StatusCode, Value) {
    let mut b = Request::get(path);
    if let Some(p) = pin {
        b = b.header("x-setup-pin", p);
    }
    let resp = app
        .clone()
        .oneshot(b.body(Body::empty()).unwrap())
        .await
        .unwrap();
    let status = resp.status();
    let bytes = resp.into_body().collect().await.unwrap().to_bytes();
    (
        status,
        serde_json::from_slice(&bytes).unwrap_or(Value::Null),
    )
}

#[tokio::test]
async fn hello_is_public_and_identifies_an_unclaimed_installer() {
    let (_t, app) = app(Some("123456"));

    // hello needs no PIN and tells an agent this is a Box installer, unclaimed,
    // and that a setup PIN is required before the destructive endpoints.
    let (s, v) = get(&app, "/api/hello", None).await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(v["thebox"], "installer");
    assert_eq!(v["state"], "unclaimed");
    assert_eq!(v["pin_required"], true);
    assert_eq!(v["port"], 2693);

    // The destructive endpoints stay behind the PIN.
    let (s, _) = get(&app, "/api/probe", None).await;
    assert_eq!(s, StatusCode::FORBIDDEN);
    let (s, _) = get(&app, "/api/probe", Some("wrong")).await;
    assert_eq!(s, StatusCode::FORBIDDEN);

    // The correct PIN passes the gate (probe itself may find no disks in the
    // sandbox, but that is a 200 body, never a 403).
    let (s, _) = get(&app, "/api/probe", Some("123456")).await;
    assert_ne!(s, StatusCode::FORBIDDEN);
}

#[tokio::test]
async fn hello_reports_no_pin_on_an_open_installer() {
    let (_t, app) = app(None);
    let (s, v) = get(&app, "/api/hello", None).await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(v["pin_required"], false);
}
