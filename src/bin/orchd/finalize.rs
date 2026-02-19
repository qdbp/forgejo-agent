use std::fs::{self, OpenOptions};
use std::io::Write as _;
use std::path::Path;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, anyhow};
use tracing::{info, info_span};

use forgejo_agent::api::{ApiHttpError, ForgejoClient, MergePullMethod};
use forgejo_agent::config::AgentConfig;

use forgejo_agent::orchd_dispatch_core::{DispatchEventKind, DispatchState};
use forgejo_agent::types::{ApiPullRequest, IssueRef, OpenState, OrchdRuntimeState, RepoRef};

use super::cli::FinalizeDispatchArgs;
use super::db;
use super::forgejoctl_cmd;
use super::inquisition::{InquisitionSpec, maybe_spawn_inquisition};
use super::lexicon::{
    DIRECTIVE_AUDIT, DIRECTIVE_AUDIT_FAILURE, DIRECTIVE_IMPL, directive_uses_worktree,
};
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

#[derive(Debug, Clone)]
struct LandingTarget<'a> {
    kind: &'static str,
    repo_ref: RepoRef,
    git_workdir: &'a Path,
    principal_workdir: Option<&'a Path>,
    git_remote: &'a str,
    git_base: &'a str,
    git_branch: &'a str,
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

fn parse_context_remaining_pct(log_file: &Path) -> Option<u8> {
    let raw = fs::read_to_string(log_file).ok()?;
    for line in raw.lines().rev() {
        let lower = line.to_ascii_lowercase();
        if !lower.contains("context") || !line.contains('%') {
            continue;
        }
        let mut best: Option<u8> = None;
        for token in line.split(|ch: char| !ch.is_ascii_digit()) {
            if token.is_empty() {
                continue;
            }
            let Ok(value) = token.parse::<u16>() else {
                continue;
            };
            if value <= 100 {
                best = u8::try_from(value).ok();
            }
        }
        if best.is_some() {
            return best;
        }
    }
    None
}

fn prompt_bytes_for_dispatch(run_dir: &Path) -> u64 {
    let prompt_path = run_dir.join("prompt.md");
    fs::metadata(prompt_path).map_or(0, |meta| meta.len())
}

fn whoami_login(api: &ForgejoClient, cfg: &AgentConfig) -> Result<String> {
    let value = api.whoami(cfg).context("whoami failed")?;
    let login = value
        .get("login")
        .and_then(|value| value.as_str())
        .ok_or_else(|| anyhow!("whoami missing login field"))?;
    Ok(login.to_string())
}

fn merge_endpoint_reports_try_again_later(http: &ApiHttpError) -> bool {
    http.status == 405 && http.body.contains("Please try again later")
}

fn fetch_open_pr_mergeable_state(
    api: &ForgejoClient,
    cfg: &AgentConfig,
    repo_ref: &RepoRef,
    pr_number: u64,
) -> Option<bool> {
    api.list_pull_requests(cfg, repo_ref, "open", 200)
        .ok()?
        .into_iter()
        .find(|pr| pr.number == pr_number)
        .and_then(|pr| pr.mergeable)
}

fn classify_persistent_merge_block(mergeable_state: Option<bool>) -> (&'static str, &'static str) {
    if mergeable_state == Some(false) {
        (
            "pr_not_mergeable",
            "mergeability is false after retry; manual conflict resolution is required",
        )
    } else {
        ("pr_merge_blocked", "merge still blocked after retry")
    }
}

fn issue_identity_matches_pr(pr: &ApiPullRequest, issue_ref: &IssueRef) -> bool {
    let title_prefix = format!("{issue_ref}: ");
    let issue_token = format!("-i{}-", issue_ref.number);
    pr.title.starts_with(&title_prefix) || pr.head.ref_name.contains(&issue_token)
}

fn superseded_pr_candidates(
    open_prs: Vec<ApiPullRequest>,
    issue_ref: &IssueRef,
    landed_pr_number: u64,
) -> Vec<ApiPullRequest> {
    let mut candidates = open_prs
        .into_iter()
        .filter(|pr| {
            pr.number != landed_pr_number
                && !pr.merged
                && pr.state.eq_ignore_ascii_case("open")
                && pr.head.ref_name.starts_with("orchd/d")
                && issue_identity_matches_pr(pr, issue_ref)
        })
        .collect::<Vec<_>>();
    candidates.sort_by_key(|pr| pr.number);
    candidates
}

