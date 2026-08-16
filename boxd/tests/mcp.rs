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

/// Like [`app`], but able to run build steps: phases execute directly on the
/// host (the commands below are our own), where the real thing runs them in
/// the podman sandbox. The sandbox mechanics have their own VM test.
fn app_with_builds() -> (TempDir, Router, String) {
    let tmp = TempDir::new().unwrap();
    let paths = Paths::new(tmp.path().to_path_buf());
    paths.ensure().unwrap();
    let builder = LocalBuilder::new(&paths);
    let token = boxd::auth::mint_session(&paths, "test-agent").unwrap();
    let state = AppState::with_build_exec(paths, Box::new(builder), boxd::build::BuildExec::Direct);
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
        .oneshot(req.extension(local()).body(Body::empty()).unwrap())
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
        "link_repo",
        "sync_repo",
        "unlink_repo",
        "verify_service",
        "channel_update",
        "job_status",
        "provision_machine",
    ] {
        assert!(names.contains(&expected), "missing tool {expected}");
    }

    let unknown = rpc(
        &app,
        &token,
        json!({"jsonrpc": "2.0", "id": 2, "method": "nope"}),
    )
    .await;
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

    let text = |v: &Value| {
        v["result"]["content"][0]["text"]
            .as_str()
            .unwrap()
            .to_string()
    };

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

/// The deploy loop, end to end and offline: a service linked to a git
/// repository, synced through MCP, redeployed when the repository moves.
///
/// The upstream is a real git repo on disk (file:// needs no forge account),
/// so this exercises the same fetch/checkout/deploy machinery a GitHub-linked
/// service runs — everything except the HTTPS auth header.
#[tokio::test]
async fn mcp_repo_linked_service_follows_the_repository() {
    let (_tmp, app, token) = app();

    let text = |v: &Value| {
        v["result"]["content"][0]["text"]
            .as_str()
            .unwrap()
            .to_string()
    };

    // A real upstream with a first commit.
    let upstream = TempDir::new().unwrap();
    let git = |args: &[&str]| {
        let out = std::process::Command::new("git")
            .args(args)
            .env("GIT_AUTHOR_NAME", "t")
            .env("GIT_AUTHOR_EMAIL", "t@t")
            .env("GIT_COMMITTER_NAME", "t")
            .env("GIT_COMMITTER_EMAIL", "t@t")
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "git {args:?}: {}",
            String::from_utf8_lossy(&out.stderr)
        );
    };
    let up = upstream.path().to_str().unwrap().to_string();
    git(&["init", "-q", "-b", "main", &up]);
    std::fs::write(upstream.path().join("index.html"), "pulled v1").unwrap();
    git(&["-C", &up, "add", "."]);
    git(&["-C", &up, "commit", "-qm", "v1"]);

    // The service exists first (any deploy creates it), then gains its link.
    // Linking through link_repo needs a connected forge, which an offline test
    // cannot have — so the link is written the way a restored config would
    // arrive: directly in box.toml.
    let deploy = call_tool(
        &app,
        &token,
        "deploy_static_site",
        json!({ "name": "blog", "index_html": "placeholder" }),
    )
    .await;
    assert_eq!(deploy["result"]["isError"], false, "{deploy}");

    {
        let paths = boxd::paths::Paths::new(_tmp.path().to_path_buf());
        let mut config = boxd::config::BoxConfig::load(&paths).unwrap();
        config.services[0].repo = Some(boxd::config::RepoLink {
            forge: "github".into(),
            repo: "local/blog".into(),
            clone_url: format!("file://{up}"),
            branch: "main".into(),
            subdir: None,
            build: None,
        });
        config.save(&paths).unwrap();
    }

    // First sync deploys the repository's content over the placeholder.
    let sync = call_tool(&app, &token, "sync_repo", json!({ "name": "blog" })).await;
    assert_eq!(sync["result"]["isError"], false, "{sync}");
    assert!(text(&sync).contains("deployed"), "{}", text(&sync));
    let (status, body) = get(&app, "/sites/blog/", "127.0.0.1", Some(&token)).await;
    assert_eq!(status, StatusCode::OK);
    assert!(body.contains("pulled v1"), "{body}");

    // Nothing new upstream → nothing happens, and it says so.
    let sync = call_tool(&app, &token, "sync_repo", json!({ "name": "blog" })).await;
    assert!(text(&sync).contains("up_to_date"), "{}", text(&sync));

    // Push. Sync. Live. That is the whole product.
    std::fs::write(upstream.path().join("index.html"), "pulled v2").unwrap();
    git(&["-C", &up, "add", "."]);
    git(&["-C", &up, "commit", "-qm", "v2"]);
    let sync = call_tool(&app, &token, "sync_repo", json!({ "name": "blog" })).await;
    assert!(text(&sync).contains("deployed"), "{}", text(&sync));
    let (_, body) = get(&app, "/sites/blog/", "127.0.0.1", Some(&token)).await;
    assert!(body.contains("pulled v2"), "{body}");

    // The checkout that got published is clean: no .git, no repo history.
    let paths = boxd::paths::Paths::new(_tmp.path().to_path_buf());
    assert!(paths.repo_tree_dir("blog").join("index.html").exists());
    assert!(!paths.repo_tree_dir("blog").join(".git").exists());

    // The link is visible to agents — including whether the last pull worked,
    // because a quietly-failing poller must not look like a healthy one.
    let services = call_tool(&app, &token, "list_services", json!({})).await;
    assert!(
        text(&services).contains("local/blog"),
        "{}",
        text(&services)
    );
    let parsed: Value = serde_json::from_str(&text(&services)).unwrap();
    let blog = parsed
        .as_array()
        .unwrap()
        .iter()
        .find(|s| s["name"] == "blog")
        .unwrap();
    assert_eq!(blog["last_sync"]["ok"], true, "{blog}");

    // The console's service page tells the same story: source, sync state,
    // and a live (data-poll) logs section — the "why is it not working" page.
    let (status, page) = get(&app, "/service/blog", "127.0.0.1", Some(&token)).await;
    assert_eq!(status, StatusCode::OK);
    assert!(page.contains("local/blog"), "source shown");
    assert!(
        page.contains("in step"),
        "sync state shown: missing from page"
    );
    assert!(page.contains("data-poll"), "logs refresh live");
    // And its Sync-now button runs the real sync.
    let resp = app
        .clone()
        .oneshot(
            Request::post("/service/blog/sync")
                .header("authorization", format!("Bearer {token}"))
                .extension(local())
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::SEE_OTHER);
    let loc = resp.headers()["location"].to_str().unwrap().to_string();
    assert!(loc.contains("ok="), "sync-now reports its outcome: {loc}");

    // The journal narrates what just happened, for owner and agent alike.
    let story = call_tool(&app, &token, "journal", json!({})).await;
    assert!(
        text(&story).contains("deployed blog"),
        "the journal must tell the deploy story: {}",
        text(&story)
    );

    // And the end-to-end checker tells the truth about an unpublished service.
    let verify = call_tool(&app, &token, "verify_service", json!({ "name": "blog" })).await;
    assert_eq!(verify["result"]["isError"], false, "{verify}");
    assert!(text(&verify).contains("not published"), "{}", text(&verify));
    let off = call_tool(&app, &token, "unlink_repo", json!({ "name": "blog" })).await;
    assert_eq!(off["result"]["isError"], false, "{off}");
    let (_, body) = get(&app, "/sites/blog/", "127.0.0.1", Some(&token)).await;
    assert!(
        body.contains("pulled v2"),
        "unlink must not undeploy: {body}"
    );
    let sync = call_tool(&app, &token, "sync_repo", json!({ "name": "blog" })).await;
    assert_eq!(
        sync["result"]["isError"], true,
        "sync after unlink must refuse"
    );
}

