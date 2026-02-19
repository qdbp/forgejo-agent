use std::fmt::Write as _;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result, anyhow};
use chrono::Utc;
use serde_json::json;

use forgejo_agent::api::ForgejoClient;
use forgejo_agent::config::AgentConfig;
use forgejo_agent::types::RepoRef;

use super::db::{
    self, DeployEnqueueOutcome, DeployJob, DeployJobFailureUpdate, DeployJobSuccessUpdate,
};
use super::repo;
use super::state::{AppState, EventRecord, WebhookPayload};
use super::telemetry::log_line;
use super::template;

const DEPLOY_REPO_OWNER: &str = "main";
const DEPLOY_REPO_NAME: &str = "forgejo-agent";
const DEPLOY_SERVICE_FILE: &str = "/home/main/.config/systemd/user/orchd.service";
const DEPLOY_MANAGED_REPO_ENV: &str = "ORCHD_DEPLOY_MANAGED_REPO";

#[derive(Debug)]
struct DeploySuccess {
    checkout_path: PathBuf,
    log_path: PathBuf,
}

#[derive(Debug)]
struct DeployFailure {
    reason_code: String,
    error_text: String,
    checkout_path: Option<PathBuf>,
    log_path: Option<PathBuf>,
    rollback_status: String,
}

fn managed_repo_from_override(raw: Option<&str>) -> RepoRef {
    if let Some(candidate) = raw.map(str::trim).filter(|value| !value.is_empty())
        && let Ok(parsed) = RepoRef::parse(candidate)
    {
        return parsed;
    }
    RepoRef::new(DEPLOY_REPO_OWNER, DEPLOY_REPO_NAME)
}

fn managed_repo() -> RepoRef {
    let override_value = std::env::var(DEPLOY_MANAGED_REPO_ENV).ok();
    managed_repo_from_override(override_value.as_deref())
}

fn is_managed_repo(repo: &RepoRef) -> bool {
    let managed = managed_repo();
    repo.owner == managed.owner && repo.repo == managed.repo
}

fn is_null_sha(value: &str) -> bool {
    let value = value.trim();
    value.len() == 40 && value.bytes().all(|byte| byte == b'0')
}

fn short_sha(value: &str) -> String {
    let trimmed = value.trim();
    if trimmed.len() <= 12 {
        return trimmed.to_string();
    }
    trimmed[..12].to_string()
}

fn deploy_checkout_root(db_path: &Path) -> Result<PathBuf> {
    let root = db_path
        .parent()
        .map(PathBuf::from)
        .ok_or_else(|| anyhow!("db path has no parent"))?
        .join("deploy-checkouts");
    fs::create_dir_all(&root)
        .with_context(|| format!("failed to create deploy checkout root {}", root.display()))?;
    Ok(root)
}

fn deploy_log_path(db_path: &Path, deploy_job_id: i64) -> Result<PathBuf> {
    let root = db_path
        .parent()
        .map(PathBuf::from)
        .ok_or_else(|| anyhow!("db path has no parent"))?
        .join("deploy-runs");
    fs::create_dir_all(&root)
        .with_context(|| format!("failed creating deploy run dir {}", root.display()))?;
    Ok(root.join(format!("deploy-{deploy_job_id}.log")))
}

fn deploy_checkout_path(db_path: &Path, repo: &RepoRef) -> Result<PathBuf> {
    Ok(deploy_checkout_root(db_path)?
        .join(&repo.owner)
        .join(&repo.repo))
}

fn machine_login(api: &ForgejoClient, cfg: &AgentConfig) -> Result<String> {
    let who = api.whoami(cfg).context("whoami failed")?;
    let login = who
        .get("login")
        .and_then(serde_json::Value::as_str)
        .ok_or_else(|| anyhow!("whoami response missing login field"))?;
    Ok(login.to_ascii_lowercase())
}

