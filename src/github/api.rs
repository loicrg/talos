use crate::domain::Repository;
use anyhow::{Context, Result, bail};
use serde::Deserialize;
use std::{fmt, process::Stdio, time::Duration};
use tokio::{process::Command, time};

const API_ROOT: &str = "https://api.github.com";
const API_VERSION: &str = "2026-03-10";

pub struct GitHubClient {
    http: reqwest::Client,
    token: String,
    api_root: String,
}

impl Drop for GitHubClient {
    fn drop(&mut self) {
        use zeroize::Zeroize;
        self.token.zeroize();
    }
}

#[derive(Clone, Debug, Deserialize)]
pub struct ApiRunner {
    pub id: u64,
    pub name: String,
    pub status: String,
    #[serde(default)]
    pub busy: Option<bool>,
    #[serde(default)]
    pub version: Option<String>,
    #[serde(default)]
    pub labels: Vec<ApiLabel>,
}

#[derive(Clone, Debug, Deserialize)]
pub struct ApiLabel {
    pub name: String,
}

impl ApiRunner {
    pub fn labels(&self) -> Vec<String> {
        self.labels.iter().map(|label| label.name.clone()).collect()
    }
}

#[derive(Debug, Deserialize)]
struct RunnerPage {
    runners: Vec<ApiRunner>,
    total_count: usize,
}

#[derive(Debug, Deserialize)]
struct TokenResponse {
    token: String,
    expires_at: String,
}

impl Drop for TokenResponse {
    fn drop(&mut self) {
        use zeroize::Zeroize;
        self.token.zeroize();
    }
}

pub struct RegistrationToken {
    value: String,
    pub expires_at: String,
}

impl RegistrationToken {
    pub fn expose(&self) -> &str {
        &self.value
    }
}

impl fmt::Debug for RegistrationToken {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("RegistrationToken")
            .field("value", &"[REDACTED]")
            .field("expires_at", &self.expires_at)
            .finish()
    }
}

impl Drop for RegistrationToken {
    fn drop(&mut self) {
        use zeroize::Zeroize;
        self.value.zeroize();
    }
}

#[derive(Clone, Debug, Deserialize)]
pub struct RunnerRelease {
    pub tag_name: String,
    pub assets: Vec<ReleaseAsset>,
}

#[derive(Clone, Debug, Deserialize)]
pub struct ReleaseAsset {
    pub name: String,
    pub browser_download_url: String,
    pub digest: Option<String>,
}

impl GitHubClient {
    pub async fn from_gh_cli() -> Result<Self> {
        let child = Command::new("gh")
            .args(["auth", "token", "--hostname", "github.com"])
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .kill_on_drop(true)
            .spawn()
            .context("start `gh auth token`; install GitHub CLI and authenticate first")?;
        let result = time::timeout(Duration::from_secs(10), child.wait_with_output())
            .await
            .context("timed out reading local GitHub CLI authentication")?
            .context("read local GitHub CLI authentication")?;
        use zeroize::Zeroize;
        let mut token_output = result.stdout;
        if !result.status.success() {
            token_output.zeroize();
            bail!("GitHub CLI has no usable github.com authentication; run `gh auth login`");
        }
        let token_result = std::str::from_utf8(&token_output)
            .map(|value| value.trim().to_owned())
            .context("GitHub CLI returned a non-UTF8 token");
        token_output.zeroize();
        let token = token_result?;
        if token.is_empty() {
            bail!("GitHub CLI returned an empty token; run `gh auth login`");
        }
        let http = reqwest::Client::builder()
            .user_agent(concat!("talos/", env!("CARGO_PKG_VERSION")))
            .timeout(Duration::from_secs(20))
            .build()
            .context("build GitHub API client")?;
        Ok(Self {
            http,
            token,
            api_root: API_ROOT.to_owned(),
        })
    }

