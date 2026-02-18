use std::ffi::OsString;
use std::fs;
use std::fs::OpenOptions;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::Instant;

use anyhow::Result;
use chrono::{DateTime, Duration as ChronoDuration, Utc};
use serde_json::json;
use tracing::{info, info_span};

use forgejo_agent::api::ForgejoClient;
use forgejo_agent::orchd_dispatch_core::{
    DispatchBackendKind, DispatchIntentV1, DispatchPolicyOutcome, DispatchState,
    PolicyDecision as DispatchPolicyDecision, RunHandle,
};
use forgejo_agent::types::{ApiIssue, IssueRef, RepoRef};

use super::cli::{DispatchBackend, DispatchMode};
use super::db;
use super::dispatch_config::{
    DispatchConfig, DispatchDirectiveConfig, DispatchPromptEnvelopeConfig, DispatchRankAclConfig,
    DispatchRoleConfig,
};
use super::errors::DispatchError;
use super::forgejoctl_cmd;
use super::lexicon;
use super::reading_material;
use super::repo;
use super::run_dispatch::{
    CodexSandbox, CodexSessionId, DispatchExecSidecarV1, DispatchExecSpecV1,
};
use super::state::{AppState, DecisionRecord, EventRecord};
use super::telemetry::{log_line, record_phase_latency_ms};
use super::template;

pub(super) const STARTING_DISPATCH_STALE_AFTER_SEC: i64 = 120;

fn strip_linux_deleted_suffix(path: &Path) -> Option<PathBuf> {
    let raw = path.to_str()?;
    let stripped = raw.strip_suffix(" (deleted)")?;
    Some(PathBuf::from(stripped))
}

fn resolve_orchd_exe_from(
    current_exe: Option<PathBuf>,
    argv0: Option<PathBuf>,
    path_env: Option<OsString>,
) -> Result<PathBuf> {
    let argv0_for_path_search = argv0
        .as_ref()
        .filter(|path| path.components().count() == 1)
        .cloned();

    let mut candidates = Vec::new();
    if let Some(path) = current_exe {
        candidates.push(path.clone());
        if let Some(stripped) = strip_linux_deleted_suffix(&path) {
            candidates.push(stripped);
        }
    }
    if let Some(path) = argv0 {
        candidates.push(path);
    }

    if let Some(existing) = candidates.into_iter().find(|candidate| candidate.exists()) {
        return Ok(existing);
    }

    if let (Some(path_env), Some(argv0)) = (path_env, argv0_for_path_search) {
        for dir in std::env::split_paths(&path_env) {
            let candidate = dir.join(&argv0);
            if candidate.exists() {
                return Ok(candidate);
            }
        }
    }

    anyhow::bail!("unable to resolve orchd executable path")
}

fn resolve_orchd_exe() -> Result<PathBuf> {
    resolve_orchd_exe_from(
        std::env::current_exe().ok(),
        std::env::args_os().next().map(PathBuf::from),
        std::env::var_os("PATH"),
    )
}

fn fail_dispatch_start(db_path: &Path, dispatch_id: i64, lock_path: &Path, err: &DispatchError) {
    let _ =
        db::update_dispatch_failed_start(db_path, dispatch_id, err.reason_code(), &err.to_string());
    let _ = fs::remove_file(lock_path);
}

#[derive(Debug, Clone)]
struct DispatchPlan {
    actor: String,
    event_type: String,
    directive: DispatchDirectiveConfig,
    role: DispatchRoleConfig,
    workdir: PathBuf,
    principal_workdir: Option<PathBuf>,
    sidecar: Option<DispatchSidecarPlan>,
    git_remote: String,
    git_base: String,
    git_branch: String,
    intent: DispatchIntentV1,
    issue_ref: IssueRef,
    issue_title: String,
    issue_body: String,
    issue_url: String,
    issue_session_id: Option<String>,
    issue_delta_md: String,
    dispatch_id: i64,
    lock_path: PathBuf,
    run_dir: PathBuf,
}

#[derive(Debug, Clone)]
struct DispatchSidecarPlan {
    repo_full_name: String,
    workdir: PathBuf,
    principal_workdir: Option<PathBuf>,
    git_remote: String,
    git_base: String,
    git_branch: String,
}

#[derive(Debug, Clone)]
struct DispatchRunArtifacts {
    spec_path: PathBuf,
}

trait DispatchBackendAdapter {
    fn launch(
        &self,
        dispatch_config: &DispatchConfig,
        plan: &DispatchPlan,
        artifacts: &DispatchRunArtifacts,
    ) -> Result<RunHandle, DispatchError>;

    fn probe(
        &self,
        dispatch: &db::InflightDispatch,
        repo_full_name: &str,
        issue_number: u64,
    ) -> Result<bool, DispatchError>;
}

#[derive(Debug, Clone, Copy)]
struct SystemdBackendAdapter;

