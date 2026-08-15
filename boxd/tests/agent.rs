//! The post-provision agent loop, end-to-end over the real router but from
//! *off-box* (a non-loopback peer, so trusted-local never applies). This is the
//! half `boxd provision` hands to the agent: redeem the one-time pairing code
//! over HTTP for a session token, then configure and maintain the Box over MCP
//! with that token as a Bearer header — exactly what a remote agent does after a
//! freshly provisioned Box comes up.

use axum::body::Body;
use axum::extract::ConnectInfo;
use axum::http::{Request, StatusCode};
use axum::Router;
use boxd::paths::Paths;
use boxd::store::local::LocalBuilder;
use boxd::web::{self, AppState};
use http_body_util::BodyExt;
use serde_json::{json, Value};
use std::net::SocketAddr;
use tempfile::TempDir;
use tower::ServiceExt;

/// A public (TEST-NET-3) peer — never loopback, so the request is treated like
/// any remote agent and must carry a session token.
fn remote() -> ConnectInfo<SocketAddr> {
    ConnectInfo(SocketAddr::from(([203, 0, 113, 7], 54321)))
}

fn app() -> (TempDir, Paths, Router) {
    let tmp = TempDir::new().unwrap();
    let paths = Paths::new(tmp.path().to_path_buf());
    paths.ensure().unwrap();
    let builder = LocalBuilder::new(&paths);
    let state = AppState::new(paths.clone(), Box::new(builder));
    (tmp, paths, web::router(state))
}

async fn mcp_post(app: &Router, body: Value, bearer: Option<&str>) -> (StatusCode, Value) {
    let mut b = Request::post("/mcp")
        .header("content-type", "application/json")
        .extension(remote());
    if let Some(t) = bearer {
        b = b.header("authorization", format!("Bearer {t}"));
    }
    let response = app
        .clone()
        .oneshot(b.body(Body::from(body.to_string())).unwrap())
        .await
        .unwrap();
    let status = response.status();
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    (
        status,
        serde_json::from_slice(&bytes).unwrap_or(Value::Null),
    )
}

async fn redeem(app: &Router, content_type: &str, body: String) -> (StatusCode, Value) {
    let response = app
        .clone()
        .oneshot(
            Request::post("/pair/redeem")
                .header("content-type", content_type)
                .header("accept", "application/json")
                .extension(remote())
                .body(Body::from(body))
                .unwrap(),
        )
        .await
        .unwrap();
    let status = response.status();
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    (
        status,
        serde_json::from_slice(&bytes).unwrap_or(Value::Null),
    )
}

/// What the 401 signpost tells agents to send: a JSON body. This exact request
/// was once refused by a form-only parser — the Box handing out instructions
/// its own front door rejected.
async fn redeem_json(app: &Router, code: &str) -> (StatusCode, Value) {
    redeem(app, "application/json", json!({ "code": code }).to_string()).await
}

/// What the /pair browser form sends.
async fn redeem_form(app: &Router, code: &str) -> (StatusCode, Value) {
    redeem(
        app,
        "application/x-www-form-urlencoded",
        format!("code={code}"),
    )
    .await
}

async fn call_tool(app: &Router, token: &str, name: &str, arguments: Value) -> Value {
    let (status, v) = mcp_post(
        app,
        json!({
            "jsonrpc": "2.0", "id": 1, "method": "tools/call",
            "params": { "name": name, "arguments": arguments },
        }),
        Some(token),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "tool {name}: {v}");
    v
}