    async fn request(&self, method: reqwest::Method, url: &str) -> Result<reqwest::Response> {
        let response = self
            .http
            .request(method, url)
            .bearer_auth(&self.token)
            .header(reqwest::header::ACCEPT, "application/vnd.github+json")
            .header("X-GitHub-Api-Version", API_VERSION)
            .send()
            .await
            .with_context(|| format!("connect to GitHub API endpoint {url}"))?;
        if !response.status().is_success() {
            let status = response.status();
            let body = response.text().await.unwrap_or_default();
            let message = serde_json::from_str::<serde_json::Value>(&body)
                .ok()
                .and_then(|value| {
                    value
                        .get("message")
                        .and_then(serde_json::Value::as_str)
                        .map(str::to_owned)
                })
                .unwrap_or_else(|| "no additional error detail".to_owned())
                .replace(&self.token, "[REDACTED]");
            bail!("GitHub API returned {status} for {url}: {message}");
        }
        Ok(response)
    }

    pub async fn validate_repository(&self, repository: &Repository) -> Result<()> {
        let url = format!(
            "{}/repos/{}/{}",
            self.api_root, repository.owner, repository.name
        );
        self.request(reqwest::Method::GET, &url)
            .await
            .with_context(|| format!("validate access to repository '{repository}'"))?;
        Ok(())
    }

    pub async fn check_connectivity(&self) -> Result<()> {
        self.request(
            reqwest::Method::GET,
            &format!("{}/rate_limit", self.api_root),
        )
        .await
        .context("check GitHub API connectivity")?;
        Ok(())
    }

    pub async fn list_runners(&self, repository: &Repository) -> Result<Vec<ApiRunner>> {
        let mut page = 1;
        let mut runners = Vec::new();
        loop {
            let url = format!(
                "{}/repos/{}/{}/actions/runners?per_page=100&page={page}",
                self.api_root, repository.owner, repository.name
            );
            let response: RunnerPage = self
                .request(reqwest::Method::GET, &url)
                .await?
                .json()
                .await
                .with_context(|| format!("parse runner list for '{repository}'"))?;
            let reached_end = response.runners.is_empty()
                || runners.len() + response.runners.len() >= response.total_count;
            runners.extend(response.runners);
            if reached_end {
                return Ok(runners);
            }
            page += 1;
        }
    }

    pub async fn delete_runner(&self, repository: &Repository, runner_id: u64) -> Result<()> {
        let url = format!(
            "{}/repos/{}/{}/actions/runners/{runner_id}",
            self.api_root, repository.owner, repository.name
        );
        self.request(reqwest::Method::DELETE, &url)
            .await
            .with_context(|| format!("delete GitHub runner {runner_id} from '{repository}'"))?;
        Ok(())
    }

    pub async fn registration_token(&self, repository: &Repository) -> Result<RegistrationToken> {
        self.create_short_lived_token(repository, "registration-token")
            .await
    }

    pub async fn removal_token(&self, repository: &Repository) -> Result<RegistrationToken> {
        self.create_short_lived_token(repository, "remove-token")
            .await
    }

    async fn create_short_lived_token(
        &self,
        repository: &Repository,
        kind: &str,
    ) -> Result<RegistrationToken> {
        let url = format!(
            "{}/repos/{}/{}/actions/runners/{kind}",
            self.api_root, repository.owner, repository.name
        );
        let mut body: TokenResponse = self
            .request(reqwest::Method::POST, &url)
            .await?
            .json()
            .await
            .with_context(|| format!("parse GitHub runner {kind} response"))?;
        Ok(RegistrationToken {
            value: std::mem::take(&mut body.token),
            expires_at: std::mem::take(&mut body.expires_at),
        })
    }

