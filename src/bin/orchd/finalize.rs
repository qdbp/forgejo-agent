use std::fs::OpenOptions;
use std::io::Write as _;
use std::path::Path;
use std::time::Instant;

use anyhow::{Context, Result, anyhow};
use tracing::{info, info_span};

use forgejo_agent::api::ForgejoClient;
use forgejo_agent::config::AgentConfig;
use forgejo_agent::orchd_dispatch_core::{DispatchEventKind, DispatchState};
use forgejo_agent::types::OrchdRuntimeState;

use super::cli::FinalizeDispatchArgs;
use super::db;
use super::forgejoctl_cmd;
use super::repo;
use super::telemetry::record_phase_latency_ms;

#[derive(Debug, Clone, Copy)]
struct TerminalStatusSpec {
    event_kind: DispatchEventKind,
    runtime_state: OrchdRuntimeState,
    state_literal: DispatchState,
}

fn parse_terminal_status_spec(status: &str) -> Result<TerminalStatusSpec> {
    match status {
        "completed" => Ok(TerminalStatusSpec {
            event_kind: DispatchEventKind::Complete,
            runtime_state: OrchdRuntimeState::Completed,
            state_literal: DispatchState::Completed,
        }),
        "timed_out" => Ok(TerminalStatusSpec {
            event_kind: DispatchEventKind::Timeout,
            runtime_state: OrchdRuntimeState::Failed,
            state_literal: DispatchState::TimedOut,
        }),
        "failed_runtime" | "stopped_no_final_answer" => Ok(TerminalStatusSpec {
            event_kind: DispatchEventKind::FailRuntime,
            runtime_state: OrchdRuntimeState::Failed,
            state_literal: DispatchState::FailedRuntime,
        }),
        other => Err(anyhow!("unsupported finalize status '{other}'")),
    }
}

fn append_completion_section(completion_file: &Path, header: &str, lines: &[String]) -> Result<()> {
    if lines.is_empty() {
        return Ok(());
    }
    let mut file = OpenOptions::new()
        .append(true)
        .open(completion_file)
        .with_context(|| {
            format!(
                "failed opening completion file for append: {}",
                completion_file.display()
            )
        })?;
    writeln!(file)?;
    writeln!(file, "---")?;
    writeln!(file, "{header}:")?;
    for line in lines {
        writeln!(file, "- {line}")?;
    }
    Ok(())
}

fn create_pull_request_for_dispatch(args: &FinalizeDispatchArgs) -> Result<String> {
    let forgejo_config = args
        .forgejo_config
        .clone()
        .ok_or_else(|| anyhow!("missing --forgejo-config; cannot create pull request"))?;
    let cfg = AgentConfig::load(Some(forgejo_config), Some(args.token_file.clone()))?;
    let api = ForgejoClient::new(&cfg)?;

    let repo = &args.issue_ref.repo;
    let head_branch = args.git_branch.trim();
    let base_branch = args.git_base.trim();
    if head_branch.is_empty() {
        return Err(anyhow!("missing git branch; cannot create pull request"));
    }
    if base_branch.is_empty() {
        return Err(anyhow!("missing base branch; cannot create pull request"));
    }

    let body = format!("Refs: {}\n\nIssue: {}\n", args.issue_url, args.issue_ref);
    let try_heads = [
        head_branch.to_string(),
        format!("{}:{head_branch}", repo.owner),
    ];

    let mut last_err: Option<anyhow::Error> = None;
    for head in &try_heads {
        match api.create_pull_request(&cfg, repo, &args.issue_title, head, base_branch, &body) {
            Ok(value) => {
                let url = value
                    .get("html_url")
                    .and_then(serde_json::Value::as_str)
                    .or_else(|| value.get("url").and_then(serde_json::Value::as_str))
                    .unwrap_or("")
                    .to_string();
                if url.is_empty() {
                    return Ok("(pull request created; URL missing in response)".to_string());
                }
                return Ok(url);
            }
            Err(err) => last_err = Some(err),
        }
    }
    Err(last_err.unwrap_or_else(|| anyhow!("pull request creation failed")))
}

