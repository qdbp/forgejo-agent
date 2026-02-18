use anyhow::{Context, Result, bail};
use reqwest::Method;
use reqwest::blocking::{Client, RequestBuilder};
use reqwest::header::LOCATION;
use reqwest::redirect::Policy;
use serde::Serialize;
use serde::de::DeserializeOwned;
use serde_json::Value;
use url::Url;

use crate::config::AgentConfig;
use crate::types::{ApiIssue, ApiLabel, ApiPullRequest, IssueRef, OpenState, RepoRef};

#[derive(Debug, Clone)]
pub struct ForgejoClient {
    base_url: Url,
    http: Client,
}

#[derive(Debug, thiserror::Error)]
#[error("Forgejo API error {status} {method} {path}: {body}")]
pub struct ApiHttpError {
    pub status: u16,
    pub method: String,
    pub path: String,
    pub body: String,
}

#[derive(Debug)]
struct ApiSuccess {
    path: String,
    body: String,
}

#[derive(Debug, Clone, Serialize)]
struct CreateRepoBody<'a> {
    name: &'a str,
    description: &'a str,
    private: bool,
    auto_init: bool,
}

#[derive(Debug, Clone, Serialize)]
pub struct CreateLabelBody<'a> {
    pub name: &'a str,
    pub color: &'a str,
    pub description: &'a str,
    pub exclusive: bool,
}

#[derive(Debug, Clone, Serialize)]
struct CreateIssueBody<'a> {
    title: &'a str,
    body: &'a str,
}

#[derive(Debug, Clone, Serialize)]
struct AddLabelIdsBody {
    labels: Vec<u64>,
}

#[derive(Debug, Clone, Serialize)]
struct PatchIssueStateBody {
    state: OpenState,
}

#[derive(Debug, Clone, Serialize)]
struct PatchIssueBody<'a> {
    #[serde(skip_serializing_if = "Option::is_none")]
    title: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    body: Option<&'a str>,
}

#[derive(Debug, Clone, Serialize)]
struct PatchIssueAssigneesBody {
    assignees: Vec<String>,
}

#[derive(Debug, Clone, Serialize)]
struct CommentBody<'a> {
    body: &'a str,
}

#[derive(Debug, Clone, Serialize)]
struct CreatePullRequestBody<'a> {
    title: &'a str,
    head: &'a str,
    base: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    body: Option<&'a str>,
}

#[derive(Debug, Clone, Copy, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum MergePullMethod {
    Merge,
    Rebase,
    RebaseMerge,
    Squash,
    FastForwardOnly,
    ManuallyMerged,
}

#[derive(Debug, Clone, Serialize)]
struct MergePullRequestBody<'a> {
    #[serde(rename = "Do")]
    method: MergePullMethod,
    #[serde(skip_serializing_if = "Option::is_none")]
    head_commit_id: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    delete_branch_after_merge: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    force_merge: Option<bool>,
}

#[derive(Debug, Clone, Serialize)]
struct HookConfig<'a> {
    url: &'a str,
    content_type: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    secret: Option<&'a str>,
}

#[derive(Debug, Clone, Serialize)]
struct CreateHookBody<'a> {
    #[serde(rename = "type")]
    kind: &'a str,
    config: HookConfig<'a>,
    events: Vec<&'a str>,
    active: bool,
}

impl ForgejoClient {
    pub fn new(cfg: &AgentConfig) -> Result<Self> {
        let http = Client::builder()
            .user_agent("forgejo-agent/0.1")
            .redirect(Policy::none())
            .build()
            .context("failed to create HTTP client")?;
        Ok(Self {
            base_url: cfg.base_url.clone(),
            http,
        })
    }

    fn endpoint(&self, path: &str) -> Result<Url> {
        self.base_url
            .join(path)
            .with_context(|| format!("failed building endpoint URL for {path}"))
    }

    fn request(&self, cfg: &AgentConfig, method: &Method, url: Url) -> RequestBuilder {
        self.http
            .request(method.clone(), url)
            .header("Accept", "application/json")
            .header("Authorization", format!("token {}", cfg.token))
    }