    pub async fn latest_runner_release(&self) -> Result<RunnerRelease> {
        let url = format!("{}/repos/actions/runner/releases/latest", self.api_root);
        self.request(reqwest::Method::GET, &url)
            .await?
            .json()
            .await
            .context("parse latest GitHub Actions runner release")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mock_client(api_root: String) -> GitHubClient {
        GitHubClient {
            http: reqwest::Client::new(),
            token: "test-secret-token".into(),
            api_root,
        }
    }

    #[test]
    fn parses_runner_api_response_and_custom_labels() {
        let page: RunnerPage = serde_json::from_str(
            r#"{"total_count":1,"runners":[{"id":9,"name":"builder-01","status":"online","busy":false,"version":"2.330.0","labels":[{"name":"self-hosted"},{"name":"talos"}]}]}"#,
        )
        .unwrap();
        assert_eq!(page.runners[0].labels(), ["self-hosted", "talos"]);
        assert_eq!(page.runners[0].version.as_deref(), Some("2.330.0"));
        assert_eq!(page.runners[0].busy, Some(false));
    }

    #[test]
    fn missing_busy_state_stays_unknown() {
        let page: RunnerPage = serde_json::from_str(
            r#"{"total_count":1,"runners":[{"id":9,"name":"builder-01","status":"online","labels":[]}]}"#,
        )
        .unwrap();
        assert_eq!(page.runners[0].busy, None);
    }

    #[test]
    fn registration_token_debug_output_is_redacted() {
        let token = RegistrationToken {
            value: "super-secret".into(),
            expires_at: "later".into(),
        };
        assert!(!format!("{token:?}").contains("super-secret"));
    }

    #[tokio::test]
    async fn list_runners_uses_authenticated_pagination_and_fixture_data() {
        let mut server = mockito::Server::new_async().await;
        let first = server
            .mock("GET", "/repos/acme/ci/actions/runners?per_page=100&page=1")
            .match_header("authorization", "Bearer test-secret-token")
            .with_status(200)
            .with_header("content-type", "application/json")
            .with_body(r#"{"total_count":2,"runners":[{"id":1,"name":"runner-a","status":"online","busy":false,"labels":[{"name":"self-hosted"}]}]}"#)
            .create_async()
            .await;
        let second = server
            .mock("GET", "/repos/acme/ci/actions/runners?per_page=100&page=2")
            .match_header("authorization", "Bearer test-secret-token")
            .with_status(200)
            .with_header("content-type", "application/json")
            .with_body(r#"{"total_count":2,"runners":[{"id":2,"name":"runner-b","status":"offline","busy":false,"labels":[]}] }"#)
            .create_async()
            .await;
        let client = mock_client(server.url());
        let runners = client
            .list_runners(&"acme/ci".parse().unwrap())
            .await
            .unwrap();
        assert_eq!(
            runners
                .iter()
                .map(|runner| runner.name.as_str())
                .collect::<Vec<_>>(),
            ["runner-a", "runner-b"]
        );
        first.assert_async().await;
        second.assert_async().await;
    }

    #[tokio::test]
    async fn delete_runner_uses_repository_scoped_api_endpoint() {
        let mut server = mockito::Server::new_async().await;
        let delete = server
            .mock("DELETE", "/repos/acme/ci/actions/runners/42")
            .match_header("authorization", "Bearer test-secret-token")
            .with_status(204)
            .create_async()
            .await;
        let client = mock_client(server.url());
        client
            .delete_runner(&"acme/ci".parse().unwrap(), 42)
            .await
            .unwrap();
        delete.assert_async().await;
    }

    #[tokio::test]
    async fn registration_request_keeps_token_secret_and_redacts_api_errors() {
        let mut server = mockito::Server::new_async().await;
        let registration = server
            .mock("POST", "/repos/acme/ci/actions/runners/registration-token")
            .match_header("authorization", "Bearer test-secret-token")
            .with_status(201)
            .with_header("content-type", "application/json")
            .with_body(r#"{"token":"registration-secret","expires_at":"2026-09-27T10:00:00Z"}"#)
            .create_async()
            .await;
        let client = mock_client(server.url());
        let token = client
            .registration_token(&"acme/ci".parse().unwrap())
            .await
            .unwrap();
        assert_eq!(token.expose(), "registration-secret");
        assert!(!format!("{token:?}").contains("registration-secret"));
        registration.assert_async().await;

        let error_mock = server
            .mock("GET", "/repos/acme/ci/actions/runners?per_page=100&page=1")
            .with_status(403)
            .with_header("content-type", "application/json")
            .with_body(r#"{"message":"test-secret-token"}"#)
            .create_async()
            .await;
        let error = client
            .list_runners(&"acme/ci".parse().unwrap())
            .await
            .unwrap_err();
        assert!(!format!("{error:#}").contains("test-secret-token"));
        error_mock.assert_async().await;
    }
}
