//! GitHub API integration.
//!
//! gentooit interacts with GitHub in three ways:
//!
//! 1. **Reading upstream releases** to discover versions and archive URLs
//!    (via the releases API).
//! 2. **Downloading upstream release archives** to build Manifests.
//! 3. **Opening pull requests** against the downstream overlay repo, using the
//!    standard fork-and-PR model.
//!
//! For the service layer, the same client supports GitHub App authentication
//! (JWT + installation tokens) so the service can act on behalf of repositories.

use base64::engine::{general_purpose::STANDARD, Engine};
use secrecy::ExposeSecret;
use std::path::Path;
use std::sync::Arc;

use octocrab::{Octocrab, OctocrabBuilder};

/// A wrapper around `octocrab`, the GitHub API client.
#[derive(Debug, Clone)]
pub struct GitHub {
    inner: Arc<Octocrab>,
}

impl GitHub {
    /// Create an unauthenticated client (works for public repos within
    /// GitHub's anonymous rate limit, suitable for read-only operations like
    /// listing releases and downloading public archives).
    pub fn anonymous() -> anyhow::Result<GitHub> {
        let oc = Octocrab::builder().build()?;
        Ok(GitHub {
            inner: Arc::new(oc),
        })
    }

    /// Create a client authenticated with a personal access token.
    pub fn with_token(token: impl Into<String>) -> anyhow::Result<GitHub> {
        let oc = Octocrab::builder().personal_token(token.into()).build()?;
        Ok(GitHub {
            inner: Arc::new(oc),
        })
    }
    /// Create a client authenticated as a GitHub App.
    pub fn with_app(app_id: i64, key_path: &Path) -> anyhow::Result<GitHub> {
        let key_bytes = std::fs::read(key_path)?;
        let key = jsonwebtoken::EncodingKey::from_rsa_pem(&key_bytes)?;
        let oc = OctocrabBuilder::new()
            .app(octocrab::models::AppId::from(app_id as u64), key)
            .build()?;
        Ok(GitHub {
            inner: Arc::new(oc),
        })
    }

    /// Re-authenticate an app client as a specific installation, so it can act
    /// on behalf of a repository owner.
    pub fn with_installation(&self, installation_id: u64) -> anyhow::Result<GitHub> {
        let inner = self
            .inner
            .installation(octocrab::models::InstallationId::from(installation_id))?;
        Ok(GitHub {
            inner: Arc::new(inner),
        })
    }

    /// Get the GitHub App installation for a repository. Requires App auth.
    pub async fn get_repository_installation(
        &self,
        owner: &str,
        repo: &str,
    ) -> anyhow::Result<octocrab::models::Installation> {
        let installation = self
            .inner
            .apps()
            .get_repository_installation(owner, repo)
            .await?;
        Ok(installation)
    }

    /// Fetch a text file from a repository root. Returns None if the file does
    /// not exist.
    pub async fn fetch_file(
        &self,
        owner: &str,
        repo: &str,
        path: &str,
    ) -> anyhow::Result<Option<String>> {
        let route = format!("/repos/{owner}/{repo}/contents/{path}");
        let json: serde_json::Value = match self.inner.get(&route, None::<&()>).await {
            Ok(v) => v,
            Err(_) => return Ok(None),
        };
        let content = match json.get("content").and_then(|c| c.as_str()) {
            Some(c) => c,
            None => return Ok(None),
        };
        let decoded = STANDARD.decode(content.replace('\n', ""))?;
        Ok(Some(String::from_utf8(decoded)?))
    }

    /// Get an installation access token for a repository's installation.
    pub async fn installation_token_for_repo(
        &self,
        owner: &str,
        repo: &str,
    ) -> anyhow::Result<Option<String>> {
        let installation = self.get_repository_installation(owner, repo).await?;
        let client = self.with_installation(*installation.id)?;
        client.installation_token().await
    }

    /// Resolve the authenticated user's login from the current token. Works
    /// for PATs, fine-grained tokens, and GitHub App user access tokens.
    pub async fn resolve_username(&self) -> anyhow::Result<String> {
        let user = self.inner.current().user().await?;
        Ok(user.login)
    }

    /// Get an installation access token if this client is authenticated as a
    /// GitHub App installation. Returns None if not an installation client or
    /// if the request fails.
    pub async fn installation_token(&self) -> anyhow::Result<Option<String>> {
        match self.inner.installation_token().await {
            Ok(token) => Ok(Some(token.expose_secret().to_string())),
            Err(_) => Ok(None),
        }
    }