    fn same_origin(a: &Url, b: &Url) -> bool {
        a.scheme() == b.scheme()
            && a.host_str() == b.host_str()
            && a.port_or_known_default() == b.port_or_known_default()
    }

    fn api_path_with_query(url: &Url) -> String {
        let mut path = url.path().to_string();
        if let Some(query) = url.query() {
            path.push('?');
            path.push_str(query);
        }
        path
    }

    fn repo_api_route(url: &Url) -> Option<String> {
        let rest = url.path().strip_prefix("/api/v1/repos/")?;
        let mut segments = rest.split('/').filter(|segment| !segment.is_empty());
        let _owner = segments.next()?;
        let _repo = segments.next()?;
        let mut route = segments.collect::<Vec<_>>().join("/");
        route = route.trim_end_matches('/').to_string();
        if let Some(query) = url.query()
            && !query.is_empty()
        {
            route.push('?');
            route.push_str(query);
        }
        Some(route)
    }

    fn is_repo_canonicalization_redirect(from: &Url, to: &Url) -> bool {
        let Some(from_route) = Self::repo_api_route(from) else {
            return false;
        };
        let Some(to_route) = Self::repo_api_route(to) else {
            return false;
        };
        from_route == to_route
    }

    fn send_json<T, B>(
        &self,
        cfg: &AgentConfig,
        method: &Method,
        path: &str,
        body: Option<&B>,
    ) -> Result<T>
    where
        T: DeserializeOwned,
        B: Serialize + ?Sized,
    {
        let ApiSuccess { path, body } = self.send_success(cfg, method, path, body)?;
        serde_json::from_str(&body).with_context(|| {
            format!(
                "failed parsing JSON response for {} {}: {}",
                method,
                path,
                body.chars().take(200).collect::<String>()
            )
        })
    }

    fn send_empty<B>(
        &self,
        cfg: &AgentConfig,
        method: &Method,
        path: &str,
        body: Option<&B>,
    ) -> Result<()>
    where
        B: Serialize + ?Sized,
    {
        self.send_success(cfg, method, path, body).map(|_| ())
    }

    fn send_success<B>(
        &self,
        cfg: &AgentConfig,
        method: &Method,
        path: &str,
        body: Option<&B>,
    ) -> Result<ApiSuccess>
    where
        B: Serialize + ?Sized,
    {
        const MAX_REDIRECTS: u8 = 10;

        let mut url = self.endpoint(path)?;

        for _ in 0..MAX_REDIRECTS {
            let req = self.request(cfg, method, url.clone());
            let req = if let Some(body) = body {
                req.header("Content-Type", "application/json").json(body)
            } else {
                req
            };
            let resp = req
                .send()
                .with_context(|| format!("request failed: {method} {}", url.as_str()))?;
            let status = resp.status();
            if status.is_redirection() {
                let location = resp
                    .headers()
                    .get(LOCATION)
                    .and_then(|value| value.to_str().ok())
                    .map(str::to_owned)
                    .context("redirect response missing Location header")?;
                let next = url
                    .join(&location)
                    .with_context(|| format!("invalid redirect Location header: {location}"))?;
                if !Self::same_origin(&self.base_url, &next) {
                    bail!("refusing cross-origin redirect to {}", next.as_str());
                }
                if !next.path().starts_with("/api/v1/") {
                    bail!("refusing non-API redirect to {}", next.as_str());
                }
                if method != Method::GET && !Self::is_repo_canonicalization_redirect(&url, &next) {
                    bail!(
                        "unexpected redirect for {} {} -> {}",
                        method,
                        Self::api_path_with_query(&url),
                        Self::api_path_with_query(&next)
                    );
                }
                url = next;
                continue;
            }

            let body = resp.text().with_context(|| {
                format!("failed reading response body for {method} {}", url.as_str())
            })?;
            if !status.is_success() {
                return Err(ApiHttpError {
                    status: status.as_u16(),
                    method: method.to_string(),
                    path: Self::api_path_with_query(&url),
                    body,
                }
                .into());
            }
            return Ok(ApiSuccess {
                path: Self::api_path_with_query(&url),
                body,
            });
        }

        bail!("too many redirects for {method} {}", url.as_str());
    }

