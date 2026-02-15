use std::path::PathBuf;
use std::process::{Command, Stdio};

use anyhow::{Context, Result, anyhow};
use serde_json::json;

use forgejo_agent::api::ForgejoClient;
use forgejo_agent::types::{IssueRef, OrchdRuntimeState, RepoRef};

use super::state::{AppState, DecisionRecord};
use super::telemetry::log_line;

#[derive(Clone)]
pub(super) struct CommentIdentity {
    pub(super) forgejoctl_bin: PathBuf,
    pub(super) config_file: Option<PathBuf>,
    pub(super) token_file: PathBuf,
}

pub(super) fn dispatch_comment_identity(
    state: &AppState,
    decision: &DecisionRecord,
) -> Option<CommentIdentity> {
    let dispatch_config = state.dispatch_config.as_ref()?;
    let role_name = decision.target_role.as_deref()?;
    let role = dispatch_config.roles.get(role_name)?;
    Some(CommentIdentity {
        forgejoctl_bin: dispatch_config.forgejoctl_bin.clone(),
        config_file: state.forgejo_config_file.clone(),
        token_file: role.token_file.clone(),
    })
}

pub(super) async fn post_issue_comment(
    state: AppState,
    repo_full_name: &str,
    issue_number: u64,
    body: String,
) -> Result<()> {
    let repo = RepoRef::parse(repo_full_name)?;
    let issue = IssueRef {
        repo,
        number: issue_number,
    };
    tokio::task::spawn_blocking(move || -> Result<()> {
        let api = ForgejoClient::new(&state.cfg)?;
        api.comment_issue(&state.cfg, &issue, &body)
    })
    .await
    .context("comment task join failure")??;
    Ok(())
}

const fn orchd_runtime_label_meta(state: OrchdRuntimeState) -> (&'static str, &'static str, bool) {
    match state {
        OrchdRuntimeState::Queued => ("d4c5f9", "dispatch accepted and queued", true),
        OrchdRuntimeState::Running => ("1d76db", "dispatch currently running", true),
        OrchdRuntimeState::Blocked => (
            "d73a4a",
            "dispatch blocked on a dependency or operator decision",
            true,
        ),
        OrchdRuntimeState::Failed => ("b60205", "dispatch failed", true),
        OrchdRuntimeState::Completed => ("0e8a16", "dispatch completed successfully", true),
    }
}

fn is_orchd_state_label(label: &str) -> bool {
    OrchdRuntimeState::from_label(label).is_some()
}

pub(super) async fn project_issue_runtime_state(
    state: AppState,
    repo_full_name: &str,
    issue_number: u64,
    runtime_state: OrchdRuntimeState,
    identity: Option<CommentIdentity>,
) -> Result<()> {
    if let Some(identity) = identity {
        match project_issue_runtime_state_as_role(
            repo_full_name,
            issue_number,
            runtime_state,
            identity,
        )
        .await
        {
            Ok(()) => {
                return Ok(());
            }
            Err(role_err) => {
                log_line(
                    "runtime_state_projection_role_fallback",
                    json!({
                        "repo": repo_full_name,
                        "issue_number": issue_number,
                        "runtime_state": runtime_state.as_str(),
                        "error": role_err.to_string(),
                    }),
                );
            }
        }
    }
    project_issue_runtime_state_with_api(state, repo_full_name, issue_number, runtime_state).await
}

async fn project_issue_runtime_state_as_role(
    repo_full_name: &str,
    issue_number: u64,
    runtime_state: OrchdRuntimeState,
    identity: CommentIdentity,
) -> Result<()> {
    let issue_ref = format!("{repo_full_name}#{issue_number}");
    let runtime_state_name = runtime_state.as_str().to_string();
    tokio::task::spawn_blocking(move || -> Result<()> {
        let mut cmd = Command::new(&identity.forgejoctl_bin);
        if let Some(config_file) = identity.config_file.as_ref() {
            cmd.arg("--config").arg(config_file);
        }
        let output = cmd
            .args(["--token-file", &identity.token_file.to_string_lossy()])
            .args([
                "issue",
                "orchd-state",
                &issue_ref,
                "--to",
                &runtime_state_name,
            ])
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .output()
            .with_context(|| {
                format!(
                    "failed to spawn forgejoctl orchd-state command: {}",
                    identity.forgejoctl_bin.display()
                )
            })?;
        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            return Err(anyhow!(
                "forgejoctl orchd-state failed for {issue_ref}: {}",
                stderr.trim()
            ));
        }
        Ok(())
    })
    .await
    .context("runtime state task join failure")??;
    Ok(())
}

async fn project_issue_runtime_state_with_api(
    state: AppState,
    repo_full_name: &str,
    issue_number: u64,
    runtime_state: OrchdRuntimeState,
) -> Result<()> {
    let repo_full_name = repo_full_name.to_string();
    tokio::task::spawn_blocking(move || -> Result<()> {
        let api = ForgejoClient::new(&state.cfg)?;
        let repo = RepoRef::parse(&repo_full_name)?;
        let issue = IssueRef {
            repo,
            number: issue_number,
        };
        let existing = api.get_issue(&state.cfg, &issue)?;
        let (color, description, exclusive) = orchd_runtime_label_meta(runtime_state);
        let target_id = api
            .ensure_label(
                &state.cfg,
                &issue.repo,
                runtime_state.label(),
                color,
                description,
                exclusive,
            )?
            .id;

        let mut replacement_ids = existing
            .labels
            .iter()
            .filter(|label| !is_orchd_state_label(&label.name))
            .map(|label| label.id)
            .collect::<Vec<_>>();
        replacement_ids.push(target_id);
        replacement_ids.sort_unstable();
        replacement_ids.dedup();
        let _ = api.replace_issue_label_ids(&state.cfg, &issue, replacement_ids)?;
        Ok(())
    })
    .await
    .context("runtime state task join failure")??;
    Ok(())
}
