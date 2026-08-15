//! GitLab, reached through the same device flow — with one honest difference.
//!
//! GitLab has no per-repository consent. Its `read_api` scope covers every
//! project the person can see, so connecting a GitLab account gives this Box
//! broader reach than connecting a GitHub one does. That is GitLab's model, not
//! a shortcut taken here, and the console says it out loud rather than letting
//! someone assume the GitHub story applies.
//!
//! The other difference is structural: an OAuth application is registered per
//! instance, so a self-hosted GitLab needs its own. There is no single
//! registration that can work everywhere, which is why this forge asks for a
//! client id instead of shipping one.
//!
//! Requires GitLab 17.9 or newer, where the device grant went generally
//! available. Older instances have no `/oauth/authorize_device` at all.

use anyhow::{Context, Result};
use serde_json::Value;

use crate::config::ForgeConfig;
use crate::forge::{Endpoints, Forge, Repo, Requirements};

pub const DEFAULT_BASE_URL: &str = "https://gitlab.com";

/// An application registered on gitlab.com, if the product ships one. Self-
/// managed instances can never use it, so unlike GitHub this is a convenience
/// rather than the expected path.
pub const CLIENT_ID: Option<&str> = None;

/// `read_api` to list projects, `read_repository` to clone them. Deliberately
/// no write scope: nothing in the deploy loop needs to change the person's
/// code, and a token that cannot push cannot rewrite history if it leaks.
const SCOPE: &str = "read_api read_repository";

pub struct GitLab;

fn base_url(cfg: &ForgeConfig) -> String {
    cfg.base_url
        .as_deref()
        .unwrap_or(DEFAULT_BASE_URL)
        .trim_end_matches('/')
        .to_string()
}

impl Forge for GitLab {
    fn id(&self) -> &'static str {
        "gitlab"
    }
    fn title(&self) -> &'static str {
        "GitLab"
    }
    fn description(&self) -> &'static str {
        "Deploy from a project on GitLab.com or your own GitLab. Read-only access to the projects you can see."
    }
    fn requirements(&self) -> Requirements {
        Requirements {
            needs_own_app: true,
            needs_base_url: true,
            // GitLab grants by scope, not by project. Saying otherwise would be
            // promising a limit that does not exist.
            per_repo_consent: false,
        }
    }
    fn steps(&self) -> Vec<&'static str> {
        vec![
            "On GitLab, add an application under User Settings → Applications with the scopes read_api and read_repository.",
            "Untick \"Confidential\" — the device flow is a public client and will be refused otherwise.",
            "Give the Box the Application ID, then open the link it shows you and type in the code.",
        ]
    }

    fn git_auth(&self, cfg: &ForgeConfig, token: &str) -> crate::forge::GitAuth {
        // `oauth2` is GitLab's documented basic-auth username for OAuth tokens.
        // Scoped to this instance's own address, and to nothing else.
        crate::forge::GitAuth::basic(format!("{}/", base_url(cfg)), "oauth2", token)
    }

    fn endpoints(&self, cfg: &ForgeConfig) -> Result<Endpoints> {
        let base = base_url(cfg);
        let baked = (base == DEFAULT_BASE_URL).then_some(CLIENT_ID).flatten();
        let client_id = cfg
            .client_id
            .as_deref()
            .or(baked)
            .with_context(|| {
                format!(
                    "no GitLab application is configured for {base}. Add one under \
                     User Settings → Applications with scopes read_api and \
                     read_repository, leave \"Confidential\" unticked, and set its \
                     Application ID as client_id on the gitlab forge in box.toml."
                )
            })?
            .to_string();
        Ok(Endpoints {
            device_code_url: format!("{base}/oauth/authorize_device"),
            token_url: format!("{base}/oauth/token"),
            client_id,
            scope: Some(SCOPE),
        })
    }

    fn list_repos(&self, token: &str, cfg: &ForgeConfig) -> Result<Vec<Repo>> {
        let base = base_url(cfg);
        // `membership=true` is the difference between "projects this person is
        // on" and "every public project on the instance".
        let mut url = format!(
            "{base}/api/v4/projects?membership=true&per_page=100&order_by=last_activity_at"
        );
        let mut repos = Vec::new();
        loop {
            let (value, next) = crate::forge::get_json(&url, token)?;
            let page = value.as_array().cloned().unwrap_or_default();
            repos.extend(page.iter().filter_map(parse_project));
            match next {
                Some(n) => url = n,
                None => break,
            }
        }
        Ok(repos)
    }

    fn grant_more_url(&self, _cfg: &ForgeConfig) -> Option<String> {
        // Nothing to grant: the token already reaches everything it ever will.
        None
    }
}