    pub fn whoami(&self, cfg: &AgentConfig) -> Result<serde_json::Value> {
        self.send_json(cfg, &Method::GET, "/api/v1/user", Option::<&()>::None)
    }

    pub fn list_admin_hooks(&self, cfg: &AgentConfig) -> Result<Vec<Value>> {
        self.send_json(
            cfg,
            &Method::GET,
            "/api/v1/admin/hooks",
            Option::<&()>::None,
        )
    }

    pub fn create_admin_hook(
        &self,
        cfg: &AgentConfig,
        url: &str,
        secret: Option<&str>,
        events: &[&str],
    ) -> Result<Value> {
        let payload = CreateHookBody {
            kind: "gitea",
            config: HookConfig {
                url,
                content_type: "json",
                secret,
            },
            events: events.to_vec(),
            active: true,
        };
        self.send_json(cfg, &Method::POST, "/api/v1/admin/hooks", Some(&payload))
    }

    pub fn list_user_repos(&self, cfg: &AgentConfig, user: &str, limit: u32) -> Result<Vec<Value>> {
        let path = format!("/api/v1/users/{user}/repos?limit={limit}");
        self.send_json(cfg, &Method::GET, &path, Option::<&()>::None)
    }

    pub fn list_repo_hooks(&self, cfg: &AgentConfig, repo: &RepoRef) -> Result<Vec<Value>> {
        let path = format!(
            "/api/v1/repos/{}/{}/hooks?limit=1000",
            repo.owner, repo.repo
        );
        self.send_json(cfg, &Method::GET, &path, Option::<&()>::None)
    }

    pub fn create_repo_hook(
        &self,
        cfg: &AgentConfig,
        repo: &RepoRef,
        url: &str,
        secret: Option<&str>,
        events: &[&str],
    ) -> Result<Value> {
        let path = format!("/api/v1/repos/{}/{}/hooks", repo.owner, repo.repo);
        let payload = CreateHookBody {
            kind: "forgejo",
            config: HookConfig {
                url,
                content_type: "json",
                secret,
            },
            events: events.to_vec(),
            active: true,
        };
        self.send_json(cfg, &Method::POST, &path, Some(&payload))
    }

    pub fn create_pull_request(
        &self,
        cfg: &AgentConfig,
        repo: &RepoRef,
        title: &str,
        head: &str,
        base: &str,
        body: &str,
    ) -> Result<ApiPullRequest> {
        let path = format!("/api/v1/repos/{}/{}/pulls", repo.owner, repo.repo);
        let payload = CreatePullRequestBody {
            title,
            head,
            base,
            body: Some(body),
        };
        self.send_json(cfg, &Method::POST, &path, Some(&payload))
    }

    pub fn list_pull_requests(
        &self,
        cfg: &AgentConfig,
        repo: &RepoRef,
        state: &str,
        limit: u32,
    ) -> Result<Vec<ApiPullRequest>> {
        let path = format!(
            "/api/v1/repos/{}/{}/pulls?state={state}&limit={limit}",
            repo.owner, repo.repo
        );
        self.send_json(cfg, &Method::GET, &path, Option::<&()>::None)
    }

    pub fn merge_pull_request(
        &self,
        cfg: &AgentConfig,
        repo: &RepoRef,
        pr_number: u64,
        method: MergePullMethod,
        head_commit_id: Option<&str>,
        delete_branch_after_merge: bool,
    ) -> Result<()> {
        let path = format!(
            "/api/v1/repos/{}/{}/pulls/{}/merge",
            repo.owner, repo.repo, pr_number
        );
        let payload = MergePullRequestBody {
            method,
            head_commit_id,
            delete_branch_after_merge: Some(delete_branch_after_merge),
            force_merge: None,
        };
        self.send_empty(cfg, &Method::POST, &path, Some(&payload))
    }

