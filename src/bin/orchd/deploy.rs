use std::fmt::Write as _;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::thread;
use std::time::Duration;

use anyhow::{Context, Result, anyhow};
use chrono::Utc;
use rusqlite::{Connection, OptionalExtension};
use serde_json::json;

use forgejo_agent::api::ForgejoClient;
use forgejo_agent::config::AgentConfig;
use forgejo_agent::types::RepoRef;

use super::cli::{Cli, DeployWorkerArgs};
use super::db::{
    self, DeployEnqueueOutcome, DeployJob, DeployJobFailureUpdate, DeployJobSuccessUpdate,
    DeployRunStatus,
};
use super::dispatch_config::load_dispatch_config;
use super::paths::{expand_tilde_path, resolve_dispatch_config_path};
use super::repo;
use super::state::{AppState, EventRecord, WebhookPayload};
use super::telemetry::log_line;
use super::template;

const DEPLOY_REPO_OWNER: &str = "main";
const DEPLOY_REPO_NAME: &str = "forgejo-agent";
const DEPLOY_MANAGED_REPO_ENV: &str = "ORCHD_DEPLOY_MANAGED_REPO";
const DEPLOY_RELEASE_BIN_SUBDIR: &str = "bin";
const DEPLOY_RELEASES_DIRNAME: &str = "deploy-releases";
const DEFAULT_ORCHD_BIN: &str = "~/.local/bin/orchd";
const DEFAULT_FORGEJOCTL_BIN: &str = "~/.local/bin/forgejoctl";

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

#[derive(Debug, Clone)]
struct DeployWorkerContext {
    db_path: PathBuf,
    cfg: AgentConfig,
    repo_branches: std::collections::HashMap<String, String>,
    worker_identity: String,
}

impl DeployWorkerContext {
    fn from_app_state(state: &AppState, worker_identity: &str) -> Self {
        let repo_branches = state
            .dispatch_config
            .snapshot()
            .map(|cfg| {
                cfg.repo_bindings
                    .iter()
                    .map(|(repo, binding)| (repo.clone(), binding.git_base.clone()))
                    .collect()
            })
            .unwrap_or_default();
        Self {
            db_path: state.db_path.clone(),
            cfg: state.cfg.clone(),
            repo_branches,
            worker_identity: worker_identity.to_string(),
        }
    }
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

fn deploy_release_root(db_path: &Path, repo: &RepoRef, branch: &str) -> Result<PathBuf> {
    let root = db_path
        .parent()
        .map(PathBuf::from)
        .ok_or_else(|| anyhow!("db path has no parent"))?
        .join(DEPLOY_RELEASES_DIRNAME)
        .join(&repo.owner)
        .join(&repo.repo)
        .join(branch);
    fs::create_dir_all(&root)
        .with_context(|| format!("failed to create deploy release root {}", root.display()))?;
    Ok(root)
}

fn deploy_release_dir(
    db_path: &Path,
    repo: &RepoRef,
    branch: &str,
    target_sha: &str,
) -> Result<PathBuf> {
    Ok(deploy_release_root(db_path, repo, branch)?.join(target_sha))
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

fn append_command_log(log: &mut String, label: &str, output: &std::process::Output) {
    let _ = writeln!(log, "== {label} ==");
    let _ = writeln!(log, "status={:?}", output.status.code());
    log.push_str("--- stdout ---\n");
    log.push_str(String::from_utf8_lossy(&output.stdout).as_ref());
    log.push_str("\n--- stderr ---\n");
    log.push_str(String::from_utf8_lossy(&output.stderr).as_ref());
    log.push('\n');
}

fn run_logged_command(command: &mut Command, log: &mut String, label: &str) -> Result<()> {
    let output = command
        .output()
        .with_context(|| format!("failed spawning command for {label}"))?;
    append_command_log(log, label, &output);
    if !output.status.success() {
        return Err(anyhow!(
            "command failed for {label}: status {:?}",
            output.status.code()
        ));
    }
    Ok(())
}

fn copy_file_force(from: &Path, to: &Path) -> Result<()> {
    if let Some(parent) = to.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("failed creating parent {}", parent.display()))?;
    }
    if to.exists() {
        fs::remove_file(to).with_context(|| format!("failed removing {}", to.display()))?;
    }
    fs::copy(from, to)
        .with_context(|| format!("failed copying {} -> {}", from.display(), to.display()))?;
    Ok(())
}