fn reconcile_superseded_open_prs(
    api: &ForgejoClient,
    cfg: &AgentConfig,
    repo_ref: &RepoRef,
    issue_ref: &IssueRef,
    landed_pr_number: u64,
    landed_pr_url: &str,
    lines: &mut Vec<String>,
) -> Result<()> {
    let prefix = format!("[{}] ", repo_ref);
    let open_prs = api
        .list_pull_requests(cfg, repo_ref, "open", 200)
        .context("listing open PRs for superseded reconciliation failed")?;
    let candidates = superseded_pr_candidates(open_prs, issue_ref, landed_pr_number);

    for candidate in candidates {
        let candidate_ref = IssueRef {
            repo: repo_ref.clone(),
            number: candidate.number,
        };
        let body = format!(
            "Superseded by #{} ({}) after successful re-dispatch; closing.",
            landed_pr_number, landed_pr_url
        );
        if let Err(err) = api.comment_issue(cfg, &candidate_ref, &body) {
            lines.push(format!(
                "{prefix}pr_supersede_comment_failed: #{}: {err:#}",
                candidate.number
            ));
        } else {
            lines.push(format!(
                "{prefix}pr_supersede_comment: #{} -> #{} ({})",
                candidate.number, landed_pr_number, landed_pr_url
            ));
        }
        if let Err(err) = api.set_issue_open_state(cfg, &candidate_ref, OpenState::Closed) {
            lines.push(format!(
                "{prefix}pr_supersede_close_failed: #{}: {err:#}",
                candidate.number
            ));
        } else {
            lines.push(format!(
                "{prefix}pr_supersede_close: #{} -> #{} ({})",
                candidate.number, landed_pr_number, landed_pr_url
            ));
        }
    }

    Ok(())
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
                if attempt < MAX_ATTEMPTS {
                    if http.status >= 500 {
                        lines.push(format!(
                            "pr_merge_retry: transient server error status={} attempt={}/{}",
                            http.status, attempt, MAX_ATTEMPTS
                        ));
                        std::thread::sleep(Duration::from_secs(SLEEP_SEC));
                        continue;
                    }
                    if merge_endpoint_reports_try_again_later(http) {
                        lines.push(format!(
                            "pr_merge_retry: mergeability not ready status={} attempt={}/{}",
                            http.status, attempt, MAX_ATTEMPTS
                        ));
                        std::thread::sleep(Duration::from_secs(SLEEP_SEC));
                        continue;
                    }
                }
                return Err(err);
            }
        }
    }
    Ok(())
}

