use std::fs;
use std::fs::OpenOptions;
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result, anyhow};
use chrono::Utc;

use forgejo_agent::types::RepoRef;

use super::db;
use super::dispatch_config::DispatchRoleConfig;
use super::errors::DispatchError;
use super::state::AppState;

pub(super) const DEFAULT_GIT_REMOTE: &str = "origin";
pub(super) const DEFAULT_GIT_BASE_BRANCH: &str = "main";

fn git_sanitize_token(input: &str, max_len: usize) -> String {
    let mut out = String::with_capacity(input.len());
    let mut last_dash = false;
    for ch in input.chars() {
        if ch.is_ascii_alphanumeric() {
            out.push(ch.to_ascii_lowercase());
            last_dash = false;
        } else if !last_dash {
            out.push('-');
            last_dash = true;
        }
        if out.len() >= max_len {
            break;
        }
    }
    out.trim_matches('-').to_string()
}

pub(super) fn dispatch_worktree_branch(
    repo_full_name: &str,
    issue_number: u64,
    dispatch_id: i64,
    directive: &str,
) -> String {
    let repo_slug = git_sanitize_token(repo_full_name, 24);
    let directive = git_sanitize_token(directive, 12);
    format!("orchd/d{dispatch_id}/r{repo_slug}-i{issue_number}-{directive}")
}

fn git_run_checked(repo_root: &Path, args: &[&str]) -> Result<(), DispatchError> {
    let output = Command::new("git")
        .arg("-C")
        .arg(repo_root)
        .args(args)
        .output()
        .map_err(|err| DispatchError::Io(format!("failed to invoke git: {err}")))?;
    if output.status.success() {
        return Ok(());
    }
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    Err(DispatchError::Io(format!(
        "git failed (cwd={}) args={args:?} status={:?}\nstdout:\n{stdout}\nstderr:\n{stderr}",
        repo_root.display(),
        output.status.code()
    )))
}

pub(super) fn create_dispatch_worktree(
    db_path: &Path,
    token_file: &Path,
    repo_root: &Path,
    worktree_dir: &Path,
    branch: &str,
    remote: &str,
    base_branch: &str,
) -> Result<(), DispatchError> {
    if worktree_dir.exists() {
        return Err(DispatchError::Io(format!(
            "dispatch worktree path already exists: {}",
            worktree_dir.display()
        )));
    }
    let git_dir = repo_root.join(".git");
    if !git_dir.exists() {
        return Err(DispatchError::Io(format!(
            "repo root is not a git checkout: {}",
            repo_root.display()
        )));
    }
    let _ = git_checked_with_token(
        db_path,
        token_file,
        Some(repo_root),
        &["fetch", remote, base_branch],
    )?;
    let base_ref = format!("{remote}/{base_branch}");
    git_run_checked(
        repo_root,
        &[
            "worktree",
            "add",
            "-B",
            branch,
            &worktree_dir.to_string_lossy(),
            &base_ref,
        ],
    )?;
    Ok(())
}

pub(super) fn lock_root(db_path: &Path) -> Result<PathBuf, DispatchError> {
    let root = db_path
        .parent()
        .map(PathBuf::from)
        .ok_or_else(|| DispatchError::Io("db path has no parent".to_string()))?
        .join("locks");
    fs::create_dir_all(&root).map_err(|err| {
        DispatchError::Io(format!(
            "failed to create lock dir {}: {err}",
            root.display()
        ))
    })?;
    Ok(root)
}

pub(super) fn run_root(db_path: &Path) -> Result<PathBuf, DispatchError> {
    let root = db_path
        .parent()
        .map(PathBuf::from)
        .ok_or_else(|| DispatchError::Io("db path has no parent".to_string()))?
        .join("dispatch-runs");
    fs::create_dir_all(&root).map_err(|err| {
        DispatchError::Io(format!(
            "failed to create run dir {}: {err}",
            root.display()
        ))
    })?;
    Ok(root)
}

fn repo_store_root(db_path: &Path) -> Result<PathBuf, DispatchError> {
    let root = db_path
        .parent()
        .map(PathBuf::from)
        .ok_or_else(|| DispatchError::Io("db path has no parent".to_string()))?
        .join("repos");
    fs::create_dir_all(&root).map_err(|err| {
        DispatchError::Io(format!(
            "failed to create repo store dir {}: {err}",
            root.display()
        ))
    })?;
    Ok(root)
}

fn repo_checkout_root(
    db_path: &Path,
    role: &DispatchRoleConfig,
    repo: &RepoRef,
) -> Result<PathBuf, DispatchError> {
    Ok(repo_store_root(db_path)?
        .join(&role.forgejo_login)
        .join(&repo.owner)
        .join(&repo.repo))
}

