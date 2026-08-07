use std::path::{Path as FsPath, PathBuf};

use axum::{
    extract::{Path, Request, State},
    http::{header, StatusCode},
    middleware::Next,
    response::{IntoResponse, Redirect, Response},
};

use crate::manifest;
use crate::store;

use super::SharedState;

/// Host-based routing for tunnel traffic, applied BEFORE the router: when a
/// request's Host matches a service's domain in the current generation, serve
/// that service's site and never fall through to the dashboard. Domains are
/// validated to be real hostnames (never localhost or an IP), so local
/// dashboard access is unaffected.
pub async fn host_dispatch(
    State(state): State<SharedState>,
    request: Request,
    next: Next,
) -> Response {
    let host = request
        .headers()
        .get(header::HOST)
        .and_then(|value| value.to_str().ok())
        .map(|value| {
            value
                .split(':')
                .next()
                .unwrap_or("")
                .trim_end_matches('.')
                .to_ascii_lowercase()
        })
        .unwrap_or_default();

    if host.contains('.') {
        if let Some(service) = service_for_host(&state, &host) {
            let raw_path = request.uri().path();
            let rel = urlencoding::decode(raw_path)
                .map(|decoded| decoded.into_owned())
                .unwrap_or_else(|_| raw_path.to_string());
            return serve(state, &service, rel.trim_start_matches('/')).await;
        }
    }
    next.run(request).await
}

fn service_for_host(state: &SharedState, host: &str) -> Option<String> {
    let root = store::current_store_path(&state.paths)?;
    let manifest = manifest::read_manifest(&root).ok()?;
    manifest
        .services
        .iter()
        .find(|s| s.domain.as_deref() == Some(host))
        .map(|s| s.name.clone())
}

pub async fn redirect_to_slash(Path(name): Path<String>) -> Redirect {
    Redirect::permanent(&format!("/sites/{name}/"))
}

pub async fn serve_index(State(state): State<SharedState>, Path(name): Path<String>) -> Response {
    serve(state, &name, "").await
}

pub async fn serve_path(
    State(state): State<SharedState>,
    Path((name, path)): Path<(String, String)>,
) -> Response {
    serve(state, &name, &path).await
}

/// Serve a file for a deployed static site out of the *current generation*.
/// The profile symlink is resolved per request, so a generation switch or
/// rollback takes effect atomically for all subsequent requests.
async fn serve(state: SharedState, name: &str, rel: &str) -> Response {
    let Some(root) = store::current_store_path(&state.paths) else {
        return not_found("no active generation");
    };
    let www = root.join("services").join(name).join("www");
    if !www.is_dir() {
        return not_found("service not found");
    }
    let Some(mut file) = resolve_safe(&www, rel) else {
        return not_found("path not found");
    };
    if file.is_dir() {
        file = file.join("index.html");
    }
    match tokio::fs::read(&file).await {
        Ok(bytes) => {
            let mime = mime_guess::from_path(&file).first_or_octet_stream();
            let content_type = if mime.type_() == mime_guess::mime::TEXT {
                format!("{mime}; charset=utf-8")
            } else {
                mime.to_string()
            };
            ([(header::CONTENT_TYPE, content_type)], bytes).into_response()
        }
        Err(_) => not_found("path not found"),
    }
}

/// Join a client-supplied relative path onto a root, refusing anything that
/// could step outside it.
pub fn resolve_safe(root: &FsPath, rel: &str) -> Option<PathBuf> {
    let mut path = root.to_path_buf();
    for part in rel.split('/') {
        if part.is_empty() || part == "." {
            continue;
        }
        if part == ".." || part.contains('\\') || part.contains('\0') {
            return None;
        }
        path.push(part);
    }
    Some(path)
}

fn not_found(msg: &str) -> Response {
    (StatusCode::NOT_FOUND, msg.to_string()).into_response()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_traversal() {
        let root = FsPath::new("/srv/www");
        assert_eq!(
            resolve_safe(root, "a/b.css"),
            Some(PathBuf::from("/srv/www/a/b.css"))
        );
        assert_eq!(resolve_safe(root, ""), Some(PathBuf::from("/srv/www")));
        assert_eq!(
            resolve_safe(root, "././x"),
            Some(PathBuf::from("/srv/www/x"))
        );
        assert!(resolve_safe(root, "../secret").is_none());
        assert!(resolve_safe(root, "a/../../secret").is_none());
        assert!(resolve_safe(root, "a\\..\\b").is_none());
    }
}
