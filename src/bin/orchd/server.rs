use std::net::SocketAddr;
use std::time::Duration as StdDuration;

use anyhow::{Context, Result, anyhow};
use axum::body::Bytes;
use axum::extract::State;
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use chrono::{DateTime, TimeDelta, Utc};
use rusqlite::OptionalExtension;
use rusqlite::params;
use serde_json::json;

use forgejo_agent::api::ForgejoClient;
use forgejo_agent::config::AgentConfig;
use forgejo_agent::policy::STATE_LABEL_COLOR;
use forgejo_agent::types::{IssueRef, OrchdRuntimeState, RepoRef, WorkflowState};

use super::cli::{Cli, DispatchMode};
use super::db;
use super::dispatch;
use super::dispatch_config::DispatchTriggerGuardrailsConfig;
use super::dispatch_config_live::{DispatchConfigHandle, run_dispatch_config_reload_loop};
use super::errors::runtime_state_for_dispatch_error;
use super::inquisition::{InquisitionSpec, maybe_spawn_inquisition};
use super::lexicon::{
    DIRECTIVE_AUDIT, DIRECTIVE_AUDIT_FAILURE, DIRECTIVE_IMPL, EVENT_ISSUE_COMMENT, EVENT_ISSUES,
};
use super::notifier;
use super::paths::{expand_tilde_path, resolve_dispatch_config_path};
use super::projection;
use super::role;
use super::state::{
    AppState, ErrorEnvelope, EventRecord, HealthEnvelope, WebhookOutcome, WebhookPayload,
};
use super::telemetry::log_line;
use super::webhook::{
    decide, extract_event_context, extract_header, load_secret, synthetic_delivery_id,
    trigger_dedupe_key, verify_signature,
};

pub(super) async fn run_server(cli: Cli) -> Result<()> {
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
    let identity_cfg = cfg.clone();
    tokio::task::spawn_blocking(move || enforce_machine_identity(&identity_cfg))
        .await
        .context("startup identity guard task failed")??;
    let dispatch_config_path = resolve_dispatch_config_path(&cli.dispatch_config)?;
    let dispatch_config = match cli.dispatch_mode {
        DispatchMode::DryRun => DispatchConfigHandle::Disabled,
        DispatchMode::Exec => DispatchConfigHandle::load(dispatch_config_path.clone())?,
    };
    if matches!(cli.dispatch_mode, DispatchMode::Exec) {
        if cli.skip_startup_role_check {
            log_line(
                "startup_role_check_skipped",
                json!({
                    "reason": "--skip-startup-role-check",
                }),
            );
        } else {
            let startup_cfg = cfg.clone();
            let startup_dispatch = dispatch_config
                .snapshot()
                .ok_or_else(|| anyhow!("dispatch config missing in exec mode"))?;
            let roles_checked = startup_dispatch.roles.len();
            tokio::task::spawn_blocking(move || {
                role::enforce_startup_role_check(startup_dispatch.as_ref(), &startup_cfg)
            })
            .await
            .context("startup role check task failed")??;
            log_line(
                "startup_role_check_passed",
                json!({
                    "roles_checked": roles_checked,
                }),
            );
        }
    }

    let db_path = expand_tilde_path(&cli.db_path)?;
    db::init_db(&db_path)?;
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
        dispatch_mode: cli.dispatch_mode,
        dispatch_backend: cli.dispatch_backend,
        dispatch_config,
    };
    let mode_name = state.dispatch_mode.as_str();
    let backend_name = state.dispatch_backend.as_str();

    match dispatch::heal_stale_inflight_dispatches(&state.db_path) {
        Ok(healed) => {
            log_line(
                "startup_stale_heal_complete",
                json!({
                    "healed_dispatches": healed,
                }),
            );
        }
        Err(err) => {
            log_line(
                "startup_stale_heal_failed",
                json!({
                    "error": err.to_string(),
                }),
            );
        }
    }

    let heartbeat_state = state.clone();
    tokio::spawn(async move {
        run_heartbeat_loop(heartbeat_state, cli.heartbeat_sec).await;
    });

    if matches!(state.dispatch_mode, DispatchMode::Exec) {
        let reload_handle = state.dispatch_config.clone();
        tokio::spawn(async move {
            run_dispatch_config_reload_loop(reload_handle, cli.dispatch_config_reload_sec).await;
        });
    }

    let reconcile_state = state.clone();
    tokio::spawn(async move {
        run_reconcile_loop(reconcile_state, cli.reconcile_sec).await;
    });

    let queue_state = state.clone();
    tokio::spawn(async move {
        run_dispatch_queue_loop(queue_state, cli.heartbeat_sec).await;
    });

    if let Some(notifications) = state
        .dispatch_config
        .snapshot()
        .map(|cfg| cfg.notifications.clone())
        && notifications.enabled
    {
        let notification_state = state.clone();
        log_line(
            "notification_loop_start",
            json!({
                "poll_sec": notifications.poll_sec,
                "phases": notifications.phases.iter().map(|phase| phase.as_db_str()).collect::<Vec<_>>(),
                "app_name": notifications.app_name.as_str(),
                "watch_login": notifications.watch_login.as_str(),
                "notify_send_bin": notifications.notify_send_bin.to_string_lossy(),
            }),
        );
        tokio::spawn(async move {
            notifier::run_notification_loop(notification_state, notifications).await;
        });
    }

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