#[tokio::test]
async fn agent_pairs_over_http_then_drives_mcp_from_off_box() {
    let (_tmp, paths, app) = app();

    // 1. Off-box and unauthenticated: /mcp is refused.
    let (status, _) = mcp_post(
        &app,
        json!({"jsonrpc": "2.0", "id": 1, "method": "tools/list"}),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);

    // 2. A Box comes up with an enrollment code seeded from the install handoff
    //    (what `boxd provision` puts in the orders). The agent redeems it over
    //    HTTP for a session token — asking for JSON gets the token in the body,
    //    no browser cookie, no SSH.
    let code = boxd::auth::mint_code(&paths, "enrollment").unwrap();
    let (status, body) = redeem_json(&app, &code).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let token = body["token"]
        .as_str()
        .expect("token in redeem response")
        .to_string();

    // The pairing code is single-use: a replay is refused.
    let (status, _) = redeem_json(&app, &code).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);

    // Browsers speak forms at the same door: a fresh code redeems that way too.
    let code2 = boxd::auth::mint_code(&paths, "enrollment").unwrap();
    let (status, body) = redeem_form(&app, &code2).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert!(body["token"].as_str().is_some(), "{body}");

    // 3. With the token as a Bearer header, the agent drives /mcp from off-box.
    let (status, init) = mcp_post(
        &app,
        json!({
            "jsonrpc": "2.0", "id": 0, "method": "initialize",
            "params": {
                "protocolVersion": "2025-06-18",
                "capabilities": {},
                "clientInfo": { "name": "agent", "version": "0" },
            },
        }),
        Some(&token),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(init["result"]["serverInfo"]["name"], "the-box");

    // Configure: deploy a service over MCP.
    let deploy = call_tool(
        &app,
        &token,
        "deploy_static_site",
        json!({ "name": "welcome", "index_html": "<h1>hi from the agent</h1>" }),
    )
    .await;
    assert_eq!(deploy["result"]["isError"], false, "{deploy}");

    // Maintain: inspect generations and status over MCP.
    let gens = call_tool(&app, &token, "list_generations", json!({})).await;
    assert_eq!(gens["result"]["isError"], false, "{gens}");
    let st = call_tool(&app, &token, "get_status", json!({})).await;
    assert_eq!(st["result"]["isError"], false, "{st}");

    // 4. A bogus token is refused, off-box.
    let (status, _) = mcp_post(
        &app,
        json!({"jsonrpc": "2.0", "id": 3, "method": "tools/list"}),
        Some("not-a-real-token"),
    )
    .await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);

    // 5. And the session shows up in the operator's device list (revocable).
    let sessions = boxd::auth::list(&paths);
    assert!(sessions.iter().any(|s| s.label == "agent"));
}

async fn get_pair(app: &Router, proxied: bool) -> (StatusCode, String) {
    let mut b = Request::get("/pair").extension(remote());
    if proxied {
        b = b.header("x-forwarded-for", "9.9.9.9");
    }
    let resp = app
        .clone()
        .oneshot(b.body(Body::empty()).unwrap())
        .await
        .unwrap();
    let status = resp.status();
    let bytes = resp.into_body().collect().await.unwrap().to_bytes();
    (status, String::from_utf8_lossy(&bytes).to_string())
}

async fn post_claim(app: &Router, proxied: bool) -> (StatusCode, Option<String>, Option<String>) {
    let mut b = Request::post("/pair/claim").extension(remote());
    if proxied {
        b = b.header("x-forwarded-for", "9.9.9.9");
    }
    let resp = app
        .clone()
        .oneshot(b.body(Body::empty()).unwrap())
        .await
        .unwrap();
    let status = resp.status();
    let s = |k: &str| {
        resp.headers()
            .get(k)
            .and_then(|v| v.to_str().ok())
            .map(String::from)
    };
    (status, s("location"), s("set-cookie"))
}

/// First-run claim: a freshly flashed Box (no code, no session) is claimable
/// from the LAN, never through a tunnel, and locks down once claimed.
#[tokio::test]
async fn first_run_claim_flow() {
    let (_tmp, paths, app) = app();

    // Fresh box on the LAN: /pair offers to claim.
    let (s, body) = get_pair(&app, false).await;
    assert_eq!(s, StatusCode::OK);
    assert!(
        body.contains("Claim this Box"),
        "fresh box should offer claim"
    );

    // Through a tunnel (proxied): no claim offered, and claiming is refused.
    let (_s, pbody) = get_pair(&app, true).await;
    assert!(
        !pbody.contains("Claim this Box"),
        "proxied /pair must not offer claim"
    );
    let (s, loc, cookie) = post_claim(&app, true).await;
    assert_eq!(s, StatusCode::SEE_OTHER);
    assert!(loc.unwrap().contains("local+network"));
    assert!(cookie.is_none());
    assert!(
        boxd::auth::is_claimable(&paths),
        "still unclaimed after a tunnel attempt"
    );

    // From the LAN: claiming mints the first session (a cookie) and redirects home.
    let (s, loc, cookie) = post_claim(&app, false).await;
    assert_eq!(s, StatusCode::SEE_OTHER);
    assert_eq!(loc.as_deref(), Some("/?ok=Box+claimed"));
    assert!(cookie
        .expect("claim sets a session cookie")
        .contains("box_session="));

    // Now claimed: /pair reverts to code entry, and a second claim is refused.
    assert!(!boxd::auth::is_claimable(&paths));
    let (_s, body) = get_pair(&app, false).await;
    assert!(!body.contains("Claim this Box"));
    assert!(body.contains("Pair this device"));
    let (_s, loc, _c) = post_claim(&app, false).await;
    assert!(loc.unwrap().contains("already+claimed"));
}