fn copy_if_exists(from: &Path, to: &Path) -> Result<bool> {
    if !from.exists() {
        return Ok(false);
    }
    copy_file_force(from, to)?;
    Ok(true)
}

fn install_release_binary(binary_name: &str, source: &Path, dest: &Path) -> Result<()> {
    copy_file_force(source, dest)
        .with_context(|| format!("failed installing {binary_name} into {}", dest.display()))?;
    let mut perms = fs::metadata(dest)
        .with_context(|| format!("failed reading metadata for {}", dest.display()))?
        .permissions();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        perms.set_mode(0o755);
        fs::set_permissions(dest, perms)
            .with_context(|| format!("failed setting mode for {}", dest.display()))?;
    }
    Ok(())
}

fn restart_orchd_service(log: &mut String) -> Result<()> {
    run_logged_command(
        Command::new("systemctl")
            .arg("--user")
            .arg("restart")
            .arg("orchd.service"),
        log,
        "systemctl --user restart orchd.service",
    )?;
    run_logged_command(
        Command::new("systemctl")
            .arg("--user")
            .arg("--quiet")
            .arg("is-active")
            .arg("orchd.service"),
        log,
        "systemctl --user --quiet is-active orchd.service",
    )
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

#[derive(Debug)]
struct DeployBinaryPaths {
    orchd_dest: PathBuf,
    forgejoctl_dest: PathBuf,
}

fn deploy_binary_paths() -> Result<DeployBinaryPaths> {
    let orchd_dest = std::env::var("ORCHD_BIN").unwrap_or_else(|_| DEFAULT_ORCHD_BIN.to_string());
    let forgejoctl_dest =
        std::env::var("FORGEJOCTL_BIN").unwrap_or_else(|_| DEFAULT_FORGEJOCTL_BIN.to_string());
    Ok(DeployBinaryPaths {
        orchd_dest: expand_tilde_path(&orchd_dest)?,
        forgejoctl_dest: expand_tilde_path(&forgejoctl_dest)?,
    })
}

fn build_release(
    checkout: &Path,
    release_dir: &Path,
    log: &mut String,
) -> Result<(PathBuf, PathBuf)> {
    run_logged_command(
        Command::new("cargo")
            .arg("build")
            .arg("--release")
            .arg("--manifest-path")
            .arg(checkout.join("Cargo.toml")),
        log,
        "cargo build --release",
    )?;
    let release_bin_dir = release_dir.join(DEPLOY_RELEASE_BIN_SUBDIR);
    fs::create_dir_all(&release_bin_dir)
        .with_context(|| format!("failed creating {}", release_bin_dir.display()))?;
    let orchd_src = checkout.join("target").join("release").join("orchd");
    let forgejoctl_src = checkout
        .join("target")
        .join("release")
        .join("forgejo-agent");
    if !orchd_src.is_file() {
        return Err(anyhow!(
            "orchd binary missing after build: {}",
            orchd_src.display()
        ));
    }
    if !forgejoctl_src.is_file() {
        return Err(anyhow!(
            "forgejoctl binary missing after build: {}",
            forgejoctl_src.display()
        ));
    }
    let orchd_release = release_bin_dir.join("orchd");
    let forgejoctl_release = release_bin_dir.join("forgejoctl");
    copy_file_force(&orchd_src, &orchd_release)?;
    copy_file_force(&forgejoctl_src, &forgejoctl_release)?;
    let mut perms = fs::metadata(&orchd_release)?.permissions();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        perms.set_mode(0o755);
        fs::set_permissions(&orchd_release, perms.clone())?;
        fs::set_permissions(&forgejoctl_release, perms)?;
    }
    Ok((orchd_release, forgejoctl_release))
}