fn ensure_checkout_at_sha(
    db_path: &Path,
    cfg: &AgentConfig,
    repo: &RepoRef,
    branch: &str,
    target_sha: &str,
) -> Result<PathBuf> {
    let api = ForgejoClient::new(cfg)?;
    let login = machine_login(&api, cfg)?;
    let repo_full_name = repo.to_string();
    let git_url = repo::forgejo_http_git_url(&cfg.base_url, &login, &repo_full_name)?;
    let checkout = deploy_checkout_path(db_path, repo)?;
    let git_dir = checkout.join(".git");

    if git_dir.is_dir() {
        let _ = repo::git_checked(
            &checkout,
            &[
                "remote",
                "set-url",
                repo::DEFAULT_GIT_REMOTE,
                git_url.as_str(),
            ],
        );
    } else if checkout.exists() {
        return Err(anyhow!(
            "deploy checkout path exists but is not a git repo: {}",
            checkout.display()
        ));
    } else {
        if let Some(parent) = checkout.parent() {
            fs::create_dir_all(parent).with_context(|| {
                format!(
                    "failed creating deploy checkout parent directory {}",
                    parent.display()
                )
            })?;
        }
        repo::git_checked_with_token(
            db_path,
            &cfg.token_file,
            None,
            &[
                "clone",
                "--origin",
                repo::DEFAULT_GIT_REMOTE,
                git_url.as_str(),
                &checkout.to_string_lossy(),
            ],
        )
        .map_err(|err| anyhow!("deploy clone failed: {err}"))?;
    }

    repo::git_checked_with_token(
        db_path,
        &cfg.token_file,
        Some(&checkout),
        &["fetch", repo::DEFAULT_GIT_REMOTE, branch],
    )
    .map_err(|err| anyhow!("deploy fetch failed: {err}"))?;

    let commit_obj = format!("{target_sha}^{{commit}}");
    if repo::git_checked(&checkout, &["cat-file", "-e", commit_obj.as_str()]).is_err() {
        let _ = repo::git_checked_with_token(
            db_path,
            &cfg.token_file,
            Some(&checkout),
            &["fetch", repo::DEFAULT_GIT_REMOTE, target_sha],
        );
    }
    repo::git_checked(&checkout, &["checkout", "-f", "--detach", target_sha])
        .context("deploy checkout failed")?;
    Ok(checkout)
}

fn run_deploy_script(checkout: &Path, log_path: &Path) -> Result<()> {
    let script_path = checkout.join("scripts/deploy-local.sh");
    if !script_path.is_file() {
        return Err(anyhow!(
            "deploy script missing in checkout: {}",
            script_path.display()
        ));
    }
    let output = Command::new("bash")
        .arg(script_path.as_os_str())
        .env("ORCHD_SERVICE_FILE", DEPLOY_SERVICE_FILE)
        .current_dir(checkout)
        .output()
        .with_context(|| format!("failed spawning deploy script from {}", checkout.display()))?;

    let mut log = String::new();
    let _ = writeln!(log, "checkout={}", checkout.display());
    let _ = writeln!(log, "script={}", script_path.display());
    let _ = writeln!(log, "status={:?}", output.status.code());
    log.push_str("--- stdout ---\n");
    log.push_str(String::from_utf8_lossy(&output.stdout).as_ref());
    log.push_str("\n--- stderr ---\n");
    log.push_str(String::from_utf8_lossy(&output.stderr).as_ref());
    log.push('\n');
    fs::write(log_path, log)
        .with_context(|| format!("failed writing deploy log {}", log_path.display()))?;

    if output.status.success() {
        Ok(())
    } else {
        Err(anyhow!(
            "deploy script exited with status {:?}",
            output.status.code()
        ))
    }
}

fn detect_rollback_status() -> String {
    let status = Command::new("systemctl")
        .arg("--user")
        .arg("--quiet")
        .arg("is-active")
        .arg("orchd.service")
        .status();
    match status {
        Ok(status) if status.success() => "service_active".to_string(),
        Ok(_) => "service_inactive".to_string(),
        Err(_) => "service_status_unknown".to_string(),
    }
}

fn issue_template_path() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("templates")
        .join("orchd-deploy-failure.md")
}