fn enforce_machine_identity(cfg: &AgentConfig) -> Result<()> {
    let api = ForgejoClient::new(cfg).context("failed to initialize Forgejo client")?;
    let whoami = api
        .whoami(cfg)
        .context("failed to resolve authenticated Forgejo identity")?;
    let login = whoami
        .get("login")
        .and_then(serde_json::Value::as_str)
        .ok_or_else(|| anyhow!("Forgejo /api/v1/user response missing login"))?;
    let allow_main = std::env::var("ORCHD_ALLOW_MAIN_LOGIN")
        .ok()
        .is_some_and(|value| value == "1");
    if login.eq_ignore_ascii_case("main") && !allow_main {
        return Err(anyhow!(
            "orchd refuses to run with Forgejo login 'main'; use the orchd machine token"
        ));
    }
    if login.eq_ignore_ascii_case("main") && allow_main {
        log_line(
            "startup_identity_override",
            json!({
                "login": login,
                "reason": "ORCHD_ALLOW_MAIN_LOGIN=1",
            }),
        );
    } else {
        log_line(
            "startup_identity",
            json!({
                "login": login,
            }),
        );
    }
    Ok(())
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
    Json(HealthEnvelope {
        status: "ok",
        build: env!("FORGEJO_AGENT_BUILD"),
        git_sha: option_env!("FORGEJO_AGENT_GIT_SHA").filter(|s| !s.is_empty()),
    })
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
            let _ = db::upsert_repo_seen(&db_path, repo_full_name);

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
                &[EVENT_ISSUES, EVENT_ISSUE_COMMENT],
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

const fn default_trigger_guardrails() -> DispatchTriggerGuardrailsConfig {
    DispatchTriggerGuardrailsConfig {
        max_depth_per_issue: 6,
        max_dispatches_per_window: 12,
        window_sec: 3600,
        cooldown_sec: 60,
        deny_immediate_self_loop: true,
    }
}

fn apply_trigger_guardrails(
    state: &AppState,
    record: &EventRecord,
    decision: &super::state::DecisionRecord,
) -> Result<Option<String>> {
    if !decision.trigger_apply_guardrails {
        return Ok(None);
    }
    let Some(issue_number) = record.issue_number else {
        return Ok(None);
    };
    let Some(target_role) = decision.target_role.as_deref() else {
        return Ok(None);
    };

    let guardrails = state
        .dispatch_config
        .snapshot()
        .map(|config| config.trigger_guardrails.clone())
        .unwrap_or_else(default_trigger_guardrails);
    let lookback = TimeDelta::seconds(i64::try_from(guardrails.window_sec).unwrap_or(i64::MAX));
    let since = (Utc::now() - lookback).to_rfc3339();
    let guardrail_stats = db::issue_trigger_guardrail_stats(
        &state.db_path,
        &record.repo_full_name,
        issue_number,
        &since,
    )?;

    if guardrail_stats.total >= u64::from(guardrails.max_depth_per_issue) {
        return Ok(Some("guardrail_depth".to_string()));
    }
    if guardrail_stats.recent >= u64::from(guardrails.max_dispatches_per_window) {
        return Ok(Some("guardrail_rate".to_string()));
    }
    // Self-loop guardrail is meant to prevent trigger-fired dispatches that target the same
    // principal that produced the triggering event (codex<->codex ping-pong is handled by
    // rate/depth/cooldown, and we still want humans to be able to send repeated follow-ups).
    if guardrails.deny_immediate_self_loop {
        let actor = record
            .actor_login
            .as_deref()
            .unwrap_or_default()
            .trim()
            .to_ascii_lowercase();
        if !actor.is_empty() && actor == target_role.trim().to_ascii_lowercase() {
            return Ok(Some("guardrail_self_loop".to_string()));
        }
    }
    if guardrails.cooldown_sec > 0
        && let Some(last_created_at) = guardrail_stats.last_created_at.as_deref()
        && let Ok(parsed) = DateTime::parse_from_rfc3339(last_created_at)
    {
        let elapsed = Utc::now().signed_duration_since(parsed.with_timezone(&Utc));
        let cooldown =
            TimeDelta::seconds(i64::try_from(guardrails.cooldown_sec).unwrap_or(i64::MAX));
        if elapsed < cooldown {
            return Ok(Some("guardrail_cooldown".to_string()));
        }
    }

    Ok(None)
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
    let mut context = extract_event_context(&event_type, &payload);

    let record = EventRecord {
        delivery_id: delivery_id.clone(),
        event_type: event_type.clone(),
        repo_full_name: context
            .as_ref()
            .map_or_else(|| "<unknown>".to_string(), |ctx| ctx.repo_full_name.clone()),
        issue_number: context.as_ref().and_then(|ctx| ctx.issue_number),
        source_issue_id: context.as_ref().and_then(|ctx| ctx.source_issue_id),
        source_issue_anchor_at: context
            .as_ref()
            .and_then(|ctx| ctx.source_issue_anchor_at.clone()),
        action: payload.action.clone(),
        actor_login: context.as_ref().and_then(|ctx| ctx.actor_login.clone()),
        event_text: context.as_ref().and_then(|ctx| ctx.text.clone()),
        source_comment_id: context.as_ref().and_then(|ctx| ctx.source_comment_id),
        source_created_at: context
            .as_ref()
            .and_then(|ctx| ctx.source_created_at.clone()),
        raw_json: String::from_utf8_lossy(body).to_string(),
    };

    let Some(event_id) = db::insert_event(&state.db_path, &record)? else {
        return Ok(WebhookOutcome {
            status: "duplicate".to_string(),
            delivery_id,
            event_type,
            decision: "duplicate".to_string(),
            reason_code: "duplicate_delivery".to_string(),
            duplicate: true,
        });
    };
    let _ = db::upsert_repo_seen(&state.db_path, &record.repo_full_name);

    let dispatch_config = state.dispatch_config.snapshot();

    if record.event_type == EVENT_ISSUE_COMMENT
        && record.action.as_deref() == Some("created")
        && let (Some(issue_number), Some(ctx)) = (record.issue_number, context.as_mut())
    {
        ctx.conversation_role = db::latest_issue_role_comment_actor_login_before_event_id(
            &state.db_path,
            &record.repo_full_name,
            issue_number,
            event_id,
        )?;
    }
    let mut decision = decide(
        &event_type,
        record.action.as_deref(),
        context.as_ref(),
        dispatch_config
            .as_ref()
            .map(|config| config.triggers.as_slice()),
    );

    if decision.would_dispatch
        && let Some(trigger_id) = decision.trigger_id.as_deref()
    {
        let dedupe_key = trigger_dedupe_key(&record, trigger_id);
        decision.trigger_dedupe_key = Some(dedupe_key.clone());
        let accepted = db::claim_trigger_dispatch_dedupe(
            &state.db_path,
            event_id,
            trigger_id,
            &dedupe_key,
            &record.repo_full_name,
            record.issue_number,
        )?;
        if !accepted {
            decision.suppress_dispatch("trigger_duplicate");
        }
    }

    if decision.would_dispatch
        && let Some(reason) = apply_trigger_guardrails(state, &record, &decision)?
    {
        decision.suppress_dispatch(reason);
    }

    let decision_id = db::insert_decision(&state.db_path, event_id, &record, &decision)?;

    let mut status_projected = false;
    let mut status_error: Option<String> = None;
    if decision.would_dispatch {
        if let Some(issue_number) = record.issue_number {
            let dispatch_identity = projection::dispatch_comment_identity(state, &decision);
            if let Err(err) = projection::project_issue_runtime_state(
                state.clone(),
                &record.repo_full_name,
                issue_number,
                OrchdRuntimeState::Queued,
                None,
                dispatch_identity.clone(),
            )
            .await
            {
                status_error = Some(err.to_string());
            } else {
                status_projected = true;
            }
            match state.dispatch_mode {
                DispatchMode::DryRun => {}
                DispatchMode::Exec => {
                    let defer_impl = match decision.directive.as_deref() {
                        Some(DIRECTIVE_IMPL) => match db::latest_repo_inflight_impl_dispatch_id(
                            &state.db_path,
                            &record.repo_full_name,
                        ) {
                            Ok(Some(inflight)) => {
                                log_line(
                                    "dispatch_deferred_repo_busy",
                                    json!({
                                        "repo": record.repo_full_name,
                                        "issue_number": issue_number,
                                        "directive": DIRECTIVE_IMPL,
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
                        match dispatch::dispatch_issue(
                            state.clone(),
                            decision_id,
                            event_id,
                            &record,
                            &decision,
                        )
                        .await
                        {
                            Ok(()) => {
                                if let Err(err) = projection::project_issue_runtime_state(
                                    state.clone(),
                                    &record.repo_full_name,
                                    issue_number,
                                    OrchdRuntimeState::Running,
                                    None,
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
                                    && let Err(err) = db::upsert_issue_role_cursor_event_id(
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
                                let runtime_state = runtime_state_for_dispatch_error(&err);
                                let projection = projection::project_issue_runtime_state(
                                    state.clone(),
                                    &record.repo_full_name,
                                    issue_number,
                                    runtime_state,
                                    Some(err.reason_code()),
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

                                if runtime_state == OrchdRuntimeState::Failed
                                    && decision.target_role.as_deref() != Some("codex-audit")
                                    && decision.directive.as_deref() != Some(DIRECTIVE_AUDIT)
                                    && decision.directive.as_deref()
                                        != Some(DIRECTIVE_AUDIT_FAILURE)
                                    && let Some(identity) = dispatch_identity.clone()
                                {
                                    let db_path = state.db_path.clone();
                                    let default_owner = state.cfg.default_repo.owner.clone();
                                    let repo_full_name = record.repo_full_name.clone();
                                    let repo_full_name_for_log = repo_full_name.clone();
                                    let directive = decision.directive.clone();
                                    let role_name = decision.target_role.clone();
                                    let reason_code = err.reason_code().to_string();
                                    let reason_code_for_log = reason_code.clone();
                                    let error_text = err.to_string();
                                    let spawn_outcome = tokio::task::spawn_blocking(move || {
                                        let repo = RepoRef::parse(&repo_full_name)
                                            .context("invalid repo_full_name")?;
                                        let source_issue = IssueRef {
                                            repo,
                                            number: issue_number,
                                        };
                                        let dispatch_id = db::latest_issue_dispatch_id(
                                            &db_path,
                                            &repo_full_name,
                                            issue_number,
                                        )
                                        .ok()
                                        .flatten();
                                        let run_dir = dispatch_id.and_then(|dispatch_id| {
                                            db_path.parent().map(|parent| {
                                                parent
                                                    .join("dispatch-runs")
                                                    .join(format!("dispatch-{dispatch_id}"))
                                                    .to_string_lossy()
                                                    .into_owned()
                                            })
                                        });
                                        let spec = InquisitionSpec {
                                            source_issue,
                                            source_issue_title: None,
                                            source_issue_url: None,
                                            dispatch_id,
                                            directive,
                                            role_name,
                                            reason_code,
                                            exit_code: None,
                                            run_dir,
                                            log_file: None,
                                            completion_file: None,
                                            error_text: Some(error_text),
                                        };
                                        maybe_spawn_inquisition(
                                            &db_path,
                                            &default_owner,
                                            &identity,
                                            spec,
                                        )
                                    })
                                    .await;
                                    match spawn_outcome {
                                        Ok(Ok(())) => {}
                                        Ok(Err(err)) => log_line(
                                            "inquisition_spawn_failed",
                                            json!({
                                                "repo": repo_full_name_for_log,
                                                "issue_number": issue_number,
                                                "reason_code": reason_code_for_log,
                                                "error": err.to_string(),
                                            }),
                                        ),
                                        Err(err) => log_line(
                                            "inquisition_spawn_join_failed",
                                            json!({
                                                "repo": repo_full_name_for_log,
                                                "issue_number": issue_number,
                                                "reason_code": reason_code_for_log,
                                                "error": err.to_string(),
                                            }),
                                        ),
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

    db::update_decision_comment_status(
        &state.db_path,
        decision_id,
        status_projected,
        status_error,
    )?;

    log_line(
        "decision",
        json!({
            "delivery_id": record.delivery_id,
            "event_type": record.event_type,
            "action": record.action,
            "repo": record.repo_full_name,
            "issue_number": record.issue_number,
            "actor": record.actor_login,
            "decision": decision.decision,
            "reason_code": decision.reason_code,
            "decision_source": decision.decision_source,
            "trigger_id": decision.trigger_id,
            "trigger_dedupe_key": decision.trigger_dedupe_key,
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
    let items = db::queued_impl_decisions(&state.db_path, 10)?;
    for item in items {
        let repo_full_name = item.record.repo_full_name.clone();
        if let Ok(Some(inflight)) =
            db::latest_repo_inflight_impl_dispatch_id(&state.db_path, &repo_full_name)
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
        let dispatch_identity = projection::dispatch_comment_identity(state, &item.decision);
        match dispatch::dispatch_issue(
            state.clone(),
            item.decision_id,
            item.event_id,
            &item.record,
            &item.decision,
        )
        .await
        {
            Ok(()) => {
                let _ = projection::project_issue_runtime_state(
                    state.clone(),
                    &item.record.repo_full_name,
                    issue_number,
                    OrchdRuntimeState::Running,
                    None,
                    dispatch_identity.clone(),
                )
                .await;
                if let Some(role_name) = item.decision.target_role.as_deref() {
                    let _ = db::upsert_issue_role_cursor_event_id(
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
    let conn = db::open_db(&state.db_path)?;
    let queue_depth: i64 = conn.query_row(
        "SELECT COUNT(*) FROM decisions WHERE would_dispatch = 1 AND comment_posted = 0",
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

    let (scanned_open, normalized_triage) =
        tokio::task::spawn_blocking(move || -> Result<(usize, usize)> {
            let api = ForgejoClient::new(&cfg)?;
            let issues = api.list_issues(&cfg, &repo, "open", 100)?;
            let scanned_open = issues.len();

            // Keep open issues from being "stateless" for workflow operations:
            // missing workflow label => default to triage.
            let mut triage_id = None;
            let mut normalized_triage = 0usize;
            for issue in issues {
                if issue.pull_request.is_some() {
                    continue;
                }
                let Ok(None) = issue.workflow_state() else {
                    continue;
                };

                let id = if let Some(id) = triage_id {
                    id
                } else {
                    let (name, color, description, exclusive) = STATE_LABEL_COLOR
                        .iter()
                        .find(|(name, ..)| *name == WorkflowState::Triage.label())
                        .copied()
                        .unwrap_or(("state/triage", "8a8a8a", "needs triage", true));
                    let ensured =
                        api.ensure_label(&cfg, &repo, name, color, description, exclusive)?;
                    triage_id = Some(ensured.id);
                    ensured.id
                };

                let issue_ref = IssueRef {
                    repo: repo.clone(),
                    number: issue.number,
                };
                let _ = api.add_issue_label_ids(&cfg, &issue_ref, vec![id]);
                normalized_triage += 1;
            }

            Ok((scanned_open, normalized_triage))
        })
        .await
        .context("reconcile join failure")??;

    let conn = db::open_db(&state.db_path)?;
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
            "normalized_triage": normalized_triage,
            "status": "ok",
        }),
    );
    Ok(())
}