/// The build step, end to end and offline: a repository that is NOT a file
/// tree until a build runs — install phase first, build phase after it, the
/// detected output directory deployed, a broken build refused without taking
/// the site down. Execution is direct (no container) because this test's
/// commands are our own; the podman sandbox itself is proven in the VM test.
#[tokio::test]
async fn mcp_repo_with_build_step_builds_then_deploys() {
    let (_tmp, app, token) = app_with_builds();
    let text = |v: &Value| {
        v["result"]["content"][0]["text"]
            .as_str()
            .unwrap()
            .to_string()
    };

    let upstream = TempDir::new().unwrap();
    let git = |args: &[&str]| {
        let out = std::process::Command::new("git")
            .args(args)
            .env("GIT_AUTHOR_NAME", "t")
            .env("GIT_AUTHOR_EMAIL", "t@t")
            .env("GIT_COMMITTER_NAME", "t")
            .env("GIT_COMMITTER_EMAIL", "t@t")
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "git {args:?}: {}",
            String::from_utf8_lossy(&out.stderr)
        );
    };
    let up = upstream.path().to_str().unwrap().to_string();
    git(&["init", "-q", "-b", "main", &up]);
    // No index.html anywhere: this repo serves nothing until a build runs.
    std::fs::write(upstream.path().join("page.src"), "assembled, not committed").unwrap();
    std::fs::write(upstream.path().join("README"), "a repo that needs building").unwrap();
    git(&["-C", &up, "add", "."]);
    git(&["-C", &up, "commit", "-qm", "v1"]);

    let deploy = call_tool(
        &app,
        &token,
        "deploy_static_site",
        json!({ "name": "site", "index_html": "placeholder" }),
    )
    .await;
    assert_eq!(deploy["result"]["isError"], false, "{deploy}");

    // The link arrives the way a restored config would: directly in box.toml
    // (link_repo needs a connected forge, which an offline test cannot have).
    // The build depends on the install phase's output, so a deploy proves the
    // phases ran in order. No output_dir: dist/ must be detected.
    {
        let paths = boxd::paths::Paths::new(_tmp.path().to_path_buf());
        let mut config = boxd::config::BoxConfig::load(&paths).unwrap();
        config.services[0].repo = Some(boxd::config::RepoLink {
            forge: "github".into(),
            repo: "local/site".into(),
            clone_url: format!("file://{up}"),
            branch: "main".into(),
            subdir: None,
            build: Some(boxd::build::BuildSpec {
                command: "mkdir -p dist && cp staged.src dist/index.html".into(),
                install: Some("cp page.src staged.src".into()),
                output_dir: None,
            }),
        });
        config.save(&paths).unwrap();
    }

    let sync = call_tool(&app, &token, "sync_repo", json!({ "name": "site" })).await;
    assert_eq!(sync["result"]["isError"], false, "{sync}");
    assert!(text(&sync).contains("deployed"), "{}", text(&sync));
    let (status, body) = get(&app, "/sites/site/", "127.0.0.1", Some(&token)).await;
    assert_eq!(status, StatusCode::OK);
    assert!(body.contains("assembled, not committed"), "{body}");

    // Push → rebuilt → live. The loop, now with a build in the middle.
    std::fs::write(upstream.path().join("page.src"), "assembled v2").unwrap();
    git(&["-C", &up, "add", "."]);
    git(&["-C", &up, "commit", "-qm", "v2"]);
    let sync = call_tool(&app, &token, "sync_repo", json!({ "name": "site" })).await;
    assert!(text(&sync).contains("deployed"), "{}", text(&sync));
    let (_, body) = get(&app, "/sites/site/", "127.0.0.1", Some(&token)).await;
    assert!(body.contains("assembled v2"), "{body}");

    // A commit that breaks the build: the sync fails, the error carries the
    // build log (an agent needs the compiler's words, not "it failed"), and
    // the site keeps serving what last built.
    git(&["-C", &up, "rm", "-q", "page.src"]);
    git(&["-C", &up, "commit", "-qm", "break the build"]);
    let sync = call_tool(&app, &token, "sync_repo", json!({ "name": "site" })).await;
    assert_eq!(
        sync["result"]["isError"], true,
        "a broken build must not deploy"
    );
    assert!(text(&sync).contains("build log"), "{}", text(&sync));
    let (_, body) = get(&app, "/sites/site/", "127.0.0.1", Some(&token)).await;
    assert!(
        body.contains("assembled v2"),
        "a broken build must not take the site down: {body}"
    );

    // And the failure is visible later, not just in the moment.
    let services = call_tool(&app, &token, "list_services", json!({})).await;
    let parsed: Value = serde_json::from_str(&text(&services)).unwrap();
    let site = parsed
        .as_array()
        .unwrap()
        .iter()
        .find(|s| s["name"] == "site")
        .unwrap();
    assert_eq!(site["last_sync"]["ok"], false, "{site}");
}

