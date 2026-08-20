//! GitHub, reached as a GitHub App through the device flow.
//!
//! An App rather than an OAuth app, and an App rather than a personal access
//! token, for one reason: consent is per repository. The person ticks the repos
//! they want on GitHub's own installation screen, and the token this Box ends
//! up holding reaches those and nothing else. A classic PAT's `repo` scope, by
//! contrast, is every repository they own — the same over-broad credential
//! problem as a Cloudflare API token, handed to the part of the system that
//! also builds code it did not write.
//!
//! Only a client id ships in the OS image. The device flow has no client
//! secret, so there is nothing confidential to leak out of a machine we do not
//! control — which is what makes this grant the right one for an appliance.

use anyhow::{bail, Context, Result};
use serde_json::Value;

use crate::config::ForgeConfig;
use crate::forge::{Endpoints, Forge, Repo, Requirements};

const API: &str = "https://api.github.com";

/// The Box's own GitHub App.
///
/// A client id is public by design — it ships in every copy of this OS and is
/// visible to anyone who looks. That is safe here precisely because the device
/// flow has no client secret: there is no companion credential that would turn
/// knowing this string into being able to act as the app. The App also holds no
/// client secret at all, deliberately, so there is none to leak later.
///
/// Overridable per Box: anyone who would rather not depend on a registration
/// administered by someone else can point their Box at their own App. That is
/// the whole mitigation for the one piece of this design that is centralised.
pub const CLIENT_ID: Option<&str> = Some("Iv23liudDywv4yTPeFxz");

/// The App's public slug, used to build the "share more repositories" link.
pub const APP_SLUG: Option<&str> = Some("the-box-integration");

pub struct GitHub;

impl Forge for GitHub {
    fn id(&self) -> &'static str {
        "github"
    }
    fn title(&self) -> &'static str {
        "GitHub"
    }
    fn description(&self) -> &'static str {
        "Deploy from a repository on GitHub. You choose which repositories this Box can see."
    }
    fn requirements(&self) -> Requirements {
        Requirements {
            needs_own_app: false,
            needs_base_url: false,
            per_repo_consent: true,
        }
    }
    fn steps(&self) -> Vec<&'static str> {
        vec![
            "Open the link the Box gives you and type in the code it shows.",
            "Choose the repositories this Box may read. You can change that later on GitHub, and nothing else on your account is shared.",
        ]
    }

    fn git_auth(&self, _cfg: &ForgeConfig, token: &str) -> crate::forge::GitAuth {
        // `x-access-token` is GitHub's documented basic-auth username for app
        // tokens. Scoped to github.com, so nothing else ever sees the header.
        crate::forge::GitAuth::basic("https://github.com/", "x-access-token", token)
    }

    fn endpoints(&self, cfg: &ForgeConfig) -> Result<Endpoints> {
        let client_id = cfg
            .client_id
            .as_deref()
            .or(CLIENT_ID)
            .context(
                "this Box has no GitHub App to authenticate with. Register one \
                 (Settings → Developer settings → GitHub Apps), tick \"Enable \
                 Device Flow\", and set its Client ID as client_id on the github \
                 forge in box.toml.",
            )?
            .to_string();
        Ok(Endpoints {
            device_code_url: "https://github.com/login/device/code".into(),
            token_url: "https://github.com/login/oauth/access_token".into(),
            client_id,
            // A GitHub App's permissions come from its registration and from
            // what the person ticked when installing it. Asking for a scope
            // here would be claiming to decide something we do not decide.
            scope: None,
        })
    }

    /// Every repository shared with this Box, across every account and org the
    /// App was installed on.
    ///
    /// Two levels, because that is how App consent works: the person installs
    /// the App somewhere (their user, an org), and within each installation
    /// picks repositories. A user token can enumerate both.
    fn list_repos(&self, token: &str, _cfg: &ForgeConfig) -> Result<Vec<Repo>> {
        let mut repos = Vec::new();
        for id in installation_ids(token)? {
            let mut url = format!("{API}/user/installations/{id}/repositories?per_page=100");
            loop {
                let (value, next) = crate::forge::get_json(&url, token)?;
                let page = value
                    .get("repositories")
                    .and_then(Value::as_array)
                    .cloned()
                    .unwrap_or_default();
                repos.extend(page.iter().filter_map(parse_repo));
                match next {
                    Some(n) => url = n,
                    None => break,
                }
            }
        }
        repos.sort_by(|a, b| a.full_name.cmp(&b.full_name));
        repos.dedup_by(|a, b| a.full_name == b.full_name);
        Ok(repos)
    }

    fn grant_more_url(&self, cfg: &ForgeConfig) -> Option<String> {
        let slug = cfg.app_slug.as_deref().or(APP_SLUG)?;
        Some(format!("https://github.com/apps/{slug}/installations/new"))
    }

    fn empty_hint(&self, token: &str, cfg: &ForgeConfig) -> Option<String> {
        let (value, _) =
            crate::forge::get_json(&format!("{API}/user/installations?per_page=100"), token)
                .ok()?;
        let installs = value.get("installations").and_then(Value::as_array)?;
        Some(diagnose_empty(
            installs,
            self.grant_more_url(cfg).as_deref(),
        ))
    }
}