fn evaluate_landing_target(
    args: &FinalizeDispatchArgs,
    target: LandingTarget<'_>,
) -> LandingOutcome {
    if args.directive != DIRECTIVE_IMPL {
        return LandingOutcome::NotRequired;
    }

    let prefix = format!("[{}] ", target.repo_ref);
    let branch = target.git_branch.trim();
    if branch.is_empty() {
        return LandingOutcome::Failure {
            reason_code: "landing_missing_branch".to_string(),
            lines: vec![format!(
                "{prefix}pr landing failed: missing git branch name"
            )],
        };
    }

    let status_out = match repo::git_checked(
        target.git_workdir,
        &["status", "--porcelain", "--untracked-files=no"],
    ) {
        Ok(out) => repo::git_stdout_trim(&out),
        Err(err) => {
            return LandingOutcome::Failure {
                reason_code: "landing_git_status_failed".to_string(),
                lines: vec![format!("{prefix}pr landing failed: {err:#}")],
            };
        }
    };
    if !status_out.is_empty() {
        let retry_mention = if args.role_name.starts_with("codex") {
            format!("@{} impl", args.role_name)
        } else {
            String::new()
        };
        let body = format!(
            "PR landing blocked: worktree has uncommitted changes.\n- repo: `{}`\n- branch: `{}`\n\nretry: {}\n\nhint: commit your changes (or discard them) and re-run.\n",
            target.repo_ref, branch, retry_mention,
        );
        return LandingOutcome::Blocked {
            reason_code: "worktree_dirty".to_string(),
            lines: vec![format!(
                "{prefix}pr landing blocked: uncommitted changes in worktree"
            )],
            comment: Some(LandingCommentSpec {
                delivery_kind: "worktree_dirty",
                dedupe_key: format!(
                    "dispatch:{}:{}:worktree_dirty",
                    args.dispatch_id, target.kind
                ),
                body,
            }),
        };
    }

    let head_sha = match repo::git_checked(target.git_workdir, &["rev-parse", "HEAD"])
        .map(|out| repo::git_stdout_trim(&out))
    {
        Ok(value) if !value.trim().is_empty() => value,
        Ok(_) => {
            return LandingOutcome::Failure {
                reason_code: "landing_git_head_failed".to_string(),
                lines: vec![format!("{prefix}pr landing failed: head sha was empty")],
            };
        }
        Err(err) => {
            return LandingOutcome::Failure {
                reason_code: "landing_git_head_failed".to_string(),
                lines: vec![format!("{prefix}pr landing failed: {err:#}")],
            };
        }
    };
    let base_ref = format!("{}/{}", target.git_remote, target.git_base);
    if let Ok(base_out) = repo::git_checked(target.git_workdir, &["rev-parse", base_ref.as_str()])
        && repo::git_stdout_trim(&base_out) == head_sha
    {
        return LandingOutcome::Success(vec![format!("{prefix}pr landing: skipped (no changes)")]);
    }

    let cfg = match AgentConfig::load(args.forgejo_config.clone(), Some(args.token_file.clone())) {
        Ok(cfg) => cfg,
        Err(err) => {
            return LandingOutcome::Failure {
                reason_code: "landing_config_error".to_string(),
                lines: vec![format!("{prefix}pr landing failed: {err:#}")],
            };
        }
    };
    let api = match ForgejoClient::new(&cfg) {
        Ok(api) => api,
        Err(err) => {
            return LandingOutcome::Failure {
                reason_code: "landing_api_client_error".to_string(),
                lines: vec![format!("{prefix}pr landing failed: {err:#}")],
            };
        }
    };

    let repo_ref = target.repo_ref.clone();
    let repo_full_name = repo_ref.to_string();

    let prs = match api.list_pull_requests(&cfg, &repo_ref, "all", 200) {
        Ok(prs) => prs,
        Err(err) => {
            return LandingOutcome::Failure {
                reason_code: "landing_pr_list_failed".to_string(),
                lines: vec![format!("{prefix}pr landing failed: {err:#}")],
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
                lines: vec![format!("{prefix}pr landing failed: {err:#}")],
            };
        }
    };
    let git_url = match repo::forgejo_http_git_url(&cfg.base_url, &login, &repo_full_name) {
        Ok(url) => url,
        Err(err) => {
            return LandingOutcome::Failure {
                reason_code: "landing_git_url_failed".to_string(),
                lines: vec![format!("{prefix}pr landing failed: {err}")],
            };
        }
    };

    let mut lines = Vec::new();
    if let Some(pr) = existing_merged_pr {
        lines.push(format!(
            "{prefix}pr landing: already merged #{} ({})",
            pr.number, pr.html_url
        ));
        if let Err(err) = reconcile_superseded_open_prs(
            &api,
            &cfg,
            &repo_ref,
            &args.issue_ref,
            pr.number,
            &pr.html_url,
            &mut lines,
        ) {
            lines.push(format!("{prefix}pr_supersede_scan_failed: {err:#}"));
        }
        if let Some(principal_workdir) = target.principal_workdir {
            lines.extend(repo::best_effort_sync_principal(
                &args.db_path,
                &args.token_file,
                principal_workdir,
                target.git_base,
                &git_url,
            ));
        }
        return LandingOutcome::Success(lines);
    }

    let head_short = match repo::git_checked(target.git_workdir, &["rev-parse", "--short", "HEAD"])
        .map(|out| repo::git_stdout_trim(&out))
    {
        Ok(value) => value,
        Err(_) => head_sha.chars().take(12).collect::<String>(),
    };
    if let Err(err) = repo::git_checked_with_token(
        &args.db_path,
        &args.token_file,
        Some(target.git_workdir),
        &["push", &git_url, &format!("HEAD:{branch}")],
    ) {
        return LandingOutcome::Failure {
            reason_code: "landing_git_push_failed".to_string(),
            lines: vec![format!("{prefix}pr landing failed: {err}")],
        };
    }
    lines.push(format!(
        "{prefix}git_push: {head_short} -> {branch} ({git_url})"
    ));

    let prs = match api.list_pull_requests(&cfg, &repo_ref, "all", 200) {
        Ok(prs) => prs,
        Err(err) => {
            return LandingOutcome::Failure {
                reason_code: "landing_pr_list_failed".to_string(),
                lines: vec![format!("{prefix}pr landing failed: {err:#}")],
            };
        }
    };
    let pr = if let Some(existing) = prs.into_iter().find(|pr| pr.head.ref_name == branch) {
        existing
    } else {
        let title = format!("{}: {}", args.issue_ref, args.issue_title);
        let body = format!("Automated PR for {}.", args.issue_ref);
        match api.create_pull_request(&cfg, &repo_ref, &title, branch, target.git_base, &body) {
            Ok(pr) => {
                lines.push(format!(
                    "{prefix}pr_create: #{} ({})",
                    pr.number, pr.html_url
                ));
                pr
            }
            Err(err) => {
                return LandingOutcome::Failure {
                    reason_code: "landing_pr_create_failed".to_string(),
                    lines: vec![format!("{prefix}pr landing failed: {err:#}")],
                };
            }
        }
    };

    match merge_ff_only_with_retry(&api, &cfg, &repo_ref, pr.number, &head_sha, &mut lines) {
        Ok(()) => {
            lines.push(format!(
                "{prefix}pr_merge: ff-only #{} ({})",
                pr.number, pr.html_url
            ));
            if let Err(err) = reconcile_superseded_open_prs(
                &api,
                &cfg,
                &repo_ref,
                &args.issue_ref,
                pr.number,
                &pr.html_url,
                &mut lines,
            ) {
                lines.push(format!("{prefix}pr_supersede_scan_failed: {err:#}"));
            }
            if let Some(principal_workdir) = target.principal_workdir {
                lines.extend(repo::best_effort_sync_principal(
                    &args.db_path,
                    &args.token_file,
                    principal_workdir,
                    target.git_base,
                    &git_url,
                ));
            }
            LandingOutcome::Success(lines)
        }
        Err(err) => {
            let Some(http) = err.downcast_ref::<ApiHttpError>() else {
                return LandingOutcome::Failure {
                    reason_code: "landing_pr_merge_failed".to_string(),
                    lines: vec![format!("{prefix}pr landing failed: {err:#}")],
                };
            };
            // Forgejo/Gitea return 405 for "merge not currently allowed", including cases where the
            // mergeability check is still in progress ("Please try again later"). We treat that
            // transient form as recoverable, alongside 409/423 and intermittent 5xx from the merge
            // endpoint, because a local fetch+rebase+retry can still land (or produce a concrete
            // rebase-conflict punt back to the implementing role).
            if !(matches!(http.status, 409 | 423)
                || http.status >= 500
                || merge_endpoint_reports_try_again_later(http))
            {
                return LandingOutcome::Failure {
                    reason_code: "landing_pr_merge_failed".to_string(),
                    lines: vec![format!("{prefix}pr landing failed: {http}")],
                };
            }

            lines.push(format!(
                "{prefix}pr_merge_retry: status={} attempting rebase",
                http.status
            ));
            let fetch_result = repo::git_checked_with_token(
                &args.db_path,
                &args.token_file,
                Some(target.git_workdir),
                &["fetch", &git_url, target.git_base],
            );
            if let Err(err) = fetch_result {
                return LandingOutcome::Failure {
                    reason_code: "landing_git_fetch_failed".to_string(),
                    lines: vec![format!("{prefix}pr landing failed: {err}")],
                };
            }

            let rebase_result = repo::git_checked(target.git_workdir, &["rebase", "FETCH_HEAD"]);
            if let Err(err) = rebase_result {
                let _ = repo::git_checked(target.git_workdir, &["rebase", "--abort"]);
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
                        ("base_branch", target.git_base),
                        ("retry_mention", retry_mention.as_str()),
                        ("error", &format!("{err:#}")),
                    ],
                    "pr rebase conflict",
                )
                .unwrap_or_else(|tpl_err| {
                    format!(
                        "PR landing blocked (rebase conflicts).\n- PR: {}\n- branch: `{}` (base `{}`)\n\nretry: {}\n\nerror: {err:#}\n(template error: {tpl_err})\n",
                        pr.html_url, branch, target.git_base, retry_mention
                    )
                });

                return LandingOutcome::Blocked {
                    reason_code: "pr_rebase_conflict".to_string(),
                    lines: vec![format!(
                        "{prefix}pr landing blocked: rebase conflict: {err:#}"
                    )],
                    comment: Some(LandingCommentSpec {
                        delivery_kind: "pr_rebase_conflict",
                        dedupe_key: format!(
                            "dispatch:{}:{}:pr_rebase_conflict",
                            args.dispatch_id, target.kind
                        ),
                        body: rendered,
                    }),
                };
            }

            let rebased_sha = match repo::git_checked(target.git_workdir, &["rev-parse", "HEAD"])
                .map(|out| repo::git_stdout_trim(&out))
            {
                Ok(value) if !value.trim().is_empty() => value,
                Ok(_) => {
                    return LandingOutcome::Failure {
                        reason_code: "landing_git_head_failed".to_string(),
                        lines: vec![format!(
                            "{prefix}pr landing failed: rebased head sha was empty"
                        )],
                    };
                }
                Err(err) => {
                    return LandingOutcome::Failure {
                        reason_code: "landing_git_head_failed".to_string(),
                        lines: vec![format!("{prefix}pr landing failed: {err:#}")],
                    };
                }
            };
            let rebased_short =
                repo::git_checked(target.git_workdir, &["rev-parse", "--short", "HEAD"])
                    .map(|out| repo::git_stdout_trim(&out))
                    .unwrap_or_else(|_| rebased_sha.chars().take(12).collect::<String>());

            // `git push --force-with-lease` without an explicit ref lease relies on remote-tracking
            // refs (e.g. refs/remotes/origin/<branch>). We push to an authenticated URL, so we
            // may not have a remote name with updated tracking state. Provide an explicit lease
            // pinned to the pre-rebase head to avoid "stale info" false negatives while
            // preserving safety (no overwrite if someone else updated the branch).
            let lease_flag = format!("--force-with-lease=refs/heads/{branch}:{head_sha}");

            let push_result = repo::git_checked_with_token(
                &args.db_path,
                &args.token_file,
                Some(target.git_workdir),
                &[
                    "push",
                    lease_flag.as_str(),
                    &git_url,
                    &format!("HEAD:{branch}"),
                ],
            );
            if let Err(err) = push_result {
                return LandingOutcome::Failure {
                    reason_code: "landing_git_push_failed".to_string(),
                    lines: vec![format!("{prefix}pr landing failed: {err}")],
                };
            }
            lines.push(format!("{prefix}git_rebase: now at {rebased_short}"));
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
                        "{prefix}pr_merge: ff-only #{} ({})",
                        pr.number, pr.html_url
                    ));
                    if let Err(err) = reconcile_superseded_open_prs(
                        &api,
                        &cfg,
                        &repo_ref,
                        &args.issue_ref,
                        pr.number,
                        &pr.html_url,
                        &mut lines,
                    ) {
                        lines.push(format!("{prefix}pr_supersede_scan_failed: {err:#}"));
                    }
                    if let Some(principal_workdir) = target.principal_workdir {
                        lines.extend(repo::best_effort_sync_principal(
                            &args.db_path,
                            &args.token_file,
                            principal_workdir,
                            target.git_base,
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
                    let mergeable_state = err
                        .downcast_ref::<ApiHttpError>()
                        .and_then(|http| merge_endpoint_reports_try_again_later(http).then_some(()))
                        .and_then(|()| {
                            fetch_open_pr_mergeable_state(&api, &cfg, &repo_ref, pr.number)
                        });
                    let (reason_code, detail) = classify_persistent_merge_block(mergeable_state);
                    let template_error = if reason_code == "pr_not_mergeable" {
                        format!("{detail}\n\nraw error: {err:#}")
                    } else {
                        format!("{err:#}")
                    };
                    let template_path = args
                        .git_workdir
                        .join("templates/orchd-landing-pr-merge-blocked.md");
                    let body = template::render_prompt_file(
                        &template_path,
                        &[
                            ("pr_url", pr.html_url.as_str()),
                            ("branch", branch),
                            ("base_branch", target.git_base),
                            ("retry_mention", retry_mention.as_str()),
                            ("error", template_error.as_str()),
                        ],
                        "pr merge blocked",
                    )
                    .unwrap_or_else(|tpl_err| {
                        format!(
                            "PR landing blocked: merge still not possible after rebase.\n- PR: {}\n- branch: `{}` (base `{}`)\n\nretry: {}\n\nerror: {err:#}\n(template error: {tpl_err})\n",
                            pr.html_url, branch, target.git_base, retry_mention
                        )
                    });
                    LandingOutcome::Blocked {
                        reason_code: reason_code.to_string(),
                        lines: vec![format!("{prefix}pr landing blocked: {detail}: {err:#}")],
                        comment: Some(LandingCommentSpec {
                            delivery_kind: reason_code,
                            dedupe_key: format!(
                                "dispatch:{}:{}:{}",
                                args.dispatch_id, target.kind, reason_code
                            ),
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

fn work_state_transition_target(
    directive: &str,
    state_literal: DispatchState,
) -> Option<&'static str> {
    // Keep work-plane workflow semantics independent from dispatch runtime outcomes.
    // `state/blocked` is reserved for explicit dependency blockers, not generic dispatch failures.
    (directive_uses_worktree(directive) && state_literal == DispatchState::Completed)
        .then_some("done")
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
    let (sidecar_outcome, primary_outcome, landing_outcome) =
        if matches!(reported_status, ReportedStatus::Completed) {
            let primary_target = LandingTarget {
                kind: "primary",
                repo_ref: args.issue_ref.repo.clone(),
                git_workdir: &args.git_workdir,
                principal_workdir: args.principal_workdir.as_deref(),
                git_remote: args.git_remote.as_str(),
                git_base: args.git_base.as_str(),
                git_branch: args.git_branch.as_str(),
            };

            let sidecar_target = if let Some(repo_ref) = args.sidecar_repo.clone() {
                let git_workdir = args
                    .sidecar_git_workdir
                    .as_deref()
                    .ok_or_else(|| anyhow!("sidecar_git_workdir missing for {repo_ref}"))?;
                let git_remote = args
                    .sidecar_git_remote
                    .as_deref()
                    .ok_or_else(|| anyhow!("sidecar_git_remote missing for {repo_ref}"))?;
                let git_base = args
                    .sidecar_git_base
                    .as_deref()
                    .ok_or_else(|| anyhow!("sidecar_git_base missing for {repo_ref}"))?;
                let git_branch = args
                    .sidecar_git_branch
                    .as_deref()
                    .ok_or_else(|| anyhow!("sidecar_git_branch missing for {repo_ref}"))?;
                Some(LandingTarget {
                    kind: "sidecar",
                    repo_ref,
                    git_workdir,
                    principal_workdir: args.sidecar_principal_workdir.as_deref(),
                    git_remote,
                    git_base,
                    git_branch,
                })
            } else {
                None
            };

            let sidecar_outcome =
                sidecar_target.map(|target| evaluate_landing_target(&args, target));
            if let Some(LandingOutcome::Blocked { .. } | LandingOutcome::Failure { .. }) =
                sidecar_outcome.as_ref()
            {
                (
                    sidecar_outcome.clone(),
                    LandingOutcome::NotRequired,
                    sidecar_outcome
                        .clone()
                        .unwrap_or(LandingOutcome::NotRequired),
                )
            } else {
                let primary_outcome = evaluate_landing_target(&args, primary_target);
                (sidecar_outcome, primary_outcome.clone(), primary_outcome)
            }
        } else {
            (
                None,
                LandingOutcome::NotRequired,
                LandingOutcome::NotRequired,
            )
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

    let context_pct = parse_context_remaining_pct(&args.log_file);
    let prompt_bytes = prompt_bytes_for_dispatch(&args.run_dir);
    let session_id = args.session_id.trim();
    let session_id = if session_id.is_empty() {
        None
    } else {
        Some(session_id)
    };
    if let Err(err) = db::record_timer_context_completion(
        &args.db_path,
        args.dispatch_id,
        session_id,
        status_spec.state_literal.as_db_str(),
        prompt_bytes,
        context_pct,
    ) {
        eprintln!("finalize-dispatch: timer context update failed: {err}");
    }

    if let Some(outcome) = sidecar_outcome.as_ref() {
        let header = match args.sidecar_repo.as_ref() {
            Some(repo) => format!("Landing ({repo})"),
            None => "Landing (sidecar)".to_string(),
        };
        let landing_lines = match outcome {
            LandingOutcome::Success(lines)
            | LandingOutcome::Blocked { lines, .. }
            | LandingOutcome::Failure { lines, .. } => lines.as_slice(),
            LandingOutcome::NotRequired => &[],
        };
        if let Err(err) =
            append_completion_section(&args.completion_file, header.as_str(), landing_lines)
        {
            eprintln!("finalize-dispatch: failed appending landing info: {err}");
        }
    }
    let primary_header = format!("Landing ({})", args.issue_ref.repo);
    let primary_lines = match &primary_outcome {
        LandingOutcome::Success(lines)
        | LandingOutcome::Blocked { lines, .. }
        | LandingOutcome::Failure { lines, .. } => lines.as_slice(),
        LandingOutcome::NotRequired => &[],
    };
    if let Err(err) = append_completion_section(
        &args.completion_file,
        primary_header.as_str(),
        primary_lines,
    ) {
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

    let work_state_target =
        work_state_transition_target(args.directive.as_str(), status_spec.state_literal);

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
        && args.directive != DIRECTIVE_AUDIT_FAILURE
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

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::classify_persistent_merge_block;
    use super::issue_identity_matches_pr;
    use super::merge_endpoint_reports_try_again_later;
    use super::superseded_pr_candidates;
    use super::work_state_transition_target;
    use forgejo_agent::api::ApiHttpError;
    use forgejo_agent::orchd_dispatch_core::DispatchState;
    use forgejo_agent::types::{ApiPrBranchInfo, ApiPullRequest, IssueRef};

    fn fake_pr(
        number: u64,
        state: &str,
        title: &str,
        head_ref: &str,
        merged: bool,
    ) -> ApiPullRequest {
        ApiPullRequest {
            number,
            state: state.to_string(),
            title: title.to_string(),
            html_url: format!("http://127.0.0.1:3000/o/r/pulls/{number}"),
            merged,
            mergeable: None,
            head: ApiPrBranchInfo {
                ref_name: head_ref.to_string(),
                sha: format!("head-{number}"),
            },
            base: ApiPrBranchInfo {
                ref_name: "main".to_string(),
                sha: "base".to_string(),
            },
        }
    }

    #[test]
    fn merge_endpoint_transient_405_is_detected() {
        let http = ApiHttpError {
            status: 405,
            method: "POST".to_string(),
            path: "/api/v1/repos/o/r/pulls/1/merge".to_string(),
            body: "{\"message\":\"Please try again later\"}".to_string(),
        };
        assert!(merge_endpoint_reports_try_again_later(&http));

        let other_405 = ApiHttpError {
            status: 405,
            method: "POST".to_string(),
            path: "/api/v1/repos/o/r/pulls/1/merge".to_string(),
            body: "{\"message\":\"Pull request is work in progress\"}".to_string(),
        };
        assert!(!merge_endpoint_reports_try_again_later(&other_405));
    }

    #[test]
    fn persistent_merge_block_is_classified_from_mergeable_state() {
        assert_eq!(
            classify_persistent_merge_block(Some(false)),
            (
                "pr_not_mergeable",
                "mergeability is false after retry; manual conflict resolution is required"
            )
        );
        assert_eq!(
            classify_persistent_merge_block(None),
            ("pr_merge_blocked", "merge still blocked after retry")
        );
        assert_eq!(
            classify_persistent_merge_block(Some(true)),
            ("pr_merge_blocked", "merge still blocked after retry")
        );
    }

    #[test]
    fn merge_blocked_template_renders_parseable_retry_directive() {
        let template_path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("templates")
            .join("orchd-landing-pr-merge-blocked.md");
        let rendered = super::template::render_prompt_file(
            &template_path,
            &[
                ("pr_url", "http://127.0.0.1:3000/o/r/pulls/1"),
                ("branch", "orchd/branch"),
                ("base_branch", "main"),
                ("retry_mention", "@codex-orch impl"),
                ("error", "boom"),
            ],
            "test landing template",
        )
        .expect("template renders");

        let parsed = super::super::webhook::parse_directive(&rendered)
            .expect("rendered template includes a parseable directive line");
        assert_eq!(parsed.role, "codex-orch");
        assert_eq!(parsed.directive, "impl");
    }

    #[test]
    fn work_state_transition_only_happens_on_completed_impl() {
        assert_eq!(
            work_state_transition_target(super::DIRECTIVE_IMPL, DispatchState::Completed),
            Some("done")
        );
        assert_eq!(
            work_state_transition_target(super::DIRECTIVE_IMPL, DispatchState::Blocked),
            None
        );
        assert_eq!(
            work_state_transition_target(super::DIRECTIVE_IMPL, DispatchState::FailedRuntime),
            None
        );
        assert_eq!(
            work_state_transition_target(super::DIRECTIVE_IMPL, DispatchState::TimedOut),
            None
        );
        assert_eq!(
            work_state_transition_target(
                super::super::lexicon::DIRECTIVE_DESIGN,
                DispatchState::Completed
            ),
            None
        );
    }

    #[test]
    fn issue_identity_match_accepts_title_or_branch_token() {
        let issue_ref = IssueRef::parse("main/forgejo-agent#139").expect("issue ref parses");

        let by_title = fake_pr(
            10,
            "open",
            "main/forgejo-agent#139: title",
            "orchd/d10/rmain-forgejo-agent-i999-impl",
            false,
        );
        assert!(issue_identity_matches_pr(&by_title, &issue_ref));

        let by_branch_token = fake_pr(
            11,
            "open",
            "unrelated title",
            "orchd/d11/rmain-forgejo-agent-i139-impl",
            false,
        );
        assert!(issue_identity_matches_pr(&by_branch_token, &issue_ref));

        let no_match = fake_pr(
            12,
            "open",
            "main/forgejo-agent#140: title",
            "orchd/d12/rmain-forgejo-agent-i140-impl",
            false,
        );
        assert!(!issue_identity_matches_pr(&no_match, &issue_ref));
    }

    #[test]
    fn superseded_candidates_are_conservative_and_stable() {
        let issue_ref = IssueRef::parse("main/forgejo-agent#139").expect("issue ref parses");
        let candidates = superseded_pr_candidates(
            vec![
                fake_pr(
                    130,
                    "open",
                    "main/forgejo-agent#139: landed",
                    "orchd/d213/rmain-forgejo-agent-i139-impl",
                    false,
                ),
                fake_pr(
                    126,
                    "open",
                    "main/forgejo-agent#139: stale",
                    "orchd/d204/rmain-forgejo-agent-i139-impl",
                    false,
                ),
                fake_pr(
                    120,
                    "open",
                    "main/forgejo-agent#120: other issue",
                    "orchd/d190/rmain-forgejo-agent-i120-impl",
                    false,
                ),
                fake_pr(
                    125,
                    "closed",
                    "main/forgejo-agent#139: closed",
                    "orchd/d1/rmain-forgejo-agent-i139-impl",
                    false,
                ),
                fake_pr(
                    127,
                    "open",
                    "main/forgejo-agent#139: merged",
                    "orchd/d205/rmain-forgejo-agent-i139-impl",
                    true,
                ),
                fake_pr(
                    128,
                    "open",
                    "main/forgejo-agent#139: not orchd branch",
                    "feature/i139-manual",
                    false,
                ),
                fake_pr(
                    129,
                    "open",
                    "unrelated",
                    "orchd/d206/rmain-forgejo-agent-i139-impl",
                    false,
                ),
            ],
            &issue_ref,
            130,
        );
        let numbers = candidates
            .into_iter()
            .map(|pr| pr.number)
            .collect::<Vec<_>>();
        assert_eq!(numbers, vec![126, 129]);
    }
}