/// The webhook receiver: its authentication is the HMAC signature. A push
/// with the right signature syncs the linked service immediately; a wrong
/// signature is refused; a Box with webhooks unset reveals nothing.
#[tokio::test]
async fn webhook_push_syncs_the_linked_service() {
    let (_tmp, app, token) = app();
    let paths = boxd::paths::Paths::new(_tmp.path().to_path_buf());
    let text = |v: &Value| {
        v["result"]["content"][0]["text"]
            .as_str()
            .unwrap()
            .to_string()
    };

    let hook = |body: String, sig: Option<String>| {
        let app = app.clone();
        async move {
            let mut req = Request::post("/hooks/github")
                .header("content-type", "application/json")
                .extension(remote_hook());
            if let Some(s) = sig {
                req = req.header("x-hub-signature-256", s);
            }
            let resp = app
                .oneshot(req.body(Body::from(body)).unwrap())
                .await
                .unwrap();
            let status = resp.status();
            let bytes = resp.into_body().collect().await.unwrap().to_bytes();
            (status, String::from_utf8_lossy(&bytes).to_string())
        }
    };

    // Unset: the route plays dead.
    let (status, _) = hook("{}".into(), None).await;
    assert_eq!(status, StatusCode::NOT_FOUND);

    // A linked local repo, like the poller tests use.
    let upstream = TempDir::new().unwrap();
    let git = |args: &[&str]| {
        let out = std::process::Command::new("git")
            .args(args)
            .env("GIT_AUTHOR_NAME", "t")
            .env("GIT_AUTHOR_EMAIL", "t@t")
            .env("GIT_COMMITTER_NAME", "t")
            .env("GIT_COMMITTER_EMAIL", "t@t")
            .output()
            .unwrap();
        assert!(out.status.success(), "git {args:?}");
    };
    let up = upstream.path().to_str().unwrap().to_string();
    git(&["init", "-q", "-b", "main", &up]);
    std::fs::write(upstream.path().join("index.html"), "hooked v1").unwrap();
    git(&["-C", &up, "add", "."]);
    git(&["-C", &up, "commit", "-qm", "v1"]);

    let deploy = call_tool(
        &app,
        &token,
        "deploy_static_site",
        json!({ "name": "blog", "index_html": "placeholder" }),
    )
    .await;
    assert_eq!(deploy["result"]["isError"], false, "{deploy}");
    {
        let mut config = boxd::config::BoxConfig::load(&paths).unwrap();
        config.services[0].repo = Some(boxd::config::RepoLink {
            forge: "github".into(),
            repo: "owner/blog".into(),
            clone_url: format!("file://{up}"),
            branch: "main".into(),
            subdir: None,
            build: None,
        });
        config.save(&paths).unwrap();
    }
    boxd::secrets::set(&paths, "webhook-secret", "s3cret").unwrap();

    let payload = json!({
        "ref": "refs/heads/main",
        "repository": { "full_name": "owner/blog" },
    })
    .to_string();
    let sign = |body: &str| {
        let mac = boxd::util::hmac_sha256(b"s3cret", body.as_bytes());
        format!(
            "sha256={}",
            mac.iter().map(|b| format!("{b:02x}")).collect::<String>()
        )
    };

    // Wrong signature: refused, nothing deployed.
    let (status, _) = hook(payload.clone(), Some("sha256=deadbeef".into())).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    let (_, body) = get(&app, "/sites/blog/", "127.0.0.1", Some(&token)).await;
    assert!(
        body.contains("placeholder"),
        "unsigned pushes must not deploy: {body}"
    );

    // Right signature: the push IS the deploy.
    let (status, resp) = hook(payload.clone(), Some(sign(&payload))).await;
    assert_eq!(status, StatusCode::OK, "{resp}");
    assert!(resp.contains("blog"), "{resp}");
    let (_, body) = get(&app, "/sites/blog/", "127.0.0.1", Some(&token)).await;
    assert!(body.contains("hooked v1"), "{body}");

    // GitHub's registration ping gets a pong.
    let ping = json!({ "zen": "Design for failure." }).to_string();
    let (status, resp) = hook(ping.clone(), Some(sign(&ping))).await;
    assert_eq!(status, StatusCode::OK);
    assert!(resp.contains("pong"), "{resp}");

    // A push to an unlinked branch syncs nothing and says so.
    let other = json!({
        "ref": "refs/heads/feature",
        "repository": { "full_name": "owner/blog" },
    })
    .to_string();
    let (status, resp) = hook(other.clone(), Some(sign(&other))).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        serde_json::from_str::<Value>(&resp).unwrap()["synced"],
        json!([]),
        "{resp}"
    );

    // The last_sync the operator sees reflects the webhook's work.
    let services = call_tool(&app, &token, "list_services", json!({})).await;
    let parsed: Value = serde_json::from_str(&text(&services)).unwrap();
    let blog = parsed
        .as_array()
        .unwrap()
        .iter()
        .find(|s| s["name"] == "blog")
        .unwrap();
    assert_eq!(blog["last_sync"]["ok"], true, "{blog}");
}