/// The three ways an installation list adds up to zero repositories, told
/// apart. Pure, so the shapes seen on a real account stay pinned in tests.
pub fn diagnose_empty(installs: &[Value], grant_url: Option<&str>) -> String {
    let where_to = grant_url.unwrap_or("the app's installation page on GitHub");
    if installs.is_empty() {
        return format!(
            "They authorized the app but it is not installed on any account yet. \
             Send them {where_to} — they pick an account and tick repositories there."
        );
    }
    // An installation whose permissions carry no repository access at all
    // means the APP REGISTRATION requests none — nothing the person ticks can
    // help, and only the app's owner can fix it. This happened live: the
    // registration was saved without Contents access, the install screen had
    // nothing meaningful to offer, and every list came back empty.
    let no_permissions = installs.iter().all(|i| {
        i.get("permissions")
            .and_then(Value::as_object)
            .map(|p| !p.contains_key("contents"))
            .unwrap_or(true)
    });
    if no_permissions {
        return "The app is installed, but its registration requests no repository \
                permissions — sharing repositories grants nothing until that changes. \
                This is fixed by the APP'S OWNER (not the person): app settings → \
                Permissions & events → Repository permissions → Contents: Read-only, \
                then each installation approves the change."
            .to_string();
    }
    format!(
        "The app is installed and has permissions, but no repositories are ticked. \
         Send them {where_to} — under Repository access they choose what to share."
    )
}

fn installation_ids(token: &str) -> Result<Vec<u64>> {
    let mut ids = Vec::new();
    let mut url = format!("{API}/user/installations?per_page=100");
    loop {
        let (value, next) = crate::forge::get_json(&url, token)?;
        let Some(list) = value.get("installations").and_then(Value::as_array) else {
            bail!("GitHub did not list any installations for this account");
        };
        ids.extend(
            list.iter()
                .filter_map(|i| i.get("id").and_then(Value::as_u64)),
        );
        match next {
            Some(n) => url = n,
            None => break,
        }
    }
    Ok(ids)
}

/// One repository out of GitHub's JSON. Anything missing the fields we need to
/// clone it is skipped rather than guessed at.
pub fn parse_repo(v: &Value) -> Option<Repo> {
    Some(Repo {
        full_name: v.get("full_name").and_then(Value::as_str)?.to_string(),
        // A repository with no commits has no default branch. It is listable
        // and not deployable, so name the branch GitHub would create.
        default_branch: v
            .get("default_branch")
            .and_then(Value::as_str)
            .unwrap_or("main")
            .to_string(),
        private: v.get("private").and_then(Value::as_bool).unwrap_or(true),
        clone_url: v.get("clone_url").and_then(Value::as_str)?.to_string(),
    })
}