    /// Post a comment on an issue or pull request.
    pub async fn post_comment(
        &self,
        owner: &str,
        repo: &str,
        number: u64,
        body: &str,
    ) -> anyhow::Result<()> {
        self.inner
            .issues(owner, repo)
            .create_comment(number, body)
            .await?;
        Ok(())
    }

    /// Fetch the latest release for `owner/repo`. Returns None if there are no
    /// releases.
    pub async fn latest_release(&self, owner: &str, repo: &str) -> anyhow::Result<Option<Release>> {
        let releases = self
            .inner
            .repos(owner, repo)
            .releases()
            .list()
            .per_page(1)
            .send()
            .await?;
        Ok(releases.items.into_iter().next().map(Release::from))
    }

    /// Fetch a specific release by tag name.
    pub async fn release_by_tag(
        &self,
        owner: &str,
        repo: &str,
        tag: &str,
    ) -> anyhow::Result<Release> {
        let release = self
            .inner
            .repos(owner, repo)
            .releases()
            .get_by_tag(tag)
            .await?;
        Ok(Release::from(release))
    }

    /// List all tags for `owner/repo` (used to discover versions when releases
    /// aren't used).
    pub async fn tags(&self, owner: &str, repo: &str) -> anyhow::Result<Vec<String>> {
        let response = self
            .inner
            .repos(owner, repo)
            .list_tags()
            .per_page(100)
            .send()
            .await?;
        Ok(response.items.into_iter().map(|t| t.name).collect())
    }

    /// Create a pull request in `owner/repo`.
    pub async fn create_pull_request(
        &self,
        owner: &str,
        repo: &str,
        title: &str,
        head: &str,
        base: &str,
        body: &str,
    ) -> anyhow::Result<PullRequest> {
        let pr = self
            .inner
            .pulls(owner, repo)
            .create(title, head, base)
            .body(body)
            .send()
            .await?;
        Ok(PullRequest {
            number: pr.number,
            html_url: pr.html_url.map(|u| u.to_string()).unwrap_or_default(),
            title: pr.title.unwrap_or_default(),
        })
    }

    /// Get the default branch of a repo (to know what to base a PR on).
    pub async fn default_branch(&self, owner: &str, repo: &str) -> anyhow::Result<String> {
        let info = self.inner.repos(owner, repo).get().await?;
        Ok(info.default_branch.unwrap_or_else(|| "master".to_string()))
    }

    /// Download a URL to `dest_path`, with optional auth.
    pub async fn download(&self, url: &str, dest_path: &Path) -> anyhow::Result<()> {
        let client = reqwest::Client::new();
        let resp = client.get(url).send().await?;
        if !resp.status().is_success() {
            anyhow::bail!("failed to download {url}: HTTP {}", resp.status());
        }
        let bytes = resp.bytes().await?;
        if let Some(parent) = dest_path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(dest_path, bytes)?;
        Ok(())
    }

    /// Create a fork of `owner/repo` on behalf of the authenticated user.
    pub async fn fork(&self, owner: &str, repo: &str) -> anyhow::Result<()> {
        self.inner.repos(owner, repo).create_fork().send().await?;
        Ok(())
    }
}

/// A GitHub release as gentooit needs it.
#[derive(Debug, Clone)]
pub struct Release {
    pub tag_name: String,
    pub name: Option<String>,
    pub body: Option<String>,
    pub assets: Vec<ReleaseAsset>,
    /// The archive URL of the associated git tag tarball.
    pub tarball_url: Option<String>,
}

impl From<octocrab::models::repos::Release> for Release {
    fn from(r: octocrab::models::repos::Release) -> Self {
        Release {
            tag_name: r.tag_name,
            name: r.name,
            body: r.body,
            assets: r
                .assets
                .into_iter()
                .map(|a| ReleaseAsset {
                    name: a.name,
                    browser_download_url: a.browser_download_url.to_string(),
                })
                .collect(),
            tarball_url: r.tarball_url.map(|u| u.to_string()),
        }
    }
}

/// A release asset (a downloadable file attached to a release).
#[derive(Debug, Clone)]
pub struct ReleaseAsset {
    pub name: String,
    pub browser_download_url: String,
}

/// A pull request summary, in the shape gentooit returns.
#[derive(Debug, Clone)]
pub struct PullRequest {
    pub number: u64,
    pub html_url: String,
    pub title: String,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn build_with_token() {
        assert!(GitHub::with_token("test").is_ok());
    }
}