impl DispatchBackendAdapter for SystemdBackendAdapter {
    fn launch(
        &self,
        _dispatch_config: &DispatchConfig,
        plan: &DispatchPlan,
        artifacts: &DispatchRunArtifacts,
    ) -> Result<RunHandle, DispatchError> {
        let unit_name = format!("orchd-dispatch-{}", plan.dispatch_id);
        let unit_ref = format!("{unit_name}.service");
        let orchd_bin = resolve_orchd_exe().map_err(|err| {
            DispatchError::Launch(format!("failed resolving orchd executable: {err}"))
        })?;
        let output = Command::new("systemd-run")
            .args(["--user", "--collect", "--unit", &unit_name])
            .arg(&orchd_bin)
            .args(["run-dispatch", "--spec"])
            .arg(&artifacts.spec_path)
            .output()
            .map_err(|err| DispatchError::Launch(format!("failed launching systemd-run: {err}")))?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            let stdout = String::from_utf8_lossy(&output.stdout);

            let mut detail = String::new();
            let trimmed_stderr = stderr.trim();
            if !trimmed_stderr.is_empty() {
                detail.push_str("\nstderr:\n");
                detail.push_str(trimmed_stderr);
            }
            let trimmed_stdout = stdout.trim();
            if !trimmed_stdout.is_empty() {
                detail.push_str("\nstdout:\n");
                detail.push_str(trimmed_stdout);
            }

            return Err(DispatchError::Launch(format!(
                "systemd-run exited with status {}{}",
                output.status, detail
            )));
        }
        Ok(RunHandle {
            backend_kind: DispatchBackendKind::Systemd,
            backend_ref: unit_ref,
        })
    }

    fn probe(
        &self,
        dispatch: &db::InflightDispatch,
        _repo_full_name: &str,
        _issue_number: u64,
    ) -> Result<bool, DispatchError> {
        let unit_ref = dispatch
            .backend_ref
            .as_deref()
            .ok_or_else(|| DispatchError::Launch("missing systemd unit ref".to_string()))?;
        let output = Command::new("systemctl")
            .args([
                "--user",
                "show",
                "--property=ActiveState",
                "--value",
                unit_ref,
            ])
            .output()
            .map_err(|err| {
                DispatchError::Launch(format!("failed probing systemd unit state: {err}"))
            })?;
        if !output.status.success() {
            return Ok(false);
        }
        let active_state = String::from_utf8_lossy(&output.stdout).trim().to_string();
        Ok(matches!(
            active_state.as_str(),
            "active" | "activating" | "reloading"
        ))
    }
}

#[derive(Debug, Clone, Copy)]
struct LocalBackendAdapter;

impl DispatchBackendAdapter for LocalBackendAdapter {
    fn launch(
        &self,
        _dispatch_config: &DispatchConfig,
        _plan: &DispatchPlan,
        artifacts: &DispatchRunArtifacts,
    ) -> Result<RunHandle, DispatchError> {
        let log_path = artifacts.spec_path.parent().map_or_else(
            || PathBuf::from("local-backend.log"),
            |parent| parent.join("local-backend.log"),
        );
        let stdout = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&log_path)
            .map_err(|err| DispatchError::Io(format!("failed opening local backend log: {err}")))?;
        let stderr = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&log_path)
            .map_err(|err| DispatchError::Io(format!("failed opening local backend log: {err}")))?;
        let orchd_bin = resolve_orchd_exe().map_err(|err| {
            DispatchError::Io(format!("failed resolving orchd executable: {err}"))
        })?;
        let child = Command::new(&orchd_bin)
            .args(["run-dispatch", "--spec"])
            .arg(&artifacts.spec_path)
            .stdout(Stdio::from(stdout))
            .stderr(Stdio::from(stderr))
            .spawn()
            .map_err(|err| DispatchError::Io(format!("failed launching local backend: {err}")))?;
        Ok(RunHandle {
            backend_kind: DispatchBackendKind::Local,
            backend_ref: child.id().to_string(),
        })
    }

    fn probe(
        &self,
        dispatch: &db::InflightDispatch,
        _repo_full_name: &str,
        _issue_number: u64,
    ) -> Result<bool, DispatchError> {
        let pid = dispatch
            .backend_ref
            .as_deref()
            .ok_or_else(|| DispatchError::Io("missing local backend pid ref".to_string()))?;
        let status = Command::new("kill")
            .arg("-0")
            .arg(pid)
            .status()
            .map_err(|err| DispatchError::Io(format!("failed probing local backend pid: {err}")))?;
        Ok(status.success())
    }
}

fn codex_sandbox_for_directive(directive: &str) -> CodexSandbox {
    match directive {
        lexicon::DIRECTIVE_DESIGN
        | lexicon::DIRECTIVE_INVESTIGATE
        | lexicon::DIRECTIVE_REPLY
        | lexicon::DIRECTIVE_AUDIT => CodexSandbox::ReadOnly,
        _ => CodexSandbox::WorkspaceWrite,
    }
}

pub(super) fn render_fresh_preamble(
    prompt_envelopes: &DispatchPromptEnvelopeConfig,
    rank_acl: &DispatchRankAclConfig,
    role_name: &str,
) -> Result<String, DispatchError> {
    let preamble_template = fs::read_to_string(&prompt_envelopes.preamble_file).map_err(|err| {
        DispatchError::Io(format!(
            "failed reading prompt preamble {}: {err}",
            prompt_envelopes.preamble_file.display()
        ))
    })?;
    let role_card_file = prompt_envelopes.role_card_file_for(role_name);
    let role_card_md = fs::read_to_string(&role_card_file).map_err(|err| {
        DispatchError::Io(format!(
            "failed reading role card {} for role {}: {err}",
            role_card_file.display(),
            role_name
        ))
    })?;
    let acl_summary_md = rank_acl.acl_summary_markdown(role_name);
    let role_card_with_acl_md = format!("{role_card_md}\n\n{acl_summary_md}");
    template::render_prompt(
        &preamble_template,
        &[("role_card_md", &role_card_with_acl_md)],
    )
}

