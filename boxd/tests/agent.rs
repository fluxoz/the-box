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
    (status, serde_json::from_slice(&bytes).unwrap_or(Value::Null))
}

async fn redeem_json(app: &Router, code: &str) -> (StatusCode, Value) {
    let response = app
        .clone()
        .oneshot(
            Request::post("/pair/redeem")
                .header("content-type", "application/x-www-form-urlencoded")
                .header("accept", "application/json")
                .extension(remote())
                .body(Body::from(format!("code={code}")))
                .unwrap(),
        )
        .await
        .unwrap();
    let status = response.status();
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    (status, serde_json::from_slice(&bytes).unwrap_or(Value::Null))
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