/// Webhooks arrive from the internet through the tunnel; they present like
/// any proxied request.
fn remote_hook() -> ConnectInfo<SocketAddr> {
    ConnectInfo(SocketAddr::from(([203, 0, 113, 9], 443)))
}

/// Pull requests get previews with the PR's own lifecycle: opened → a
/// sibling service on the PR branch serves; synchronize → it follows; closed
/// → it is gone. Fork PRs are refused by policy, not by accident.
#[tokio::test]
async fn pull_request_preview_lives_and_dies_with_the_pr() {
    let (_tmp, app, token) = app();
    let paths = boxd::paths::Paths::new(_tmp.path().to_path_buf());
    let text = |v: &Value| {
        v["result"]["content"][0]["text"]
            .as_str()
            .unwrap()
            .to_string()
    };

    let hook = |body: String, sig: String| {
        let app = app.clone();
        async move {
            let resp = app
                .oneshot(
                    Request::post("/hooks/github")
                        .header("content-type", "application/json")
                        .header("x-hub-signature-256", sig)
                        .extension(remote_hook())
                        .body(Body::from(body))
                        .unwrap(),
                )
                .await
                .unwrap();
            let status = resp.status();
            let bytes = resp.into_body().collect().await.unwrap().to_bytes();
            (status, String::from_utf8_lossy(&bytes).to_string())
        }
    };
    let sign = |body: &str| {
        let mac = boxd::util::hmac_sha256(b"s3cret", body.as_bytes());
        format!(
            "sha256={}",
            mac.iter().map(|b| format!("{b:02x}")).collect::<String>()
        )
    };

    // Upstream with main + a PR branch that changes the page.
    let upstream = TempDir::new().unwrap();
    let git = |args: &[&str]| {
        let out = std::process::Command::new("git")
            .args(args)
            .env("GIT_AUTHOR_NAME", "t")
            .env("GIT_AUTHOR_EMAIL", "t@t")
            .env("GIT_COMMITTER_NAME", "t")
            .env("GIT_COMMITTER_EMAIL", "t@t")
            .output()
            .unwrap();
        assert!(out.status.success(), "git {args:?}");
    };
    let up = upstream.path().to_str().unwrap().to_string();
    git(&["init", "-q", "-b", "main", &up]);
    std::fs::write(upstream.path().join("index.html"), "main content").unwrap();
    git(&["-C", &up, "add", "."]);
    git(&["-C", &up, "commit", "-qm", "main"]);
    git(&["-C", &up, "checkout", "-qb", "feature"]);
    std::fs::write(upstream.path().join("index.html"), "preview content v1").unwrap();
    git(&["-C", &up, "add", "."]);
    git(&["-C", &up, "commit", "-qm", "pr1"]);
    git(&["-C", &up, "checkout", "-q", "main"]);

    // The linked parent service.
    let deploy = call_tool(
        &app,
        &token,
        "deploy_static_site",
        json!({ "name": "app", "index_html": "placeholder" }),
    )
    .await;
    assert_eq!(deploy["result"]["isError"], false, "{deploy}");
    {
        let mut config = boxd::config::BoxConfig::load(&paths).unwrap();
        config.services[0].repo = Some(boxd::config::RepoLink {
            forge: "github".into(),
            repo: "owner/app".into(),
            clone_url: format!("file://{up}"),
            branch: "main".into(),
            subdir: None,
            build: None,
        });
        config.save(&paths).unwrap();
    }
    boxd::secrets::set(&paths, "webhook-secret", "s3cret").unwrap();

    let pr_payload = |action: &str| {
        json!({
            "action": action,
            "repository": { "full_name": "owner/app" },
            "pull_request": {
                "number": 7,
                "head": { "ref": "feature", "repo": { "full_name": "owner/app" } },
            },
        })
        .to_string()
    };

    // Opened: the preview exists and serves the PR branch.
    let (status, resp) = hook(pr_payload("opened"), sign(&pr_payload("opened"))).await;
    assert_eq!(status, StatusCode::OK, "{resp}");
    assert!(resp.contains("app-pr7"), "{resp}");
    let (_, body) = get(&app, "/sites/app-pr7/", "127.0.0.1", Some(&token)).await;
    assert!(body.contains("preview content v1"), "{body}");
    // The parent is untouched.
    let (_, body) = get(&app, "/sites/app/", "127.0.0.1", Some(&token)).await;
    assert!(body.contains("placeholder"), "{body}");

    // Synchronize after a push to the PR branch: the preview follows.
    git(&["-C", &up, "checkout", "-q", "feature"]);
    std::fs::write(upstream.path().join("index.html"), "preview content v2").unwrap();
    git(&["-C", &up, "add", "."]);
    git(&["-C", &up, "commit", "-qm", "pr1 update"]);
    git(&["-C", &up, "checkout", "-q", "main"]);
    let (status, _) = hook(pr_payload("synchronize"), sign(&pr_payload("synchronize"))).await;
    assert_eq!(status, StatusCode::OK);
    let (_, body) = get(&app, "/sites/app-pr7/", "127.0.0.1", Some(&token)).await;
    assert!(body.contains("preview content v2"), "{body}");

    // A fork PR is refused by policy.
    let fork = json!({
        "action": "opened",
        "repository": { "full_name": "owner/app" },
        "pull_request": {
            "number": 8,
            "head": { "ref": "evil", "repo": { "full_name": "stranger/app" } },
        },
    })
    .to_string();
    let (status, resp) = hook(fork.clone(), sign(&fork)).await;
    assert_eq!(status, StatusCode::OK);
    assert!(resp.contains("fork"), "{resp}");
    let services = call_tool(&app, &token, "list_services", json!({})).await;
    assert!(!text(&services).contains("pr8"), "no preview for a fork PR");

    // Closed: the preview is gone; the parent remains.
    let (status, resp) = hook(pr_payload("closed"), sign(&pr_payload("closed"))).await;
    assert_eq!(status, StatusCode::OK, "{resp}");
    assert!(resp.contains("app-pr7"), "{resp}");
    let services = call_tool(&app, &token, "list_services", json!({})).await;
    assert!(!text(&services).contains("app-pr7"), "{}", text(&services));
    let (_, body) = get(&app, "/sites/app/", "127.0.0.1", Some(&token)).await;
    assert!(body.contains("placeholder"), "parent survives the PR close");
}