fn install_release_with_rollback(
    orchd_release: &Path,
    forgejoctl_release: &Path,
    paths: &DeployBinaryPaths,
    rollback_dir: &Path,
    log: &mut String,
) -> Result<String> {
    fs::create_dir_all(rollback_dir)
        .with_context(|| format!("failed creating rollback dir {}", rollback_dir.display()))?;
    let orchd_backup = rollback_dir.join("orchd.prev");
    let forgejoctl_backup = rollback_dir.join("forgejoctl.prev");
    let had_orchd = copy_if_exists(&paths.orchd_dest, &orchd_backup)?;
    let had_forgejoctl = copy_if_exists(&paths.forgejoctl_dest, &forgejoctl_backup)?;

    install_release_binary("orchd", orchd_release, &paths.orchd_dest)?;
    install_release_binary("forgejoctl", forgejoctl_release, &paths.forgejoctl_dest)?;

    if let Err(err) = restart_orchd_service(log) {
        let _ = writeln!(log, "restart failed; entering rollback: {err:#}");
        if had_orchd {
            let _ = install_release_binary("orchd", &orchd_backup, &paths.orchd_dest);
        }
        if had_forgejoctl {
            let _ =
                install_release_binary("forgejoctl", &forgejoctl_backup, &paths.forgejoctl_dest);
        }
        let rollback_status = if restart_orchd_service(log).is_ok() {
            "rollback_restored".to_string()
        } else {
            detect_rollback_status()
        };
        return Err(anyhow!(
            "service restart failed after install: {err:#}; rollback={rollback_status}"
        ));
    }
    Ok("not_needed".to_string())
}

fn current_db_schema_version(db_path: &Path) -> Result<i64> {
    let conn = Connection::open(db_path)
        .with_context(|| format!("failed opening {}", db_path.display()))?;
    let table_exists: bool = conn
        .query_row(
            "SELECT 1 FROM sqlite_master WHERE type='table' AND name='schema_migrations' LIMIT 1",
            [],
            |_| Ok(()),
        )
        .optional()?
        .is_some();
    if !table_exists {
        return Ok(0);
    }
    let value = conn
        .query_row("SELECT MAX(version) FROM schema_migrations", [], |row| {
            row.get::<_, Option<i64>>(0)
        })
        .optional()?
        .flatten()
        .unwrap_or(0);
    Ok(value)
}

fn schema_contract_from_built_orchd(orchd_bin: &Path, log: &mut String) -> Result<(i64, i64)> {
    let output = Command::new(orchd_bin)
        .arg("schema-contract")
        .arg("--json")
        .output()
        .with_context(|| format!("failed spawning {} schema-contract", orchd_bin.display()))?;
    append_command_log(log, "orchd schema-contract --json", &output);
    if !output.status.success() {
        return Err(anyhow!(
            "schema-contract command failed with status {:?}",
            output.status.code()
        ));
    }
    let raw = String::from_utf8_lossy(&output.stdout);
    let value: serde_json::Value = serde_json::from_str(raw.as_ref())
        .with_context(|| format!("invalid schema-contract JSON: {}", raw.trim()))?;
    let latest = value
        .get("latest")
        .and_then(serde_json::Value::as_i64)
        .ok_or_else(|| anyhow!("schema-contract JSON missing latest"))?;
    let min_compatible = value
        .get("min_compatible")
        .and_then(serde_json::Value::as_i64)
        .ok_or_else(|| anyhow!("schema-contract JSON missing min_compatible"))?;
    Ok((latest, min_compatible))
}

