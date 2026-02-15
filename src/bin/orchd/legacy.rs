use std::fs;
use std::fs::OpenOptions;
use std::io::Write as _;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::Duration as StdDuration;
use std::time::Instant;

use anyhow::{Context, Result, anyhow};
use axum::body::Bytes;
use axum::extract::State;
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use chrono::{DateTime, Duration as ChronoDuration, Utc};
use clap::Parser;
use rusqlite::{Connection, OptionalExtension, Transaction, TransactionBehavior, params};
use serde_json::json;
use tracing::{info, info_span};

use forgejo_agent::api::ForgejoClient;
use forgejo_agent::config::AgentConfig;
use forgejo_agent::orchd_dispatch_core::{
    DispatchBackendKind, DispatchEventKind, DispatchIntentV1, DispatchPolicyOutcome, DispatchState,
    PolicyDecision as DispatchPolicyDecision, RunHandle, reduce_dispatch_state,
};
use forgejo_agent::types::{ApiIssue, IssueRef, OrchdRuntimeState, RepoRef};

use super::cli::{Cli, DispatchBackend, DispatchMode, FinalizeDispatchArgs, OrchdCommand};
use super::dispatch_config::{
    DispatchConfig, DispatchDirectiveConfig, DispatchRoleConfig, load_dispatch_config,
};
use super::errors::{DispatchError, runtime_state_for_dispatch_error};
use super::paths::expand_tilde_path;
use super::state::{
    AppState, DecisionRecord, ErrorEnvelope, EventRecord, HealthEnvelope, IssueEventDeltaRow,
    WebhookOutcome, WebhookPayload,
};
use super::telemetry::{init_telemetry, log_line, record_phase_latency_ms};
use super::tmux::{
    TmuxRunScriptInputs, build_tmux_exec_run_script, build_tmux_tui_run_script,
    issue_tmux_window_name, tmux_spawn_or_respawn_window, tmux_window_has_live_pane,
};
use super::webhook::{
    decide, extract_event_context, extract_header, load_secret, synthetic_delivery_id,
    verify_signature,
};

const STARTING_DISPATCH_STALE_AFTER_SEC: i64 = 120;

enum DispatchReservation {
    Started(i64),
    InFlightIssue(i64),
    InFlightRepo(i64),
}

#[derive(Clone)]
struct CommentIdentity {
    forgejoctl_bin: PathBuf,
    config_file: Option<PathBuf>,
    token_file: PathBuf,
}

struct DispatchInsert {
    decision_id: i64,
    repo_full_name: String,
    issue_number: u64,
    actor_login: Option<String>,
    directive: String,
    target_role: String,
    started_at: String,
}

#[derive(Debug)]
struct InflightDispatch {
    id: i64,
    status: String,
    started_at: String,
    backend_kind: Option<String>,
    backend_ref: Option<String>,
    tmux_session: Option<String>,
    tmux_window: Option<String>,
    lock_path: Option<String>,
}

#[derive(Debug, Clone)]
struct DispatchPlan {
    actor: String,
    event_type: String,
    directive: DispatchDirectiveConfig,
    role: DispatchRoleConfig,
    workdir: PathBuf,
    git_remote: String,
    git_base: String,
    git_branch: String,
    intent: DispatchIntentV1,
    issue_ref: IssueRef,
    issue_title: String,
    issue_body: String,
    issue_url: String,
    issue_session_id: Option<String>,
    issue_delta_summary: String,
    dispatch_id: i64,
    lock_path: PathBuf,
    run_dir: PathBuf,
    tmux_window: String,
}

#[derive(Debug, Clone)]
struct DispatchRunArtifacts {
    script_path: PathBuf,
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
        dispatch: &InflightDispatch,
        repo_full_name: &str,
        issue_number: u64,
    ) -> Result<bool, DispatchError>;
}

#[derive(Debug, Clone, Copy)]
struct TmuxBackendAdapter;

impl DispatchBackendAdapter for TmuxBackendAdapter {
    fn launch(
        &self,
        dispatch_config: &DispatchConfig,
        plan: &DispatchPlan,
        artifacts: &DispatchRunArtifacts,
    ) -> Result<RunHandle, DispatchError> {
        tmux_spawn_or_respawn_window(
            &dispatch_config.tmux.session,
            &plan.tmux_window,
            &artifacts.script_path,
            dispatch_config.tmux.remain_on_exit,
        )?;
        Ok(RunHandle {
            backend_kind: DispatchBackendKind::Tmux,
            backend_ref: format!("{}:{}", dispatch_config.tmux.session, plan.tmux_window),
        })
    }