fn render_dispatch_md(
    prompt_envelopes: &DispatchPromptEnvelopeConfig,
    plan: &DispatchPlan,
    prompt_mode: &str,
) -> Result<String, DispatchError> {
    let turn_type = if prompt_mode == "fresh" {
        "first turn in this issue"
    } else {
        "follow-up turn in an existing issue session"
    };
    let trigger = match plan.event_type.as_str() {
        lexicon::EVENT_ISSUE_COMMENT => "a new issue comment arrived",
        lexicon::EVENT_ISSUES => "an issue event triggered dispatch",
        _ => "a Forgejo webhook event triggered dispatch",
    };
    let issue_ref = plan.issue_ref.to_string();
    template::render_prompt_file(
        &prompt_envelopes.turn_context_file,
        &[
            ("actor", &plan.actor),
            ("issue_ref", &issue_ref),
            ("turn_type", turn_type),
            ("trigger", trigger),
        ],
        "turn context",
    )
}

fn render_issue_md(
    prompt_envelopes: &DispatchPromptEnvelopeConfig,
    plan: &DispatchPlan,
    prompt_mode: &str,
) -> Result<String, DispatchError> {
    let issue_title = plan.issue_title.as_str();
    if prompt_mode == "fresh" {
        let issue_body = if plan.issue_body.trim().is_empty() {
            "(empty)"
        } else {
            plan.issue_body.as_str()
        };
        let issue_history = if plan.issue_delta_md.trim().is_empty() {
            "(no prior issue activity captured)"
        } else {
            plan.issue_delta_md.as_str()
        };
        template::render_prompt_file(
            &prompt_envelopes.issue_fresh_file,
            &[
                ("issue_title", issue_title),
                ("issue_body", issue_body),
                ("issue_history", issue_history),
            ],
            "issue fresh",
        )
    } else {
        let issue_delta = if plan.issue_delta_md.trim().is_empty() {
            "(no new issue activity)"
        } else {
            plan.issue_delta_md.as_str()
        };
        template::render_prompt_file(
            &prompt_envelopes.issue_followup_file,
            &[("issue_title", issue_title), ("issue_delta", issue_delta)],
            "issue followup",
        )
    }
}

async fn fetch_issue(state: AppState, issue: IssueRef) -> Result<ApiIssue, DispatchError> {
    tokio::task::spawn_blocking(move || {
        let api = ForgejoClient::new(&state.cfg)
            .map_err(|err| DispatchError::IssueFetch(err.to_string()))?;
        api.get_issue(&state.cfg, &issue)
            .map_err(|err| DispatchError::IssueFetch(err.to_string()))
    })
    .await
    .map_err(|err| DispatchError::IssueFetch(err.to_string()))?
}

fn probe_dispatch_liveness(
    dispatch: &db::InflightDispatch,
    repo_full_name: &str,
    issue_number: u64,
) -> Result<bool, DispatchError> {
    match dispatch.backend_kind {
        Some(DispatchBackendKind::Systemd) => {
            SystemdBackendAdapter.probe(dispatch, repo_full_name, issue_number)
        }
        Some(DispatchBackendKind::Local) => {
            LocalBackendAdapter.probe(dispatch, repo_full_name, issue_number)
        }
        None => Ok(false),
    }
}

pub(super) fn is_stale_starting_dispatch(
    dispatch: &db::InflightDispatch,
    repo_full_name: &str,
    issue_number: u64,
) -> bool {
    let Ok(started_at) = DateTime::parse_from_rfc3339(&dispatch.started_at) else {
        return true;
    };
    let age = Utc::now() - started_at.with_timezone(&Utc);
    if age < ChronoDuration::seconds(STARTING_DISPATCH_STALE_AFTER_SEC) {
        return false;
    }
    if dispatch.backend_kind.is_none() {
        return true;
    }
    if dispatch.backend_ref.is_none() {
        return true;
    }
    match probe_dispatch_liveness(dispatch, repo_full_name, issue_number) {
        Ok(alive) => !alive,
        Err(err) => {
            log_line(
                "dispatch_heal_probe_failed",
                json!({
                    "dispatch_id": dispatch.id,
                    "status": dispatch.status,
                    "repo": repo_full_name,
                    "issue_number": issue_number,
                    "error": err.to_string(),
                }),
            );
            false
        }
    }
}

fn should_heal_dispatch_stale(
    dispatch: &db::InflightDispatch,
    repo_full_name: &str,
    issue_number: u64,
) -> bool {
    match dispatch.status {
        DispatchState::Running => {
            match probe_dispatch_liveness(dispatch, repo_full_name, issue_number) {
                Ok(alive) => !alive,
                Err(err) => {
                    log_line(
                        "dispatch_heal_probe_failed",
                        json!({
                            "dispatch_id": dispatch.id,
                            "status": dispatch.status,
                            "repo": repo_full_name,
                            "issue_number": issue_number,
                            "error": err.to_string(),
                        }),
                    );
                    false
                }
            }
        }
        DispatchState::Starting => {
            is_stale_starting_dispatch(dispatch, repo_full_name, issue_number)
        }
        _ => false,
    }
}

fn find_issue_inflight_dispatch_with_healing(
    db_path: &Path,
    repo_full_name: &str,
    issue_number: u64,
) -> Result<Option<i64>> {
    loop {
        let Some(dispatch) =
            db::latest_issue_inflight_dispatch(db_path, repo_full_name, issue_number)?
        else {
            return Ok(None);
        };
        if !should_heal_dispatch_stale(&dispatch, repo_full_name, issue_number) {
            return Ok(Some(dispatch.id));
        }
        db::mark_dispatch_failed_runtime(
            db_path,
            dispatch.id,
            "stale_dispatch_autohealed",
            "auto-healed stale in-flight dispatch before launch",
        )?;
        if let Some(lock_path) = dispatch.lock_path.as_deref() {
            let _ = fs::remove_file(lock_path);
        }
        log_line(
            "dispatch_autohealed",
            json!({
                "dispatch_id": dispatch.id,
                "repo": repo_full_name,
                "issue_number": issue_number,
                "status": dispatch.status,
                "reason_code": "stale_dispatch_autohealed",
            }),
        );
    }
}