fn git_askpass_script_path(db_path: &Path) -> Result<PathBuf, DispatchError> {
    Ok(db_path
        .parent()
        .map(PathBuf::from)
        .ok_or_else(|| DispatchError::Io("db path has no parent".to_string()))?
        .join("git-askpass.sh"))
}

fn ensure_git_askpass_script(db_path: &Path) -> Result<PathBuf, DispatchError> {
    let path = git_askpass_script_path(db_path)?;
    if path.is_file() {
        return Ok(path);
    }
    let contents = r#"#!/bin/sh
set -eu
cat "${ORCHD_GIT_TOKEN_FILE:?missing ORCHD_GIT_TOKEN_FILE}"
"#;
    fs::write(&path, contents).map_err(|err| {
        DispatchError::Io(format!(
            "failed writing git askpass helper {}: {err}",
            path.display()
        ))
    })?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        let mut perms = fs::metadata(&path)
            .map_err(|err| {
                DispatchError::Io(format!(
                    "failed stat git askpass helper {}: {err}",
                    path.display()
                ))
            })?
            .permissions();
        perms.set_mode(0o700);
        fs::set_permissions(&path, perms).map_err(|err| {
            DispatchError::Io(format!(
                "failed chmod git askpass helper {}: {err}",
                path.display()
            ))
        })?;
    }
    Ok(path)
}

fn git_output_with_token(
    db_path: &Path,
    token_file: &Path,
    workdir: Option<&Path>,
    args: &[&str],
) -> Result<std::process::Output, DispatchError> {
    let askpass = ensure_git_askpass_script(db_path)?;
    let mut cmd = Command::new("git");
    if let Some(workdir) = workdir {
        cmd.arg("-C").arg(workdir);
    }
    cmd.args(args)
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("GIT_ASKPASS", &askpass)
        .env("ORCHD_GIT_TOKEN_FILE", token_file);
    cmd.output()
        .map_err(|err| DispatchError::Io(format!("failed to invoke git: {err}")))
}

pub(super) fn git_checked_with_token(
    db_path: &Path,
    token_file: &Path,
    workdir: Option<&Path>,
    args: &[&str],
) -> Result<std::process::Output, DispatchError> {
    let output = git_output_with_token(db_path, token_file, workdir, args)?;
    if output.status.success() {
        return Ok(output);
    }
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    let cwd = workdir
        .map(|p| p.display().to_string())
        .unwrap_or_else(|| "<none>".to_string());
    Err(DispatchError::Io(format!(
        "git failed (cwd={cwd}) args={args:?} status={:?}\nstdout:\n{stdout}\nstderr:\n{stderr}",
        output.status.code()
    )))
}

pub(super) fn forgejo_http_git_url(
    base_url: &url::Url,
    username: &str,
    repo_full_name: &str,
) -> Result<String, DispatchError> {
    let repo = RepoRef::parse(repo_full_name)
        .map_err(|_| DispatchError::InvalidIssueRef(repo_full_name.to_string()))?;
    let mut url = base_url.clone();
    url.set_username(username).map_err(|()| {
        DispatchError::Io(format!("failed setting username '{username}' in git URL"))
    })?;
    let base_path = url.path().trim_end_matches('/');
    let new_path = if base_path.is_empty() {
        format!("/{}/{}.git", repo.owner, repo.repo)
    } else {
        format!("{base_path}/{}/{}.git", repo.owner, repo.repo)
    };
    url.set_path(&new_path);
    Ok(url.to_string())
}

pub(super) fn ensure_repo_checkout(
    state: &AppState,
    role: &DispatchRoleConfig,
    repo_full_name: &str,
) -> Result<PathBuf, DispatchError> {
    let repo = RepoRef::parse(repo_full_name)
        .map_err(|_| DispatchError::InvalidIssueRef(repo_full_name.to_string()))?;
    let checkout = repo_checkout_root(&state.db_path, role, &repo)?;
    let git_dir = checkout.join(".git");
    if git_dir.is_dir() {
        let _ = git_checked_with_token(
            &state.db_path,
            &role.token_file,
            Some(&checkout),
            &["fetch", DEFAULT_GIT_REMOTE, DEFAULT_GIT_BASE_BRANCH],
        );
        let _ = db::update_repo_local_path(&state.db_path, repo_full_name, &checkout);
        return Ok(checkout);
    }
    if checkout.exists() {
        return Err(DispatchError::Io(format!(
            "repo checkout path exists but is not a git repo: {}",
            checkout.display()
        )));
    }
    if let Some(parent) = checkout.parent() {
        fs::create_dir_all(parent).map_err(|err| {
            DispatchError::Io(format!(
                "failed to create repo checkout parent dir {}: {err}",
                parent.display()
            ))
        })?;
    }
    let url = forgejo_http_git_url(&state.cfg.base_url, &role.forgejo_login, repo_full_name)?;
    git_checked_with_token(
        &state.db_path,
        &role.token_file,
        None,
        &[
            "clone",
            "--origin",
            DEFAULT_GIT_REMOTE,
            &url,
            &checkout.to_string_lossy(),
        ],
    )?;
    let _ = db::update_repo_local_path(&state.db_path, repo_full_name, &checkout);
    Ok(checkout)
}