/// The trust ceremony, end to end: a destructive call from a normal session
/// queues instead of running; the console tap runs the exact call; a denial
/// is recorded and nothing happens; an autonomous session skips the queue.
#[tokio::test]
async fn destructive_ops_wait_for_the_human_tap() {
    let (_tmp, app, token) = app();
    let text = |v: &Value| {
        v["result"]["content"][0]["text"]
            .as_str()
            .unwrap()
            .to_string()
    };
    let post = |path: String, token: String| {
        let app = app.clone();
        async move {
            app.oneshot(
                Request::post(path.as_str())
                    .header("authorization", format!("Bearer {token}"))
                    .extension(local())
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap()
            .status()
        }
    };

    let deploy = call_tool(
        &app,
        &token,
        "deploy_static_site",
        json!({ "name": "blog", "index_html": "precious" }),
    )
    .await;
    assert_eq!(deploy["result"]["isError"], false, "{deploy}");

    // 1. The ask queues; nothing is deleted.
    let del = call_tool(&app, &token, "delete_service", json!({ "name": "blog" })).await;
    assert_eq!(del["result"]["isError"], false, "{del}");
    let body: Value = serde_json::from_str(&text(&del)).unwrap();
    let id = body["pending_approval"]
        .as_str()
        .expect("a pending id")
        .to_string();
    assert!(body["would"].as_str().unwrap().contains("blog"), "{body}");
    let services = call_tool(&app, &token, "list_services", json!({})).await;
    assert!(
        text(&services).contains("blog"),
        "not deleted before approval"
    );

    let st = call_tool(&app, &token, "approval_status", json!({ "id": id })).await;
    assert!(text(&st).contains("pending"), "{}", text(&st));

    // 2. The tap runs the exact call.
    let status = post(format!("/approvals/{id}/approve"), token.clone()).await;
    assert_eq!(status, StatusCode::SEE_OTHER);
    let services = call_tool(&app, &token, "list_services", json!({})).await;
    assert!(
        !text(&services).contains("blog"),
        "approval must actually delete"
    );
    let st = call_tool(&app, &token, "approval_status", json!({ "id": id })).await;
    assert!(text(&st).contains("approved"), "{}", text(&st));
    assert!(
        text(&st).contains("deleted"),
        "the agent sees the result: {}",
        text(&st)
    );

    // 3. A denial is a recorded no.
    let deploy = call_tool(
        &app,
        &token,
        "deploy_static_site",
        json!({ "name": "diary", "index_html": "also precious" }),
    )
    .await;
    assert_eq!(deploy["result"]["isError"], false, "{deploy}");
    let del = call_tool(&app, &token, "delete_service", json!({ "name": "diary" })).await;
    let body: Value = serde_json::from_str(&text(&del)).unwrap();
    let id2 = body["pending_approval"].as_str().unwrap().to_string();
    let status = post(format!("/approvals/{id2}/deny"), token.clone()).await;
    assert_eq!(status, StatusCode::SEE_OTHER);
    let st = call_tool(&app, &token, "approval_status", json!({ "id": id2 })).await;
    assert!(text(&st).contains("denied"), "{}", text(&st));
    let services = call_tool(&app, &token, "list_services", json!({})).await;
    assert!(text(&services).contains("diary"), "denied means untouched");

    // 4. An explicitly autonomous session skips the queue.
    let paths = boxd::paths::Paths::new(_tmp.path().to_path_buf());
    let session = boxd::auth::list(&paths)
        .into_iter()
        .find(|s| s.label == "test-agent")
        .expect("the test session");
    assert!(!session.autonomous, "off by default");
    boxd::auth::set_autonomous(&paths, &session.id, true).unwrap();
    let del = call_tool(&app, &token, "delete_service", json!({ "name": "diary" })).await;
    assert_eq!(del["result"]["isError"], false, "{del}");
    assert!(
        text(&del).contains("deleted"),
        "autonomous runs immediately: {}",
        text(&del)
    );
    // The decisions made along the way are part of the Box's story.
    let story = call_tool(&app, &token, "journal", json!({})).await;
    assert!(text(&story).contains("you approved"), "{}", text(&story));
    assert!(text(&story).contains("you denied"), "{}", text(&story));
    let services = call_tool(&app, &token, "list_services", json!({})).await;
    assert!(!text(&services).contains("diary"), "{}", text(&services));
}

/// The proxy and the resident both reach their upstreams through curl. The
/// nix package build runs tests in a sandbox without it; the devshell and
/// CI's cargo-test job have it, so the real coverage happens there and the
/// package build skips honestly (the connect_script.rs precedent).
fn curl_present() -> bool {
    let ok = std::process::Command::new("curl")
        .arg("--version")
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .is_ok();
    if !ok {
        eprintln!("skipping: this test needs curl, absent in this build environment");
    }
    ok
}

/// The OpenAI-compatible endpoint: minted keys gate it, requests route to
/// the Box's model server, and the errors name their fixes. The upstream is
/// a stub speaking just enough of the wire format to prove the plumbing.
#[tokio::test]
async fn ai_endpoint_speaks_openai_with_minted_keys() {
    if !curl_present() {
        return;
    }
    let (_tmp, app, token) = app();
    let paths = boxd::paths::Paths::new(_tmp.path().to_path_buf());
    let text = |v: &Value| {
        v["result"]["content"][0]["text"]
            .as_str()
            .unwrap()
            .to_string()
    };

    // A stub model server on a real port (the proxy reaches it via curl).
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let stub = axum::Router::new()
        .route(
            "/v1/chat/completions",
            axum::routing::post(|| async {
                axum::Json(json!({
                    "id": "chatcmpl-stub",
                    "choices": [{ "message": { "role": "assistant", "content": "hello from your own hardware" } }],
                }))
            }),
        )
        .route(
            "/v1/models",
            axum::routing::get(|| async {
                axum::Json(json!({ "data": [{ "id": "llama3.2" }] }))
            }),
        );
    tokio::spawn(async move { axum::serve(listener, stub).await.unwrap() });

    // Before any key exists: refused, with the minting path named.
    let post_v1 = |body: String, key: Option<String>| {
        let app = app.clone();
        async move {
            let mut req = Request::post("/v1/chat/completions")
                .header("content-type", "application/json")
                .extension(remote_hook());
            if let Some(k) = key {
                req = req.header("authorization", format!("Bearer {k}"));
            }
            let resp = app
                .oneshot(req.body(Body::from(body)).unwrap())
                .await
                .unwrap();
            let status = resp.status();
            let bytes = resp.into_body().collect().await.unwrap().to_bytes();
            (status, String::from_utf8_lossy(&bytes).to_string())
        }
    };
    let (status, body) = post_v1("{}".into(), None).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    assert!(body.contains("ai_key_create"), "{body}");

    // Mint a key over MCP; the secret is shown once and is recognizable.
    let minted = call_tool(
        &app,
        &token,
        "ai_key_create",
        json!({ "label": "notes-app" }),
    )
    .await;
    assert_eq!(minted["result"]["isError"], false, "{minted}");
    let key = serde_json::from_str::<Value>(&text(&minted)).unwrap()["key"]
        .as_str()
        .unwrap()
        .to_string();
    assert!(key.starts_with("boxai_"));

    // A key but no model server: the error names the ollama preset.
    let (status, body) = post_v1("{}".into(), Some(key.clone())).await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
    assert!(body.contains("ollama"), "{body}");

    // Declare the stub as this Box's ollama service (port is what routing
    // reads; nothing needs to actually run through podman here).
    {
        let mut config = boxd::config::BoxConfig::load(&paths).unwrap();
        config.services.push(boxd::config::ServiceConfig {
            name: "ollama".into(),
            template: "container".into(),
            params: json!({ "image": "ollama/ollama", "container_port": 11434 }),
            domain: None,
            public: false,
            port: Some(port),
            repo: None,
            created_at: chrono::Utc::now(),
        });
        config.save(&paths).unwrap();
    }

    // The full round trip, wearing the standard wire format.
    let req = json!({ "model": "llama3.2", "messages": [{ "role": "user", "content": "hi" }] });
    let (status, body) = post_v1(req.to_string(), Some(key.clone())).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert!(body.contains("hello from your own hardware"), "{body}");

    // Models enumerate; revoked keys stop cold.
    let resp = app
        .clone()
        .oneshot(
            Request::get("/v1/models")
                .header("authorization", format!("Bearer {key}"))
                .extension(remote_hook())
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let bytes = resp.into_body().collect().await.unwrap().to_bytes();
    assert!(String::from_utf8_lossy(&bytes).contains("llama3.2"));

    let keys = call_tool(&app, &token, "ai_keys", json!({})).await;
    let id = serde_json::from_str::<Value>(&text(&keys)).unwrap()[0]["id"]
        .as_str()
        .unwrap()
        .to_string();
    let revoked = call_tool(&app, &token, "ai_key_revoke", json!({ "id": id })).await;
    assert_eq!(revoked["result"]["isError"], false, "{revoked}");
    let (status, _) = post_v1(req.to_string(), Some(key)).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED, "revoked means revoked");
}

/// The resident, end to end against a stub brain: configure it, run a
/// report, and see the summary in the journal, the concern named, and the
/// destructive suggestion QUEUED for the human — never run.
#[tokio::test]
async fn resident_reports_and_suggestions_wait_for_the_human() {
    if !curl_present() {
        return;
    }
    let (_tmp, app, token) = app();
    let paths = boxd::paths::Paths::new(_tmp.path().to_path_buf());
    let text = |v: &Value| {
        v["result"]["content"][0]["text"]
            .as_str()
            .unwrap()
            .to_string()
    };

    // A stub brain that always worries about the same thing.
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let stub = axum::Router::new().route(
        "/v1/chat/completions",
        axum::routing::post(|| async {
            let report = json!({
                "summary": "Your Box is healthy, though one service worries me.",
                "concerns": ["the service 'old' has been failing to sync"],
                "suggested_actions": [{
                    "tool": "delete_service",
                    "arguments": { "name": "old" },
                    "reason": "it has failed for a week and serves nothing",
                }],
            });
            axum::Json(json!({
                "choices": [{ "message": { "role": "assistant",
                    "content": report.to_string() } }],
            }))
        }),
    );
    tokio::spawn(async move { axum::serve(listener, stub).await.unwrap() });

    // A service for the suggestion to be about.
    let deploy = call_tool(
        &app,
        &token,
        "deploy_static_site",
        json!({ "name": "old", "index_html": "relic" }),
    )
    .await;
    assert_eq!(deploy["result"]["isError"], false, "{deploy}");

    let configured = call_tool(
        &app,
        &token,
        "resident_configure",
        json!({
            "base_url": format!("http://127.0.0.1:{port}/v1"),
            "model": "stub-1",
            "api_key": "test-key",
        }),
    )
    .await;
    assert_eq!(configured["result"]["isError"], false, "{configured}");

    let report = call_tool(&app, &token, "resident_report_now", json!({})).await;
    assert_eq!(report["result"]["isError"], false, "{report}");
    let parsed: Value = serde_json::from_str(&text(&report)).unwrap();
    assert!(parsed["summary"].as_str().unwrap().contains("healthy"));
    assert_eq!(parsed["suggestions_queued"], 1);

    // The journal carries the resident's words...
    let story = call_tool(&app, &token, "journal", json!({})).await;
    assert!(text(&story).contains("worries me"), "{}", text(&story));
    assert!(text(&story).contains("concern:"), "{}", text(&story));

    // ...the suggestion WAITS on the approvals ledger...
    let pending = boxd::approvals::list(&paths);
    assert_eq!(pending.len(), 1);
    assert_eq!(pending[0].requested_by, "resident");
    assert!(pending[0].summary.contains("resident suggests"));

    // ...and nothing ran: the service is untouched.
    let (_, body) = get(&app, "/sites/old/", "127.0.0.1", Some(&token)).await;
    assert!(
        body.contains("relic"),
        "suggestion must not execute: {body}"
    );
}

/// The Boxfile: deploy config that lives in the repo. A linked repository
/// with a box.toml builds by its own declaration; changing the file in a
/// commit changes the next deploy; breaking it fails loudly, not silently.
#[tokio::test]
async fn boxfile_in_the_repo_declares_the_build() {
    let (_tmp, app, token) = app_with_builds();
    let paths = boxd::paths::Paths::new(_tmp.path().to_path_buf());
    let text = |v: &Value| {
        v["result"]["content"][0]["text"]
            .as_str()
            .unwrap()
            .to_string()
    };

    let upstream = TempDir::new().unwrap();
    let git = |args: &[&str]| {
        let out = std::process::Command::new("git")
            .args(args)
            .env("GIT_AUTHOR_NAME", "t")
            .env("GIT_AUTHOR_EMAIL", "t@t")
            .env("GIT_COMMITTER_NAME", "t")
            .env("GIT_COMMITTER_EMAIL", "t@t")
            .output()
            .unwrap();
        assert!(out.status.success(), "git {args:?}");
    };
    let up = upstream.path().to_str().unwrap().to_string();
    git(&["init", "-q", "-b", "main", &up]);
    std::fs::write(upstream.path().join("page.src"), "declared in the repo").unwrap();
    std::fs::write(
        upstream.path().join("box.toml"),
        "[build]\ncommand = \"mkdir -p dist && cp page.src dist/index.html\"\noutput_dir = \"dist\"\n",
    )
    .unwrap();
    git(&["-C", &up, "add", "."]);
    git(&["-C", &up, "commit", "-qm", "v1"]);

    let deploy = call_tool(
        &app,
        &token,
        "deploy_static_site",
        json!({ "name": "site", "index_html": "placeholder" }),
    )
    .await;
    assert_eq!(deploy["result"]["isError"], false, "{deploy}");
    {
        // Linked with NO build config at all: the repo speaks for itself.
        let mut config = boxd::config::BoxConfig::load(&paths).unwrap();
        config.services[0].repo = Some(boxd::config::RepoLink {
            forge: "github".into(),
            repo: "local/site".into(),
            clone_url: format!("file://{up}"),
            branch: "main".into(),
            subdir: None,
            build: None,
        });
        config.save(&paths).unwrap();
    }

    let sync = call_tool(&app, &token, "sync_repo", json!({ "name": "site" })).await;
    assert_eq!(sync["result"]["isError"], false, "{sync}");
    let (_, body) = get(&app, "/sites/site/", "127.0.0.1", Some(&token)).await;
    assert!(body.contains("declared in the repo"), "{body}");

    // Config as code: a commit that changes the Boxfile changes the deploy.
    std::fs::write(
        upstream.path().join("box.toml"),
        "[build]\ncommand = \"mkdir -p out && printf 'reconfigured by a commit' > out/index.html\"\noutput_dir = \"out\"\n",
    )
    .unwrap();
    git(&["-C", &up, "add", "."]);
    git(&["-C", &up, "commit", "-qm", "v2 changes the build"]);
    let sync = call_tool(&app, &token, "sync_repo", json!({ "name": "site" })).await;
    assert_eq!(sync["result"]["isError"], false, "{sync}");
    let (_, body) = get(&app, "/sites/site/", "127.0.0.1", Some(&token)).await;
    assert!(body.contains("reconfigured by a commit"), "{body}");

    // A malformed Boxfile fails the sync loudly, naming the file.
    std::fs::write(upstream.path().join("box.toml"), "[build\nbroken").unwrap();
    git(&["-C", &up, "add", "."]);
    git(&["-C", &up, "commit", "-qm", "v3 breaks the file"]);
    let sync = call_tool(&app, &token, "sync_repo", json!({ "name": "site" })).await;
    assert_eq!(sync["result"]["isError"], true);
    assert!(text(&sync).contains("box.toml"), "{}", text(&sync));
    // The last good deploy keeps serving.
    let (_, body) = get(&app, "/sites/site/", "127.0.0.1", Some(&token)).await;
    assert!(body.contains("reconfigured by a commit"), "{body}");
}

/// A machine with no sandbox refuses a build-step sync with an explanation,
/// not a broken deploy — and never runs repository code on the bare host.
#[tokio::test]
async fn mcp_build_step_without_a_sandbox_is_refused_honestly() {
    let (_tmp, app, token) = app(); // BuildExec::detect() on a test machine: Unavailable
    let text = |v: &Value| {
        v["result"]["content"][0]["text"]
            .as_str()
            .unwrap()
            .to_string()
    };

    let upstream = TempDir::new().unwrap();
    let up = upstream.path().to_str().unwrap().to_string();
    let git = |args: &[&str]| {
        let out = std::process::Command::new("git")
            .args(args)
            .env("GIT_AUTHOR_NAME", "t")
            .env("GIT_AUTHOR_EMAIL", "t@t")
            .env("GIT_COMMITTER_NAME", "t")
            .env("GIT_COMMITTER_EMAIL", "t@t")
            .output()
            .unwrap();
        assert!(out.status.success(), "git {args:?}");
    };
    git(&["init", "-q", "-b", "main", &up]);
    std::fs::write(upstream.path().join("page.src"), "x").unwrap();
    git(&["-C", &up, "add", "."]);
    git(&["-C", &up, "commit", "-qm", "v1"]);

    let deploy = call_tool(
        &app,
        &token,
        "deploy_static_site",
        json!({ "name": "site", "index_html": "placeholder" }),
    )
    .await;
    assert_eq!(deploy["result"]["isError"], false, "{deploy}");
    {
        let paths = boxd::paths::Paths::new(_tmp.path().to_path_buf());
        let mut config = boxd::config::BoxConfig::load(&paths).unwrap();
        config.services[0].repo = Some(boxd::config::RepoLink {
            forge: "github".into(),
            repo: "local/site".into(),
            clone_url: format!("file://{up}"),
            branch: "main".into(),
            subdir: None,
            build: Some(boxd::build::BuildSpec {
                command: "echo pwned > /tmp/never".into(),
                install: None,
                output_dir: None,
            }),
        });
        config.save(&paths).unwrap();
    }
    let sync = call_tool(&app, &token, "sync_repo", json!({ "name": "site" })).await;
    assert_eq!(sync["result"]["isError"], true, "{sync}");
    assert!(text(&sync).contains("build sandbox"), "{}", text(&sync));
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
    // The refusal is also a signpost — an agent's first contact lands exactly
    // here, and "pairing required" with no next step once sent one to SSH.
    let (status, body) = get(&app, "/", "localhost:2693", None).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    assert!(
        body.contains("/pair/redeem"),
        "the 401 must say how to pair: {body}"
    );
    assert!(
        body.contains("Bearer"),
        "the 401 must say how to authenticate: {body}"
    );

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

/// The work runner refuses in the right order with the right signposts: no
/// credentials -> name the tool that stores them; credentials but no sandbox
/// (a dev machine) -> say so; and the key never echoes back.
#[tokio::test]
async fn work_refuses_honestly_and_never_echoes_the_key() {
    let (_tmp, app, token) = app();
    let text = |v: &Value| {
        v["result"]["content"][0]["text"]
            .as_str()
            .unwrap()
            .to_string()
    };

    // Before any credentials: the refusal names work_configure.
    let start = call_tool(
        &app,
        &token,
        "work_start",
        json!({ "service": "site", "prompt": "fix the tests" }),
    )
    .await;
    assert_eq!(start["result"]["isError"], true, "{start}");
    assert!(text(&start).contains("work_configure"), "{}", text(&start));

    // Store a key; it is accepted and never repeated back.
    let conf = call_tool(
        &app,
        &token,
        "work_configure",
        json!({ "api_key": "sk-ant-test-not-a-real-key" }),
    )
    .await;
    assert_eq!(conf["result"]["isError"], false, "{conf}");
    assert!(!conf.to_string().contains("sk-ant-test"), "{conf}");

    // With credentials on a machine without the sandbox: honest about that.
    let start = call_tool(
        &app,
        &token,
        "work_start",
        json!({ "service": "site", "prompt": "fix the tests" }),
    )
    .await;
    assert_eq!(start["result"]["isError"], true, "{start}");
    let t = text(&start);
    assert!(
        t.contains("sandbox") || t.contains("no service named"),
        "should fail on sandbox or service lookup, honestly: {t}"
    );
}

/// The router: with no local model server, /v1 forwards to the configured
/// cloud fallback carrying the owner's key, and the stats count the trip.
#[tokio::test]
async fn router_falls_back_to_the_cloud_and_counts_it() {
    if !curl_present() {
        return;
    }
    let (_tmp, app, token) = app();
    let text = |v: &Value| {
        v["result"]["content"][0]["text"]
            .as_str()
            .unwrap()
            .to_string()
    };

    // A stub cloud that echoes the auth it was shown and bills 7 tokens.
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let stub = axum::Router::new().route(
        "/v1/chat/completions",
        axum::routing::post(|headers: axum::http::HeaderMap| async move {
            let auth = headers
                .get("authorization")
                .and_then(|v| v.to_str().ok())
                .unwrap_or("")
                .to_string();
            axum::Json(serde_json::json!({
                "choices": [{ "message": { "role": "assistant",
                    "content": format!("cloud answered under {auth}") } }],
                "usage": { "completion_tokens": 7 }
            }))
        }),
    );
    tokio::spawn(async move {
        axum::serve(listener, stub).await.unwrap();
    });

    let conf = call_tool(
        &app,
        &token,
        "router_configure",
        json!({ "base_url": format!("http://127.0.0.1:{port}/v1"), "api_key": "sk-cloud-test" }),
    )
    .await;
    assert_eq!(conf["result"]["isError"], false, "{conf}");

    let minted = call_tool(&app, &token, "ai_key_create", json!({ "label": "app" })).await;
    let key = serde_json::from_str::<Value>(&text(&minted)).unwrap()["key"]
        .as_str()
        .unwrap()
        .to_string();

    let req = Request::post("/v1/chat/completions")
        .header("content-type", "application/json")
        .header("authorization", format!("Bearer {key}"))
        .extension(remote_hook())
        .body(Body::from(
            json!({ "model": "whatever", "messages": [{ "role": "user", "content": "hi" }] })
                .to_string(),
        ))
        .unwrap();
    let resp = app.clone().oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let body =
        String::from_utf8_lossy(&resp.into_body().collect().await.unwrap().to_bytes()).to_string();
    assert!(body.contains("cloud answered"), "{body}");
    assert!(
        body.contains("Bearer sk-cloud-test"),
        "the owner's key must reach the fallback: {body}"
    );

    let status = call_tool(&app, &token, "router_status", json!({})).await;
    let t = text(&status);
    let v: Value = serde_json::from_str(&t).unwrap();
    assert_eq!(v["stats"]["cloud_requests"], 1, "{t}");
    assert_eq!(v["stats"]["cloud_tokens"], 7, "{t}");
    assert_eq!(v["stats"]["local_requests"], 0, "{t}");
}
