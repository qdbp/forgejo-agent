use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result, anyhow, bail};
use serde::Serialize;

use super::cli::{IssueResumeArgs, IssueSessionsArgs};
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

fn normalized_role_filter(role: Option<&str>) -> Result<Option<String>> {
    role.map(|raw| {
        let role = raw.trim().to_ascii_lowercase();
        if role.is_empty() {
            bail!("role filter must be non-empty when provided");
        }
        Ok(role)
    })
    .transpose()
}

fn ensure_repo_known(db_path: &Path, repo: &str, repo_full_name: &str) -> Result<()> {
    if db::repo_is_known(db_path, repo_full_name)? {
        return Ok(());
    }

    let prefix = format!("{ISSUE_OWNER}/");
    let mut known = db::list_known_repo_full_names(db_path, 25)?
        .into_iter()
        .map(|full| {
            full.strip_prefix(&prefix)
                .map(ToOwned::to_owned)
                .unwrap_or(full)
        })
        .collect::<Vec<_>>();
    known.sort();
    known.dedup();

    if known.is_empty() {
        bail!("unknown repo '{repo}' (owner {ISSUE_OWNER}); orchd db has no known repos yet");
    }

    bail!(
        "unknown repo '{repo}' (owner {ISSUE_OWNER}); known repos: {}",
        known.join(", ")
    );
}

#[derive(Debug, Clone, Serialize)]
struct IssueSessionSummary {
    dispatch_id: i64,
    status: String,
    target_role: String,
    codex_session_id: String,
}

fn issue_session_summaries(rows: &[db::IssueResumeDispatch]) -> Vec<IssueSessionSummary> {
    rows.iter()
        .map(|row| IssueSessionSummary {
            dispatch_id: row.id,
            status: row.status.as_db_str().to_string(),
            target_role: row.target_role.clone(),
            codex_session_id: row.codex_session_id.clone().unwrap_or_default(),
        })
        .collect()
}

pub(super) fn issue_sessions_command(db_path_raw: &str, args: IssueSessionsArgs) -> Result<()> {
    let repo = validate_repo_name(&args.repo)?;
    let repo_full_name = format!("{ISSUE_OWNER}/{repo}");
    let db_path = expand_tilde_path(db_path_raw)?;
    ensure_repo_known(&db_path, repo, &repo_full_name)?;
    let role_filter = normalized_role_filter(args.role.as_deref())?;
    let rows = db::list_issue_resume_dispatches(
        &db_path,
        &repo_full_name,
        args.issue_number,
        role_filter.as_deref(),
    )?;
    let summaries = issue_session_summaries(&rows);

    if args.json {
        println!("{}", serde_json::to_string_pretty(&summaries)?);
        return Ok(());
    }

    println!(
        "{:<12} {:<16} {:<18} {}",
        "dispatch_id", "status", "role", "session_id"
    );
    for row in summaries {
        println!(
            "{:<12} {:<16} {:<18} {}",
            row.dispatch_id, row.status, row.target_role, row.codex_session_id
        );
    }
    Ok(())
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
    ensure_repo_known(&db_path, repo, &repo_full_name)?;
    if let Some(active) =
        db::latest_issue_active_dispatch(&db_path, &repo_full_name, args.issue_number)?
    {
        bail!(
            "issue {issue_ref} has in-flight dispatch {} ({})",
            active.id,
            active.status.as_db_str()
        );
    }
    let role_filter = normalized_role_filter(args.role.as_deref())?;
    let all_rows = db::list_issue_resume_dispatches(
        &db_path,
        &repo_full_name,
        args.issue_number,
        role_filter.as_deref(),
    )?;
    if all_rows.is_empty() {
        bail!("issue {issue_ref} has no associated codex_session_id");
    }

    let latest = if let Some(dispatch_id) = args.dispatch_id {
        all_rows
            .iter()
            .find(|row| row.id == dispatch_id)
            .cloned()
            .ok_or_else(|| {
                anyhow!(
                    "dispatch {} is not a resumable session for {}",
                    dispatch_id,
                    issue_ref
                )
            })?
    } else {
        if role_filter.is_none() {
            let unique_roles = all_rows
                .iter()
                .map(|row| row.target_role.as_str())
                .collect::<std::collections::BTreeSet<_>>();
            if unique_roles.len() > 1 {
                let roles = unique_roles.into_iter().collect::<Vec<_>>().join(", ");
                bail!(
                    "issue {} has sessions for multiple roles ({}); re-run with --role <role> or --dispatch-id <id> (hint: orchd issue sessions {} {})",
                    issue_ref,
                    roles,
                    repo,
                    args.issue_number
                );
            }
        }
        all_rows
            .first()
            .cloned()
            .ok_or_else(|| anyhow!("issue {issue_ref} has no associated codex_session_id"))?
    };

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
