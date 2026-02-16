use std::fs::OpenOptions;
use std::io::Write as _;
use std::path::Path;
use std::time::Instant;

use anyhow::{Context, Result, anyhow};
use tracing::{info, info_span};

use forgejo_agent::orchd_dispatch_core::{DispatchEventKind, DispatchState};
use forgejo_agent::types::OrchdRuntimeState;

use super::cli::FinalizeDispatchArgs;
use super::db;
use super::forgejoctl_cmd;
use super::lexicon::{DIRECTIVE_IMPL, directive_uses_worktree};
use super::repo;
use super::telemetry::record_phase_latency_ms;

#[derive(Debug, Clone, Copy)]
enum ReportedStatus {
    Completed,
    TimedOut,
    FailedRuntime,
}

#[derive(Debug, Clone)]
struct TerminalStatusSpec {
    event_kind: DispatchEventKind,
    runtime_state: OrchdRuntimeState,
    state_literal: DispatchState,
    reason_code: String,
}

#[derive(Debug, Clone)]
enum LandingOutcome {
    NotRequired,
    Success(Vec<String>),
    Conflict(Vec<String>),
    Failure(Vec<String>),
}

fn parse_reported_status(status: &str) -> Result<ReportedStatus> {
    match status {
        "completed" => Ok(ReportedStatus::Completed),
        "timed_out" => Ok(ReportedStatus::TimedOut),
        "failed_runtime" | "stopped_no_final_answer" => Ok(ReportedStatus::FailedRuntime),
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

fn is_retryable_autoland_conflict(error_text: &str) -> bool {
    let lower = error_text.to_ascii_lowercase();
    [
        "non-fast-forward",
        "failed to push some refs",
        "fetch first",
        "not possible to fast-forward",
        "cannot fast-forward",
        "merge --ff-only",
    ]
    .iter()
    .any(|needle| lower.contains(needle))
}

fn evaluate_landing(args: &FinalizeDispatchArgs) -> LandingOutcome {
    if args.directive != DIRECTIVE_IMPL {
        return LandingOutcome::NotRequired;
    }

    let Some(principal_workdir) = args.principal_workdir.as_deref() else {
        return LandingOutcome::Failure(vec![
            "autoland failed: missing --principal-workdir for impl dispatch".to_string(),
        ]);
    };

    match repo::autoland_and_sync_principal(
        &args.db_path,
        &args.token_file,
        &args.git_workdir,
        principal_workdir,
        &args.git_remote,
        &args.git_base,
    ) {
        Ok(lines) => LandingOutcome::Success(lines),
        Err(err) => {
            let rendered = format!("{err:#}");
            let line = format!("autoland failed: {rendered}");
            if is_retryable_autoland_conflict(&rendered) {
                LandingOutcome::Conflict(vec![line])
            } else {
                LandingOutcome::Failure(vec![line])
            }
        }
    }
}

fn terminal_spec_from_outcome(
    reported: ReportedStatus,
    fallback_reason: &str,
    landing: &LandingOutcome,
) -> TerminalStatusSpec {
    match reported {
        ReportedStatus::TimedOut => TerminalStatusSpec {
            event_kind: DispatchEventKind::Timeout,
            runtime_state: OrchdRuntimeState::Failed,
            state_literal: DispatchState::TimedOut,
            reason_code: fallback_reason.to_string(),
        },
        ReportedStatus::FailedRuntime => TerminalStatusSpec {
            event_kind: DispatchEventKind::FailRuntime,
            runtime_state: OrchdRuntimeState::Failed,
            state_literal: DispatchState::FailedRuntime,
            reason_code: fallback_reason.to_string(),
        },
        ReportedStatus::Completed => match landing {
            LandingOutcome::NotRequired | LandingOutcome::Success(_) => TerminalStatusSpec {
                event_kind: DispatchEventKind::Complete,
                runtime_state: OrchdRuntimeState::Completed,
                state_literal: DispatchState::Completed,
                reason_code: fallback_reason.to_string(),
            },
            LandingOutcome::Conflict(_) => TerminalStatusSpec {
                event_kind: DispatchEventKind::Block,
                runtime_state: OrchdRuntimeState::Blocked,
                state_literal: DispatchState::Blocked,
                reason_code: "autoland_conflict_retry_requested".to_string(),
            },
            LandingOutcome::Failure(_) => TerminalStatusSpec {
                event_kind: DispatchEventKind::FailRuntime,
                runtime_state: OrchdRuntimeState::Failed,
                state_literal: DispatchState::FailedRuntime,
                reason_code: "autoland_failed".to_string(),
            },
        },
    }
}

fn maybe_post_conflict_retry_comment(
    args: &FinalizeDispatchArgs,
    conflict: &[String],
) -> Result<()> {
    if conflict.is_empty() {
        return Ok(());
    }

    let dedupe_key = format!("dispatch:{}:autoland_conflict_retry", args.dispatch_id);
    let should_send =
        db::record_notification_delivery(&args.db_path, &dedupe_key, "autoland_conflict_retry")?;
    if !should_send {
        return Ok(());
    }

    let mention_line = if args.role_name.starts_with("codex") {
        format!("\n\n@{} impl", args.role_name)
    } else {
        String::new()
    };

    let details = conflict.join("\n");
    let body = format!(
        "Autoland could not fast-forward because `main` changed while this run was in flight. \
Please rebase on the latest `main` and continue.\n\n{details}{mention_line}"
    );

    forgejoctl_cmd::run_forgejoctl(
        &args.forgejoctl_bin,
        args.forgejo_config.as_deref(),
        &args.token_file,
        &[
            "issue",
            "comment",
            &args.issue_ref.to_string(),
            "--body",
            &body,
        ],
    )
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

    let reported_status = parse_reported_status(&args.status)?;
    let landing_outcome = if matches!(reported_status, ReportedStatus::Completed) {
        evaluate_landing(&args)
    } else {
        LandingOutcome::NotRequired
    };
    let status_spec =
        terminal_spec_from_outcome(reported_status, &args.reason_code, &landing_outcome);

    let phase_update_start = Instant::now();
    let did_transition = db::update_dispatch_terminal(
        &args.db_path,
        args.dispatch_id,
        status_spec.event_kind,
        &status_spec.reason_code,
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

    let landing_lines = match &landing_outcome {
        LandingOutcome::Success(lines)
        | LandingOutcome::Conflict(lines)
        | LandingOutcome::Failure(lines) => lines.as_slice(),
        LandingOutcome::NotRequired => &[],
    };
    if let Err(err) = append_completion_section(&args.completion_file, "Landing", landing_lines) {
        eprintln!("finalize-dispatch: failed appending landing info: {err}");
    }

    if let LandingOutcome::Conflict(conflict_lines) = &landing_outcome
        && let Err(err) = maybe_post_conflict_retry_comment(&args, conflict_lines)
    {
        eprintln!("finalize-dispatch: conflict retry comment failed: {err}");
    }

    let work_state_target = directive_uses_worktree(args.directive.as_str()).then_some(
        if status_spec.state_literal == DispatchState::Completed {
            "review"
        } else {
            "blocked"
        },
    );

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
    let issue_ref = args.issue_ref.to_string();
    let orchd_state_result = if status_spec.runtime_state == OrchdRuntimeState::Failed
        && !status_spec.reason_code.trim().is_empty()
    {
        forgejoctl_cmd::run_forgejoctl(
            &args.forgejoctl_bin,
            args.forgejo_config.as_deref(),
            &args.token_file,
            &[
                "issue",
                "orchd-state",
                &issue_ref,
                "--to",
                status_spec.runtime_state.as_str(),
                "--reason-code",
                status_spec.reason_code.as_str(),
            ],
        )
    } else {
        forgejoctl_cmd::run_forgejoctl(
            &args.forgejoctl_bin,
            args.forgejo_config.as_deref(),
            &args.token_file,
            &[
                "issue",
                "orchd-state",
                &issue_ref,
                "--to",
                status_spec.runtime_state.as_str(),
            ],
        )
    };
    if let Err(err) = orchd_state_result {
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

    info!("finalize dispatch completed");
    Ok(())
}