    pub fn get_repo(&self, cfg: &AgentConfig, repo: &RepoRef) -> Result<Option<serde_json::Value>> {
        let path = format!("/api/v1/repos/{}/{}", repo.owner, repo.repo);
        match self.send_json(cfg, &Method::GET, &path, Option::<&()>::None) {
            Ok(value) => Ok(Some(value)),
            Err(err) => {
                if let Some(http) = err.downcast_ref::<ApiHttpError>()
                    && http.status == 404
                {
                    return Ok(None);
                }
                Err(err)
            }
        }
    }

    pub fn ensure_repo(&self, cfg: &AgentConfig, repo: &RepoRef, description: &str) -> Result<()> {
        if self.get_repo(cfg, repo)?.is_some() {
            return Ok(());
        }
        if repo.owner != cfg.default_repo.owner {
            bail!(
                "repo owner {} is not current auth owner {}; repo auto-create currently supports default owner only",
                repo.owner,
                cfg.default_repo.owner
            );
        }
        let body = CreateRepoBody {
            name: &repo.repo,
            description,
            private: true,
            auto_init: true,
        };

        // We intentionally create repos under the configured default owner (usually `main`), not
        // under whatever principal token happens to be executing forgejoctl/orchd. Doing so via
        // the admin endpoint keeps "repo ensure" deterministic for swarm automation.
        let path = format!("/api/v1/admin/users/{}/repos", repo.owner);
        let _: serde_json::Value = self.send_json(cfg, &Method::POST, &path, Some(&body))?;
        Ok(())
    }

    pub fn list_labels(&self, cfg: &AgentConfig, repo: &RepoRef) -> Result<Vec<ApiLabel>> {
        const PAGE_LIMIT: u32 = 1000;
        const MAX_PAGES: u32 = 200;

        let mut labels = Vec::new();
        for page in 1..=MAX_PAGES {
            let path = format!(
                "/api/v1/repos/{}/{}/labels?limit={PAGE_LIMIT}&page={page}",
                repo.owner, repo.repo
            );
            let page_labels: Vec<ApiLabel> =
                self.send_json(cfg, &Method::GET, &path, Option::<&()>::None)?;
            if page_labels.is_empty() {
                return Ok(labels);
            }
            labels.extend(page_labels);
        }
        bail!(
            "label listing for {}/{} exceeded pagination safety cap ({MAX_PAGES} pages)",
            repo.owner,
            repo.repo
        )
    }

    pub fn create_label(
        &self,
        cfg: &AgentConfig,
        repo: &RepoRef,
        body: &CreateLabelBody<'_>,
    ) -> Result<ApiLabel> {
        let path = format!("/api/v1/repos/{}/{}/labels", repo.owner, repo.repo);
        self.send_json(cfg, &Method::POST, &path, Some(body))
    }

    pub fn ensure_label(
        &self,
        cfg: &AgentConfig,
        repo: &RepoRef,
        name: &str,
        color: &str,
        description: &str,
        exclusive: bool,
    ) -> Result<ApiLabel> {
        let labels = self.list_labels(cfg, repo)?;
        if let Some(existing) = labels.into_iter().find(|label| label.name == name) {
            return Ok(existing);
        }
        self.create_label(
            cfg,
            repo,
            &CreateLabelBody {
                name,
                color,
                description,
                exclusive,
            },
        )
    }

    pub fn list_issues(
        &self,
        cfg: &AgentConfig,
        repo: &RepoRef,
        state: &str,
        limit: u32,
    ) -> Result<Vec<ApiIssue>> {
        let path = format!(
            "/api/v1/repos/{}/{}/issues?state={state}&limit={limit}",
            repo.owner, repo.repo
        );
        self.send_json(cfg, &Method::GET, &path, Option::<&()>::None)
    }