pub(super) fn heal_stale_inflight_dispatches(db_path: &Path) -> Result<usize, DispatchError> {
    let inflight =
        db::list_inflight_dispatches(db_path).map_err(|err| DispatchError::Db(err.to_string()))?;
    let mut healed = 0usize;
    for dispatch in inflight {
        if !should_heal_dispatch_stale(&dispatch, &dispatch.repo_full_name, dispatch.issue_number) {
            continue;
        }
        db::mark_dispatch_failed_runtime(
            db_path,
            dispatch.id,
            "stale_dispatch_autohealed",
            "auto-healed stale in-flight dispatch during startup sweep",
        )
        .map_err(|err| DispatchError::Db(err.to_string()))?;
        if let Some(lock_path) = dispatch.lock_path.as_deref() {
            let _ = fs::remove_file(lock_path);
        }
        log_line(
            "dispatch_autohealed",
            json!({
                "dispatch_id": dispatch.id,
                "repo": dispatch.repo_full_name,
                "issue_number": dispatch.issue_number,
                "status": dispatch.status,
                "reason_code": "stale_dispatch_autohealed",
            }),
        );
        healed += 1;
    }
    Ok(healed)
}

async fn plan_dispatch(
    state: &AppState,
    dispatch_config: &DispatchConfig,
    decision_id: i64,
    current_event_id: i64,
    record: &EventRecord,
    decision: &DecisionRecord,
) -> Result<DispatchPlan, DispatchError> {
    let directive_name = decision
        .directive
        .as_deref()
        .ok_or_else(|| DispatchError::DirectiveNotConfigured("<none>".to_string()))?;
    let directive = dispatch_config
        .directives
        .get(directive_name)
        .ok_or_else(|| DispatchError::DirectiveNotConfigured(directive_name.to_string()))?
        .clone();

    let role_name = decision
        .target_role
        .as_deref()
        .unwrap_or(directive.role.as_str())
        .to_ascii_lowercase();
    let role = dispatch_config
        .roles
        .get(&role_name)
        .ok_or_else(|| DispatchError::RoleNotConfigured(role_name.clone()))?
        .clone();

    let actor = record
        .actor_login
        .clone()
        .unwrap_or_default()
        .to_ascii_lowercase();
    let self_directive =
        decision.reason_code == "explicit_directive" && !actor.is_empty() && actor == role_name;
    // Assignee replies are implicitly scoped by the ticket assignment itself; allowlist applies to
    // explicit directives, not to "reply because you're assigned here" triggers.
    let assignee_reply = decision.reason_code == "assignee_reply";
    let bypass_allowlist =
        self_directive || assignee_reply || dispatch_config.rank_acl.has_role_policy(&actor);
    let policy_decision = if bypass_allowlist
        || dispatch_config
            .allowed_actors
            .iter()
            .any(|allowed| allowed == &actor)
    {
        DispatchPolicyDecision::allow()
    } else {
        DispatchPolicyDecision::deny(format!("actor '{actor}' is not allowlisted"))
    };
    if policy_decision.outcome != DispatchPolicyOutcome::Allow {
        return Err(DispatchError::ActorNotAllowed(actor));
    }
    if assignee_reply {
        dispatch_config
            .rank_acl
            .assert_target_can_execute(&role_name, directive_name)
            .map_err(|err| DispatchError::RankAclDenied(err.to_string()))?;
    } else {
        dispatch_config
            .rank_acl
            .assert_actor_can_dispatch(&actor, &role_name, directive_name)
            .map_err(|err| DispatchError::RankAclDenied(err.to_string()))?;
    }

    let issue_number = record
        .issue_number
        .ok_or_else(|| DispatchError::InvalidIssueRef(record.repo_full_name.clone()))?;
    let intent = DispatchIntentV1 {
        intent_id: format!("event-{current_event_id}-decision-{decision_id}"),
        repo_full_name: record.repo_full_name.clone(),
        issue_number,
        role: role_name.clone(),
        directive: directive_name.to_string(),
        actor_login: actor.clone(),
        delivery_id: record.delivery_id.clone(),
        parent_dispatch_id: None,
        created_at: Utc::now(),
        policy_snapshot: Some("cp3-rank-acl-v1".to_string()),
    };

    let repo_binding = dispatch_config.repo_bindings.get(&intent.repo_full_name);
    if intent.directive == lexicon::DIRECTIVE_IMPL && repo_binding.is_none() {
        return Err(DispatchError::RepoBindingMissing(
            intent.repo_full_name.clone(),
        ));
    }

    if let Some(dispatch_id) = find_issue_inflight_dispatch_with_healing(
        &state.db_path,
        &intent.repo_full_name,
        intent.issue_number,
    )
    .map_err(|err| DispatchError::Db(err.to_string()))?
    {
        return Err(DispatchError::IssueDispatchInFlight {
            repo_full_name: intent.repo_full_name,
            issue_number: intent.issue_number,
            dispatch_id,
        });
    }

    let issue_session_id = db::latest_issue_role_codex_session_id(
        &state.db_path,
        &intent.repo_full_name,
        intent.issue_number,
        &intent.role,
    )
    .map_err(|err| DispatchError::Db(err.to_string()))?;
    let lock_path = repo::acquire_repo_lock(&state.db_path, &intent.repo_full_name)?;

    let repo_ref = RepoRef::parse(&intent.repo_full_name)
        .map_err(|_| DispatchError::InvalidIssueRef(intent.repo_full_name.clone()))?;
    let issue_ref = IssueRef {
        repo: repo_ref,
        number: intent.issue_number,
    };
    let issue = fetch_issue(state.clone(), issue_ref.clone()).await?;

    // Fresh sessions must be treated as amnesiac: we do not assume any prior role memory,
    // even if a cursor exists (e.g. if a previous session was lost or corrupted).
    let prompt_mode_fresh = issue_session_id.is_none();
    let previous_event_cursor = if prompt_mode_fresh {
        None
    } else {
        db::issue_role_cursor_event_id(
            &state.db_path,
            &intent.repo_full_name,
            intent.issue_number,
            &intent.role,
        )
        .map_err(|err| DispatchError::Db(err.to_string()))?
    };
    let delta_limit = if prompt_mode_fresh { 2000 } else { 200 };
    let delta_rows = db::issue_delta_rows(
        &state.db_path,
        &intent.repo_full_name,
        intent.issue_number,
        previous_event_cursor,
        current_event_id,
        delta_limit,
    )
    .map_err(|err| DispatchError::Db(err.to_string()))?;
    let issue_delta_md = if prompt_mode_fresh {
        db::render_issue_history(&delta_rows, delta_limit)
    } else {
        db::summarize_issue_delta(&delta_rows)
    };

    let now = Utc::now().to_rfc3339();
    let dispatch_id = match db::reserve_dispatch_starting(
        &state.db_path,
        db::DispatchInsert {
            decision_id,
            repo_full_name: &intent.repo_full_name,
            issue_number: intent.issue_number,
            actor_login: record.actor_login.as_deref(),
            directive: &intent.directive,
            target_role: &intent.role,
            started_at: &now,
        },
    )
    .map_err(|err| DispatchError::Db(err.to_string()))?
    {
        db::DispatchReservation::Started(dispatch_id) => dispatch_id,
        db::DispatchReservation::InFlightIssue(dispatch_id)
        | db::DispatchReservation::InFlightRepo(dispatch_id) => {
            let _ = fs::remove_file(&lock_path);
            return Err(DispatchError::IssueDispatchInFlight {
                repo_full_name: intent.repo_full_name.clone(),
                issue_number: intent.issue_number,
                dispatch_id,
            });
        }
    };

    let plan_build: Result<DispatchPlan, DispatchError> = async {
        let run_dir = repo::run_root(&state.db_path)?.join(format!("dispatch-{dispatch_id}"));
        fs::create_dir_all(&run_dir)
            .map_err(|err| DispatchError::Io(format!("failed to create run dir: {err}")))?;
        let issue_title = issue.title;
        let issue_body = issue.body.unwrap_or_default();
        let issue_url = issue.html_url;

        let base_repo_checkout = repo::ensure_repo_checkout(state, &role, &intent.repo_full_name)?;
        if db::repo_labels_ensured_at(&state.db_path, &intent.repo_full_name)
            .unwrap_or(None)
            .is_none()
        {
            let repo_full_name = intent.repo_full_name.clone();
            let forgejoctl_bin = dispatch_config.forgejoctl_bin.clone();
            let config_file = state.forgejo_config_file.clone();
            let token_file = role.token_file.clone();
            let ensure_outcome = tokio::task::spawn_blocking(move || {
                forgejoctl_cmd::run_forgejoctl(
                    &forgejoctl_bin,
                    config_file.as_deref(),
                    &token_file,
                    &["repo", "ensure", &repo_full_name],
                )
            })
            .await;
            match ensure_outcome {
                Ok(Ok(())) => {
                    let _ = db::update_repo_labels_ensured(
                        &state.db_path,
                        &intent.repo_full_name,
                        true,
                        None,
                    );
                }
                Ok(Err(err)) => {
                    let _ = db::update_repo_labels_ensured(
                        &state.db_path,
                        &intent.repo_full_name,
                        false,
                        Some(&err.to_string()),
                    );
                }
                Err(err) => {
                    let _ = db::update_repo_labels_ensured(
                        &state.db_path,
                        &intent.repo_full_name,
                        false,
                        Some(&format!("ensure join failure: {err}")),
                    );
                }
            }
        }
        let (workdir, git_remote, git_base, git_branch) =
            if lexicon::directive_uses_worktree(&intent.directive) {
                let git_remote = repo_binding.map_or_else(
                    || repo::DEFAULT_GIT_REMOTE.to_string(),
                    |binding| binding.git_remote.clone(),
                );
                let git_base = repo_binding.map_or_else(
                    || repo::DEFAULT_GIT_BASE_BRANCH.to_string(),
                    |binding| binding.git_base.clone(),
                );
                let git_branch = repo::dispatch_worktree_branch(
                    &intent.repo_full_name,
                    intent.issue_number,
                    dispatch_id,
                    directive_name,
                );
                let workdir = run_dir.join("worktree");
                repo::create_dispatch_worktree(
                    &state.db_path,
                    &role.token_file,
                    &base_repo_checkout,
                    &workdir,
                    &git_branch,
                    &git_remote,
                    &git_base,
                )?;
                (workdir, git_remote, git_base, git_branch)
            } else {
                (
                    base_repo_checkout,
                    repo::DEFAULT_GIT_REMOTE.to_string(),
                    repo::DEFAULT_GIT_BASE_BRANCH.to_string(),
                    String::new(),
                )
            };
        let principal_workdir = if intent.directive == lexicon::DIRECTIVE_IMPL {
            repo_binding.map(|binding| binding.local_path.clone())
        } else {
            None
        };
        let sidecar = if lexicon::directive_uses_worktree(&intent.directive) {
            if let Some(sidecar_repo) = repo_binding.and_then(|binding| binding.sidecar_repo.as_ref())
            {
                let sidecar_full_name = sidecar_repo.to_string();
                let sidecar_binding = dispatch_config
                    .repo_bindings
                    .get(&sidecar_full_name)
                    .ok_or_else(|| {
                        DispatchError::Io(format!(
                            "repo binding for {} references sidecar repo {sidecar_full_name} but it is missing from dispatch config",
                            intent.repo_full_name
                        ))
                    })?;
                let sidecar_checkout =
                    repo::ensure_repo_checkout(state, &role, sidecar_full_name.as_str())?;

                let sidecar_git_branch = repo::dispatch_worktree_branch(
                    sidecar_full_name.as_str(),
                    intent.issue_number,
                    dispatch_id,
                    directive_name,
                );
                let sidecar_dir = run_dir.join("sidecar").join(format!(
                    "{}__{}",
                    sidecar_repo.owner, sidecar_repo.repo
                ));
                if let Some(parent) = sidecar_dir.parent() {
                    fs::create_dir_all(parent).map_err(|err| {
                        DispatchError::Io(format!(
                            "failed creating sidecar worktree parent {}: {err}",
                            parent.display()
                        ))
                    })?;
                }
                repo::create_dispatch_worktree(
                    &state.db_path,
                    &role.token_file,
                    &sidecar_checkout,
                    &sidecar_dir,
                    &sidecar_git_branch,
                    &sidecar_binding.git_remote,
                    &sidecar_binding.git_base,
                )?;
                let sidecar_principal_workdir = if intent.directive == lexicon::DIRECTIVE_IMPL {
                    Some(sidecar_binding.local_path.clone())
                } else {
                    None
                };
                Some(DispatchSidecarPlan {
                    repo_full_name: sidecar_full_name,
                    workdir: sidecar_dir,
                    principal_workdir: sidecar_principal_workdir,
                    git_remote: sidecar_binding.git_remote.clone(),
                    git_base: sidecar_binding.git_base.clone(),
                    git_branch: sidecar_git_branch,
                })
            } else {
                None
            }
        } else {
            None
        };

        Ok(DispatchPlan {
            actor,
            event_type: record.event_type.clone(),
            directive,
            role,
            workdir,
            principal_workdir,
            sidecar,
            git_remote,
            git_base,
            git_branch,
            intent,
            issue_ref,
            issue_title,
            issue_body,
            issue_url,
            issue_session_id,
            issue_delta_md,
            dispatch_id,
            lock_path: lock_path.clone(),
            run_dir,
        })
    }
    .await;

    match plan_build {
        Ok(plan) => Ok(plan),
        Err(err) => {
            fail_dispatch_start(&state.db_path, dispatch_id, &lock_path, &err);
            Err(err)
        }
    }
}

