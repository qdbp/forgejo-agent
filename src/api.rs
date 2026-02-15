use anyhow::{Context, Result, bail};
use reqwest::Method;
use reqwest::blocking::{Client, RequestBuilder};
use serde::Serialize;
use serde::de::DeserializeOwned;
use serde_json::Value;

use crate::config::AgentConfig;
use crate::types::{ApiIssue, ApiLabel, IssueRef, OpenState, RepoRef};

#[derive(Debug, Clone)]
pub struct ForgejoClient {
    base_url: String,
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
            .build()
            .context("failed to create HTTP client")?;
        Ok(Self {
            base_url: cfg.base_url.as_str().trim_end_matches('/').to_string(),
            http,
        })
    }

    fn endpoint(&self, path: &str) -> String {
        format!("{}{}", self.base_url, path)
    }

    fn request(&self, cfg: &AgentConfig, method: &Method, path: &str) -> RequestBuilder {
        self.http
            .request(method.clone(), self.endpoint(path))
            .header("Accept", "application/json")
            .header("Authorization", format!("token {}", cfg.token))
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
        let req = self.request(cfg, method, path);
        let req = if let Some(body) = body {
            req.header("Content-Type", "application/json").json(body)
        } else {
            req
        };

        let resp = req
            .send()
            .with_context(|| format!("request failed: {method} {path}"))?;
        let status = resp.status();
        let text = resp
            .text()
            .with_context(|| format!("failed reading response body for {method} {path}"))?;

        if !status.is_success() {
            return Err(ApiHttpError {
                status: status.as_u16(),
                method: method.to_string(),
                path: path.to_string(),
                body: text,
            }
            .into());
        }

        serde_json::from_str(&text).with_context(|| {
            format!(
                "failed parsing JSON response for {} {}: {}",
                method,
                path,
                text.chars().take(200).collect::<String>()
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
        let req = self.request(cfg, method, path);
        let req = if let Some(body) = body {
            req.header("Content-Type", "application/json").json(body)
        } else {
            req
        };
        let resp = req
            .send()
            .with_context(|| format!("request failed: {method} {path}"))?;
        let status = resp.status();
        let text = resp
            .text()
            .with_context(|| format!("failed reading response body for {method} {path}"))?;
        if !status.is_success() {
            return Err(ApiHttpError {
                status: status.as_u16(),
                method: method.to_string(),
                path: path.to_string(),
                body: text,
            }
            .into());
        }
        Ok(())
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
    ) -> Result<Value> {
        let path = format!("/api/v1/repos/{}/{}/pulls", repo.owner, repo.repo);
        let payload = CreatePullRequestBody {
            title,
            head,
            base,
            body: Some(body),
        };
        self.send_json(cfg, &Method::POST, &path, Some(&payload))
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

        let _: serde_json::Value =
            self.send_json(cfg, &Method::POST, "/api/v1/user/repos", Some(&body))?;
        Ok(())
    }

    pub fn list_labels(&self, cfg: &AgentConfig, repo: &RepoRef) -> Result<Vec<ApiLabel>> {
        let path = format!(
            "/api/v1/repos/{}/{}/labels?limit=1000",
            repo.owner, repo.repo
        );
        self.send_json(cfg, &Method::GET, &path, Option::<&()>::None)
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