fn enforce_schema_contract_guard(
    db_path: &Path,
    orchd_bin: &Path,
    log: &mut String,
) -> Result<(i64, i64, i64)> {
    let db_schema = current_db_schema_version(db_path)?;
    let (latest_schema, min_compatible_schema) = schema_contract_from_built_orchd(orchd_bin, log)?;
    if latest_schema < db_schema {
        return Err(anyhow!(
            "schema downgrade blocked: source schema {latest_schema} < db schema {db_schema}"
        ));
    }
    if db_schema < min_compatible_schema {
        return Err(anyhow!(
            "schema incompatible: db schema {db_schema} < binary minimum compatible {min_compatible_schema}"
        ));
    }
    Ok((latest_schema, min_compatible_schema, db_schema))
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

fn deploy_branch_for_repo(ctx: &DeployWorkerContext, repo_full_name: &str) -> String {
    ctx.repo_branches
        .get(repo_full_name)
        .cloned()
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
    let ctx = DeployWorkerContext::from_app_state(state, "embedded-orchd");
    let Ok(repo_ref) = RepoRef::parse(record.repo_full_name.as_str()) else {
        return Ok(());
    };
    if !is_managed_repo(&repo_ref) {
        return Ok(());
    }
    let expected_branch = deploy_branch_for_repo(&ctx, record.repo_full_name.as_str());
    let Some((branch, target_sha)) = extract_push_target(payload, expected_branch.as_str()) else {
        return Ok(());
    };
    let enqueue = db::enqueue_deploy_job(
        &ctx.db_path,
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
    let ctx = DeployWorkerContext::from_app_state(state, "embedded-orchd");
    tokio::task::spawn_blocking(move || reconcile_head_enqueue_blocking(&ctx))
        .await
        .context("deploy reconcile join failure")?
}

fn reconcile_head_enqueue_blocking(ctx: &DeployWorkerContext) -> Result<()> {
    let db_path = ctx.db_path.as_path();
    let cfg = &ctx.cfg;
    let repo = managed_repo();
    let repo_full_name = repo.to_string();
    let branch = deploy_branch_for_repo(ctx, repo_full_name.as_str());
    let Some(remote_head) = read_remote_branch_head(db_path, cfg, &repo, branch.as_str())? else {
        return Ok(());
    };
    let latest_deployed =
        db::latest_deployed_sha(db_path, repo_full_name.as_str(), branch.as_str())?;
    if latest_deployed.as_deref() == Some(remote_head.as_str()) {
        return Ok(());
    }
    if db::has_active_deploy_for_target(
        db_path,
        repo_full_name.as_str(),
        branch.as_str(),
        remote_head.as_str(),
    )? {
        return Ok(());
    }
    let delivery_id = format!("reconcile:{}:{}", repo_full_name, Utc::now().timestamp());
    let enqueue = db::enqueue_deploy_job(
        db_path,
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
}

fn run_deploy_phase<T>(
    ctx: &DeployWorkerContext,
    job: &DeployJob,
    phase: &str,
    detail_json: Option<&str>,
    operation: impl FnOnce() -> Result<T>,
) -> Result<T> {
    let run_id = db::start_deploy_run(&ctx.db_path, job, phase, detail_json)
        .with_context(|| format!("failed starting deploy phase {phase}"))?;
    match operation() {
        Ok(value) => {
            db::finish_deploy_run(&ctx.db_path, run_id, DeployRunStatus::Succeeded, None, None)?;
            Ok(value)
        }
        Err(err) => {
            let err_text = format!("{err:#}");
            let _ = db::finish_deploy_run(
                &ctx.db_path,
                run_id,
                DeployRunStatus::Failed,
                Some("phase_failed"),
                Some(err_text.as_str()),
            );
            Err(err)
        }
    }
}

fn write_deploy_log(log_path: &Path, log: &str) -> Result<()> {
    fs::write(log_path, log)
        .with_context(|| format!("failed writing deploy log {}", log_path.display()))
}

fn deploy_failure_from_error(
    log: &mut String,
    log_path: &Path,
    reason_code: &str,
    error: anyhow::Error,
    checkout_path: Option<PathBuf>,
    rollback_status: &str,
) -> DeployFailure {
    let _ = writeln!(log, "failure_reason={reason_code}");
    let _ = writeln!(log, "failure_error={error:#}");
    let _ = write_deploy_log(log_path, log);
    DeployFailure {
        reason_code: reason_code.to_string(),
        error_text: format!("{error:#}"),
        checkout_path,
        log_path: Some(log_path.to_path_buf()),
        rollback_status: rollback_status.to_string(),
    }
}

fn execute_deploy_job(
    ctx: &DeployWorkerContext,
    job: &DeployJob,
) -> std::result::Result<DeploySuccess, DeployFailure> {
    let log_path = match deploy_log_path(&ctx.db_path, job.id) {
        Ok(path) => path,
        Err(err) => {
            return Err(DeployFailure {
                reason_code: "deploy_log_path_failed".to_string(),
                error_text: format!("{err:#}"),
                checkout_path: None,
                log_path: None,
                rollback_status: "not_attempted".to_string(),
            });
        }
    };
    let mut log = String::new();
    let _ = writeln!(log, "worker_identity={}", ctx.worker_identity);
    let _ = writeln!(log, "repo={}", job.repo_full_name);
    let _ = writeln!(log, "branch={}", job.target_branch);
    let _ = writeln!(log, "target_sha={}", job.target_sha);
    let _ = writeln!(log, "deploy_job_id={}", job.id);

    let repo = match RepoRef::parse(job.repo_full_name.as_str()) {
        Ok(repo) => repo,
        Err(err) => {
            return Err(deploy_failure_from_error(
                &mut log,
                &log_path,
                "deploy_invalid_repo",
                anyhow!(err.to_string()),
                None,
                "not_attempted",
            ));
        }
    };
    if !is_managed_repo(&repo) {
        return Err(deploy_failure_from_error(
            &mut log,
            &log_path,
            "deploy_repo_not_managed",
            anyhow!("repo {} is not deploy-managed", job.repo_full_name),
            None,
            "not_attempted",
        ));
    }

    let checkout_path = match run_deploy_phase(ctx, job, "checkout", None, || {
        ensure_checkout_at_sha(
            &ctx.db_path,
            &ctx.cfg,
            &repo,
            job.target_branch.as_str(),
            job.target_sha.as_str(),
        )
    }) {
        Ok(path) => path,
        Err(err) => {
            return Err(deploy_failure_from_error(
                &mut log,
                &log_path,
                "deploy_checkout_failed",
                err,
                None,
                "not_attempted",
            ));
        }
    };
    let release_dir = match deploy_release_dir(
        &ctx.db_path,
        &repo,
        job.target_branch.as_str(),
        job.target_sha.as_str(),
    ) {
        Ok(path) => path,
        Err(err) => {
            return Err(deploy_failure_from_error(
                &mut log,
                &log_path,
                "deploy_release_dir_failed",
                err,
                Some(checkout_path),
                "not_attempted",
            ));
        }
    };
    let build_detail = json!({
        "release_dir": release_dir,
    })
    .to_string();
    let (orchd_release, forgejoctl_release) =
        match run_deploy_phase(ctx, job, "build", Some(build_detail.as_str()), || {
            build_release(&checkout_path, &release_dir, &mut log)
        }) {
            Ok(paths) => paths,
            Err(err) => {
                return Err(deploy_failure_from_error(
                    &mut log,
                    &log_path,
                    "deploy_build_failed",
                    err,
                    Some(checkout_path),
                    "not_attempted",
                ));
            }
        };
    let schema_guard = run_deploy_phase(ctx, job, "schema_guard", None, || {
        enforce_schema_contract_guard(&ctx.db_path, orchd_release.as_path(), &mut log)
    });
    if let Err(err) = schema_guard {
        return Err(deploy_failure_from_error(
            &mut log,
            &log_path,
            "deploy_schema_downgrade_blocked",
            err,
            Some(checkout_path),
            "not_attempted",
        ));
    }
    let install_paths = match deploy_binary_paths() {
        Ok(paths) => paths,
        Err(err) => {
            return Err(deploy_failure_from_error(
                &mut log,
                &log_path,
                "deploy_paths_failed",
                err,
                Some(checkout_path),
                "not_attempted",
            ));
        }
    };
    let release_before = match db::deploy_release_state(
        &ctx.db_path,
        job.repo_full_name.as_str(),
        job.target_branch.as_str(),
    ) {
        Ok(value) => value,
        Err(err) => {
            return Err(deploy_failure_from_error(
                &mut log,
                &log_path,
                "deploy_release_state_read_failed",
                err,
                Some(checkout_path),
                "not_attempted",
            ));
        }
    };
    let (previous_active_sha, previous_previous_sha) = release_before
        .map(|state| (state.active_sha, state.previous_sha))
        .unwrap_or((None, None));
    if let Some(previous_previous_sha) = previous_previous_sha {
        let _ = writeln!(log, "previous_previous_sha={previous_previous_sha}");
    }
    let rollback_dir = release_dir
        .join("rollback")
        .join(format!("attempt-{}", job.attempt_count));
    let install_detail = json!({
        "orchd_dest": install_paths.orchd_dest,
        "forgejoctl_dest": install_paths.forgejoctl_dest,
        "rollback_dir": rollback_dir,
    })
    .to_string();
    let activate_result =
        run_deploy_phase(ctx, job, "activate", Some(install_detail.as_str()), || {
            install_release_with_rollback(
                orchd_release.as_path(),
                forgejoctl_release.as_path(),
                &install_paths,
                rollback_dir.as_path(),
                &mut log,
            )
        });
    let rollback_status = match activate_result {
        Ok(status) => status,
        Err(err) => {
            return Err(deploy_failure_from_error(
                &mut log,
                &log_path,
                "deploy_activate_failed",
                err,
                Some(checkout_path),
                detect_rollback_status().as_str(),
            ));
        }
    };
    if let Err(err) = db::upsert_deploy_release_state(
        &ctx.db_path,
        job.repo_full_name.as_str(),
        job.target_branch.as_str(),
        Some(job.target_sha.as_str()),
        previous_active_sha.as_deref(),
    ) {
        return Err(deploy_failure_from_error(
            &mut log,
            &log_path,
            "deploy_release_state_write_failed",
            err,
            Some(checkout_path),
            rollback_status.as_str(),
        ));
    }
    if let Err(err) = write_deploy_log(&log_path, &log) {
        return Err(deploy_failure_from_error(
            &mut log,
            &log_path,
            "deploy_log_write_failed",
            err,
            Some(checkout_path),
            rollback_status.as_str(),
        ));
    }
    Ok(DeploySuccess {
        checkout_path,
        log_path,
    })
}

fn run_worker_once_blocking(ctx: &DeployWorkerContext) -> Result<()> {
    let Some(job) = db::claim_next_deploy_job(&ctx.db_path)? else {
        return Ok(());
    };
    let _ = db::mark_deploy_job_worker_identity(&ctx.db_path, job.id, ctx.worker_identity.as_str());

    match execute_deploy_job(ctx, &job) {
        Ok(success) => {
            db::complete_deploy_job_success(
                &ctx.db_path,
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
                    "worker_identity": ctx.worker_identity,
                    "log_path": success.log_path.to_string_lossy(),
                }),
            );
        }
        Err(failure) => {
            let incident_issue_number = open_deploy_failure_issue(
                &ctx.cfg,
                &job,
                failure.checkout_path.as_deref(),
                failure.log_path.as_deref(),
                failure.rollback_status.as_str(),
                failure.error_text.as_str(),
            )
            .unwrap_or(None);
            db::complete_deploy_job_failure(
                &ctx.db_path,
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
                    "worker_identity": ctx.worker_identity,
                    "log_path": failure
                        .log_path
                        .map(|path| path.to_string_lossy().into_owned()),
                    "error": failure.error_text,
                }),
            );
        }
    }
    Ok(())
}