fn render_deploy_failure_issue(
    job: &DeployJob,
    checkout_path: Option<&Path>,
    log_path: Option<&Path>,
    rollback_status: &str,
    error_text: &str,
) -> String {
    let repo_full_name = job.repo_full_name.as_str();
    let source_delivery = job.source_delivery_id.as_deref().unwrap_or("<none>");
    let source_actor = job.source_actor_login.as_deref().unwrap_or("<none>");
    let checkout_path = checkout_path
        .map(|path| path.to_string_lossy().into_owned())
        .unwrap_or_else(|| "<none>".to_string());
    let log_path = log_path
        .map(|path| path.to_string_lossy().into_owned())
        .unwrap_or_else(|| "<none>".to_string());

    template::render_prompt_file(
        issue_template_path().as_path(),
        &[
            ("repo_full_name", repo_full_name),
            ("target_branch", job.target_branch.as_str()),
            ("target_sha", job.target_sha.as_str()),
            ("source_delivery_id", source_delivery),
            ("source_actor_login", source_actor),
            ("checkout_path", checkout_path.as_str()),
            ("log_path", log_path.as_str()),
            ("rollback_status", rollback_status),
            ("error_text", error_text),
        ],
        "deploy failure issue",
    )
    .unwrap_or_else(|render_err| {
        format!(
            "@codex-orch impl\n\nDeploy failed for `{repo_full_name}` at `{}`.\n\nInstruction: if the fix is clear, implement it immediately.\n\nerror: {error_text}\nrollback: {rollback_status}\ncheckout: {checkout_path}\nlog: {log_path}\n(template error: {render_err})\n",
            job.target_sha
        )
    })
}

fn open_deploy_failure_issue(
    cfg: &AgentConfig,
    job: &DeployJob,
    checkout_path: Option<&Path>,
    log_path: Option<&Path>,
    rollback_status: &str,
    error_text: &str,
) -> Result<Option<u64>> {
    let repo = RepoRef::parse(job.repo_full_name.as_str())
        .context("invalid repo_full_name in deploy job")?;
    let api = ForgejoClient::new(cfg)?;
    let title = format!(
        "deploy failure: {} {}@{}",
        repo,
        job.target_branch,
        short_sha(&job.target_sha)
    );
    let body =
        render_deploy_failure_issue(job, checkout_path, log_path, rollback_status, error_text);
    let created = api.create_issue(cfg, &repo, title.as_str(), body.as_str())?;
    Ok(Some(created.number))
}

fn deploy_branch_for_repo(state: &AppState, repo_full_name: &str) -> String {
    state
        .dispatch_config
        .snapshot()
        .and_then(|cfg| {
            cfg.repo_bindings
                .get(repo_full_name)
                .map(|binding| binding.git_base.clone())
        })
        .filter(|branch| !branch.trim().is_empty())
        .unwrap_or_else(|| repo::DEFAULT_GIT_BASE_BRANCH.to_string())
}

pub(super) fn extract_push_target(
    payload: &WebhookPayload,
    expected_branch: &str,
) -> Option<(String, String)> {
    if payload.deleted.unwrap_or(false) {
        return None;
    }
    let push_ref = payload.push_ref.as_deref()?.trim();
    let branch = push_ref.strip_prefix("refs/heads/")?.trim();
    if branch != expected_branch {
        return None;
    }
    let target_sha = payload.after.as_deref()?.trim();
    if target_sha.is_empty() || is_null_sha(target_sha) {
        return None;
    }
    Some((branch.to_string(), target_sha.to_string()))
}

pub(super) fn enqueue_push_event(
    state: &AppState,
    record: &EventRecord,
    payload: &WebhookPayload,
    event_id: i64,
) -> Result<()> {
    let Ok(repo_ref) = RepoRef::parse(record.repo_full_name.as_str()) else {
        return Ok(());
    };
    if !is_managed_repo(&repo_ref) {
        return Ok(());
    }
    let expected_branch = deploy_branch_for_repo(state, record.repo_full_name.as_str());
    let Some((branch, target_sha)) = extract_push_target(payload, expected_branch.as_str()) else {
        return Ok(());
    };
    let enqueue = db::enqueue_deploy_job(
        &state.db_path,
        record.repo_full_name.as_str(),
        branch.as_str(),
        target_sha.as_str(),
        Some(event_id),
        Some(record.delivery_id.as_str()),
        record.actor_login.as_deref(),
    )?;
    log_line(
        "deploy_enqueue_push",
        json!({
            "repo": record.repo_full_name,
            "branch": branch,
            "target_sha": target_sha,
            "delivery_id": record.delivery_id,
            "source": "push",
            "result": match enqueue {
                DeployEnqueueOutcome::Inserted => "inserted",
                DeployEnqueueOutcome::Existing => "existing",
            },
        }),
    );
    Ok(())
}

