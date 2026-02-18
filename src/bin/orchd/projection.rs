use std::path::PathBuf;
use std::process::{Command, Stdio};

use anyhow::{Context, Result, anyhow};
use serde_json::json;

use forgejo_agent::api::ForgejoClient;
use forgejo_agent::policy::{
    is_orchd_failure_label, is_orchd_reason_label, orchd_failure_label, orchd_reason_label,
};
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
    let token_file = if let Some(control_plane) = dispatch_config.control_plane.as_ref() {
        control_plane.token_file.clone()
    } else {
        let role_name = decision.target_role.as_deref()?;
        let role = dispatch_config.roles.get(role_name)?;
        role.token_file.clone()
    };
    Some(CommentIdentity {
        forgejoctl_bin: dispatch_config.forgejoctl_bin.clone(),
        config_file: state.forgejo_config_file.clone(),
        token_file,
    })
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
    runtime_reason_code: Option<&str>,
    identity: Option<CommentIdentity>,
) -> Result<()> {
    match project_issue_runtime_state_with_api(
        state.clone(),
        repo_full_name,
        issue_number,
        runtime_state,
        runtime_reason_code,
    )
    .await
    {
        Ok(()) => Ok(()),
        Err(api_err) => {
            if let Some(identity) = identity {
                log_line(
                    "runtime_state_projection_api_fallback",
                    json!({
                        "repo": repo_full_name,
                        "issue_number": issue_number,
                        "runtime_state": runtime_state.as_str(),
                        "runtime_reason_code": runtime_reason_code,
                        "error": api_err.to_string(),
                    }),
                );
                project_issue_runtime_state_as_role(
                    repo_full_name,
                    issue_number,
                    runtime_state,
                    runtime_reason_code,
                    identity,
                )
                .await
            } else {
                Err(api_err)
            }
        }
    }
}

async fn project_issue_runtime_state_as_role(
    repo_full_name: &str,
    issue_number: u64,
    runtime_state: OrchdRuntimeState,
    runtime_reason_code: Option<&str>,
    identity: CommentIdentity,
) -> Result<()> {
    let issue_ref = format!("{repo_full_name}#{issue_number}");
    let runtime_state_name = runtime_state.as_str().to_string();
    let runtime_reason_code = runtime_reason_code
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(ToOwned::to_owned);
    tokio::task::spawn_blocking(move || -> Result<()> {
        let mut cmd = Command::new(&identity.forgejoctl_bin);
        if let Some(config_file) = identity.config_file.as_ref() {
            cmd.arg("--config").arg(config_file);
        }
        let mut args = vec![
            "--token-file".to_string(),
            identity.token_file.to_string_lossy().into_owned(),
            "issue".to_string(),
            "orchd-state".to_string(),
            issue_ref.clone(),
            "--to".to_string(),
            runtime_state_name.clone(),
        ];
        if let Some(reason_code) = runtime_reason_code.as_deref() {
            args.push("--reason-code".to_string());
            args.push(reason_code.to_string());
        }
        let output = cmd
            .args(&args)
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
    runtime_reason_code: Option<&str>,
) -> Result<()> {
    let repo_full_name = repo_full_name.to_string();
    let runtime_reason_code = runtime_reason_code
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(ToOwned::to_owned);
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
        let reason_label_id = match runtime_state {
            OrchdRuntimeState::Failed => runtime_reason_code
                .as_deref()
                .and_then(orchd_failure_label)
                .map(|reason_label_name| {
                    api.ensure_label(
                        &state.cfg,
                        &issue.repo,
                        &reason_label_name,
                        "8a1c2d",
                        "dispatch failed for this reason",
                        false,
                    )
                    .map(|label| label.id)
                })
                .transpose()?,
            OrchdRuntimeState::Completed => None,
            _ => runtime_reason_code
                .as_deref()
                .and_then(orchd_reason_label)
                .map(|reason_label_name| {
                    api.ensure_label(
                        &state.cfg,
                        &issue.repo,
                        &reason_label_name,
                        "fbca04",
                        "orchd dispatch status reason code",
                        false,
                    )
                    .map(|label| label.id)
                })
                .transpose()?,
        };

        let mut replacement_ids = existing
            .labels
            .iter()
            .filter(|label| {
                !is_orchd_state_label(&label.name)
                    && !is_orchd_failure_label(&label.name)
                    && !is_orchd_reason_label(&label.name)
            })
            .map(|label| label.id)
            .collect::<Vec<_>>();
        replacement_ids.push(target_id);
        if let Some(reason_label_id) = reason_label_id {
            replacement_ids.push(reason_label_id);
        }
        replacement_ids.sort_unstable();
        replacement_ids.dedup();
        let _ = api.replace_issue_label_ids(&state.cfg, &issue, replacement_ids)?;
        Ok(())
    })
    .await
    .context("runtime state task join failure")??;
    Ok(())
}