fn materialize_run_artifacts(
    state: &AppState,
    dispatch_config: &DispatchConfig,
    plan: &DispatchPlan,
) -> Result<DispatchRunArtifacts, DispatchError> {
    let orders_template = fs::read_to_string(&plan.directive.prompt_file).map_err(|err| {
        DispatchError::Io(format!(
            "failed reading prompt {}: {err}",
            plan.directive.prompt_file.display()
        ))
    })?;
    let orders_md = template::render_prompt(&orders_template, &[])?;

    let (prompt_mode, envelope_path) = if plan.issue_session_id.is_some() {
        (
            "followup",
            &dispatch_config.prompt_envelopes.followup_envelope,
        )
    } else {
        ("fresh", &dispatch_config.prompt_envelopes.fresh_envelope)
    };

    let preamble_md = if prompt_mode == "fresh" {
        render_fresh_preamble(
            &dispatch_config.prompt_envelopes,
            &dispatch_config.rank_acl,
            &plan.intent.role,
        )?
    } else {
        String::new()
    };

    let dispatch_md = render_dispatch_md(&dispatch_config.prompt_envelopes, plan, prompt_mode)?;
    let issue_md = render_issue_md(&dispatch_config.prompt_envelopes, plan, prompt_mode)?;
    let reading_outcome = reading_material::build_reading_material(
        &dispatch_config.reading_material,
        &plan.intent.role,
        &plan.intent.directive,
        prompt_mode,
        &plan.workdir,
        &dispatch_config.repo_bindings,
    );
    let reading_material_md = reading_outcome.markdown;

    let envelope_template = fs::read_to_string(envelope_path).map_err(|err| {
        DispatchError::Io(format!(
            "failed reading prompt envelope {}: {err}",
            envelope_path.display()
        ))
    })?;
    let prompt = template::render_prompt(
        &envelope_template,
        &[
            ("preamble_md", &preamble_md),
            ("dispatch_md", &dispatch_md),
            ("reading_material_md", &reading_material_md),
            ("issue_md", &issue_md),
            ("orders_md", &orders_md),
        ],
    )?;

    let prompt_path = plan.run_dir.join("prompt.md");
    fs::write(&prompt_path, prompt)
        .map_err(|err| DispatchError::Io(format!("failed writing prompt: {err}")))?;
    fs::write(plan.run_dir.join("prompt_mode.txt"), prompt_mode)
        .map_err(|err| DispatchError::Io(format!("failed writing prompt mode: {err}")))?;
    let doc_plan_path = plan.run_dir.join("doc-plan.json");
    let doc_plan_json = serde_json::to_string_pretty(&reading_outcome.doc_plan)
        .map_err(|err| DispatchError::Io(format!("failed serializing doc plan: {err}")))?;
    fs::write(&doc_plan_path, doc_plan_json)
        .map_err(|err| DispatchError::Io(format!("failed writing doc plan: {err}")))?;

    let spec_path = plan.run_dir.join("run-spec.json");
    let summary_path = plan.run_dir.join("summary.md");
    let completion_path = plan.run_dir.join("completion.md");
    let last_message_path = plan.run_dir.join("last_message.md");
    let codex_log_path = plan.run_dir.join("codex.log");
    let marker_path = plan.run_dir.join("start.marker");
    let issue_ref_text = format!(
        "{}#{}",
        plan.intent.repo_full_name, plan.intent.issue_number
    );

    let issue_session_id = plan
        .issue_session_id
        .as_deref()
        .filter(|value| !value.trim().is_empty())
        .map(CodexSessionId::parse)
        .transpose()
        .map_err(|err| DispatchError::Io(format!("invalid codex session id: {err:#}")))?;

    let spec = DispatchExecSpecV1 {
        version: 1,
        dispatch_id: plan.dispatch_id,
        db_path: state.db_path.clone(),
        lock_path: plan.lock_path.clone(),
        run_dir: plan.run_dir.clone(),
        prompt_path,
        summary_path,
        completion_path,
        last_message_path,
        codex_log_path,
        marker_path,
        issue_ref: issue_ref_text,
        issue_title: plan.issue_title.clone(),
        issue_url: plan.issue_url.clone(),
        forgejoctl_bin: dispatch_config.forgejoctl_bin.clone(),
        forgejo_config_file: state.forgejo_config_file.clone(),
        token_file: plan.role.token_file.clone(),
        control_token_file: dispatch_config
            .control_plane
            .as_ref()
            .map(|control| control.token_file.clone()),
        workdir: plan.workdir.clone(),
        principal_workdir: plan.principal_workdir.clone(),
        sidecar: plan.sidecar.as_ref().map(|sidecar| DispatchExecSidecarV1 {
            repo_full_name: sidecar.repo_full_name.clone(),
            workdir: sidecar.workdir.clone(),
            principal_workdir: sidecar.principal_workdir.clone(),
            git_remote: sidecar.git_remote.clone(),
            git_base: sidecar.git_base.clone(),
            git_branch: sidecar.git_branch.clone(),
        }),
        codex_sandbox: codex_sandbox_for_directive(&plan.intent.directive),
        git_remote: plan.git_remote.clone(),
        git_base: plan.git_base.clone(),
        git_branch: plan.git_branch.clone(),
        codex_bin: plan.role.codex_bin.clone(),
        codex_role_arg: plan.role.codex_role_arg.clone(),
        issue_session_id,
        directive: plan.intent.directive.clone(),
        role_name: plan.intent.role.clone(),
        timeout_sec: plan.directive.timeout_sec,
    };

    match state.dispatch_mode {
        DispatchMode::Exec => spec
            .write_json(&spec_path)
            .map_err(|err| DispatchError::Io(format!("failed writing run spec: {err:#}")))?,
        DispatchMode::DryRun => return Err(DispatchError::ConfigNotLoaded),
    }

    Ok(DispatchRunArtifacts { spec_path })
}