fn read_remote_branch_head(
    db_path: &Path,
    cfg: &AgentConfig,
    repo: &RepoRef,
    branch: &str,
) -> Result<Option<String>> {
    let api = ForgejoClient::new(cfg)?;
    let login = machine_login(&api, cfg)?;
    let git_url = repo::forgejo_http_git_url(&cfg.base_url, &login, repo.to_string().as_str())?;
    let output = repo::git_checked_with_token(
        db_path,
        &cfg.token_file,
        None,
        &[
            "ls-remote",
            git_url.as_str(),
            &format!("refs/heads/{branch}"),
        ],
    )
    .map_err(|err| anyhow!("ls-remote failed: {err}"))?;
    let stdout = String::from_utf8_lossy(&output.stdout);
    let head = stdout
        .lines()
        .find_map(|line| line.split_whitespace().next())
        .map(ToOwned::to_owned);
    Ok(head)
}

pub(super) async fn reconcile_head_enqueue(state: &AppState) -> Result<()> {
    let db_path = state.db_path.clone();
    let cfg = state.cfg.clone();
    let repo = managed_repo();
    let repo_full_name = repo.to_string();
    let branch = deploy_branch_for_repo(state, repo_full_name.as_str());
    tokio::task::spawn_blocking(move || -> Result<()> {
        let Some(remote_head) = read_remote_branch_head(&db_path, &cfg, &repo, branch.as_str())?
        else {
            return Ok(());
        };
        let latest_deployed =
            db::latest_deployed_sha(&db_path, repo_full_name.as_str(), branch.as_str())?;
        if latest_deployed.as_deref() == Some(remote_head.as_str()) {
            return Ok(());
        }
        if db::has_active_deploy_for_target(
            &db_path,
            repo_full_name.as_str(),
            branch.as_str(),
            remote_head.as_str(),
        )? {
            return Ok(());
        }
        let delivery_id = format!("reconcile:{}:{}", repo_full_name, Utc::now().timestamp());
        let enqueue = db::enqueue_deploy_job(
            &db_path,
            repo_full_name.as_str(),
            branch.as_str(),
            remote_head.as_str(),
            None,
            Some(delivery_id.as_str()),
            Some("orchd"),
        )?;
        log_line(
            "deploy_enqueue_reconcile",
            json!({
                "repo": repo_full_name,
                "branch": branch,
                "target_sha": remote_head,
                "source": "reconcile",
                "result": match enqueue {
                    DeployEnqueueOutcome::Inserted => "inserted",
                    DeployEnqueueOutcome::Existing => "existing",
                },
            }),
        );
        Ok(())
    })
    .await
    .context("deploy reconcile join failure")?
}

fn execute_deploy_job(
    state: &AppState,
    job: &DeployJob,
) -> std::result::Result<DeploySuccess, DeployFailure> {
    let repo = match RepoRef::parse(job.repo_full_name.as_str()) {
        Ok(repo) => repo,
        Err(err) => {
            return Err(DeployFailure {
                reason_code: "deploy_invalid_repo".to_string(),
                error_text: err.to_string(),
                checkout_path: None,
                log_path: None,
                rollback_status: "not_attempted".to_string(),
            });
        }
    };
    if !is_managed_repo(&repo) {
        return Err(DeployFailure {
            reason_code: "deploy_repo_not_managed".to_string(),
            error_text: format!("repo {} is not deploy-managed", job.repo_full_name),
            checkout_path: None,
            log_path: None,
            rollback_status: "not_attempted".to_string(),
        });
    }

    let checkout_path = match ensure_checkout_at_sha(
        &state.db_path,
        &state.cfg,
        &repo,
        job.target_branch.as_str(),
        job.target_sha.as_str(),
    ) {
        Ok(path) => path,
        Err(err) => {
            return Err(DeployFailure {
                reason_code: "deploy_checkout_failed".to_string(),
                error_text: format!("{err:#}"),
                checkout_path: None,
                log_path: None,
                rollback_status: "not_attempted".to_string(),
            });
        }
    };

    let log_path = match deploy_log_path(&state.db_path, job.id) {
        Ok(path) => path,
        Err(err) => {
            return Err(DeployFailure {
                reason_code: "deploy_log_path_failed".to_string(),
                error_text: format!("{err:#}"),
                checkout_path: Some(checkout_path),
                log_path: None,
                rollback_status: "not_attempted".to_string(),
            });
        }
    };

    match run_deploy_script(&checkout_path, &log_path) {
        Ok(()) => Ok(DeploySuccess {
            checkout_path,
            log_path,
        }),
        Err(err) => Err(DeployFailure {
            reason_code: "deploy_script_failed".to_string(),
            error_text: format!("{err:#}"),
            checkout_path: Some(checkout_path),
            log_path: Some(log_path),
            rollback_status: detect_rollback_status(),
        }),
    }
}

