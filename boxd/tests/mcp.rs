//! Drives the full router in-memory: MCP protocol handshake, tool calls,
//! host-based routing of tunnel traffic, and rollback via MCP.

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

/// In-memory requests present as loopback so the auth middleware trusts them,
/// matching how local access actually reaches the daemon.
fn local() -> ConnectInfo<SocketAddr> {
    ConnectInfo(SocketAddr::from(([127, 0, 0, 1], 40000)))
}

fn app() -> (TempDir, Router, String) {
    let tmp = TempDir::new().unwrap();
    let paths = Paths::new(tmp.path().to_path_buf());
    paths.ensure().unwrap();
    let builder = LocalBuilder::new(&paths);
    let token = boxd::auth::mint_session(&paths, "test-agent").unwrap();
    let state = AppState::new(paths, Box::new(builder));
    (tmp, web::router(state), token)
}

async fn rpc(app: &Router, token: &str, body: Value) -> Value {
    let response = app
        .clone()
        .oneshot(
            Request::post("/mcp")
                .header("content-type", "application/json")
                .header("authorization", format!("Bearer {token}"))
                .extension(local())
                .body(Body::from(body.to_string()))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    serde_json::from_slice(&bytes).unwrap()
}

async fn call_tool(app: &Router, token: &str, name: &str, arguments: Value) -> Value {
    rpc(
        app,
        token,
        json!({
            "jsonrpc": "2.0", "id": 1, "method": "tools/call",
            "params": { "name": name, "arguments": arguments },
        }),
    )
    .await
}

/// GET as an operator. `token` is None for requests that should be judged
/// without credentials (a public site, or checking that the console refuses).
async fn get(app: &Router, path: &str, host: &str, token: Option<&str>) -> (StatusCode, String) {
    let mut req = Request::get(path).header("host", host);
    if let Some(t) = token {
        req = req.header("authorization", format!("Bearer {t}"));
    }
    let response = app
        .clone()
        .oneshot(
            req.extension(local())
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
    let (_tmp, app, token) = app();

    let init = rpc(
        &app,
        &token,
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
                .header("authorization", format!("Bearer {token}"))
                .extension(local())
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
        &token,
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
        "forge_options",
        "forge_connect",
        "forge_connect_status",
        "forge_repos",
        "forge_disconnect",
    ] {
        assert!(names.contains(&expected), "missing tool {expected}");
    }

    let unknown = rpc(&app, &token, json!({"jsonrpc": "2.0", "id": 2, "method": "nope"})).await;
    assert_eq!(unknown["error"]["code"], -32601);
}

/// The forge tools an agent touches before any network call is possible.
///
/// A Box that ships no application registration is the normal state of this
/// code until one exists, so what matters most here is that every refusal names
/// the missing thing. An agent that reads "unauthorized" will retry forever; an
/// agent that reads "tick Enable Device Flow" can tell the person what to do.
#[tokio::test]
async fn mcp_forge_tools_refuse_with_the_missing_piece_named() {
    let (_tmp, app, token) = app();

    let text = |v: &Value| v["result"]["content"][0]["text"].as_str().unwrap().to_string();

    // The catalogue is honest about the difference between the two forges.
    let options = call_tool(&app, &token, "forge_options", json!({})).await;
    assert_eq!(options["result"]["isError"], false, "{options}");
    let listed: Value = serde_json::from_str(&text(&options)).unwrap();
    let by_id = |id: &str| {
        listed
            .as_array()
            .unwrap()
            .iter()
            .find(|f| f["id"] == id)
            .cloned()
            .unwrap_or_else(|| panic!("no {id} in forge_options"))
    };
    let github = by_id("github");
    assert_eq!(github["connected"], false);
    assert_eq!(github["shares_only_chosen_repos"], true);
    // GitLab grants by scope, so promising per-repo consent there would be a lie.
    assert_eq!(by_id("gitlab")["shares_only_chosen_repos"], false);
    // A stock Box ships a GitHub App registration, so this needs no setup...
    assert_eq!(github["ready_to_connect"], true);
    // ...and GitLab cannot ship one, because applications are per instance.
    assert_eq!(by_id("gitlab")["ready_to_connect"], false);

    // A self-hosted GitLab can never borrow a gitlab.com registration, and the
    // refusal says which instance it is talking about.
    let gl = call_tool(
        &app,
        &token,
        "forge_connect",
        json!({ "provider": "gitlab", "base_url": "https://git.example.com" }),
    )
    .await;
    assert_eq!(gl["result"]["isError"], true, "{gl}");
    assert!(text(&gl).contains("git.example.com"), "{}", text(&gl));
    assert!(text(&gl).contains("Confidential"), "{}", text(&gl));

    // Even though starting failed, the base_url they supplied was kept — losing
    // it would make them type it again on every retry.
    let options = call_tool(&app, &token, "forge_options", json!({})).await;
    let listed: Value = serde_json::from_str(&text(&options)).unwrap();
    let stored = listed
        .as_array()
        .unwrap()
        .iter()
        .find(|f| f["id"] == "gitlab")
        .unwrap()
        .clone();
    assert_eq!(stored["connected"], false);

    // Nothing started, nothing held.
    let status = call_tool(
        &app,
        &token,
        "forge_connect_status",
        json!({ "provider": "github" }),
    )
    .await;
    assert_eq!(status["result"]["isError"], false, "{status}");
    assert!(text(&status).contains("disconnected"), "{}", text(&status));

    // Listing repositories without an account says so, rather than returning
    // an empty list that reads as "you have no repositories".
    let repos = call_tool(&app, &token, "forge_repos", json!({ "provider": "github" })).await;
    assert_eq!(repos["result"]["isError"], true, "{repos}");
    assert!(text(&repos).contains("connected"), "{}", text(&repos));

    // Disconnecting must not imply the grant is gone from the forge.
    let off = call_tool(
        &app,
        &token,
        "forge_disconnect",
        json!({ "provider": "github" }),
    )
    .await;
    assert_eq!(off["result"]["isError"], false, "{off}");
    assert!(text(&off).contains("NOT"), "{}", text(&off));
}

#[tokio::test]
async fn mcp_deploy_host_routing_and_rollback() {
    let (_tmp, app, token) = app();

    // Deploy v1 with a public domain via MCP.
    let deploy = call_tool(
        &app,
        &token,
        "deploy_static_site",
        json!({ "name": "hello", "index_html": "<h1>v1 via mcp</h1>", "domain": "hello.example.com" }),
    )
    .await;
    assert_eq!(deploy["result"]["isError"], false, "{deploy}");

    // Local path routing still works.
    let (status, body) = get(&app, "/sites/hello/", "127.0.0.1:2693", None).await;
    assert_eq!(status, StatusCode::OK);
    assert!(body.contains("v1 via mcp"));

    // Tunnel traffic: Host header routes straight to the service root.
    let (status, body) = get(&app, "/", "hello.example.com", None).await;
    assert_eq!(status, StatusCode::OK);
    assert!(body.contains("v1 via mcp"));

    // The dashboard must NOT be reachable through a service domain...
    let (_, body) = get(&app, "/generations", "hello.example.com", None).await;
    assert!(
        !body.contains("The Box"),
        "dashboard leaked through tunnel host"
    );

    // ...and now needs a session even from this machine: reaching loopback is
    // not authority, because every service the Box runs can reach it too.
    let (status, _) = get(&app, "/", "localhost:2693", None).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);

    // With an operator session it is reachable as before.
    let (status, body) = get(&app, "/", "localhost:2693", Some(&token)).await;
    assert_eq!(status, StatusCode::OK);
    assert!(body.contains("The Box"));

    // Update to v2, then roll back via MCP and confirm the host serves v1.
    let deploy2 = call_tool(
        &app,
        &token,
        "deploy_static_site",
        json!({ "name": "hello", "index_html": "<h1>v2 via mcp</h1>", "domain": "hello.example.com" }),
    )
    .await;
    assert_eq!(deploy2["result"]["isError"], false, "{deploy2}");
    let (_, body) = get(&app, "/", "hello.example.com", None).await;
    assert!(body.contains("v2 via mcp"));

    let rollback = call_tool(&app, &token, "rollback", json!({ "generation": 1 })).await;
    assert_eq!(rollback["result"]["isError"], false, "{rollback}");
    let (_, body) = get(&app, "/", "hello.example.com", None).await;
    assert!(body.contains("v1 via mcp"));

    // Domain conflicts are rejected as tool errors, not crashes.
    let conflict = call_tool(
        &app,
        &token,
        "deploy_static_site",
        json!({ "name": "other", "index_html": "<p>x</p>", "domain": "hello.example.com" }),
    )
    .await;
    assert_eq!(conflict["result"]["isError"], true);

    // Invalid domains (that could shadow local access) are rejected.
    let bad_domain = call_tool(
        &app,
        &token,
        "deploy_static_site",
        json!({ "name": "evil", "index_html": "<p>x</p>", "domain": "localhost" }),
    )
    .await;
    assert_eq!(bad_domain["result"]["isError"], true);

    // Rollback to a missing generation is a tool error.
    let missing = call_tool(&app, &token, "rollback", json!({ "generation": 99 })).await;
    assert_eq!(missing["result"]["isError"], true);
}
