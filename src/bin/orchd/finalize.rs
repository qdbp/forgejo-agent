use std::fs::OpenOptions;
use std::io::Write as _;
use std::path::Path;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, anyhow};
use tracing::{info, info_span};

use forgejo_agent::api::{ApiHttpError, ForgejoClient, MergePullMethod};
use forgejo_agent::config::AgentConfig;

use forgejo_agent::orchd_dispatch_core::{DispatchEventKind, DispatchState};
use forgejo_agent::types::OrchdRuntimeState;
use forgejo_agent::types::RepoRef;

use super::cli::FinalizeDispatchArgs;
use super::db;
use super::forgejoctl_cmd;
use super::inquisition::{InquisitionSpec, maybe_spawn_inquisition};
use super::lexicon::{DIRECTIVE_AUDIT, DIRECTIVE_IMPL, directive_uses_worktree};
use super::repo;
use super::telemetry::record_phase_latency_ms;
use super::template;

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
    Blocked {
        reason_code: String,
        lines: Vec<String>,
        comment: Option<LandingCommentSpec>,
    },
    Failure {
        reason_code: String,
        lines: Vec<String>,
    },
}

#[derive(Debug, Clone)]
struct LandingCommentSpec {
    delivery_kind: &'static str,
    dedupe_key: String,
    body: String,
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

fn whoami_login(api: &ForgejoClient, cfg: &AgentConfig) -> Result<String> {
    let value = api.whoami(cfg).context("whoami failed")?;
    let login = value
        .get("login")
        .and_then(|value| value.as_str())
        .ok_or_else(|| anyhow!("whoami missing login field"))?;
    Ok(login.to_string())
}

fn merge_ff_only_with_retry(
    api: &ForgejoClient,
    cfg: &AgentConfig,
    repo_ref: &RepoRef,
    pr_number: u64,
    head_sha: &str,
    lines: &mut Vec<String>,
) -> Result<()> {
    const MAX_ATTEMPTS: u32 = 3;
    const SLEEP_SEC: u64 = 10;

    for attempt in 1..=MAX_ATTEMPTS {
        let merge_attempt = api.merge_pull_request(
            cfg,
            repo_ref,
            pr_number,
            MergePullMethod::FastForwardOnly,
            Some(head_sha),
            true,
        );
        match merge_attempt {
            Ok(()) => return Ok(()),
            Err(err) => {
                let Some(http) = err.downcast_ref::<ApiHttpError>() else {
                    return Err(err);
                };
                if http.status >= 500 && attempt < MAX_ATTEMPTS {
                    lines.push(format!(
                        "pr_merge_retry: transient server error status={} attempt={}/{}",
                        http.status, attempt, MAX_ATTEMPTS
                    ));
                    std::thread::sleep(Duration::from_secs(SLEEP_SEC));
                    continue;
                }
                return Err(err);
            }
        }
    }
    Ok(())
}

fn evaluate_landing(args: &FinalizeDispatchArgs) -> LandingOutcome {
    if args.directive != DIRECTIVE_IMPL {
        return LandingOutcome::NotRequired;
    }

    let branch = args.git_branch.trim();
    if branch.is_empty() {
        return LandingOutcome::Failure {
            reason_code: "landing_missing_branch".to_string(),
            lines: vec!["pr landing failed: missing git branch name".to_string()],
        };
    }

    let cfg = match AgentConfig::load(args.forgejo_config.clone(), Some(args.token_file.clone())) {
        Ok(cfg) => cfg,
        Err(err) => {
            return LandingOutcome::Failure {
                reason_code: "landing_config_error".to_string(),
                lines: vec![format!("pr landing failed: {err:#}")],
            };
        }
    };
    let api = match ForgejoClient::new(&cfg) {
        Ok(api) => api,
        Err(err) => {
            return LandingOutcome::Failure {
                reason_code: "landing_api_client_error".to_string(),
                lines: vec![format!("pr landing failed: {err:#}")],
            };
        }
    };

    let repo_ref = args.issue_ref.repo.clone();
    let repo_full_name = repo_ref.to_string();

    let prs = match api.list_pull_requests(&cfg, &repo_ref, "all", 200) {
        Ok(prs) => prs,
        Err(err) => {
            return LandingOutcome::Failure {
                reason_code: "landing_pr_list_failed".to_string(),
                lines: vec![format!("pr landing failed: {err:#}")],
            };
        }
    };
    let existing_merged_pr = prs
        .iter()
        .find(|pr| pr.head.ref_name == branch && pr.merged)
        .cloned();

    let login = match whoami_login(&api, &cfg) {
        Ok(login) => login,
        Err(err) => {
            return LandingOutcome::Failure {
                reason_code: "landing_whoami_failed".to_string(),
                lines: vec![format!("pr landing failed: {err:#}")],
            };
        }
    };
    let git_url = match repo::forgejo_http_git_url(&cfg.base_url, &login, &repo_full_name) {
        Ok(url) => url,
        Err(err) => {
            return LandingOutcome::Failure {
                reason_code: "landing_git_url_failed".to_string(),
                lines: vec![format!("pr landing failed: {err}")],
            };
        }
    };

    let mut lines = Vec::new();
    if let Some(pr) = existing_merged_pr {
        lines.push(format!(
            "pr landing: already merged #{} ({})",
            pr.number, pr.html_url
        ));
        if let Some(principal_workdir) = args.principal_workdir.as_deref() {
            lines.extend(repo::best_effort_sync_principal(
                &args.db_path,
                &args.token_file,
                principal_workdir,
                &args.git_base,
                &git_url,
            ));
        }
        return LandingOutcome::Success(lines);
    }

    let head_sha = match repo::git_checked(&args.git_workdir, &["rev-parse", "HEAD"])
        .map(|out| repo::git_stdout_trim(&out))
    {
        Ok(value) => value,
        Err(err) => {
            return LandingOutcome::Failure {
                reason_code: "landing_git_head_failed".to_string(),
                lines: vec![format!("pr landing failed: {err:#}")],
            };
        }
    };
    let head_short = match repo::git_checked(&args.git_workdir, &["rev-parse", "--short", "HEAD"])
        .map(|out| repo::git_stdout_trim(&out))
    {
        Ok(value) => value,
        Err(_) => head_sha.chars().take(12).collect::<String>(),
    };
    if let Err(err) = repo::git_checked_with_token(
        &args.db_path,
        &args.token_file,
        Some(&args.git_workdir),
        &["push", &git_url, &format!("HEAD:{branch}")],
    ) {
        return LandingOutcome::Failure {
            reason_code: "landing_git_push_failed".to_string(),
            lines: vec![format!("pr landing failed: {err}")],
        };
    }
    lines.push(format!("git_push: {head_short} -> {branch} ({git_url})"));

    let prs = match api.list_pull_requests(&cfg, &repo_ref, "all", 200) {
        Ok(prs) => prs,
        Err(err) => {
            return LandingOutcome::Failure {
                reason_code: "landing_pr_list_failed".to_string(),
                lines: vec![format!("pr landing failed: {err:#}")],
            };
        }
    };
    let pr = if let Some(existing) = prs.into_iter().find(|pr| pr.head.ref_name == branch) {
        existing
    } else {
        let title = format!("{}: {}", args.issue_ref, args.issue_title);
        let body = format!("Automated PR for {}.", args.issue_ref);
        match api.create_pull_request(&cfg, &repo_ref, &title, branch, &args.git_base, &body) {
            Ok(pr) => {
                lines.push(format!("pr_create: #{} ({})", pr.number, pr.html_url));
                pr
            }
            Err(err) => {
                return LandingOutcome::Failure {
                    reason_code: "landing_pr_create_failed".to_string(),
                    lines: vec![format!("pr landing failed: {err:#}")],
                };
            }
        }
    };

    match merge_ff_only_with_retry(&api, &cfg, &repo_ref, pr.number, &head_sha, &mut lines) {
        Ok(()) => {
            lines.push(format!(
                "pr_merge: ff-only #{} ({})",
                pr.number, pr.html_url
            ));
            if let Some(principal_workdir) = args.principal_workdir.as_deref() {
                lines.extend(repo::best_effort_sync_principal(
                    &args.db_path,
                    &args.token_file,
                    principal_workdir,
                    &args.git_base,
                    &git_url,
                ));
            }
            LandingOutcome::Success(lines)
        }
        Err(err) => {
            let Some(http) = err.downcast_ref::<ApiHttpError>() else {
                return LandingOutcome::Failure {
                    reason_code: "landing_pr_merge_failed".to_string(),
                    lines: vec![format!("pr landing failed: {err:#}")],
                };
            };
            // Forgejo documents 409/423 here, but we have observed intermittent 5xx from the merge
            // endpoint in cases that are still recoverable via "fetch+rebase+retry".
            if !(matches!(http.status, 409 | 423) || http.status >= 500) {
                return LandingOutcome::Failure {
                    reason_code: "landing_pr_merge_failed".to_string(),
                    lines: vec![format!("pr landing failed: {http}")],
                };
            }

            lines.push(format!(
                "pr_merge_retry: status={} attempting rebase",
                http.status
            ));
            let fetch_result = repo::git_checked_with_token(
                &args.db_path,
                &args.token_file,
                Some(&args.git_workdir),
                &["fetch", &git_url, &args.git_base],
            );
            if let Err(err) = fetch_result {
                return LandingOutcome::Failure {
                    reason_code: "landing_git_fetch_failed".to_string(),
                    lines: vec![format!("pr landing failed: {err}")],
                };
            }

            let rebase_result = repo::git_checked(&args.git_workdir, &["rebase", "FETCH_HEAD"]);
            if let Err(err) = rebase_result {
                let _ = repo::git_checked(&args.git_workdir, &["rebase", "--abort"]);
                let retry_mention = if args.role_name.starts_with("codex") {
                    format!("@{} impl", args.role_name)
                } else {
                    String::new()
                };
                let template_path = args
                    .git_workdir
                    .join("templates/orchd-landing-pr-rebase-conflict.md");
                let rendered = template::render_prompt_file(
                    &template_path,
                    &[
                        ("pr_url", pr.html_url.as_str()),
                        ("branch", branch),
                        ("base_branch", args.git_base.as_str()),
                        ("retry_mention", retry_mention.as_str()),
                        ("error", &format!("{err:#}")),
                    ],
                    "pr rebase conflict",
                )
                .unwrap_or_else(|tpl_err| {
                    format!(
                        "PR landing blocked (rebase conflicts).\n- PR: {}\n- branch: `{}` (base `{}`)\n\nretry: {}\n\nerror: {err:#}\n(template error: {tpl_err})\n",
                        pr.html_url, branch, args.git_base, retry_mention
                    )
                });

                return LandingOutcome::Blocked {
                    reason_code: "pr_rebase_conflict".to_string(),
                    lines: vec![format!("pr landing blocked: rebase conflict: {err:#}")],
                    comment: Some(LandingCommentSpec {
                        delivery_kind: "pr_rebase_conflict",
                        dedupe_key: format!("dispatch:{}:pr_rebase_conflict", args.dispatch_id),
                        body: rendered,
                    }),
                };
            }

            let rebased_sha = match repo::git_checked(&args.git_workdir, &["rev-parse", "HEAD"])
                .map(|out| repo::git_stdout_trim(&out))
            {
                Ok(value) if !value.trim().is_empty() => value,
                Ok(_) => {
                    return LandingOutcome::Failure {
                        reason_code: "landing_git_head_failed".to_string(),
                        lines: vec!["pr landing failed: rebased head sha was empty".to_string()],
                    };
                }
                Err(err) => {
                    return LandingOutcome::Failure {
                        reason_code: "landing_git_head_failed".to_string(),
                        lines: vec![format!("pr landing failed: {err:#}")],
                    };
                }
            };
            let rebased_short =
                repo::git_checked(&args.git_workdir, &["rev-parse", "--short", "HEAD"])
                    .map(|out| repo::git_stdout_trim(&out))
                    .unwrap_or_else(|_| rebased_sha.chars().take(12).collect::<String>());

            let push_result = repo::git_checked_with_token(
                &args.db_path,
                &args.token_file,
                Some(&args.git_workdir),
                &[
                    "push",
                    "--force-with-lease",
                    &git_url,
                    &format!("HEAD:{branch}"),
                ],
            );
            if let Err(err) = push_result {
                return LandingOutcome::Failure {
                    reason_code: "landing_git_push_failed".to_string(),
                    lines: vec![format!("pr landing failed: {err}")],
                };
            }
            lines.push(format!("git_rebase: now at {rebased_short}"));
            let merge_retry = api.merge_pull_request(
                &cfg,
                &repo_ref,
                pr.number,
                MergePullMethod::FastForwardOnly,
                Some(&rebased_sha),
                true,
            );
            match merge_retry {
                Ok(()) => {
                    lines.push(format!(
                        "pr_merge: ff-only #{} ({})",
                        pr.number, pr.html_url
                    ));
                    if let Some(principal_workdir) = args.principal_workdir.as_deref() {
                        lines.extend(repo::best_effort_sync_principal(
                            &args.db_path,
                            &args.token_file,
                            principal_workdir,
                            &args.git_base,
                            &git_url,
                        ));
                    }
                    LandingOutcome::Success(lines)
                }
                Err(err) => {
                    let retry_mention = if args.role_name.starts_with("codex") {
                        format!("@{} impl", args.role_name)
                    } else {
                        String::new()
                    };
                    let template_path = args
                        .git_workdir
                        .join("templates/orchd-landing-pr-merge-blocked.md");
                    let body = template::render_prompt_file(
                        &template_path,
                        &[
                            ("pr_url", pr.html_url.as_str()),
                            ("branch", branch),
                            ("base_branch", args.git_base.as_str()),
                            ("retry_mention", retry_mention.as_str()),
                            ("error", &format!("{err:#}")),
                        ],
                        "pr merge blocked",
                    )
                    .unwrap_or_else(|tpl_err| {
                        format!(
                            "PR landing blocked: merge still not possible after rebase.\n- PR: {}\n- branch: `{}` (base `{}`)\n\nretry: {}\n\nerror: {err:#}\n(template error: {tpl_err})\n",
                            pr.html_url, branch, args.git_base, retry_mention
                        )
                    });
                    LandingOutcome::Blocked {
                        reason_code: "pr_merge_blocked".to_string(),
                        lines: vec![format!("pr landing blocked: merge failed: {err:#}")],
                        comment: Some(LandingCommentSpec {
                            delivery_kind: "pr_merge_blocked",
                            dedupe_key: format!("dispatch:{}:pr_merge_blocked", args.dispatch_id),
                            body,
                        }),
                    }
                }
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
            LandingOutcome::Blocked { reason_code, .. } => TerminalStatusSpec {
                event_kind: DispatchEventKind::Block,
                runtime_state: OrchdRuntimeState::Blocked,
                state_literal: DispatchState::Blocked,
                reason_code: reason_code.clone(),
            },
            LandingOutcome::Failure { reason_code, .. } => TerminalStatusSpec {
                event_kind: DispatchEventKind::FailRuntime,
                runtime_state: OrchdRuntimeState::Failed,
                state_literal: DispatchState::FailedRuntime,
                reason_code: reason_code.clone(),
            },
        },
    }
}

fn maybe_post_landing_comment(
    args: &FinalizeDispatchArgs,
    comment: &LandingCommentSpec,
) -> Result<()> {
    let should_send = db::record_notification_delivery(
        &args.db_path,
        &comment.dedupe_key,
        comment.delivery_kind,
    )?;
    if !should_send {
        return Ok(());
    }

    forgejoctl_cmd::run_forgejoctl(
        &args.forgejoctl_bin,
        args.forgejo_config.as_deref(),
        &args.token_file,
        &[
            "issue",
            "comment",
            &args.issue_ref.to_string(),
            "--body",
            &comment.body,
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
        | LandingOutcome::Blocked { lines, .. }
        | LandingOutcome::Failure { lines, .. } => lines.as_slice(),
        LandingOutcome::NotRequired => &[],
    };
    if let Err(err) = append_completion_section(&args.completion_file, "Landing", landing_lines) {
        eprintln!("finalize-dispatch: failed appending landing info: {err}");
    }

    if let LandingOutcome::Blocked {
        comment: Some(comment),
        ..
    } = &landing_outcome
        && let Err(err) = maybe_post_landing_comment(&args, comment)
    {
        eprintln!("finalize-dispatch: landing comment failed: {err}");
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

    if status_spec.runtime_state == OrchdRuntimeState::Failed
        && args.role_name != "codex-audit"
        && args.directive != DIRECTIVE_AUDIT
    {
        let default_owner =
            AgentConfig::load(args.forgejo_config.clone(), Some(args.token_file.clone()))
                .map(|cfg| cfg.default_repo.owner)
                .unwrap_or_else(|_| args.issue_ref.repo.owner.clone());
        let identity = super::projection::CommentIdentity {
            forgejoctl_bin: args.forgejoctl_bin.clone(),
            config_file: args.forgejo_config.clone(),
            token_file: args.token_file.clone(),
        };
        let spec = InquisitionSpec {
            source_issue: args.issue_ref.clone(),
            source_issue_title: Some(args.issue_title.clone()),
            source_issue_url: Some(args.issue_url.clone()),
            dispatch_id: Some(args.dispatch_id),
            directive: Some(args.directive.clone()),
            role_name: Some(args.role_name.clone()),
            reason_code: status_spec.reason_code,
            exit_code: Some(args.exit_code),
            run_dir: Some(args.run_dir.to_string_lossy().into_owned()),
            log_file: Some(args.log_file.to_string_lossy().into_owned()),
            completion_file: Some(args.completion_file.to_string_lossy().into_owned()),
            error_text: None,
        };
        if let Err(err) = maybe_spawn_inquisition(&args.db_path, &default_owner, &identity, spec) {
            eprintln!("finalize-dispatch: failed spawning inquisition ticket: {err}");
        }
    }

    info!("finalize dispatch completed");
    Ok(())
}