    pub fn get_issue(&self, cfg: &AgentConfig, issue: &IssueRef) -> Result<ApiIssue> {
        let path = format!(
            "/api/v1/repos/{}/{}/issues/{}",
            issue.repo.owner, issue.repo.repo, issue.number
        );
        self.send_json(cfg, &Method::GET, &path, Option::<&()>::None)
    }

    pub fn create_issue(
        &self,
        cfg: &AgentConfig,
        repo: &RepoRef,
        title: &str,
        body: &str,
    ) -> Result<ApiIssue> {
        let path = format!("/api/v1/repos/{}/{}/issues", repo.owner, repo.repo);
        let payload = CreateIssueBody { title, body };
        self.send_json(cfg, &Method::POST, &path, Some(&payload))
    }

    pub fn add_issue_label_ids(
        &self,
        cfg: &AgentConfig,
        issue: &IssueRef,
        label_ids: Vec<u64>,
    ) -> Result<Vec<ApiLabel>> {
        if label_ids.is_empty() {
            return Ok(Vec::new());
        }
        let path = format!(
            "/api/v1/repos/{}/{}/issues/{}/labels",
            issue.repo.owner, issue.repo.repo, issue.number
        );
        let payload = AddLabelIdsBody { labels: label_ids };
        self.send_json(cfg, &Method::POST, &path, Some(&payload))
    }

    pub fn replace_issue_label_ids(
        &self,
        cfg: &AgentConfig,
        issue: &IssueRef,
        label_ids: Vec<u64>,
    ) -> Result<Vec<ApiLabel>> {
        let path = format!(
            "/api/v1/repos/{}/{}/issues/{}/labels",
            issue.repo.owner, issue.repo.repo, issue.number
        );
        let payload = AddLabelIdsBody { labels: label_ids };
        self.send_json(cfg, &Method::PUT, &path, Some(&payload))
    }

    pub fn remove_issue_label(
        &self,
        cfg: &AgentConfig,
        issue: &IssueRef,
        label_id: u64,
    ) -> Result<()> {
        let path = format!(
            "/api/v1/repos/{}/{}/issues/{}/labels/{}",
            issue.repo.owner, issue.repo.repo, issue.number, label_id
        );
        self.send_empty::<()>(cfg, &Method::DELETE, &path, None)
    }

    pub fn set_issue_open_state(
        &self,
        cfg: &AgentConfig,
        issue: &IssueRef,
        state: OpenState,
    ) -> Result<ApiIssue> {
        let path = format!(
            "/api/v1/repos/{}/{}/issues/{}",
            issue.repo.owner, issue.repo.repo, issue.number
        );
        let payload = PatchIssueStateBody { state };
        self.send_json(cfg, &Method::PATCH, &path, Some(&payload))
    }

    pub fn update_issue(
        &self,
        cfg: &AgentConfig,
        issue: &IssueRef,
        title: Option<&str>,
        body: Option<&str>,
    ) -> Result<ApiIssue> {
        let path = format!(
            "/api/v1/repos/{}/{}/issues/{}",
            issue.repo.owner, issue.repo.repo, issue.number
        );
        let payload = PatchIssueBody { title, body };
        self.send_json(cfg, &Method::PATCH, &path, Some(&payload))
    }

    pub fn set_issue_assignees(
        &self,
        cfg: &AgentConfig,
        issue: &IssueRef,
        assignees: Vec<String>,
    ) -> Result<ApiIssue> {
        let path = format!(
            "/api/v1/repos/{}/{}/issues/{}",
            issue.repo.owner, issue.repo.repo, issue.number
        );
        let payload = PatchIssueAssigneesBody { assignees };
        self.send_json(cfg, &Method::PATCH, &path, Some(&payload))
    }

    pub fn comment_issue(&self, cfg: &AgentConfig, issue: &IssueRef, body: &str) -> Result<()> {
        let path = format!(
            "/api/v1/repos/{}/{}/issues/{}/comments",
            issue.repo.owner, issue.repo.repo, issue.number
        );
        let payload = CommentBody { body };
        let _: serde_json::Value = self.send_json(cfg, &Method::POST, &path, Some(&payload))?;
        Ok(())
    }
}