pub(super) async fn run_worker_once(state: &AppState) -> Result<()> {
    let ctx = DeployWorkerContext::from_app_state(state, "embedded-orchd");
    tokio::task::spawn_blocking(move || run_worker_once_blocking(&ctx))
        .await
        .context("deploy worker join failure")?
}

pub(super) fn deploy_worker_command(cli: &Cli, args: DeployWorkerArgs) -> Result<()> {
    let db_path = expand_tilde_path(&cli.db_path)?;
    db::init_db(db_path.as_path())?;
    let cfg = AgentConfig::load(cli.config.clone(), cli.token_file.clone())?;
    let dispatch_config_path = resolve_dispatch_config_path(&cli.dispatch_config)?;
    let dispatch_config = load_dispatch_config(dispatch_config_path.as_path())?;
    let worker_identity = args.worker_identity.unwrap_or_else(|| {
        let host = std::env::var("HOSTNAME").unwrap_or_else(|_| "localhost".to_string());
        format!("deployd:{host}:pid-{}", std::process::id())
    });
    let ctx = DeployWorkerContext {
        db_path,
        cfg,
        repo_branches: dispatch_config
            .repo_bindings
            .iter()
            .map(|(repo, binding)| (repo.clone(), binding.git_base.clone()))
            .collect(),
        worker_identity: worker_identity.clone(),
    };
    log_line(
        "deploy_worker_start",
        json!({
            "worker_identity": worker_identity,
            "interval_sec": args.interval_sec,
            "once": args.once,
        }),
    );
    if args.once {
        reconcile_head_enqueue_blocking(&ctx)?;
        return run_worker_once_blocking(&ctx);
    }
    let interval = Duration::from_secs(args.interval_sec.max(1));
    loop {
        if let Err(err) = reconcile_head_enqueue_blocking(&ctx) {
            log_line(
                "deploy_reconcile_error",
                json!({
                    "worker_identity": ctx.worker_identity,
                    "error": err.to_string(),
                }),
            );
        }
        if let Err(err) = run_worker_once_blocking(&ctx) {
            log_line(
                "deploy_worker_error",
                json!({
                    "worker_identity": ctx.worker_identity,
                    "error": err.to_string(),
                }),
            );
        }
        thread::sleep(interval);
    }
}

#[cfg(test)]
mod tests {
    use tempfile::tempdir;

    use super::{current_db_schema_version, extract_push_target, managed_repo_from_override};
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

    #[test]
    fn current_db_schema_version_defaults_to_zero_without_table() {
        let temp = tempdir().expect("tempdir");
        let db_path = temp.path().join("orchd.sqlite");
        let version = current_db_schema_version(&db_path).expect("query schema");
        assert_eq!(version, 0);
    }
}