pub(super) async fn run_worker_once(state: &AppState) -> Result<()> {
    let state = state.clone();
    tokio::task::spawn_blocking(move || -> Result<()> {
        let Some(job) = db::claim_next_deploy_job(&state.db_path)? else {
            return Ok(());
        };

        match execute_deploy_job(&state, &job) {
            Ok(success) => {
                db::complete_deploy_job_success(
                    &state.db_path,
                    job.id,
                    &DeployJobSuccessUpdate {
                        checkout_path: Some(success.checkout_path.as_path()),
                        log_path: Some(success.log_path.as_path()),
                    },
                )?;
                log_line(
                    "deploy_job_succeeded",
                    json!({
                        "deploy_job_id": job.id,
                        "job_status": job.status.as_db_str(),
                        "repo": job.repo_full_name,
                        "branch": job.target_branch,
                        "target_sha": job.target_sha,
                        "source_event_id": job.source_event_id,
                        "source_delivery_id": job.source_delivery_id,
                        "source_actor_login": job.source_actor_login,
                        "attempt_count": job.attempt_count,
                        "log_path": success.log_path.to_string_lossy(),
                    }),
                );
            }
            Err(failure) => {
                let incident_issue_number = open_deploy_failure_issue(
                    &state.cfg,
                    &job,
                    failure.checkout_path.as_deref(),
                    failure.log_path.as_deref(),
                    failure.rollback_status.as_str(),
                    failure.error_text.as_str(),
                )
                .unwrap_or(None);
                db::complete_deploy_job_failure(
                    &state.db_path,
                    job.id,
                    &DeployJobFailureUpdate {
                        reason_code: failure.reason_code.as_str(),
                        error_text: failure.error_text.as_str(),
                        checkout_path: failure.checkout_path.as_deref(),
                        log_path: failure.log_path.as_deref(),
                        incident_issue_number,
                        rollback_status: Some(failure.rollback_status.as_str()),
                    },
                )?;
                log_line(
                    "deploy_job_failed",
                    json!({
                        "deploy_job_id": job.id,
                        "job_status": job.status.as_db_str(),
                        "repo": job.repo_full_name,
                        "branch": job.target_branch,
                        "target_sha": job.target_sha,
                        "source_event_id": job.source_event_id,
                        "source_delivery_id": job.source_delivery_id,
                        "source_actor_login": job.source_actor_login,
                        "attempt_count": job.attempt_count,
                        "reason_code": failure.reason_code,
                        "rollback_status": failure.rollback_status,
                        "incident_issue_number": incident_issue_number,
                        "log_path": failure.log_path.map(|path| path.to_string_lossy().into_owned()),
                        "error": failure.error_text,
                    }),
                );
            }
        }
        Ok(())
    })
    .await
    .context("deploy worker join failure")?
}

#[cfg(test)]
mod tests {
    use super::{extract_push_target, managed_repo_from_override};
    use crate::orchd::state::WebhookPayload;

    #[test]
    fn extract_push_target_accepts_branch_tip_updates() {
        let payload = WebhookPayload {
            action: None,
            repository: None,
            issue: None,
            comment: None,
            sender: None,
            push_ref: Some("refs/heads/main".to_string()),
            after: Some("1234567890abcdef1234567890abcdef12345678".to_string()),
            deleted: Some(false),
        };
        let target = extract_push_target(&payload, "main");
        assert_eq!(
            target,
            Some((
                "main".to_string(),
                "1234567890abcdef1234567890abcdef12345678".to_string()
            ))
        );
    }

    #[test]
    fn extract_push_target_rejects_null_sha_or_wrong_branch() {
        let payload = WebhookPayload {
            action: None,
            repository: None,
            issue: None,
            comment: None,
            sender: None,
            push_ref: Some("refs/heads/dev".to_string()),
            after: Some("0000000000000000000000000000000000000000".to_string()),
            deleted: Some(false),
        };
        assert_eq!(extract_push_target(&payload, "main"), None);
    }

    #[test]
    fn managed_repo_override_parses_and_falls_back() {
        let parsed = managed_repo_from_override(Some("itest-owner/itest-repo"));
        assert_eq!(parsed.owner, "itest-owner");
        assert_eq!(parsed.repo, "itest-repo");

        let fallback = managed_repo_from_override(Some("not-a-repo-ref"));
        assert_eq!(fallback.owner, "main");
        assert_eq!(fallback.repo, "forgejo-agent");
    }
}