pub(super) fn finalize_dispatch_command(args: FinalizeDispatchArgs) -> Result<()> {
    let span = info_span!(
        "finalize_dispatch",
        dispatch_id = args.dispatch_id,
        issue = %args.issue_ref,
        directive = %args.directive,
        role = %args.role_name,
        status = %args.status,
    );
    let _entered = span.enter();
    let status_spec = parse_terminal_status_spec(&args.status)?;
    let phase_update_start = Instant::now();
    let did_transition = db::update_dispatch_terminal(
        &args.db_path,
        args.dispatch_id,
        status_spec.event_kind,
        &args.reason_code,
        args.exit_code,
        Some(&args.session_id),
    )?;
    record_phase_latency_ms(
        "finalize_update_db",
        phase_update_start.elapsed().as_secs_f64() * 1000.0,
        "ok",
    );
    if !did_transition {
        info!("finalize-dispatch: no-op (dispatch already terminal or missing)");
        return Ok(());
    }

    let mut landing_ok = true;
    let mut landing_lines: Vec<String> = Vec::new();
    if status_spec.state_literal == DispatchState::Completed {
        match args.directive.as_str() {
            "impl" => match repo::autoland_to_main(
                &args.db_path,
                &args.token_file,
                &args.git_workdir,
                &args.git_remote,
                &args.git_base,
            ) {
                Ok(line) => landing_lines.push(line),
                Err(err) => {
                    landing_ok = false;
                    landing_lines.push(format!("autoland failed: {err:#}"));
                }
            },
            "pr" => {
                if args.git_branch.trim().is_empty() {
                    landing_ok = false;
                    landing_lines.push("missing git branch; cannot create PR".to_string());
                } else {
                    match repo::push_branch(
                        &args.db_path,
                        &args.token_file,
                        &args.git_workdir,
                        &args.git_remote,
                        &args.git_branch,
                    ) {
                        Ok(line) => landing_lines.push(line),
                        Err(err) => {
                            landing_ok = false;
                            landing_lines.push(format!("push failed: {err:#}"));
                        }
                    }
                    if landing_ok {
                        let pr_url = create_pull_request_for_dispatch(&args);
                        match pr_url {
                            Ok(url) => landing_lines.push(format!("pull request: {url}")),
                            Err(err) => {
                                landing_ok = false;
                                landing_lines.push(format!("PR create failed: {err:#}"));
                            }
                        }
                    }
                }
            }
            _ => {}
        }
    } else if matches!(args.directive.as_str(), "impl" | "pr") {
        landing_ok = false;
    }

    if let Err(err) = append_completion_section(&args.completion_file, "Landing", &landing_lines) {
        eprintln!("finalize-dispatch: failed appending landing info: {err}");
    }

    let work_state_target = match args.directive.as_str() {
        "impl" | "pr" => Some(
            if status_spec.state_literal == DispatchState::Completed && landing_ok {
                "review"
            } else {
                "blocked"
            },
        ),
        _ => None,
    };

    if let Some(work_state_target) = work_state_target {
        let phase_transition_start = Instant::now();
        if let Err(err) = forgejoctl_cmd::run_forgejoctl(
            &args.forgejoctl_bin,
            args.forgejo_config.as_deref(),
            &args.token_file,
            &[
                "issue",
                "transition",
                &args.issue_ref.to_string(),
                "--to",
                work_state_target,
                "--force",
            ],
        ) {
            eprintln!("finalize-dispatch: work-state transition failed: {err}");
            record_phase_latency_ms(
                "finalize_transition",
                phase_transition_start.elapsed().as_secs_f64() * 1000.0,
                "error",
            );
        } else {
            record_phase_latency_ms(
                "finalize_transition",
                phase_transition_start.elapsed().as_secs_f64() * 1000.0,
                "ok",
            );
        }
    }

    let phase_state_start = Instant::now();
    if let Err(err) = forgejoctl_cmd::run_forgejoctl(
        &args.forgejoctl_bin,
        args.forgejo_config.as_deref(),
        &args.token_file,
        &[
            "issue",
            "orchd-state",
            &args.issue_ref.to_string(),
            "--to",
            status_spec.runtime_state.as_str(),
        ],
    ) {
        eprintln!("finalize-dispatch: orchd-state projection failed: {err}");
        record_phase_latency_ms(
            "finalize_orchd_state",
            phase_state_start.elapsed().as_secs_f64() * 1000.0,
            "error",
        );
    } else {
        record_phase_latency_ms(
            "finalize_orchd_state",
            phase_state_start.elapsed().as_secs_f64() * 1000.0,
            "ok",
        );
    }

    let phase_comment_start = Instant::now();
    if let Err(err) = forgejoctl_cmd::run_forgejoctl(
        &args.forgejoctl_bin,
        args.forgejo_config.as_deref(),
        &args.token_file,
        &[
            "issue",
            "comment",
            &args.issue_ref.to_string(),
            "--body-file",
            &args.completion_file.to_string_lossy(),
        ],
    ) {
        eprintln!("finalize-dispatch: issue comment post failed: {err}");
        record_phase_latency_ms(
            "finalize_comment",
            phase_comment_start.elapsed().as_secs_f64() * 1000.0,
            "error",
        );
    } else {
        record_phase_latency_ms(
            "finalize_comment",
            phase_comment_start.elapsed().as_secs_f64() * 1000.0,
            "ok",
        );
    }
    info!("finalize dispatch completed");
    Ok(())
}