pub(super) fn acquire_repo_lock(
    db_path: &Path,
    repo_full_name: &str,
) -> Result<PathBuf, DispatchError> {
    let slug = repo_full_name.replace('/', "__");
    let lock_path = lock_root(db_path)?.join(format!("{slug}.lock"));
    let mut file = OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .open(&lock_path)
        .map_err(|err| {
            DispatchError::Io(format!(
                "failed to create lock {}: {err}",
                lock_path.display()
            ))
        })?;
    writeln!(file, "repo={repo_full_name}")
        .and_then(|()| writeln!(file, "created_at={}", Utc::now().to_rfc3339()))
        .map_err(|err| DispatchError::Io(format!("failed writing lock metadata: {err}")))?;
    Ok(lock_path)
}

pub(super) fn git_output(workdir: &Path, args: &[&str]) -> Result<std::process::Output> {
    Command::new("git")
        .arg("-C")
        .arg(workdir)
        .args(args)
        .output()
        .with_context(|| format!("failed spawning git in {}", workdir.display()))
}

pub(super) fn git_checked(workdir: &Path, args: &[&str]) -> Result<std::process::Output> {
    let output = git_output(workdir, args)?;
    if output.status.success() {
        return Ok(output);
    }
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    Err(anyhow!(
        "git failed (cwd={}) args={args:?} status={:?}\nstdout:\n{stdout}\nstderr:\n{stderr}",
        workdir.display(),
        output.status.code()
    ))
}

pub(super) fn git_stdout_trim(output: &std::process::Output) -> String {
    String::from_utf8_lossy(&output.stdout).trim().to_string()
}

pub(super) fn best_effort_sync_principal(
    db_path: &Path,
    token_file: &Path,
    principal_workdir: &Path,
    base_branch: &str,
    source_remote_url: &str,
) -> Vec<String> {
    let mut lines = Vec::new();
    if let Err(err) = git_checked(principal_workdir, &["rev-parse", "--is-inside-work-tree"]) {
        lines.push(format!("principal_sync: skipped (not a git repo): {err:#}"));
        return lines;
    }
    let status = match git_checked(
        principal_workdir,
        &["status", "--porcelain", "--untracked-files=no"],
    ) {
        Ok(out) => git_stdout_trim(&out),
        Err(err) => {
            lines.push(format!(
                "principal_sync: skipped (git status failed): {err:#}"
            ));
            return lines;
        }
    };
    if !status.is_empty() {
        lines.push("principal_sync: skipped (principal has uncommitted changes)".to_string());
        return lines;
    }

    let current_branch =
        match git_checked(principal_workdir, &["rev-parse", "--abbrev-ref", "HEAD"]) {
            Ok(out) => git_stdout_trim(&out),
            Err(err) => {
                lines.push(format!(
                    "principal_sync: skipped (branch detect failed): {err:#}"
                ));
                return lines;
            }
        };
    if current_branch != base_branch {
        lines.push(format!(
            "principal_sync: skipped (on branch '{current_branch}', expected '{base_branch}')"
        ));
        return lines;
    }

    if let Err(err) = git_checked_with_token(
        db_path,
        token_file,
        Some(principal_workdir),
        &["fetch", source_remote_url, base_branch],
    ) {
        lines.push(format!("principal_sync: fetch failed: {err}"));
        return lines;
    }
    let before_short = git_checked(principal_workdir, &["rev-parse", "--short", "HEAD"])
        .map(|out| git_stdout_trim(&out))
        .unwrap_or_else(|_| "?".to_string());
    if let Err(err) = git_checked(principal_workdir, &["merge", "--ff-only", "FETCH_HEAD"]) {
        lines.push(format!("principal_sync: ff-only merge failed: {err:#}"));
        return lines;
    }
    let after_short = git_checked(principal_workdir, &["rev-parse", "--short", "HEAD"])
        .map(|out| git_stdout_trim(&out))
        .unwrap_or_else(|_| "?".to_string());
    lines.push(format!(
        "principal_sync: fast-forwarded {before_short} -> {after_short} from {source_remote_url}:{base_branch}"
    ));
    lines
}
