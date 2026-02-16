use std::path::PathBuf;
use std::process::Command;

use anyhow::{Context, Result, anyhow, bail};

use super::cli::IssueResumeArgs;
use super::db;
use super::paths::expand_tilde_path;

const ISSUE_OWNER: &str = "main";
const DEFAULT_CODEX_ROLE_BIN: &str = "/home/main/forgejo-agent/bin/codex-role";

fn validate_repo_name(repo: &str) -> Result<&str> {
    let repo = repo.trim();
    if repo.is_empty() {
        bail!("repo must be non-empty");
    }
    if repo.contains('/') || repo.contains('#') {
        bail!("repo must be a bare repo name (for example: forgejo-work)");
    }
    if repo.chars().any(char::is_whitespace) {
        bail!("repo must not contain whitespace");
    }
    Ok(repo)
}

fn codex_role_bin() -> PathBuf {
    std::env::var("ORCHD_CODEX_ROLE_BIN")
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from(DEFAULT_CODEX_ROLE_BIN))
}

fn codex_role_arg(target_role: &str) -> &str {
    target_role.strip_prefix("codex-").unwrap_or(target_role)
}

fn run_codex_resume(target_role: &str, session_id: &str, trailing_args: &[String]) -> Result<()> {
    let codex_role_bin = codex_role_bin();
    let role_arg = codex_role_arg(target_role);
    let mut command = Command::new(&codex_role_bin);
    command
        .arg(role_arg)
        .arg("resume")
        .arg(session_id)
        .args(trailing_args);
    let status = command.status().with_context(|| {
        format!(
            "failed to spawn codex resume with {} {}",
            codex_role_bin.display(),
            role_arg
        )
    })?;
    if status.success() {
        return Ok(());
    }
    match status.code() {
        Some(code) => bail!("codex resume exited with status code {code}"),
        None => bail!("codex resume terminated by signal"),
    }
}

pub(super) fn issue_resume_command(db_path_raw: &str, args: IssueResumeArgs) -> Result<()> {
    let repo = validate_repo_name(&args.repo)?;
    let repo_full_name = format!("{ISSUE_OWNER}/{repo}");
    let issue_ref = format!("{repo_full_name}#{}", args.issue_number);
    let db_path = expand_tilde_path(db_path_raw)?;
    if let Some(active) =
        db::latest_issue_active_dispatch(&db_path, &repo_full_name, args.issue_number)?
    {
        bail!(
            "issue {issue_ref} has in-flight dispatch {} ({})",
            active.id,
            active.status.as_db_str()
        );
    }
    let latest = db::latest_issue_resume_dispatch(&db_path, &repo_full_name, args.issue_number)?
        .ok_or_else(|| anyhow!("issue {issue_ref} has no associated codex_session_id"))?;
    if !latest.status.is_terminal() {
        bail!(
            "issue {issue_ref} has non-terminal latest dispatch {} ({})",
            latest.id,
            latest.status.as_db_str()
        );
    }
    let session_id = latest
        .codex_session_id
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .ok_or_else(|| {
            anyhow!(
                "latest dispatch {} for {issue_ref} has no codex_session_id",
                latest.id
            )
        })?
        .to_string();

    run_codex_resume(&latest.target_role, &session_id, &args.codex_resume_args)
}