    fn probe(
        &self,
        dispatch: &InflightDispatch,
        repo_full_name: &str,
        issue_number: u64,
    ) -> Result<bool, DispatchError> {
        let session = dispatch.tmux_session.as_deref().ok_or_else(|| {
            DispatchError::Tmux("missing tmux session for tmux-backed dispatch".to_string())
        })?;
        let window = dispatch
            .tmux_window
            .clone()
            .unwrap_or_else(|| issue_tmux_window_name(repo_full_name, issue_number));
        tmux_window_has_live_pane(session, &window)
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
        let log_path = artifacts.script_path.parent().map_or_else(
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
        let child = Command::new("/usr/bin/env")
            .arg("bash")
            .arg(&artifacts.script_path)
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
        dispatch: &InflightDispatch,
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

pub(super) fn run_entry() -> Result<()> {
    init_telemetry();
    let cli = Cli::parse();
    if let Some(command) = cli.command {
        // Subcommands are intentionally synchronous to avoid mixing reqwest::blocking
        // with a Tokio runtime.
        return run_command(command);
    }
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .context("failed to build tokio runtime")?;
    runtime.block_on(run_server(cli))
}

async fn run_server(cli: Cli) -> Result<()> {
    let listen_addr: SocketAddr = cli
        .listen
        .parse()
        .with_context(|| format!("invalid --listen value: {}", cli.listen))?;
    let webhook_url = {
        let ip = listen_addr.ip();
        let ip = if ip.is_unspecified() {
            std::net::IpAddr::V4(std::net::Ipv4Addr::LOCALHOST)
        } else {
            ip
        };
        format!("http://{}:{}/webhook", ip, listen_addr.port())
    };
    let config_override = cli.config.clone();
    let cfg = AgentConfig::load(cli.config, cli.token_file)?;
    let dispatch_config_path = expand_tilde_path(&cli.dispatch_config)?;
    let dispatch_config = match cli.dispatch_mode {
        DispatchMode::DryRun => None,
        DispatchMode::TmuxExec | DispatchMode::TmuxTui => {
            Some(load_dispatch_config(&dispatch_config_path)?)
        }
    };

    let db_path = expand_tilde_path(&cli.db_path)?;
    init_db(&db_path)?;
    let webhook_secret = load_secret(cli.webhook_secret_file.as_deref())?;
    let reconcile_repo = cli
        .reconcile_repo
        .unwrap_or_else(|| cfg.default_repo.clone());

    let state = AppState {
        db_path,
        webhook_secret,
        webhook_url: webhook_url.clone(),
        cfg,
        forgejo_config_file: config_override,
        reconcile_repo,
        comment_echo: !cli.no_comment_echo,
        dispatch_mode: cli.dispatch_mode,
        dispatch_backend: cli.dispatch_backend,
        dispatch_config,
    };
    let mode_name = state.dispatch_mode.as_str();
    let backend_name = state.dispatch_backend.as_str();

    let heartbeat_state = state.clone();
    tokio::spawn(async move {
        run_heartbeat_loop(heartbeat_state, cli.heartbeat_sec).await;
    });

    let reconcile_state = state.clone();
    tokio::spawn(async move {
        run_reconcile_loop(reconcile_state, cli.reconcile_sec).await;
    });

    let queue_state = state.clone();
    tokio::spawn(async move {
        run_dispatch_queue_loop(queue_state, cli.heartbeat_sec).await;
    });

    let ensure_state = state.clone();
    let app = Router::new()
        .route("/healthz", get(healthz_handler))
        .route("/webhook", post(webhook_handler))
        .with_state(state);
    let listener = tokio::net::TcpListener::bind(listen_addr)
        .await
        .with_context(|| format!("failed to bind {listen_addr}"))?;
    if let Err(err) = ensure_repo_webhooks_for_default_owner(&ensure_state).await {
        log_line(
            "repo_webhooks_ensure_failed",
            json!({
                "owner": ensure_state.cfg.default_repo.owner,
                "url": ensure_state.webhook_url,
                "error": err.to_string(),
            }),
        );
    } else {
        log_line(
            "repo_webhooks_ensured",
            json!({
                "owner": ensure_state.cfg.default_repo.owner,
                "url": ensure_state.webhook_url,
            }),
        );
    }
    log_line(
        "startup",
        json!({
            "listen": listen_addr.to_string(),
            "heartbeat_sec": cli.heartbeat_sec,
            "reconcile_sec": cli.reconcile_sec,
            "mode": mode_name,
            "backend": backend_name,
            "dispatch_config": dispatch_config_path.to_string_lossy(),
        }),
    );

    axum::serve(listener, app)
        .with_graceful_shutdown(shutdown_signal())
        .await
        .context("orchd server failed")?;

    Ok(())
}

fn run_command(command: OrchdCommand) -> Result<()> {
    match command {
        OrchdCommand::FinalizeDispatch(args) => finalize_dispatch_command(args),
    }
}

async fn shutdown_signal() {
    if let Err(err) = tokio::signal::ctrl_c().await {
        log_line(
            "shutdown_signal_error",
            json!({
                "error": err.to_string(),
            }),
        );
    }
}

async fn healthz_handler() -> Json<HealthEnvelope> {
    Json(HealthEnvelope { status: "ok" })
}

fn hook_url(hook: &serde_json::Value) -> Option<&str> {
    hook.get("config")
        .and_then(|cfg| cfg.get("url"))
        .and_then(serde_json::Value::as_str)
        .or_else(|| hook.get("url").and_then(serde_json::Value::as_str))
}

fn hook_is_active(hook: &serde_json::Value) -> bool {
    hook.get("active")
        .and_then(serde_json::Value::as_bool)
        .unwrap_or(true)
}

async fn ensure_repo_webhooks_for_default_owner(state: &AppState) -> Result<()> {
    let cfg = state.cfg.clone();
    let owner = state.cfg.default_repo.owner.clone();
    let db_path = state.db_path.clone();
    let secret = state
        .webhook_secret
        .as_ref()
        .map(|bytes| String::from_utf8_lossy(bytes).to_string());
    let webhook_url = state.webhook_url.clone();
    tokio::task::spawn_blocking(move || {
        let api = ForgejoClient::new(&cfg)?;
        let repos = api.list_user_repos(&cfg, &owner, 1000)?;
        for repo in repos {
            let Some(repo_full_name) = repo.get("full_name").and_then(serde_json::Value::as_str)
            else {
                continue;
            };
            let _ = upsert_repo_seen(&db_path, repo_full_name);

            let Ok(repo_ref) = RepoRef::parse(repo_full_name) else {
                continue;
            };
            let hooks = api.list_repo_hooks(&cfg, &repo_ref)?;
            let exists = hooks.iter().any(|hook| {
                hook_is_active(hook)
                    && hook_url(hook).is_some_and(|url| url == webhook_url.as_str())
            });
            if exists {
                continue;
            }
            let _ = api.create_repo_hook(
                &cfg,
                &repo_ref,
                &webhook_url,
                secret.as_deref(),
                &["issues", "issue_comment"],
            )?;
        }
        Ok(())
    })
    .await
    .context("repo webhook ensure join failure")?
}

async fn webhook_handler(
    State(state): State<AppState>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    match process_webhook(&state, &headers, &body).await {
        Ok(outcome) => {
            let code = if outcome.duplicate {
                StatusCode::OK
            } else {
                StatusCode::ACCEPTED
            };
            (code, Json(outcome)).into_response()
        }
        Err(err) => {
            log_line(
                "webhook_error",
                json!({
                    "error": err.to_string(),
                }),
            );
            (
                StatusCode::BAD_REQUEST,
                Json(ErrorEnvelope {
                    error: err.to_string(),
                }),
            )
                .into_response()
        }
    }
}

async fn process_webhook(
    state: &AppState,
    headers: &HeaderMap,
    body: &[u8],
) -> Result<WebhookOutcome> {
    verify_signature(state.webhook_secret.as_deref(), headers, body)?;

    let event_type = extract_header(headers, &["x-forgejo-event", "x-gitea-event"])
        .unwrap_or_else(|| "unknown".to_string());
    let delivery_id = extract_header(headers, &["x-forgejo-delivery", "x-gitea-delivery"])
        .unwrap_or_else(|| synthetic_delivery_id(body));

    let payload: WebhookPayload =
        serde_json::from_slice(body).context("invalid webhook payload JSON")?;
    let context = extract_event_context(&event_type, &payload);

    let record = EventRecord {
        delivery_id: delivery_id.clone(),
        event_type: event_type.clone(),
        repo_full_name: context
            .as_ref()
            .map_or_else(|| "<unknown>".to_string(), |ctx| ctx.repo_full_name.clone()),
        issue_number: context.as_ref().and_then(|ctx| ctx.issue_number),
        action: payload.action.clone(),
        actor_login: context.as_ref().and_then(|ctx| ctx.actor_login.clone()),
        event_text: context.as_ref().and_then(|ctx| ctx.text.clone()),
        source_comment_id: context.as_ref().and_then(|ctx| ctx.source_comment_id),
        source_created_at: context
            .as_ref()
            .and_then(|ctx| ctx.source_created_at.clone()),
        raw_json: String::from_utf8_lossy(body).to_string(),
    };

    let Some(event_id) = insert_event(&state.db_path, &record)? else {
        return Ok(WebhookOutcome {
            status: "duplicate".to_string(),
            delivery_id,
            event_type,
            decision: "duplicate".to_string(),
            reason_code: "duplicate_delivery".to_string(),
            duplicate: true,
        });
    };
    let _ = upsert_repo_seen(&state.db_path, &record.repo_full_name);

    let decision = decide(&event_type, context.as_ref());
    let decision_id = insert_decision(&state.db_path, event_id, &record, &decision)?;

    let mut status_projected = false;
    let mut status_error: Option<String> = None;
    if decision.decision == "accepted" {
        if let Some(issue_number) = record.issue_number {
            let dispatch_identity = dispatch_comment_identity(state, &decision);
            if let Err(err) = project_issue_runtime_state(
                state.clone(),
                &record.repo_full_name,
                issue_number,
                OrchdRuntimeState::Queued,
                dispatch_identity.clone(),
            )
            .await
            {
                status_error = Some(err.to_string());
            } else {
                status_projected = true;
            }
            match state.dispatch_mode {
                DispatchMode::DryRun => {
                    if state.comment_echo {
                        let comment_body = format!(
                            "orchd: accepted (dry-run) directive={} role={} reason={} would_dispatch=true delivery={}",
                            decision.directive.as_deref().unwrap_or("-"),
                            decision.target_role.as_deref().unwrap_or("-"),
                            decision.reason_code,
                            record.delivery_id
                        );
                        match post_issue_comment(
                            state.clone(),
                            &record.repo_full_name,
                            issue_number,
                            comment_body,
                        )
                        .await
                        {
                            Ok(()) => {}
                            Err(err) => {
                                if status_error.is_none() {
                                    status_error = Some(err.to_string());
                                }
                            }
                        }
                    }
                }
                DispatchMode::TmuxExec | DispatchMode::TmuxTui => {
                    let defer_impl = match decision.directive.as_deref() {
                        Some("impl") => match latest_repo_inflight_impl_dispatch_id(
                            &state.db_path,
                            &record.repo_full_name,
                        ) {
                            Ok(Some(inflight)) => {
                                log_line(
                                    "dispatch_deferred_repo_busy",
                                    json!({
                                        "repo": record.repo_full_name,
                                        "issue_number": issue_number,
                                        "directive": "impl",
                                        "inflight_dispatch_id": inflight,
                                    }),
                                );
                                true
                            }
                            Ok(None) => false,
                            Err(err) => {
                                status_error = Some(format!(
                                    "failed checking repo inflight impl dispatch: {err}"
                                ));
                                false
                            }
                        },
                        _ => false,
                    };

                    if !defer_impl {
                        match dispatch_issue(
                            state.clone(),
                            decision_id,
                            event_id,
                            &record,
                            &decision,
                        )
                        .await
                        {
                            Ok(()) => {
                                if let Err(err) = project_issue_runtime_state(
                                    state.clone(),
                                    &record.repo_full_name,
                                    issue_number,
                                    OrchdRuntimeState::Running,
                                    dispatch_identity.clone(),
                                )
                                .await
                                {
                                    if status_error.is_none() {
                                        status_error = Some(err.to_string());
                                    }
                                } else {
                                    status_projected = true;
                                }
                                if let Some(role_name) = decision.target_role.as_deref()
                                    && let Err(err) = upsert_issue_role_cursor_event_id(
                                        &state.db_path,
                                        &record.repo_full_name,
                                        issue_number,
                                        role_name,
                                        event_id,
                                    )
                                    && status_error.is_none()
                                {
                                    status_error = Some(format!(
                                        "failed updating issue cursor for role {role_name}: {err}"
                                    ));
                                }
                            }
                            Err(err) => {
                                let projection = project_issue_runtime_state(
                                    state.clone(),
                                    &record.repo_full_name,
                                    issue_number,
                                    runtime_state_for_dispatch_error(&err),
                                    dispatch_identity.clone(),
                                )
                                .await;
                                match projection {
                                    Ok(()) => {
                                        status_projected = true;
                                    }
                                    Err(projection_err) => {
                                        if status_error.is_none() {
                                            status_error = Some(projection_err.to_string());
                                        }
                                    }
                                }
                                if status_error.is_none() {
                                    status_error =
                                        Some(format!("dispatch {}: {}", err.reason_code(), err));
                                }
                            }
                        }
                    }
                }
            }
        } else {
            status_error = Some("missing issue number".to_string());
        }
    }

    update_decision_comment_status(&state.db_path, decision_id, status_projected, status_error)?;

    log_line(
        "decision",
        json!({
            "delivery_id": record.delivery_id,
            "event_type": record.event_type,
            "repo": record.repo_full_name,
            "issue_number": record.issue_number,
            "actor": record.actor_login,
            "decision": decision.decision,
            "reason_code": decision.reason_code,
            "directive": decision.directive,
            "target_role": decision.target_role,
            "status_projected": status_projected,
        }),
    );

    Ok(WebhookOutcome {
        status: "processed".to_string(),
        delivery_id,
        event_type,
        decision: decision.decision,
        reason_code: decision.reason_code,
        duplicate: false,
    })
}

async fn post_issue_comment(
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

async fn project_issue_runtime_state(
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
    .context("runtime state api task join failure")??;
    Ok(())
}

fn codex_sandbox_for_directive(directive: &str) -> &'static str {
    match directive {
        "design" | "poke" => "read-only",
        _ => "workspace-write",
    }
}

fn dispatch_comment_identity(
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

fn render_prompt(template: &str, values: &[(&str, String)]) -> String {
    let mut text = template.to_string();
    for (key, value) in values {
        let token = format!("{{{{{key}}}}}");
        text = text.replace(&token, value);
    }
    text
}

const DEFAULT_GIT_REMOTE: &str = "origin";
const DEFAULT_GIT_BASE_BRANCH: &str = "main";

fn directive_uses_worktree(directive: &str) -> bool {
    matches!(directive, "impl" | "pr")
}

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

fn dispatch_worktree_branch(
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

fn create_dispatch_worktree(
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

fn lock_root(db_path: &Path) -> Result<PathBuf, DispatchError> {
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

fn run_root(db_path: &Path) -> Result<PathBuf, DispatchError> {
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

fn git_checked_with_token(
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

fn forgejo_http_git_url(
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

fn ensure_repo_checkout(
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
        let _ = update_repo_local_path(&state.db_path, repo_full_name, &checkout);
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
    let _ = update_repo_local_path(&state.db_path, repo_full_name, &checkout);
    Ok(checkout)
}

fn acquire_repo_lock(db_path: &Path, repo_full_name: &str) -> Result<PathBuf, DispatchError> {
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

async fn plan_dispatch(
    state: &AppState,
    dispatch_config: &DispatchConfig,
    decision_id: i64,
    current_event_id: i64,
    record: &EventRecord,
    decision: &DecisionRecord,
) -> Result<DispatchPlan, DispatchError> {
    let actor = record
        .actor_login
        .clone()
        .unwrap_or_default()
        .to_ascii_lowercase();
    let policy_decision = if dispatch_config
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

    let directive_name = decision
        .directive
        .as_deref()
        .ok_or_else(|| DispatchError::DirectiveNotConfigured("<none>".to_string()))?;
    let directive = dispatch_config
        .directives
        .get(directive_name)
        .ok_or_else(|| DispatchError::DirectiveNotConfigured(directive_name.to_string()))?
        .clone();
    let role = dispatch_config
        .roles
        .get(&directive.role)
        .ok_or_else(|| DispatchError::RoleNotConfigured(directive.role.clone()))?
        .clone();

    let issue_number = record
        .issue_number
        .ok_or_else(|| DispatchError::InvalidIssueRef(record.repo_full_name.clone()))?;
    let intent = DispatchIntentV1 {
        intent_id: format!("event-{current_event_id}-decision-{decision_id}"),
        repo_full_name: record.repo_full_name.clone(),
        issue_number,
        role: directive.role.clone(),
        directive: directive_name.to_string(),
        actor_login: actor.clone(),
        delivery_id: record.delivery_id.clone(),
        parent_dispatch_id: None,
        created_at: Utc::now(),
        policy_snapshot: Some("cp2".to_string()),
    };

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

    let issue_session_id =
        latest_issue_codex_session_id(&state.db_path, &intent.repo_full_name, intent.issue_number)
            .map_err(|err| DispatchError::Db(err.to_string()))?;
    let lock_path = acquire_repo_lock(&state.db_path, &intent.repo_full_name)?;

    let repo = RepoRef::parse(&intent.repo_full_name)
        .map_err(|_| DispatchError::InvalidIssueRef(intent.repo_full_name.clone()))?;
    let issue_ref = IssueRef {
        repo,
        number: intent.issue_number,
    };
    let issue = fetch_issue(state.clone(), issue_ref.clone()).await?;
    let previous_event_cursor = issue_role_cursor_event_id(
        &state.db_path,
        &intent.repo_full_name,
        intent.issue_number,
        &intent.role,
    )
    .map_err(|err| DispatchError::Db(err.to_string()))?;
    let delta_rows = issue_delta_rows(
        &state.db_path,
        &intent.repo_full_name,
        intent.issue_number,
        previous_event_cursor,
        current_event_id,
    )
    .map_err(|err| DispatchError::Db(err.to_string()))?;
    let issue_delta_summary = summarize_issue_delta(&delta_rows);

    let now = Utc::now().to_rfc3339();
    let dispatch_id = match reserve_dispatch_starting(
        &state.db_path,
        &DispatchInsert {
            decision_id,
            repo_full_name: intent.repo_full_name.clone(),
            issue_number: intent.issue_number,
            actor_login: record.actor_login.clone(),
            directive: intent.directive.clone(),
            target_role: intent.role.clone(),
            started_at: now,
        },
    )
    .map_err(|err| DispatchError::Db(err.to_string()))?
    {
        DispatchReservation::Started(dispatch_id) => dispatch_id,
        DispatchReservation::InFlightIssue(dispatch_id) => {
            let _ = fs::remove_file(&lock_path);
            return Err(DispatchError::IssueDispatchInFlight {
                repo_full_name: intent.repo_full_name.clone(),
                issue_number: intent.issue_number,
                dispatch_id,
            });
        }
        DispatchReservation::InFlightRepo(dispatch_id) => {
            let _ = fs::remove_file(&lock_path);
            return Err(DispatchError::RepoImplDispatchInFlight {
                repo_full_name: intent.repo_full_name.clone(),
                dispatch_id,
            });
        }
    };

    let run_dir = run_root(&state.db_path)?.join(format!("dispatch-{dispatch_id}"));
    fs::create_dir_all(&run_dir)
        .map_err(|err| DispatchError::Io(format!("failed to create run dir: {err}")))?;
    let issue_title = issue.title;
    let issue_body = issue.body.unwrap_or_default();
    let issue_url = issue.html_url;

    let base_repo_checkout = ensure_repo_checkout(state, &role, &intent.repo_full_name)?;
    if repo_labels_ensured_at(&state.db_path, &intent.repo_full_name)
        .unwrap_or(None)
        .is_none()
    {
        let repo_full_name = intent.repo_full_name.clone();
        let forgejoctl_bin = dispatch_config.forgejoctl_bin.clone();
        let config_file = state.forgejo_config_file.clone();
        let token_file = role.token_file.clone();
        let ensure_outcome = tokio::task::spawn_blocking(move || {
            run_forgejoctl(
                &forgejoctl_bin,
                config_file.as_deref(),
                &token_file,
                &["repo", "ensure", &repo_full_name],
            )
        })
        .await;
        match ensure_outcome {
            Ok(Ok(())) => {
                let _ =
                    update_repo_labels_ensured(&state.db_path, &intent.repo_full_name, true, None);
            }
            Ok(Err(err)) => {
                let _ = update_repo_labels_ensured(
                    &state.db_path,
                    &intent.repo_full_name,
                    false,
                    Some(&err.to_string()),
                );
            }
            Err(err) => {
                let _ = update_repo_labels_ensured(
                    &state.db_path,
                    &intent.repo_full_name,
                    false,
                    Some(&format!("ensure join failure: {err}")),
                );
            }
        }
    }
    let (workdir, git_remote, git_base, git_branch) = if directive_uses_worktree(&intent.directive)
    {
        let git_remote = DEFAULT_GIT_REMOTE.to_string();
        let git_base = DEFAULT_GIT_BASE_BRANCH.to_string();
        let git_branch = dispatch_worktree_branch(
            &intent.repo_full_name,
            intent.issue_number,
            dispatch_id,
            directive_name,
        );
        let workdir = run_dir.join("worktree");
        create_dispatch_worktree(
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
            DEFAULT_GIT_REMOTE.to_string(),
            DEFAULT_GIT_BASE_BRANCH.to_string(),
            String::new(),
        )
    };

    Ok(DispatchPlan {
        actor,
        event_type: record.event_type.clone(),
        directive,
        role,
        workdir,
        git_remote,
        git_base,
        git_branch,
        intent,
        issue_ref,
        issue_title,
        issue_body,
        issue_url,
        issue_session_id,
        issue_delta_summary,
        dispatch_id,
        lock_path,
        run_dir,
        tmux_window: issue_tmux_window_name(&record.repo_full_name, issue_number),
    })
}

fn materialize_run_artifacts(
    state: &AppState,
    dispatch_config: &DispatchConfig,
    plan: &DispatchPlan,
) -> Result<DispatchRunArtifacts, DispatchError> {
    let directive_template = fs::read_to_string(&plan.directive.prompt_file).map_err(|err| {
        DispatchError::Io(format!(
            "failed reading prompt {}: {err}",
            plan.directive.prompt_file.display()
        ))
    })?;
    let directive_prompt = render_prompt(
        &directive_template,
        &[
            ("issue_ref", plan.issue_ref.to_string()),
            ("repo", plan.intent.repo_full_name.clone()),
            ("issue_number", plan.intent.issue_number.to_string()),
            ("directive", plan.intent.directive.clone()),
            ("target_role", plan.intent.role.clone()),
            ("actor", plan.actor.clone()),
            ("issue_title", plan.issue_title.clone()),
            ("issue_body", plan.issue_body.clone()),
            ("issue_url", plan.issue_url.clone()),
            ("event_type", plan.event_type.clone()),
            ("delivery_id", plan.intent.delivery_id.clone()),
        ],
    );
    let (prompt_mode, envelope_path, issue_delta) = if plan.issue_session_id.is_some() {
        (
            "followup",
            &dispatch_config.prompt_envelopes.followup_envelope,
            plan.issue_delta_summary.clone(),
        )
    } else {
        (
            "fresh",
            &dispatch_config.prompt_envelopes.fresh_envelope,
            "(fresh session; no prior issue delta context)".to_string(),
        )
    };
    let envelope_template = fs::read_to_string(envelope_path).map_err(|err| {
        DispatchError::Io(format!(
            "failed reading prompt envelope {}: {err}",
            envelope_path.display()
        ))
    })?;
    let prompt = render_prompt(
        &envelope_template,
        &[
            ("issue_ref", plan.issue_ref.to_string()),
            ("repo", plan.intent.repo_full_name.clone()),
            ("issue_number", plan.intent.issue_number.to_string()),
            ("directive", plan.intent.directive.clone()),
            ("target_role", plan.intent.role.clone()),
            ("actor", plan.actor.clone()),
            ("issue_title", plan.issue_title.clone()),
            ("issue_body", plan.issue_body.clone()),
            ("issue_url", plan.issue_url.clone()),
            ("event_type", plan.event_type.clone()),
            ("delivery_id", plan.intent.delivery_id.clone()),
            ("session_mode", prompt_mode.to_string()),
            ("issue_delta", issue_delta),
            ("directive_prompt", directive_prompt),
        ],
    );

    let prompt_path = plan.run_dir.join("prompt.md");
    fs::write(&prompt_path, prompt)
        .map_err(|err| DispatchError::Io(format!("failed writing prompt: {err}")))?;
    fs::write(plan.run_dir.join("prompt_mode.txt"), prompt_mode)
        .map_err(|err| DispatchError::Io(format!("failed writing prompt mode: {err}")))?;

    let script_path = plan.run_dir.join("run.sh");
    let summary_path = plan.run_dir.join("summary.md");
    let completion_path = plan.run_dir.join("completion.md");
    let last_message_path = plan.run_dir.join("last_message.md");
    let codex_log_path = plan.run_dir.join("codex.log");
    let marker_path = plan.run_dir.join("start.marker");
    let issue_ref_text = format!(
        "{}#{}",
        plan.intent.repo_full_name, plan.intent.issue_number
    );
    let tmux_locator = format!("{}:{}", dispatch_config.tmux.session, plan.tmux_window);
    let orchd_bin = std::env::current_exe()
        .map_err(|err| DispatchError::Io(format!("failed resolving orchd executable: {err}")))?;

    let script_inputs = TmuxRunScriptInputs {
        dispatch_id: plan.dispatch_id,
        db_path: &state.db_path,
        lock_path: &plan.lock_path,
        run_dir: &plan.run_dir,
        prompt_path: &prompt_path,
        summary_path: &summary_path,
        completion_path: &completion_path,
        last_message_path: &last_message_path,
        codex_log_path: &codex_log_path,
        marker_path: &marker_path,
        issue_ref_text: &issue_ref_text,
        orchd_bin: &orchd_bin,
        forgejoctl_bin: &dispatch_config.forgejoctl_bin,
        forgejo_config_file: state.forgejo_config_file.as_deref(),
        token_file: &plan.role.token_file,
        workdir: &plan.workdir,
        codex_sandbox: codex_sandbox_for_directive(&plan.intent.directive),
        git_remote: &plan.git_remote,
        git_base: &plan.git_base,
        git_branch: &plan.git_branch,
        issue_title: &plan.issue_title,
        issue_url: &plan.issue_url,
        codex_bin: &plan.role.codex_bin,
        codex_role_arg: &plan.role.codex_role_arg,
        issue_session_id: plan.issue_session_id.as_deref(),
        directive_name: &plan.intent.directive,
        role_name: &plan.intent.role,
        tmux_locator: &tmux_locator,
        timeout_sec: plan.directive.timeout_sec,
    };

    let script = match (state.dispatch_mode, state.dispatch_backend) {
        (DispatchMode::TmuxExec, _) => build_tmux_exec_run_script(&script_inputs),
        (DispatchMode::TmuxTui, DispatchBackend::Tmux) => {
            let bootstrap_prompt_path = plan.run_dir.join("bootstrap_prompt.md");
            let bootstrap_template = fs::read_to_string(
                &dispatch_config.prompt_envelopes.tmux_tui_bootstrap,
            )
            .map_err(|err| {
                DispatchError::Io(format!(
                    "failed reading tmux-tui bootstrap prompt {}: {err}",
                    dispatch_config
                        .prompt_envelopes
                        .tmux_tui_bootstrap
                        .display()
                ))
            })?;
            let bootstrap_prompt = render_prompt(
                &bootstrap_template,
                &[
                    ("issue_ref", plan.issue_ref.to_string()),
                    ("repo", plan.intent.repo_full_name.clone()),
                    ("issue_number", plan.intent.issue_number.to_string()),
                    ("directive", plan.intent.directive.clone()),
                    ("target_role", plan.intent.role.clone()),
                    ("actor", plan.actor.clone()),
                    ("issue_title", plan.issue_title.clone()),
                    ("issue_url", plan.issue_url.clone()),
                    ("event_type", plan.event_type.clone()),
                    ("delivery_id", plan.intent.delivery_id.clone()),
                    ("prompt_path", prompt_path.display().to_string()),
                ],
            );
            fs::write(&bootstrap_prompt_path, bootstrap_prompt).map_err(|err| {
                DispatchError::Io(format!("failed writing bootstrap prompt: {err}"))
            })?;
            let session_jsonl_path = plan.run_dir.join("session.jsonl.path");
            build_tmux_tui_run_script(&script_inputs, &bootstrap_prompt_path, &session_jsonl_path)
        }
        (DispatchMode::TmuxTui, DispatchBackend::Local) => {
            return Err(DispatchError::Io(
                "dispatch backend local does not support dispatch mode tmux-tui".to_string(),
            ));
        }
        (DispatchMode::DryRun, _) => return Err(DispatchError::ConfigNotLoaded),
    };

    fs::write(&script_path, script)
        .map_err(|err| DispatchError::Io(format!("failed writing run script: {err}")))?;
    Ok(DispatchRunArtifacts { script_path })
}

fn launch_dispatch_backend(
    state: &AppState,
    dispatch_config: &DispatchConfig,
    plan: &DispatchPlan,
    artifacts: &DispatchRunArtifacts,
) -> Result<RunHandle, DispatchError> {
    match state.dispatch_backend {
        DispatchBackend::Tmux => TmuxBackendAdapter.launch(dispatch_config, plan, artifacts),
        DispatchBackend::Local => LocalBackendAdapter.launch(dispatch_config, plan, artifacts),
    }
}

async fn dispatch_issue(
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
    let plan = plan_dispatch(
        &state,
        dispatch_config,
        decision_id,
        current_event_id,
        record,
        decision,
    )
    .await?;
    record_phase_latency_ms(
        "plan",
        phase_plan_start.elapsed().as_secs_f64() * 1000.0,
        "ok",
    );

    let phase_materialize_start = Instant::now();
    let artifacts = materialize_run_artifacts(&state, dispatch_config, &plan)?;
    record_phase_latency_ms(
        "materialize",
        phase_materialize_start.elapsed().as_secs_f64() * 1000.0,
        "ok",
    );

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
            let _ = update_dispatch_failed_start(
                &state.db_path,
                plan.dispatch_id,
                err.reason_code(),
                &err.to_string(),
            );
            let _ = fs::remove_file(&plan.lock_path);
            return Err(err);
        }
    };
    record_phase_latency_ms(
        "launch",
        phase_launch_start.elapsed().as_secs_f64() * 1000.0,
        "ok",
    );
    let phase_finalize_start = Instant::now();
    update_dispatch_running(
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

fn update_dispatch_terminal(
    db_path: &Path,
    dispatch_id: i64,
    status_spec: TerminalStatusSpec,
    reason_code: &str,
    exit_code: i64,
    session_id: Option<&str>,
) -> Result<bool> {
    let mut conn = open_db(db_path)?;
    let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
    let Some(current_status) = tx
        .query_row(
            "SELECT status FROM dispatches WHERE id = ?1",
            params![dispatch_id],
            |row| row.get::<_, String>(0),
        )
        .optional()?
    else {
        return Ok(false);
    };
    let Some(current_state) = DispatchState::parse_db(&current_status) else {
        return Ok(false);
    };
    if current_state.is_terminal() {
        return Ok(false);
    }
    let next_state =
        reduce_dispatch_state(current_state, status_spec.event_kind).map_err(|err| {
            anyhow!(
                "terminal transition rejected for dispatch {}: {err}",
                dispatch_id
            )
        })?;
    let ended_at = Utc::now().to_rfc3339();
    let session_id = session_id.filter(|sid| !sid.trim().is_empty());
    let rows = tx.execute(
        r"
        UPDATE dispatches
        SET status = ?2,
            reason_code = ?3,
            codex_session_id = ?4,
            exit_code = ?5,
            ended_at = ?6
        WHERE id = ?1
          AND status = ?7
        ",
        params![
            dispatch_id,
            next_state.as_db_str(),
            reason_code,
            session_id,
            exit_code,
            ended_at,
            current_status,
        ],
    )?;
    if rows == 0 {
        return Ok(false);
    }
    append_dispatch_event_tx(
        &tx,
        dispatch_id,
        status_spec.event_kind,
        Some(&current_status),
        next_state.as_db_str(),
        Some(reason_code),
        None,
    )?;
    tx.commit()?;
    Ok(true)
}

fn run_forgejoctl(
    forgejoctl_bin: &Path,
    config_file: Option<&Path>,
    token_file: &Path,
    args: &[&str],
) -> Result<()> {
    let mut cmd = Command::new(forgejoctl_bin);
    if let Some(config_file) = config_file {
        cmd.arg("--config").arg(config_file);
    }
    let status = cmd
        .arg("--token-file")
        .arg(token_file)
        .args(args)
        .status()
        .with_context(|| format!("failed invoking forgejoctl {}", forgejoctl_bin.display()))?;
    if status.success() {
        Ok(())
    } else {
        Err(anyhow!(
            "forgejoctl command failed (exit={:?}) args={:?}",
            status.code(),
            args
        ))
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

fn git_output(workdir: &Path, args: &[&str]) -> Result<std::process::Output> {
    Command::new("git")
        .arg("-C")
        .arg(workdir)
        .args(args)
        .output()
        .with_context(|| format!("failed spawning git in {}", workdir.display()))
}

fn git_checked(workdir: &Path, args: &[&str]) -> Result<std::process::Output> {
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

fn git_stdout_trim(output: &std::process::Output) -> String {
    String::from_utf8_lossy(&output.stdout).trim().to_string()
}

fn autoland_to_main(
    db_path: &Path,
    token_file: &Path,
    workdir: &Path,
    remote: &str,
    base_branch: &str,
) -> Result<String> {
    let head = git_stdout_trim(&git_checked(workdir, &["rev-parse", "--short", "HEAD"])?);
    let _ = git_checked_with_token(
        db_path,
        token_file,
        Some(workdir),
        &["fetch", remote, base_branch],
    );
    git_checked_with_token(
        db_path,
        token_file,
        Some(workdir),
        &["push", remote, &format!("HEAD:{base_branch}")],
    )?;
    Ok(format!("autoland: pushed {head} -> {remote}/{base_branch}"))
}

fn push_branch(
    db_path: &Path,
    token_file: &Path,
    workdir: &Path,
    remote: &str,
    branch: &str,
) -> Result<String> {
    let head = git_stdout_trim(&git_checked(workdir, &["rev-parse", "--short", "HEAD"])?);
    git_checked_with_token(
        db_path,
        token_file,
        Some(workdir),
        &["push", "-u", remote, &format!("HEAD:{branch}")],
    )?;
    Ok(format!("pushed branch: {head} -> {remote}/{branch}"))
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

fn finalize_dispatch_command(args: FinalizeDispatchArgs) -> Result<()> {
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
    let did_transition = update_dispatch_terminal(
        &args.db_path,
        args.dispatch_id,
        status_spec,
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
            "impl" => match autoland_to_main(
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
                    match push_branch(
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
        if let Err(err) = run_forgejoctl(
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
    if let Err(err) = run_forgejoctl(
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
    if let Err(err) = run_forgejoctl(
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

fn init_db(db_path: &Path) -> Result<()> {
    if let Some(parent) = db_path.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("failed to create db directory: {}", parent.display()))?;
    }
    let conn = open_db(db_path)?;
    conn.execute_batch(
        r"
        CREATE TABLE IF NOT EXISTS events (
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            delivery_id TEXT NOT NULL,
            event_type TEXT NOT NULL,
            repo_full_name TEXT NOT NULL,
            issue_number INTEGER,
            action TEXT,
            actor_login TEXT,
            event_text TEXT,
            source_comment_id INTEGER,
            source_created_at TEXT,
            raw_json TEXT NOT NULL,
            received_at TEXT NOT NULL,
            UNIQUE(delivery_id, event_type)
        );
        CREATE TABLE IF NOT EXISTS decisions (
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            event_id INTEGER NOT NULL,
            repo_full_name TEXT NOT NULL,
            issue_number INTEGER,
            actor_login TEXT,
            directive TEXT,
            target_role TEXT,
            decision TEXT NOT NULL,
            reason_code TEXT NOT NULL,
            would_dispatch INTEGER NOT NULL,
            comment_posted INTEGER NOT NULL DEFAULT 0,
            comment_error TEXT,
            created_at TEXT NOT NULL,
            FOREIGN KEY(event_id) REFERENCES events(id)
        );
        CREATE TABLE IF NOT EXISTS heartbeats (
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            recorded_at TEXT NOT NULL,
            queue_depth INTEGER NOT NULL,
            events_total INTEGER NOT NULL,
            decisions_total INTEGER NOT NULL,
            last_delivery_id TEXT
        );
        CREATE TABLE IF NOT EXISTS reconciles (
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            recorded_at TEXT NOT NULL,
            repo_full_name TEXT NOT NULL,
            scanned_open INTEGER NOT NULL,
            status TEXT NOT NULL,
            error_text TEXT
        );
        CREATE TABLE IF NOT EXISTS dispatches (
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            decision_id INTEGER NOT NULL,
            repo_full_name TEXT NOT NULL,
            issue_number INTEGER NOT NULL,
            actor_login TEXT,
            directive TEXT NOT NULL,
            target_role TEXT NOT NULL,
            status TEXT NOT NULL,
            backend_kind TEXT,
            backend_ref TEXT,
            reason_code TEXT,
            error_text TEXT,
            tmux_session TEXT,
            tmux_window TEXT,
            run_dir TEXT,
            lock_path TEXT,
            codex_session_id TEXT,
            exit_code INTEGER,
            started_at TEXT NOT NULL,
            ended_at TEXT,
            FOREIGN KEY(decision_id) REFERENCES decisions(id)
        );
        CREATE INDEX IF NOT EXISTS idx_dispatches_repo_status
            ON dispatches (repo_full_name, status);
        CREATE INDEX IF NOT EXISTS idx_dispatches_repo_issue
            ON dispatches (repo_full_name, issue_number, id DESC);
        CREATE TABLE IF NOT EXISTS dispatch_events (
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            dispatch_id INTEGER NOT NULL,
            event_kind TEXT NOT NULL,
            from_state TEXT,
            to_state TEXT NOT NULL,
            reason_code TEXT,
            error_text TEXT,
            created_at TEXT NOT NULL,
            FOREIGN KEY(dispatch_id) REFERENCES dispatches(id)
        );
        CREATE INDEX IF NOT EXISTS idx_dispatch_events_dispatch
            ON dispatch_events (dispatch_id, id);
        CREATE TABLE IF NOT EXISTS issue_role_cursors (
            repo_full_name TEXT NOT NULL,
            issue_number INTEGER NOT NULL,
            role_name TEXT NOT NULL,
            last_event_id INTEGER NOT NULL,
            updated_at TEXT NOT NULL,
            PRIMARY KEY (repo_full_name, issue_number, role_name)
        );
        CREATE TABLE IF NOT EXISTS repos (
            repo_full_name TEXT PRIMARY KEY,
            first_seen_at TEXT NOT NULL,
            last_seen_at TEXT NOT NULL,
            labels_ensured_at TEXT,
            local_path TEXT,
            last_error TEXT
        );
        ",
    )?;
    ensure_column_exists(&conn, "events", "event_text", "TEXT")?;
    ensure_column_exists(&conn, "events", "source_comment_id", "INTEGER")?;
    ensure_column_exists(&conn, "events", "source_created_at", "TEXT")?;
    ensure_column_exists(&conn, "dispatches", "backend_kind", "TEXT")?;
    ensure_column_exists(&conn, "dispatches", "backend_ref", "TEXT")?;
    Ok(())
}

fn ensure_column_exists(
    conn: &Connection,
    table_name: &str,
    column_name: &str,
    column_type: &str,
) -> Result<()> {
    let pragma = format!("PRAGMA table_info({table_name})");
    let mut stmt = conn.prepare(&pragma)?;
    let column_names = stmt
        .query_map([], |row| row.get::<_, String>(1))?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    if column_names.iter().any(|name| name == column_name) {
        return Ok(());
    }

    let alter = format!("ALTER TABLE {table_name} ADD COLUMN {column_name} {column_type}");
    conn.execute(&alter, [])?;
    Ok(())
}

fn open_db(path: &Path) -> Result<Connection> {
    let conn =
        Connection::open(path).with_context(|| format!("failed to open db: {}", path.display()))?;
    conn.busy_timeout(StdDuration::from_secs(5))?;
    conn.execute_batch("PRAGMA journal_mode=WAL; PRAGMA synchronous=NORMAL;")?;
    Ok(conn)
}

fn upsert_repo_seen(db_path: &Path, repo_full_name: &str) -> Result<()> {
    let conn = open_db(db_path)?;
    let now = Utc::now().to_rfc3339();
    conn.execute(
        r"
        INSERT INTO repos (repo_full_name, first_seen_at, last_seen_at, labels_ensured_at, local_path, last_error)
        VALUES (?1, ?2, ?3, NULL, NULL, NULL)
        ON CONFLICT(repo_full_name) DO UPDATE SET last_seen_at = excluded.last_seen_at
        ",
        params![repo_full_name, now, now],
    )?;
    Ok(())
}

fn repo_labels_ensured_at(db_path: &Path, repo_full_name: &str) -> Result<Option<String>> {
    let conn = open_db(db_path)?;
    let row: Option<Option<String>> = conn
        .query_row(
            "SELECT labels_ensured_at FROM repos WHERE repo_full_name = ?1",
            params![repo_full_name],
            |row| row.get::<_, Option<String>>(0),
        )
        .optional()?;
    Ok(row.flatten())
}

fn update_repo_labels_ensured(
    db_path: &Path,
    repo_full_name: &str,
    ok: bool,
    err: Option<&str>,
) -> Result<()> {
    let conn = open_db(db_path)?;
    let now = Utc::now().to_rfc3339();
    conn.execute(
        r"
        UPDATE repos
        SET labels_ensured_at = CASE WHEN ?2 THEN ?3 ELSE labels_ensured_at END,
            last_error = ?4,
            last_seen_at = ?3
        WHERE repo_full_name = ?1
        ",
        params![repo_full_name, ok, now, err],
    )?;
    Ok(())
}

fn update_repo_local_path(db_path: &Path, repo_full_name: &str, local_path: &Path) -> Result<()> {
    let conn = open_db(db_path)?;
    let now = Utc::now().to_rfc3339();
    conn.execute(
        r"
        UPDATE repos
        SET local_path = ?2,
            last_seen_at = ?3
        WHERE repo_full_name = ?1
        ",
        params![repo_full_name, local_path.to_string_lossy(), now],
    )?;
    Ok(())
}

fn insert_event(db_path: &Path, event: &EventRecord) -> Result<Option<i64>> {
    let conn = open_db(db_path)?;
    let now = Utc::now().to_rfc3339();
    let inserted = conn.execute(
        r"
        INSERT INTO events (delivery_id, event_type, repo_full_name, issue_number, action, actor_login, event_text, source_comment_id, source_created_at, raw_json, received_at)
        VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)
        ",
        params![
            event.delivery_id,
            event.event_type,
            event.repo_full_name,
            event.issue_number,
            event.action,
            event.actor_login,
            event.event_text,
            event.source_comment_id,
            event.source_created_at,
            event.raw_json,
            now,
        ],
    );

    match inserted {
        Ok(_) => Ok(Some(conn.last_insert_rowid())),
        Err(err) => {
            let duplicate = matches!(
                err,
                rusqlite::Error::SqliteFailure(sqlite_err, _)
                    if sqlite_err.extended_code == 2067
            );
            if duplicate { Ok(None) } else { Err(err.into()) }
        }
    }
}

fn insert_decision(
    db_path: &Path,
    event_id: i64,
    event: &EventRecord,
    decision: &DecisionRecord,
) -> Result<i64> {
    let conn = open_db(db_path)?;
    let now = Utc::now().to_rfc3339();
    conn.execute(
        r"
        INSERT INTO decisions
        (event_id, repo_full_name, issue_number, actor_login, directive, target_role, decision, reason_code, would_dispatch, created_at)
        VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)
        ",
        params![
            event_id,
            event.repo_full_name,
            event.issue_number,
            event.actor_login,
            decision.directive,
            decision.target_role,
            decision.decision,
            decision.reason_code,
            i64::from(decision.would_dispatch),
            now
        ],
    )?;
    Ok(conn.last_insert_rowid())
}

fn update_decision_comment_status(
    db_path: &Path,
    decision_id: i64,
    comment_posted: bool,
    comment_error: Option<String>,
) -> Result<()> {
    let conn = open_db(db_path)?;
    conn.execute(
        r"
        UPDATE decisions
        SET comment_posted = ?2, comment_error = ?3
        WHERE id = ?1
        ",
        params![decision_id, i64::from(comment_posted), comment_error],
    )?;
    Ok(())
}

fn issue_role_cursor_event_id(
    db_path: &Path,
    repo_full_name: &str,
    issue_number: u64,
    role_name: &str,
) -> Result<Option<i64>> {
    let conn = open_db(db_path)?;
    let issue_number = i64::try_from(issue_number)?;
    conn.query_row(
        r"
        SELECT last_event_id
        FROM issue_role_cursors
        WHERE repo_full_name = ?1
          AND issue_number = ?2
          AND role_name = ?3
        ",
        params![repo_full_name, issue_number, role_name],
        |row| row.get::<_, i64>(0),
    )
    .optional()
    .map_err(Into::into)
}

fn upsert_issue_role_cursor_event_id(
    db_path: &Path,
    repo_full_name: &str,
    issue_number: u64,
    role_name: &str,
    last_event_id: i64,
) -> Result<()> {
    let conn = open_db(db_path)?;
    let now = Utc::now().to_rfc3339();
    conn.execute(
        r"
        INSERT INTO issue_role_cursors (repo_full_name, issue_number, role_name, last_event_id, updated_at)
        VALUES (?1, ?2, ?3, ?4, ?5)
        ON CONFLICT(repo_full_name, issue_number, role_name)
        DO UPDATE SET
            last_event_id = excluded.last_event_id,
            updated_at = excluded.updated_at
        ",
        params![
            repo_full_name,
            i64::try_from(issue_number)?,
            role_name,
            last_event_id,
            now
        ],
    )?;
    Ok(())
}

fn issue_delta_rows(
    db_path: &Path,
    repo_full_name: &str,
    issue_number: u64,
    after_event_id: Option<i64>,
    up_to_event_id: i64,
) -> Result<Vec<IssueEventDeltaRow>> {
    let conn = open_db(db_path)?;
    let start_event_id = after_event_id.unwrap_or(0_i64);
    let issue_number = i64::try_from(issue_number)?;
    let mut stmt = conn.prepare(
        r"
        SELECT event_type, actor_login, event_text, received_at, source_created_at
        FROM events
        WHERE repo_full_name = ?1
          AND issue_number = ?2
          AND id > ?3
          AND id <= ?4
          AND event_type IN ('issue_comment', 'issues')
          AND event_text IS NOT NULL
          AND event_text != ''
        ORDER BY id ASC
        LIMIT 200
        ",
    )?;
    let rows = stmt
        .query_map(
            params![repo_full_name, issue_number, start_event_id, up_to_event_id],
            |row| {
                Ok(IssueEventDeltaRow {
                    event_type: row.get(0)?,
                    actor_login: row.get(1)?,
                    event_text: row.get(2)?,
                    received_at: row.get(3)?,
                    source_created_at: row.get(4)?,
                })
            },
        )?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(rows)
}

fn summarize_issue_delta(rows: &[IssueEventDeltaRow]) -> String {
    if rows.is_empty() {
        return "(no new issue events since last handled dispatch)".to_string();
    }

    rows.iter()
        .map(|row| {
            let timestamp = row
                .source_created_at
                .as_deref()
                .filter(|text| !text.trim().is_empty())
                .unwrap_or(row.received_at.as_str());
            let actor = row
                .actor_login
                .as_deref()
                .filter(|text| !text.trim().is_empty())
                .unwrap_or("unknown");
            let mut text = row.event_text.as_deref().unwrap_or("").replace('\n', " ");
            text = text.trim().to_string();
            if text.chars().count() > 220 {
                text = format!("{}...", text.chars().take(220).collect::<String>());
            }
            format!("- [{}] {} {}: {}", timestamp, actor, row.event_type, text)
        })
        .collect::<Vec<_>>()
        .join("\n")
}

struct DispatchTransitionPlan {
    current_status: String,
    next_state: DispatchState,
}

fn append_dispatch_event_tx(
    tx: &Transaction<'_>,
    dispatch_id: i64,
    event_kind: DispatchEventKind,
    from_state: Option<&str>,
    to_state: &str,
    reason_code: Option<&str>,
    error_text: Option<&str>,
) -> Result<()> {
    tx.execute(
        r"
        INSERT INTO dispatch_events
            (dispatch_id, event_kind, from_state, to_state, reason_code, error_text, created_at)
        VALUES
            (?1, ?2, ?3, ?4, ?5, ?6, ?7)
        ",
        params![
            dispatch_id,
            event_kind.as_db_str(),
            from_state,
            to_state,
            reason_code,
            error_text,
            Utc::now().to_rfc3339(),
        ],
    )?;
    info!(
        dispatch_id = dispatch_id,
        event_kind = event_kind.as_db_str(),
        from_state = from_state.unwrap_or(""),
        to_state = to_state,
        reason_code = reason_code.unwrap_or(""),
        "dispatch transition recorded"
    );
    Ok(())
}

fn plan_dispatch_transition(
    conn: &Connection,
    dispatch_id: i64,
    event: DispatchEventKind,
) -> Result<Option<DispatchTransitionPlan>> {
    let Some(current_status) = conn
        .query_row(
            "SELECT status FROM dispatches WHERE id = ?1",
            params![dispatch_id],
            |row| row.get::<_, String>(0),
        )
        .optional()?
    else {
        return Ok(None);
    };
    let current_state = DispatchState::parse_db(&current_status).ok_or_else(|| {
        anyhow!(
            "dispatch {} has unknown status literal '{}'",
            dispatch_id,
            current_status
        )
    })?;
    let next_state = reduce_dispatch_state(current_state, event)
        .map_err(|err| anyhow!("dispatch {dispatch_id} transition rejected: {err}"))?;
    Ok(Some(DispatchTransitionPlan {
        current_status,
        next_state,
    }))
}

fn reserve_dispatch_starting(
    db_path: &Path,
    dispatch: &DispatchInsert,
) -> Result<DispatchReservation> {
    let mut conn = open_db(db_path)?;
    let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
    let issue_number = i64::try_from(dispatch.issue_number)?;
    let starting_status = DispatchState::Starting.as_db_str();
    let running_status = DispatchState::Running.as_db_str();

    let inflight_id = tx
        .query_row(
            r"
            SELECT id
            FROM dispatches
            WHERE repo_full_name = ?1
              AND issue_number = ?2
              AND status IN (?3, ?4)
            ORDER BY id DESC
            LIMIT 1
            ",
            params![
                dispatch.repo_full_name.as_str(),
                issue_number,
                starting_status,
                running_status
            ],
            |row| row.get::<_, i64>(0),
        )
        .optional()?;
    if let Some(dispatch_id) = inflight_id {
        tx.commit()?;
        return Ok(DispatchReservation::InFlightIssue(dispatch_id));
    }

    if dispatch.directive == "impl" {
        let repo_inflight = tx
            .query_row(
                r"
                SELECT id
                FROM dispatches
                WHERE repo_full_name = ?1
                  AND directive = 'impl'
                  AND status IN (?2, ?3)
                ORDER BY id DESC
                LIMIT 1
                ",
                params![
                    dispatch.repo_full_name.as_str(),
                    starting_status,
                    running_status
                ],
                |row| row.get::<_, i64>(0),
            )
            .optional()?;
        if let Some(dispatch_id) = repo_inflight {
            tx.commit()?;
            return Ok(DispatchReservation::InFlightRepo(dispatch_id));
        }
    }

    tx.execute(
        r"
        INSERT INTO dispatches
        (decision_id, repo_full_name, issue_number, actor_login, directive, target_role, status, started_at, tmux_session)
        VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, NULL)
        ",
        params![
            dispatch.decision_id,
            dispatch.repo_full_name.as_str(),
            issue_number,
            dispatch.actor_login.as_deref(),
            dispatch.directive.as_str(),
            dispatch.target_role.as_str(),
            DispatchState::Starting.as_db_str(),
            dispatch.started_at.as_str(),
        ],
    )?;
    let dispatch_id = tx.last_insert_rowid();
    append_dispatch_event_tx(
        &tx,
        dispatch_id,
        DispatchEventKind::MarkStarting,
        None,
        DispatchState::Starting.as_db_str(),
        Some("reserved_dispatch"),
        None,
    )?;
    tx.commit()?;
    Ok(DispatchReservation::Started(dispatch_id))
}

fn update_dispatch_running(
    db_path: &Path,
    dispatch_id: i64,
    run_handle: &RunHandle,
    run_dir: &Path,
    lock_path: &Path,
) -> Result<()> {
    let mut conn = open_db(db_path)?;
    let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
    let Some(plan) = plan_dispatch_transition(&tx, dispatch_id, DispatchEventKind::MarkRunning)?
    else {
        return Err(anyhow!("dispatch {dispatch_id} not found"));
    };
    let (tmux_session, tmux_window): (Option<String>, Option<String>) = match run_handle
        .backend_kind
    {
        DispatchBackendKind::Tmux => {
            let (session, window) = run_handle.backend_ref.split_once(':').ok_or_else(|| {
                anyhow!("invalid tmux run handle ref '{}'", run_handle.backend_ref)
            })?;
            (Some(session.to_string()), Some(window.to_string()))
        }
        DispatchBackendKind::Local => (None, None),
    };
    let rows = tx.execute(
        r"
        UPDATE dispatches
        SET status = ?2,
            tmux_session = ?3,
            tmux_window = ?4,
            backend_kind = ?5,
            backend_ref = ?6,
            run_dir = ?7,
            lock_path = ?8
        WHERE id = ?1
          AND status = ?9
        ",
        params![
            dispatch_id,
            plan.next_state.as_db_str(),
            tmux_session.as_deref(),
            tmux_window.as_deref(),
            match run_handle.backend_kind {
                DispatchBackendKind::Tmux => "tmux",
                DispatchBackendKind::Local => "local",
            },
            run_handle.backend_ref.as_str(),
            run_dir.to_string_lossy(),
            lock_path.to_string_lossy(),
            plan.current_status,
        ],
    )?;
    if rows == 0 {
        return Err(anyhow!(
            "dispatch {} state changed concurrently before running transition",
            dispatch_id
        ));
    }
    append_dispatch_event_tx(
        &tx,
        dispatch_id,
        DispatchEventKind::MarkRunning,
        Some(&plan.current_status),
        plan.next_state.as_db_str(),
        Some("launch_ok"),
        None,
    )?;
    tx.commit()?;
    Ok(())
}

fn update_dispatch_failed_start(
    db_path: &Path,
    dispatch_id: i64,
    reason_code: &str,
    error_text: &str,
) -> Result<()> {
    let mut conn = open_db(db_path)?;
    let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
    let Some(plan) = plan_dispatch_transition(&tx, dispatch_id, DispatchEventKind::FailStart)?
    else {
        return Err(anyhow!("dispatch {dispatch_id} not found"));
    };
    let now = Utc::now().to_rfc3339();
    let rows = tx.execute(
        r"
        UPDATE dispatches
        SET status = ?2,
            reason_code = ?3,
            error_text = ?4,
            ended_at = ?5
        WHERE id = ?1
          AND status = ?6
        ",
        params![
            dispatch_id,
            plan.next_state.as_db_str(),
            reason_code,
            error_text,
            now,
            plan.current_status,
        ],
    )?;
    if rows == 0 {
        return Err(anyhow!(
            "dispatch {} state changed concurrently before failed_start transition",
            dispatch_id
        ));
    }
    append_dispatch_event_tx(
        &tx,
        dispatch_id,
        DispatchEventKind::FailStart,
        Some(&plan.current_status),
        plan.next_state.as_db_str(),
        Some(reason_code),
        Some(error_text),
    )?;
    tx.commit()?;
    Ok(())
}

fn latest_issue_inflight_dispatch(
    db_path: &Path,
    repo_full_name: &str,
    issue_number: u64,
) -> Result<Option<InflightDispatch>> {
    let conn = open_db(db_path)?;
    let starting_status = DispatchState::Starting.as_db_str();
    let running_status = DispatchState::Running.as_db_str();
    let dispatch = conn
        .query_row(
            r"
            SELECT id, status, started_at, backend_kind, backend_ref, tmux_session, tmux_window, lock_path
            FROM dispatches
            WHERE repo_full_name = ?1
              AND issue_number = ?2
              AND status IN (?3, ?4)
            ORDER BY id DESC
            LIMIT 1
            ",
            params![
                repo_full_name,
                i64::try_from(issue_number)?,
                starting_status,
                running_status
            ],
            |row| {
                Ok(InflightDispatch {
                    id: row.get(0)?,
                    status: row.get(1)?,
                    started_at: row.get(2)?,
                    backend_kind: row.get(3)?,
                    backend_ref: row.get(4)?,
                    tmux_session: row.get(5)?,
                    tmux_window: row.get(6)?,
                    lock_path: row.get(7)?,
                })
            },
        )
        .optional()?;
    Ok(dispatch)
}

fn latest_repo_inflight_impl_dispatch_id(
    db_path: &Path,
    repo_full_name: &str,
) -> Result<Option<i64>> {
    let conn = open_db(db_path)?;
    let starting_status = DispatchState::Starting.as_db_str();
    let running_status = DispatchState::Running.as_db_str();
    conn.query_row(
        r"
        SELECT id
        FROM dispatches
        WHERE repo_full_name = ?1
          AND directive = 'impl'
          AND status IN (?2, ?3)
        ORDER BY id DESC
        LIMIT 1
        ",
        params![repo_full_name, starting_status, running_status],
        |row| row.get::<_, i64>(0),
    )
    .optional()
    .map_err(Into::into)
}

fn probe_dispatch_liveness(
    dispatch: &InflightDispatch,
    repo_full_name: &str,
    issue_number: u64,
) -> Result<bool, DispatchError> {
    match dispatch.backend_kind.as_deref().unwrap_or("tmux") {
        "tmux" => TmuxBackendAdapter.probe(dispatch, repo_full_name, issue_number),
        "local" => LocalBackendAdapter.probe(dispatch, repo_full_name, issue_number),
        other => Err(DispatchError::Io(format!(
            "unknown backend kind '{other}' on dispatch {}",
            dispatch.id
        ))),
    }
}

fn is_stale_starting_dispatch(
    dispatch: &InflightDispatch,
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
    if dispatch.backend_kind.is_none()
        && dispatch.tmux_session.is_none()
        && dispatch.backend_ref.is_none()
    {
        return true;
    }
    if dispatch.backend_kind.as_deref() == Some("local") && dispatch.backend_ref.is_none() {
        return true;
    }
    if dispatch.backend_kind.as_deref().unwrap_or("tmux") == "tmux"
        && dispatch.tmux_session.is_none()
    {
        return true;
    }
    match probe_dispatch_liveness(dispatch, repo_full_name, issue_number) {
        Ok(has_live_pane) => !has_live_pane,
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
    dispatch: &InflightDispatch,
    repo_full_name: &str,
    issue_number: u64,
) -> bool {
    let Some(dispatch_state) = DispatchState::parse_db(dispatch.status.as_str()) else {
        return false;
    };
    match dispatch_state {
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

fn mark_dispatch_failed_runtime(
    db_path: &Path,
    dispatch_id: i64,
    reason_code: &str,
    error_text: &str,
) -> Result<()> {
    let mut conn = open_db(db_path)?;
    let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
    let event_kind = if reason_code == "stale_dispatch_autohealed" {
        DispatchEventKind::HealStale
    } else {
        DispatchEventKind::FailRuntime
    };
    let Some(plan) = plan_dispatch_transition(&tx, dispatch_id, event_kind)? else {
        return Ok(());
    };
    let ended_at = Utc::now().to_rfc3339();
    let rows = tx.execute(
        r"
        UPDATE dispatches
        SET status = ?2,
            reason_code = ?3,
            error_text = ?4,
            ended_at = ?5
        WHERE id = ?1
          AND status = ?6
        ",
        params![
            dispatch_id,
            plan.next_state.as_db_str(),
            reason_code,
            error_text,
            ended_at,
            plan.current_status,
        ],
    )?;
    if rows > 0 {
        append_dispatch_event_tx(
            &tx,
            dispatch_id,
            event_kind,
            Some(&plan.current_status),
            plan.next_state.as_db_str(),
            Some(reason_code),
            Some(error_text),
        )?;
    }
    tx.commit()?;
    Ok(())
}

fn find_issue_inflight_dispatch_with_healing(
    db_path: &Path,
    repo_full_name: &str,
    issue_number: u64,
) -> Result<Option<i64>> {
    loop {
        let Some(dispatch) = latest_issue_inflight_dispatch(db_path, repo_full_name, issue_number)?
        else {
            return Ok(None);
        };
        if !should_heal_dispatch_stale(&dispatch, repo_full_name, issue_number) {
            return Ok(Some(dispatch.id));
        }
        mark_dispatch_failed_runtime(
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

fn latest_issue_codex_session_id(
    db_path: &Path,
    repo_full_name: &str,
    issue_number: u64,
) -> Result<Option<String>> {
    let conn = open_db(db_path)?;
    let session_id = conn
        .query_row(
            r"
            SELECT codex_session_id
            FROM dispatches
            WHERE repo_full_name = ?1
              AND issue_number = ?2
              AND codex_session_id IS NOT NULL
              AND codex_session_id != ''
            ORDER BY id DESC
            LIMIT 1
            ",
            params![repo_full_name, i64::try_from(issue_number)?],
            |row| row.get::<_, String>(0),
        )
        .optional()?;
    Ok(session_id)
}

async fn run_heartbeat_loop(state: AppState, interval_sec: u64) {
    let interval = StdDuration::from_secs(interval_sec.max(1));
    loop {
        if let Err(err) = heartbeat_once(&state) {
            log_line(
                "heartbeat_error",
                json!({
                    "error": err.to_string(),
                }),
            );
        }
        tokio::time::sleep(interval).await;
    }
}

#[derive(Debug, Clone)]
struct QueuedDecision {
    decision_id: i64,
    event_id: i64,
    record: EventRecord,
    decision: DecisionRecord,
}

fn queued_impl_decisions(db_path: &Path, limit: u32) -> Result<Vec<QueuedDecision>> {
    let conn = open_db(db_path)?;
    let mut stmt = conn.prepare(
        r"
        WITH latest AS (
            SELECT repo_full_name, issue_number, target_role, MAX(id) AS decision_id
            FROM decisions
            WHERE decision = 'accepted'
              AND would_dispatch = 1
              AND directive = 'impl'
              AND issue_number IS NOT NULL
              AND target_role IS NOT NULL
            GROUP BY repo_full_name, issue_number, target_role
        )
        SELECT
            d.id,
            d.event_id,
            e.delivery_id,
            e.event_type,
            e.repo_full_name,
            e.issue_number,
            e.action,
            e.actor_login,
            e.event_text,
            e.source_comment_id,
            e.source_created_at,
            e.raw_json,
            d.decision,
            d.reason_code,
            d.directive,
            d.target_role,
            d.would_dispatch
        FROM latest l
        JOIN decisions d ON d.id = l.decision_id
        JOIN events e ON e.id = d.event_id
        WHERE NOT EXISTS (SELECT 1 FROM dispatches x WHERE x.decision_id = d.id)
        ORDER BY d.id ASC
        LIMIT ?1
        ",
    )?;
    let rows = stmt
        .query_map(params![i64::from(limit)], |row| {
            let decision_id: i64 = row.get(0)?;
            let event_id: i64 = row.get(1)?;
            let record = EventRecord {
                delivery_id: row.get(2)?,
                event_type: row.get(3)?,
                repo_full_name: row.get(4)?,
                issue_number: row
                    .get::<_, Option<i64>>(5)?
                    .and_then(|n| u64::try_from(n).ok()),
                action: row.get(6)?,
                actor_login: row.get(7)?,
                event_text: row.get(8)?,
                source_comment_id: row.get(9)?,
                source_created_at: row.get(10)?,
                raw_json: row.get(11)?,
            };
            let would_dispatch_int: i64 = row.get(16)?;
            let decision = DecisionRecord {
                decision: row.get(12)?,
                reason_code: row.get(13)?,
                directive: row.get(14)?,
                target_role: row.get(15)?,
                would_dispatch: would_dispatch_int != 0,
            };
            Ok(QueuedDecision {
                decision_id,
                event_id,
                record,
                decision,
            })
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(rows)
}

async fn run_dispatch_queue_loop(state: AppState, interval_sec: u64) {
    let interval = StdDuration::from_secs(interval_sec.max(1));
    loop {
        if let Err(err) = dispatch_queue_once(&state).await {
            log_line(
                "dispatch_queue_error",
                json!({
                    "error": err.to_string(),
                }),
            );
        }
        tokio::time::sleep(interval).await;
    }
}

async fn dispatch_queue_once(state: &AppState) -> Result<()> {
    if matches!(state.dispatch_mode, DispatchMode::DryRun) {
        return Ok(());
    }
    let items = queued_impl_decisions(&state.db_path, 10)?;
    for item in items {
        let repo_full_name = item.record.repo_full_name.clone();
        if let Ok(Some(inflight)) =
            latest_repo_inflight_impl_dispatch_id(&state.db_path, &repo_full_name)
        {
            log_line(
                "dispatch_queue_repo_busy",
                json!({
                    "repo": repo_full_name,
                    "decision_id": item.decision_id,
                    "event_id": item.event_id,
                    "inflight_dispatch_id": inflight,
                }),
            );
            continue;
        }
        let issue_number = item
            .record
            .issue_number
            .ok_or_else(|| anyhow!("queued decision missing issue number"))?;
        let dispatch_identity = dispatch_comment_identity(state, &item.decision);
        match dispatch_issue(
            state.clone(),
            item.decision_id,
            item.event_id,
            &item.record,
            &item.decision,
        )
        .await
        {
            Ok(()) => {
                let _ = project_issue_runtime_state(
                    state.clone(),
                    &item.record.repo_full_name,
                    issue_number,
                    OrchdRuntimeState::Running,
                    dispatch_identity.clone(),
                )
                .await;
                if let Some(role_name) = item.decision.target_role.as_deref() {
                    let _ = upsert_issue_role_cursor_event_id(
                        &state.db_path,
                        &item.record.repo_full_name,
                        issue_number,
                        role_name,
                        item.event_id,
                    );
                }
            }
            Err(err) => {
                log_line(
                    "dispatch_queue_dispatch_failed",
                    json!({
                        "repo": item.record.repo_full_name,
                        "issue_number": issue_number,
                        "decision_id": item.decision_id,
                        "event_id": item.event_id,
                        "reason_code": err.reason_code(),
                        "error": err.to_string(),
                    }),
                );
            }
        }
    }
    Ok(())
}

fn heartbeat_once(state: &AppState) -> Result<()> {
    let conn = open_db(&state.db_path)?;
    let queue_depth: i64 = conn.query_row(
        "SELECT COUNT(*) FROM decisions WHERE decision = 'accepted' AND would_dispatch = 1 AND comment_posted = 0",
        [],
        |row| row.get(0),
    )?;
    let events_total: i64 = conn.query_row("SELECT COUNT(*) FROM events", [], |row| row.get(0))?;
    let decisions_total: i64 =
        conn.query_row("SELECT COUNT(*) FROM decisions", [], |row| row.get(0))?;
    let last_delivery_id: Option<String> = conn
        .query_row(
            "SELECT delivery_id FROM events ORDER BY id DESC LIMIT 1",
            [],
            |row| row.get(0),
        )
        .optional()?;

    let now = Utc::now().to_rfc3339();
    conn.execute(
        r"
        INSERT INTO heartbeats (recorded_at, queue_depth, events_total, decisions_total, last_delivery_id)
        VALUES (?1, ?2, ?3, ?4, ?5)
        ",
        params![now, queue_depth, events_total, decisions_total, last_delivery_id],
    )?;

    log_line(
        "heartbeat",
        json!({
            "queue_depth": queue_depth,
            "events_total": events_total,
            "decisions_total": decisions_total,
            "last_delivery_id": last_delivery_id,
        }),
    );
    Ok(())
}

async fn run_reconcile_loop(state: AppState, interval_sec: u64) {
    let interval = StdDuration::from_secs(interval_sec.max(1));
    loop {
        if let Err(err) = reconcile_once(&state).await {
            log_line(
                "reconcile_error",
                json!({
                    "repo": state.reconcile_repo.to_string(),
                    "error": err.to_string(),
                }),
            );
        }
        tokio::time::sleep(interval).await;
    }
}

async fn reconcile_once(state: &AppState) -> Result<()> {
    if let Err(err) = ensure_repo_webhooks_for_default_owner(state).await {
        log_line(
            "repo_webhooks_ensure_error",
            json!({
                "owner": state.cfg.default_repo.owner,
                "url": state.webhook_url,
                "error": err.to_string(),
            }),
        );
    }
    let cfg = state.cfg.clone();
    let repo = state.reconcile_repo.clone();

    let scanned_open = tokio::task::spawn_blocking(move || {
        let api = ForgejoClient::new(&cfg)?;
        api.list_issues(&cfg, &repo, "open", 100)
            .map(|items| items.len())
    })
    .await
    .context("reconcile join failure")??;

    let conn = open_db(&state.db_path)?;
    let now = Utc::now().to_rfc3339();
    conn.execute(
        r"
        INSERT INTO reconciles (recorded_at, repo_full_name, scanned_open, status, error_text)
        VALUES (?1, ?2, ?3, 'ok', NULL)
        ",
        params![
            now,
            state.reconcile_repo.to_string(),
            i64::try_from(scanned_open)?
        ],
    )?;

    log_line(
        "reconcile",
        json!({
            "repo": state.reconcile_repo.to_string(),
            "scanned_open": scanned_open,
            "status": "ok",
        }),
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::orchd::state::EventContext;
    use crate::orchd::webhook::parse_directive;

    fn inflight_dispatch(
        status: &str,
        started_at: String,
        tmux_session: Option<&str>,
    ) -> InflightDispatch {
        InflightDispatch {
            id: 1,
            status: status.to_string(),
            started_at,
            backend_kind: Some("tmux".to_string()),
            backend_ref: Some("codex-orch:rmain-orchd-debug-i1".to_string()),
            tmux_session: tmux_session.map(str::to_string),
            tmux_window: None,
            lock_path: None,
        }
    }

    #[test]
    fn starting_dispatch_is_not_stale_within_grace_period() {
        let started_at = (Utc::now() - ChronoDuration::seconds(5)).to_rfc3339();
        let dispatch = inflight_dispatch("starting", started_at, None);
        assert!(!is_stale_starting_dispatch(
            &dispatch,
            "main/orchd-debug",
            1
        ));
    }

    #[test]
    fn starting_dispatch_with_invalid_timestamp_is_stale() {
        let dispatch = inflight_dispatch("starting", "invalid-timestamp".to_string(), None);
        assert!(is_stale_starting_dispatch(&dispatch, "main/orchd-debug", 1));
    }

    #[test]
    fn starting_dispatch_without_tmux_session_is_stale_after_grace_period() {
        let started_at = (Utc::now()
            - ChronoDuration::seconds(STARTING_DISPATCH_STALE_AFTER_SEC + 5))
        .to_rfc3339();
        let dispatch = inflight_dispatch("starting", started_at, None);
        assert!(is_stale_starting_dispatch(&dispatch, "main/orchd-debug", 1));
    }

    fn temp_db_path(label: &str) -> PathBuf {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("clock drift")
            .as_nanos();
        std::env::temp_dir().join(format!(
            "forgejo-agent-{label}-{}-{nanos}.sqlite",
            std::process::id()
        ))
    }

    fn sample_dispatch(decision_id: i64, issue_number: u64) -> DispatchInsert {
        DispatchInsert {
            decision_id,
            repo_full_name: "main/orchd-debug".to_string(),
            issue_number,
            actor_login: Some("main".to_string()),
            directive: "poke".to_string(),
            target_role: "codex-orch".to_string(),
            started_at: Utc::now().to_rfc3339(),
        }
    }

    fn seed_decision_id(db_path: &Path) -> i64 {
        let conn = open_db(db_path).expect("open db");
        let now = Utc::now().to_rfc3339();
        conn.execute(
            r"
            INSERT INTO events (delivery_id, event_type, repo_full_name, issue_number, action, actor_login, raw_json, received_at)
            VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)
            ",
            params![
                format!("test-delivery-{}", Utc::now().timestamp_nanos_opt().unwrap_or(0)),
                "issue_comment",
                "main/orchd-debug",
                7_i64,
                "created",
                "main",
                "{}",
                now
            ],
        )
        .expect("insert event");
        let event_id = conn.last_insert_rowid();
        conn.execute(
            r"
            INSERT INTO decisions
            (event_id, repo_full_name, issue_number, actor_login, directive, target_role, decision, reason_code, would_dispatch, created_at)
            VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)
            ",
            params![
                event_id,
                "main/orchd-debug",
                7_i64,
                "main",
                "poke",
                "codex-orch",
                "accepted",
                "explicit_directive",
                1_i64,
                Utc::now().to_rfc3339(),
            ],
        )
        .expect("insert decision");
        conn.last_insert_rowid()
    }

    fn dispatch_event_kinds(db_path: &Path, dispatch_id: i64) -> Vec<String> {
        let conn = open_db(db_path).expect("open db for event scan");
        let mut stmt = conn
            .prepare(
                "SELECT event_kind FROM dispatch_events WHERE dispatch_id = ?1 ORDER BY id ASC",
            )
            .expect("prepare event query");
        stmt.query_map(params![dispatch_id], |row| row.get::<_, String>(0))
            .expect("query event rows")
            .collect::<rusqlite::Result<Vec<_>>>()
            .expect("collect event rows")
    }

    #[test]
    fn reserve_dispatch_blocks_second_inflight_for_issue() {
        let db_path = temp_db_path("dispatch-reserve");
        init_db(&db_path).expect("db init");
        let first_decision_id = seed_decision_id(&db_path);
        let second_decision_id = seed_decision_id(&db_path);

        let first = reserve_dispatch_starting(&db_path, &sample_dispatch(first_decision_id, 7))
            .expect("first");
        let first_id = match first {
            DispatchReservation::Started(id) => id,
            DispatchReservation::InFlightIssue(_) | DispatchReservation::InFlightRepo(_) => {
                panic!("expected first reservation to start")
            }
        };
        assert_eq!(
            dispatch_event_kinds(&db_path, first_id),
            vec!["mark_starting".to_string()]
        );

        let second = reserve_dispatch_starting(&db_path, &sample_dispatch(second_decision_id, 7))
            .expect("second");
        match second {
            DispatchReservation::InFlightIssue(id) => assert_eq!(id, first_id),
            DispatchReservation::Started(_) => panic!("expected second reservation to be blocked"),
            DispatchReservation::InFlightRepo(_) => {
                panic!("expected issue-level inflight, not repo-level inflight")
            }
        }

        let _ = fs::remove_file(db_path);
    }

    #[test]
    fn reserve_dispatch_blocks_second_inflight_impl_for_repo() {
        let db_path = temp_db_path("dispatch-reserve-repo");
        init_db(&db_path).expect("db init");
        let first_decision_id = seed_decision_id(&db_path);
        let second_decision_id = seed_decision_id(&db_path);

        let mut first_insert = sample_dispatch(first_decision_id, 7);
        first_insert.directive = "impl".to_string();
        let mut second_insert = sample_dispatch(second_decision_id, 8);
        second_insert.directive = "impl".to_string();

        let first = reserve_dispatch_starting(&db_path, &first_insert).expect("first");
        let first_id = match first {
            DispatchReservation::Started(id) => id,
            DispatchReservation::InFlightIssue(_) | DispatchReservation::InFlightRepo(_) => {
                panic!("expected first reservation to start")
            }
        };

        let second = reserve_dispatch_starting(&db_path, &second_insert).expect("second");
        match second {
            DispatchReservation::InFlightRepo(id) => assert_eq!(id, first_id),
            DispatchReservation::Started(_) => panic!("expected repo-level inflight block"),
            DispatchReservation::InFlightIssue(_) => {
                panic!("expected repo-level inflight, not issue-level inflight")
            }
        }

        let _ = fs::remove_file(db_path);
    }

    #[test]
    fn stale_autoheal_records_heal_event() {
        let db_path = temp_db_path("dispatch-autoheal");
        init_db(&db_path).expect("db init");
        let decision_id = seed_decision_id(&db_path);
        let started_id = match reserve_dispatch_starting(&db_path, &sample_dispatch(decision_id, 9))
            .expect("reserve")
        {
            DispatchReservation::Started(id) => id,
            DispatchReservation::InFlightIssue(_) | DispatchReservation::InFlightRepo(_) => {
                panic!("expected started dispatch")
            }
        };
        mark_dispatch_failed_runtime(
            &db_path,
            started_id,
            "stale_dispatch_autohealed",
            "stale dispatch test",
        )
        .expect("autoheal should succeed");

        let kinds = dispatch_event_kinds(&db_path, started_id);
        assert_eq!(
            kinds,
            vec!["mark_starting".to_string(), "heal_stale".to_string()]
        );

        let conn = open_db(&db_path).expect("open db");
        let status: String = conn
            .query_row(
                "SELECT status FROM dispatches WHERE id = ?1",
                params![started_id],
                |row| row.get(0),
            )
            .expect("fetch status");
        assert_eq!(status, DispatchState::FailedRuntime.as_db_str());

        let _ = fs::remove_file(db_path);
    }

    #[test]
    fn owner_comment_without_directive_is_ignored() {
        let context = EventContext {
            repo_full_name: "main/orchd-debug".to_string(),
            issue_number: Some(1),
            actor_login: Some("main".to_string()),
            text: Some("just checking in".to_string()),
            source_comment_id: None,
            source_created_at: None,
        };
        let decision = decide("issue_comment", Some(&context));
        assert_eq!(decision.decision, "ignored");
        assert_eq!(decision.reason_code, "no_directive");
        assert!(!decision.would_dispatch);
    }

    #[test]
    fn explicit_directive_is_still_accepted() {
        let context = EventContext {
            repo_full_name: "main/orchd-debug".to_string(),
            issue_number: Some(1),
            actor_login: Some("main".to_string()),
            text: Some("@codex-orch poke".to_string()),
            source_comment_id: None,
            source_created_at: None,
        };
        let decision = decide("issue_comment", Some(&context));
        assert_eq!(decision.decision, "accepted");
        assert_eq!(decision.reason_code, "explicit_directive");
        assert_eq!(decision.directive.as_deref(), Some("poke"));
        assert_eq!(decision.target_role.as_deref(), Some("codex-orch"));
        assert!(decision.would_dispatch);
    }

    #[test]
    fn codex_alias_maps_to_orch_role() {
        let parsed = parse_directive("@codex poke").expect("directive should parse");
        assert_eq!(parsed.role, "codex-orch");
        assert_eq!(parsed.directive, "poke");
    }
}