fn launch_dispatch_backend(
    state: &AppState,
    dispatch_config: &DispatchConfig,
    plan: &DispatchPlan,
    artifacts: &DispatchRunArtifacts,
) -> Result<RunHandle, DispatchError> {
    match state.dispatch_backend {
        DispatchBackend::Systemd => SystemdBackendAdapter.launch(dispatch_config, plan, artifacts),
        DispatchBackend::Local => LocalBackendAdapter.launch(dispatch_config, plan, artifacts),
    }
}

pub(super) async fn dispatch_issue(
    state: AppState,
    decision_id: i64,
    current_event_id: i64,
    record: &EventRecord,
    decision: &DecisionRecord,
) -> Result<(), DispatchError> {
    let span = info_span!(
        "dispatch_issue",
        repo = %record.repo_full_name,
        issue = record.issue_number.unwrap_or_default(),
        event_id = current_event_id,
        decision_id = decision_id,
        backend = state.dispatch_backend.as_str(),
        mode = state.dispatch_mode.as_str(),
    );
    let _entered = span.enter();
    let dispatch_config = state
        .dispatch_config
        .as_ref()
        .ok_or(DispatchError::ConfigNotLoaded)?;
    let phase_plan_start = Instant::now();
    let plan = match plan_dispatch(
        &state,
        dispatch_config,
        decision_id,
        current_event_id,
        record,
        decision,
    )
    .await
    {
        Ok(plan) => {
            record_phase_latency_ms(
                "plan",
                phase_plan_start.elapsed().as_secs_f64() * 1000.0,
                "ok",
            );
            plan
        }
        Err(err) => {
            record_phase_latency_ms(
                "plan",
                phase_plan_start.elapsed().as_secs_f64() * 1000.0,
                "error",
            );
            return Err(err);
        }
    };

    let phase_materialize_start = Instant::now();
    let artifacts = match materialize_run_artifacts(&state, dispatch_config, &plan) {
        Ok(artifacts) => {
            record_phase_latency_ms(
                "materialize",
                phase_materialize_start.elapsed().as_secs_f64() * 1000.0,
                "ok",
            );
            artifacts
        }
        Err(err) => {
            record_phase_latency_ms(
                "materialize",
                phase_materialize_start.elapsed().as_secs_f64() * 1000.0,
                "error",
            );
            fail_dispatch_start(&state.db_path, plan.dispatch_id, &plan.lock_path, &err);
            return Err(err);
        }
    };

    let phase_launch_start = Instant::now();
    let launch_result = launch_dispatch_backend(&state, dispatch_config, &plan, &artifacts);
    let run_handle = match launch_result {
        Ok(handle) => handle,
        Err(err) => {
            record_phase_latency_ms(
                "launch",
                phase_launch_start.elapsed().as_secs_f64() * 1000.0,
                "error",
            );
            fail_dispatch_start(&state.db_path, plan.dispatch_id, &plan.lock_path, &err);
            return Err(err);
        }
    };
    record_phase_latency_ms(
        "launch",
        phase_launch_start.elapsed().as_secs_f64() * 1000.0,
        "ok",
    );
    let phase_finalize_start = Instant::now();
    db::update_dispatch_running(
        &state.db_path,
        plan.dispatch_id,
        &run_handle,
        &plan.run_dir,
        &plan.lock_path,
    )
    .map_err(|err| DispatchError::Db(err.to_string()))?;
    record_phase_latency_ms(
        "mark_running",
        phase_finalize_start.elapsed().as_secs_f64() * 1000.0,
        "ok",
    );
    info!(
        dispatch_id = plan.dispatch_id,
        repo = %plan.intent.repo_full_name,
        issue = plan.intent.issue_number,
        directive = %plan.intent.directive,
        role = %plan.intent.role,
        "dispatch launch complete"
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use chrono::{Duration as ChronoDuration, Utc};

    use std::path::PathBuf;

    use super::{DispatchBackendKind, DispatchError, DispatchState, db, resolve_orchd_exe_from};

    fn inflight_dispatch(status: DispatchState, started_at: String) -> db::InflightDispatch {
        db::InflightDispatch {
            id: 1,
            repo_full_name: "main/orchd-debug".to_string(),
            issue_number: 1,
            status,
            started_at,
            backend_kind: Some(DispatchBackendKind::Systemd),
            backend_ref: Some("orchd-dispatch-1.service".to_string()),
            lock_path: None,
        }
    }

    #[test]
    fn resolve_orchd_exe_strips_deleted_suffix_when_stripped_path_exists() {
        let dir = tempfile::tempdir().expect("create tempdir");
        let exe_path = dir.path().join("orchd");
        std::fs::write(&exe_path, b"").expect("create fake exe");

        let deleted_path = dir.path().join("orchd (deleted)");
        let resolved = resolve_orchd_exe_from(Some(deleted_path), None, None).expect("resolve exe");
        assert_eq!(resolved, exe_path);
    }

    #[test]
    fn resolve_orchd_exe_searches_path_for_bare_argv0() {
        let dir = tempfile::tempdir().expect("create tempdir");
        let exe_path = dir.path().join("orchd");
        std::fs::write(&exe_path, b"").expect("create fake exe");

        let path_env = std::env::join_paths([dir.path()]).expect("join PATH");
        let resolved = resolve_orchd_exe_from(None, Some(PathBuf::from("orchd")), Some(path_env))
            .expect("resolve exe");
        assert_eq!(resolved, exe_path);
    }

    #[test]
    fn starting_dispatch_is_not_stale_within_grace_period() {
        let started_at = (Utc::now() - ChronoDuration::seconds(5)).to_rfc3339();
        let dispatch = inflight_dispatch(DispatchState::Starting, started_at);
        assert!(!super::is_stale_starting_dispatch(
            &dispatch,
            "main/orchd-debug",
            1
        ));
    }

    #[test]
    fn starting_dispatch_with_invalid_timestamp_is_stale() {
        let dispatch = inflight_dispatch(DispatchState::Starting, "invalid-timestamp".to_string());
        assert!(super::is_stale_starting_dispatch(
            &dispatch,
            "main/orchd-debug",
            1
        ));
    }

    #[test]
    fn starting_dispatch_without_backend_ref_is_stale_after_grace_period() {
        let started_at = (Utc::now()
            - ChronoDuration::seconds(super::STARTING_DISPATCH_STALE_AFTER_SEC + 5))
        .to_rfc3339();
        let mut dispatch = inflight_dispatch(DispatchState::Starting, started_at);
        dispatch.backend_ref = None;
        assert!(super::is_stale_starting_dispatch(
            &dispatch,
            "main/orchd-debug",
            1
        ));
    }

    #[test]
    fn render_prompt_allows_brace_literals_in_injected_values() {
        let rendered = super::template::render_prompt(
            "{{issue_md}}",
            &[(
                "issue_md",
                "observed literal {{role_card_md}} in issue text",
            )],
        )
        .expect("expected literal braces in injected values to be preserved");
        assert_eq!(rendered, "observed literal {{role_card_md}} in issue text");
    }

    #[test]
    fn render_prompt_rejects_missing_template_values() {
        let err = super::template::render_prompt("{{missing_key}}", &[])
            .expect_err("expected unresolved template token error");
        assert!(matches!(err, DispatchError::PromptTemplate(_)));
    }

    #[test]
    fn investigate_runs_read_only() {
        let sandbox = super::codex_sandbox_for_directive(super::lexicon::DIRECTIVE_INVESTIGATE);
        assert!(matches!(sandbox, super::CodexSandbox::ReadOnly));
    }
}
