//! Drives the full router in-memory: MCP protocol handshake, tool calls,
//! host-based routing of tunnel traffic, and rollback via MCP.

use axum::body::Body;
use axum::http::{Request, StatusCode};
use axum::Router;
use boxd::paths::Paths;
use boxd::store::local::LocalBuilder;
use boxd::web::{self, AppState};
use http_body_util::BodyExt;
use serde_json::{json, Value};
use tempfile::TempDir;
use tower::ServiceExt;

fn app() -> (TempDir, Router) {
    let tmp = TempDir::new().unwrap();
    let paths = Paths::new(tmp.path().to_path_buf());
    paths.ensure().unwrap();
    let builder = LocalBuilder::new(&paths);
    let state = AppState::new(paths, Box::new(builder));
    (tmp, web::router(state))
}

async fn rpc(app: &Router, body: Value) -> Value {
    let response = app
        .clone()
        .oneshot(
            Request::post("/mcp")
                .header("content-type", "application/json")
                .body(Body::from(body.to_string()))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    serde_json::from_slice(&bytes).unwrap()
}

async fn call_tool(app: &Router, name: &str, arguments: Value) -> Value {
    rpc(
        app,
        json!({
            "jsonrpc": "2.0", "id": 1, "method": "tools/call",
            "params": { "name": name, "arguments": arguments },
        }),
    )
    .await
}

async fn get(app: &Router, path: &str, host: &str) -> (StatusCode, String) {
    let response = app
        .clone()
        .oneshot(
            Request::get(path)
                .header("host", host)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    let status = response.status();
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    (status, String::from_utf8_lossy(&bytes).to_string())
}

#[tokio::test]
async fn mcp_handshake_and_tools() {
    let (_tmp, app) = app();

    let init = rpc(
        &app,
        json!({
            "jsonrpc": "2.0", "id": 0, "method": "initialize",
            "params": {
                "protocolVersion": "2025-06-18",
                "capabilities": {},
                "clientInfo": { "name": "test-client", "version": "0.0.0" },
            },
        }),
    )
    .await;
    assert_eq!(init["result"]["protocolVersion"], "2025-06-18");
    assert_eq!(init["result"]["serverInfo"]["name"], "the-box");

    // notifications/initialized is a notification: accepted, no body
    let response = app
        .clone()
        .oneshot(
            Request::post("/mcp")
                .header("content-type", "application/json")
                .body(Body::from(
                    json!({"jsonrpc": "2.0", "method": "notifications/initialized"}).to_string(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::ACCEPTED);

    let tools = rpc(
        &app,
        json!({"jsonrpc": "2.0", "id": 1, "method": "tools/list"}),
    )
    .await;
    let names: Vec<&str> = tools["result"]["tools"]
        .as_array()
        .unwrap()
        .iter()
        .map(|t| t["name"].as_str().unwrap())
        .collect();
    for expected in [
        "get_status",
        "list_services",
        "deploy_static_site",
        "delete_service",
        "list_generations",
        "rollback",
    ] {
        assert!(names.contains(&expected), "missing tool {expected}");
    }

    let unknown = rpc(&app, json!({"jsonrpc": "2.0", "id": 2, "method": "nope"})).await;
    assert_eq!(unknown["error"]["code"], -32601);
}

#[tokio::test]
async fn mcp_deploy_host_routing_and_rollback() {
    let (_tmp, app) = app();

    // Deploy v1 with a public domain via MCP.
    let deploy = call_tool(
        &app,
        "deploy_static_site",
        json!({ "name": "hello", "index_html": "<h1>v1 via mcp</h1>", "domain": "hello.example.com" }),
    )
    .await;
    assert_eq!(deploy["result"]["isError"], false, "{deploy}");

    // Local path routing still works.
    let (status, body) = get(&app, "/sites/hello/", "127.0.0.1:2693").await;
    assert_eq!(status, StatusCode::OK);
    assert!(body.contains("v1 via mcp"));

    // Tunnel traffic: Host header routes straight to the service root.
    let (status, body) = get(&app, "/", "hello.example.com").await;
    assert_eq!(status, StatusCode::OK);
    assert!(body.contains("v1 via mcp"));

    // The dashboard must NOT be reachable through a service domain...
    let (_, body) = get(&app, "/generations", "hello.example.com").await;
    assert!(
        !body.contains("The Box"),
        "dashboard leaked through tunnel host"
    );

    // ...but stays reachable locally.
    let (status, body) = get(&app, "/", "localhost:2693").await;
    assert_eq!(status, StatusCode::OK);
    assert!(body.contains("The Box"));

    // Update to v2, then roll back via MCP and confirm the host serves v1.
    let deploy2 = call_tool(
        &app,
        "deploy_static_site",
        json!({ "name": "hello", "index_html": "<h1>v2 via mcp</h1>", "domain": "hello.example.com" }),
    )
    .await;
    assert_eq!(deploy2["result"]["isError"], false, "{deploy2}");
    let (_, body) = get(&app, "/", "hello.example.com").await;
    assert!(body.contains("v2 via mcp"));

    let rollback = call_tool(&app, "rollback", json!({ "generation": 1 })).await;
    assert_eq!(rollback["result"]["isError"], false, "{rollback}");
    let (_, body) = get(&app, "/", "hello.example.com").await;
    assert!(body.contains("v1 via mcp"));

    // Domain conflicts are rejected as tool errors, not crashes.
    let conflict = call_tool(
        &app,
        "deploy_static_site",
        json!({ "name": "other", "index_html": "<p>x</p>", "domain": "hello.example.com" }),
    )
    .await;
    assert_eq!(conflict["result"]["isError"], true);

    // Invalid domains (that could shadow local access) are rejected.
    let bad_domain = call_tool(
        &app,
        "deploy_static_site",
        json!({ "name": "evil", "index_html": "<p>x</p>", "domain": "localhost" }),
    )
    .await;
    assert_eq!(bad_domain["result"]["isError"], true);

    // Rollback to a missing generation is a tool error.
    let missing = call_tool(&app, "rollback", json!({ "generation": 99 })).await;
    assert_eq!(missing["result"]["isError"], true);
}