/// One project out of GitLab's JSON.
pub fn parse_project(v: &Value) -> Option<Repo> {
    Some(Repo {
        full_name: v
            .get("path_with_namespace")
            .and_then(Value::as_str)?
            .to_string(),
        default_branch: v
            .get("default_branch")
            .and_then(Value::as_str)
            .unwrap_or("main")
            .to_string(),
        // GitLab has three visibilities. Only `public` is not private, and an
        // absent field means we do not know, which is not a reason to say public.
        private: v.get("visibility").and_then(Value::as_str) != Some("public"),
        clone_url: v
            .get("http_url_to_repo")
            .and_then(Value::as_str)?
            .to_string(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn a_self_hosted_instance_gets_its_own_endpoints() {
        let mut c = ForgeConfig::new("gitlab");
        c.base_url = Some("https://git.example.com/".into());
        c.client_id = Some("appid".into());
        let ep = GitLab.endpoints(&c).unwrap();
        // The trailing slash must not survive into a double-slashed URL.
        assert_eq!(
            ep.device_code_url,
            "https://git.example.com/oauth/authorize_device"
        );
        assert_eq!(ep.token_url, "https://git.example.com/oauth/token");
        assert_eq!(ep.scope, Some(SCOPE));
    }

    #[test]
    fn a_self_hosted_instance_can_never_borrow_the_shipped_application() {
        // An application id is instance-local. Falling back to a gitlab.com one
        // here would produce an authentication error nobody could interpret.
        let mut c = ForgeConfig::new("gitlab");
        c.base_url = Some("https://git.example.com".into());
        let err = GitLab.endpoints(&c).unwrap_err().to_string();
        assert!(err.contains("git.example.com"), "{err}");
        assert!(err.contains("Confidential"), "{err}");
    }

    #[test]
    fn the_scopes_asked_for_cannot_change_the_persons_code() {
        // Compare whole scopes: `read_api` contains the string "api" without
        // being the `api` scope, which is read-write over everything.
        let asked: Vec<&str> = SCOPE.split_whitespace().collect();
        assert_eq!(asked, ["read_api", "read_repository"]);
        assert!(!asked.contains(&"api"), "the api scope can push code");
        assert!(!asked.iter().any(|s| s.starts_with("write")));
    }

    #[test]
    fn only_a_public_project_is_reported_public() {
        let p = |v: Value| parse_project(&v).unwrap().private;
        let with = |vis: &str| {
            json!({
                "path_with_namespace": "g/p", "visibility": vis,
                "http_url_to_repo": "https://gitlab.com/g/p.git"
            })
        };
        assert!(!p(with("public")));
        assert!(p(with("internal")));
        assert!(p(with("private")));
        // Unknown visibility is private, never the other way round.
        assert!(p(json!({
            "path_with_namespace": "g/p",
            "http_url_to_repo": "https://gitlab.com/g/p.git"
        })));
    }

    #[test]
    fn gitlab_offers_nothing_to_grant_because_it_has_nothing_to_grant() {
        assert_eq!(GitLab.grant_more_url(&ForgeConfig::new("gitlab")), None);
        assert!(!GitLab.requirements().per_repo_consent);
    }
}