/// Register a push webhook on a repository, pointing at this Box's receiver.
/// GitHub speaks its own dialect (no Cloudflare-style envelope; errors in a
/// `message` field; 201 on creation), so this sends for itself. The App must
/// hold "Repository webhooks: Read & write" — when it does not, GitHub's own
/// error comes back and the tool names the owner-only fix.
pub fn register_hook(token: &str, repo: &str, url: &str, secret: &str) -> Result<Value> {
    let body = serde_json::json!({
        "config": { "url": url, "content_type": "json", "secret": secret },
        "events": ["push", "pull_request"],
        "active": true,
    });
    let out = std::process::Command::new("curl")
        .args(["-sS", "-m", "30", "-X", "POST"])
        .args(["-H", &format!("Authorization: Bearer {token}")])
        .args(["-H", "Accept: application/vnd.github+json"])
        .args(["-H", "Content-Type: application/json"])
        .args(["-d", &body.to_string()])
        .args(["-w", "\n%{http_code}"])
        .arg(format!("{API}/repos/{repo}/hooks"))
        .output()
        .context("running curl against the GitHub API")?;
    if !out.status.success() {
        bail!(
            "could not reach GitHub: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    let text = String::from_utf8_lossy(&out.stdout);
    let (payload, status) = text.rsplit_once('\n').unwrap_or((text.as_ref(), ""));
    let value: Value = serde_json::from_str(payload.trim())
        .context("GitHub returned something that is not JSON")?;
    if status.trim() != "201" {
        bail!(
            "GitHub refused the webhook (HTTP {}): {}",
            status.trim(),
            value
                .get("message")
                .and_then(Value::as_str)
                .unwrap_or("no detail")
        );
    }
    Ok(value)
}

/// Open a pull request and return its html_url. Used by the work runner:
/// the agent's branch is already pushed; this is the handoff to the human.
pub fn create_pull_request(
    token: &str,
    repo: &str,
    head: &str,
    base: &str,
    title: &str,
    body: &str,
) -> Result<String> {
    let payload = serde_json::json!({ "title": title, "head": head, "base": base, "body": body });
    let out = std::process::Command::new("curl")
        .args(["-sS", "-m", "30", "-X", "POST"])
        .args(["-H", &format!("Authorization: Bearer {token}")])
        .args(["-H", "Accept: application/vnd.github+json"])
        .args(["-H", "Content-Type: application/json"])
        .args(["-d", &payload.to_string()])
        .args(["-w", "\n%{http_code}"])
        .arg(format!("{API}/repos/{repo}/pulls"))
        .output()
        .context("running curl against the GitHub API")?;
    let text = String::from_utf8_lossy(&out.stdout);
    let (payload, status) = text.rsplit_once('\n').unwrap_or((text.as_ref(), ""));
    let value: Value = serde_json::from_str(payload.trim())
        .context("GitHub returned something that is not JSON")?;
    if status.trim() != "201" {
        bail!(
            "GitHub refused the pull request (HTTP {}): {}",
            status.trim(),
            value
                .get("message")
                .and_then(Value::as_str)
                .unwrap_or("no detail")
        );
    }
    value
        .get("html_url")
        .and_then(Value::as_str)
        .map(str::to_string)
        .context("the created PR has no html_url")
}

/// Create a PRIVATE repository for the calling token's user and return
/// (full_name, ssh_url, html_url). The token is used for this call and the
/// deploy-key install, then discarded — pushes ride the deploy key.
pub fn create_repo(token: &str, name: &str) -> Result<(String, String, String)> {
    let payload = serde_json::json!({
        "name": name,
        "private": true,
        "auto_init": false,
        "description": "The Box: this machine's config and encrypted secrets (pushed by the Box itself)",
    });
    let value = gh_post(token, &format!("{API}/user/repos"), &payload)?;
    let s = |k: &str| {
        value
            .get(k)
            .and_then(Value::as_str)
            .map(str::to_string)
            .with_context(|| format!("created repo has no {k}"))
    };
    Ok((s("full_name")?, s("ssh_url")?, s("html_url")?))
}

/// Install a WRITE deploy key on a repo — the Box's own push credential, so
/// the account token never has to live on the Box.
pub fn add_deploy_key(token: &str, full_name: &str, title: &str, pubkey: &str) -> Result<()> {
    let payload = serde_json::json!({ "title": title, "key": pubkey, "read_only": false });
    gh_post(token, &format!("{API}/repos/{full_name}/keys"), &payload).map(|_| ())
}

fn gh_post(token: &str, url: &str, payload: &Value) -> Result<Value> {
    let out = std::process::Command::new("curl")
        .args(["-sS", "-m", "30", "-X", "POST"])
        .args(["-H", &format!("Authorization: Bearer {token}")])
        .args(["-H", "Accept: application/vnd.github+json"])
        .args(["-H", "Content-Type: application/json"])
        .args(["-d", &payload.to_string()])
        .args(["-w", "\n%{http_code}"])
        .arg(url)
        .output()
        .context("running curl against the GitHub API")?;
    let text = String::from_utf8_lossy(&out.stdout);
    let (body, status) = text.rsplit_once('\n').unwrap_or((text.as_ref(), ""));
    let value: Value = serde_json::from_str(body.trim())
        .context("GitHub returned something that is not JSON")?;
    if !matches!(status.trim(), "200" | "201") {
        bail!(
            "GitHub said no (HTTP {}): {}",
            status.trim(),
            value
                .get("message")
                .and_then(Value::as_str)
                .unwrap_or("no detail")
        );
    }
    Ok(value)
}

/// Leave a comment on an issue or pull request (they share the endpoint).
/// Best-effort by design at the call sites: the App may lack the permission,
/// and a preview that deploys without its comment still deployed.
pub fn comment_on_issue(token: &str, repo: &str, number: u64, body: &str) -> Result<()> {
    let payload = serde_json::json!({ "body": body });
    let out = std::process::Command::new("curl")
        .args(["-sS", "-m", "30", "-X", "POST"])
        .args(["-H", &format!("Authorization: Bearer {token}")])
        .args(["-H", "Accept: application/vnd.github+json"])
        .args(["-H", "Content-Type: application/json"])
        .args(["-d", &payload.to_string()])
        .args(["-w", "\n%{http_code}"])
        .arg(format!("{API}/repos/{repo}/issues/{number}/comments"))
        .output()
        .context("running curl against the GitHub API")?;
    let text = String::from_utf8_lossy(&out.stdout);
    let (payload, status) = text.rsplit_once('\n').unwrap_or((text.as_ref(), ""));
    if status.trim() != "201" {
        let detail = serde_json::from_str::<Value>(payload.trim())
            .ok()
            .and_then(|v| v.get("message").and_then(Value::as_str).map(str::to_string))
            .unwrap_or_else(|| "no detail".into());
        bail!(
            "GitHub refused the comment (HTTP {}): {detail}",
            status.trim()
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn cfg() -> ForgeConfig {
        ForgeConfig::new("github")
    }

    #[test]
    fn a_stock_box_can_connect_with_no_configuration_at_all() {
        // The point of shipping a registration: a person who just installed
        // this OS can connect their GitHub account without first learning what
        // an OAuth application is.
        let ep = GitHub.endpoints(&cfg()).unwrap();
        assert_eq!(ep.client_id, CLIENT_ID.unwrap());
        assert_eq!(ep.token_url, "https://github.com/login/oauth/access_token");
    }

    #[test]
    fn the_shipped_app_has_no_secret_to_leak() {
        // A device-flow client id is public by design. This test exists to stop
        // anyone "fixing" that by adding a client secret next to it — a secret
        // shipped in an OS image is a secret published, and the whole reason
        // this grant was chosen is that it needs none.
        // Only the half that ships. The test module below necessarily names the
        // thing it is forbidding, and would otherwise trip over itself.
        let src = include_str!("ghapi.rs");
        let ships = src.split("#[cfg(test)]").next().unwrap();
        assert!(
            !ships
                .to_ascii_lowercase()
                .contains(concat!("client", "_secret")),
            "no client secret belongs in an image that ships to other people"
        );
    }

    #[test]
    fn a_box_can_use_its_own_app_registration() {
        // The mitigation for the one centralised piece of this design.
        let mut c = cfg();
        c.client_id = Some("Iv23liOWN".into());
        let ep = GitHub.endpoints(&c).unwrap();
        assert_eq!(ep.client_id, "Iv23liOWN");
        assert_eq!(ep.device_code_url, "https://github.com/login/device/code");
        assert!(ep.scope.is_none());
    }

    #[test]
    fn the_share_more_link_points_at_the_installation_screen() {
        // A stock Box links to the shipped App's installation screen...
        assert_eq!(
            GitHub.grant_more_url(&cfg()).as_deref(),
            Some("https://github.com/apps/the-box-integration/installations/new")
        );
        // ...and a Box using its own App registration links to its own.
        let mut c = cfg();
        c.app_slug = Some("my-own-app".into());
        assert_eq!(
            GitHub.grant_more_url(&c).as_deref(),
            Some("https://github.com/apps/my-own-app/installations/new")
        );
    }

    #[test]
    fn a_repository_with_no_commits_still_names_a_branch() {
        let r = parse_repo(&json!({
            "full_name": "octo/hello",
            "private": false,
            "clone_url": "https://github.com/octo/hello.git"
        }))
        .unwrap();
        assert_eq!(r.default_branch, "main");
        assert!(!r.private);

        // Anything we could not clone is not offered as deployable.
        assert!(parse_repo(&json!({ "full_name": "octo/hello" })).is_none());
    }

    #[test]
    fn the_three_kinds_of_zero_repositories_get_three_different_answers() {
        // Not installed anywhere: the person can fix it, send the link.
        let nowhere = diagnose_empty(&[], Some("https://github.com/apps/x/installations/new"));
        assert!(nowhere.contains("not installed"), "{nowhere}");
        assert!(nowhere.contains("installations/new"), "{nowhere}");

        // Installed with empty permissions — the shape observed live on a real
        // account (installation 153337497, "permissions": {}): only the app's
        // owner can fix this, and saying "tick repositories" would be wrong.
        let unpermissioned = diagnose_empty(&[json!({"id": 1, "permissions": {}})], None);
        assert!(unpermissioned.contains("OWNER"), "{unpermissioned}");
        assert!(unpermissioned.contains("Contents"), "{unpermissioned}");

        // Installed, permissions fine, nothing ticked: back to the person.
        let unticked = diagnose_empty(
            &[json!({"id": 1, "permissions": {"contents": "read", "metadata": "read"}})],
            Some("https://github.com/apps/x/installations/new"),
        );
        assert!(
            unticked.contains("no repositories are ticked"),
            "{unticked}"
        );
    }

    #[test]
    fn a_repository_of_unknown_visibility_is_treated_as_private() {
        // Erring the other way would print someone's private repo name as
        // public in the console.
        let r = parse_repo(&json!({
            "full_name": "a/b", "clone_url": "https://github.com/a/b.git"
        }))
        .unwrap();
        assert!(r.private);
    }
}
