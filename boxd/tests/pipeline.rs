//! End-to-end test of the deploy pipeline with the local backend:
//! declarative config -> generation build -> atomic switch -> rollback.

use boxd::config::BoxConfig;
use boxd::ops::{self, DeployRequest};
use boxd::paths::Paths;
use boxd::store::{self, local::LocalBuilder};
use tempfile::TempDir;

fn setup() -> (TempDir, Paths, LocalBuilder) {
    let tmp = TempDir::new().unwrap();
    let paths = Paths::new(tmp.path().to_path_buf());
    paths.ensure().unwrap();
    let builder = LocalBuilder::new(&paths);
    (tmp, paths, builder)
}

fn deploy_inline(name: &str, html: &str) -> DeployRequest {
    DeployRequest::static_site(name, Some(html.into()), None, None, false)
}

fn served(paths: &Paths, service: &str, file: &str) -> String {
    let root = store::current_store_path(paths).expect("current generation");
    std::fs::read_to_string(root.join("services").join(service).join("www").join(file))
        .expect("served file")
}

#[test]
fn deploy_update_rollback_delete() {
    let (_tmp, paths, builder) = setup();

    // First deploy creates generation 1.
    let g1 = ops::deploy(&paths, &builder, deploy_inline("hello", "<h1>v1</h1>")).unwrap();
    assert_eq!(g1.number, 1);
    assert_eq!(served(&paths, "hello", "index.html"), "<h1>v1</h1>");

    // Updating the same service creates generation 2.
    let g2 = ops::deploy(&paths, &builder, deploy_inline("hello", "<h1>v2</h1>")).unwrap();
    assert_eq!(g2.number, 2);
    assert_eq!(served(&paths, "hello", "index.html"), "<h1>v2</h1>");

    // Rollback flips content AND restores declarative state.
    let rolled = ops::rollback(&paths, 1).unwrap();
    assert_eq!(rolled.number, 1);
    assert_eq!(served(&paths, "hello", "index.html"), "<h1>v1</h1>");
    let config = BoxConfig::load(&paths).unwrap();
    assert_eq!(config.services.len(), 1);

    // Applying after a rollback rebuilds from the rolled-back sources: the
    // new generation must serve v1, not the abandoned v2.
    let g3 = ops::apply(&paths, &builder).unwrap();
    assert_eq!(g3.number, 3);
    assert_eq!(served(&paths, "hello", "index.html"), "<h1>v1</h1>");

    // A second service lands next to the first.
    let g4 = ops::deploy(&paths, &builder, deploy_inline("blog", "<p>blog</p>")).unwrap();
    assert_eq!(g4.number, 4);
    assert_eq!(served(&paths, "blog", "index.html"), "<p>blog</p>");
    assert_eq!(served(&paths, "hello", "index.html"), "<h1>v1</h1>");

    // Deleting removes it from config and the new generation.
    let g5 = ops::delete_service(&paths, &builder, "blog").unwrap();
    assert_eq!(g5.number, 5);
    let root = store::current_store_path(&paths).unwrap();
    assert!(!root.join("services/blog").exists());
    let config = BoxConfig::load(&paths).unwrap();
    assert_eq!(config.services.len(), 1);

    // History is fully intact.
    let gens = store::list(&paths).unwrap();
    assert_eq!(gens.len(), 5);
    assert!(gens[4].current);
}

#[test]
fn rejects_invalid_names() {
    let (_tmp, paths, builder) = setup();
    for bad in ["", "No-Caps", "sp ace", "../evil", "-lead"] {
        assert!(
            ops::deploy(&paths, &builder, deploy_inline(bad, "x")).is_err(),
            "{bad:?} should be rejected"
        );
    }
    assert!(store::list(&paths).unwrap().is_empty());
}

#[test]
fn rollback_to_missing_generation_fails() {
    let (_tmp, paths, builder) = setup();
    ops::deploy(&paths, &builder, deploy_inline("a", "x")).unwrap();
    assert!(ops::rollback(&paths, 42).is_err());
    // Still on generation 1.
    assert_eq!(store::current(&paths).unwrap().unwrap().number, 1);
}

#[test]
fn unhealthy_deploy_auto_rolls_back() {
    let (tmp, paths, builder) = setup();

    // Healthy generation 1.
    ops::deploy(&paths, &builder, deploy_inline("site", "<h1>good</h1>")).unwrap();
    assert_eq!(served(&paths, "site", "index.html"), "<h1>good</h1>");

    // Deploy from a directory with NO index.html — the static-site health
    // check must reject it and auto-roll-back to generation 1.
    let broken = tmp.path().join("broken-src");
    std::fs::create_dir_all(&broken).unwrap();
    std::fs::write(broken.join("style.css"), "body{}").unwrap();
    let req = DeployRequest::static_site("site", None, Some(broken), None, false);
    let err = ops::deploy(&paths, &builder, req).unwrap_err();
    assert!(
        format!("{err:#}").contains("rolled back"),
        "expected auto-rollback, got: {err:#}"
    );

    // Still serving the good generation, and config was restored.
    assert_eq!(store::current(&paths).unwrap().unwrap().number, 1);
    assert_eq!(served(&paths, "site", "index.html"), "<h1>good</h1>");
    let config = BoxConfig::load(&paths).unwrap();
    assert_eq!(config.services.len(), 1);
    assert_eq!(config.services[0].params["index_html"], "<h1>good</h1>");
}

#[test]
fn config_history_tracks_generations() {
    if !boxd::history::available() {
        return; // git not on PATH here
    }
    let (_tmp, paths, builder) = setup();
    ops::deploy(&paths, &builder, deploy_inline("a", "1")).unwrap();
    ops::deploy(&paths, &builder, deploy_inline("b", "2")).unwrap();
    ops::rollback(&paths, 1).unwrap();

    let hist = boxd::history::log(&paths, 20).unwrap();
    assert_eq!(hist.len(), 3);
    assert!(hist[0].message.contains("rollback"));
    assert!(hist.iter().any(|h| h.message.contains("deploy a")));
    assert!(hist.iter().any(|h| h.message.contains("deploy b")));
}
