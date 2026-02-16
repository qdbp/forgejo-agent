use std::net::SocketAddr;
use std::time::Duration as StdDuration;

use anyhow::{Context, Result, anyhow};
use axum::body::Bytes;
use axum::extract::State;
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use chrono::Utc;
use rusqlite::OptionalExtension;
use rusqlite::params;
use serde_json::json;

use forgejo_agent::api::ForgejoClient;
use forgejo_agent::config::AgentConfig;
use forgejo_agent::types::{OrchdRuntimeState, RepoRef};

use super::cli::{Cli, DispatchMode};
use super::db;
use super::dispatch;
use super::dispatch_config::load_dispatch_config;
use super::errors::runtime_state_for_dispatch_error;
use super::lexicon::{DIRECTIVE_IMPL, EVENT_ISSUE_COMMENT, EVENT_ISSUES};
use super::notifier;
use super::paths::expand_tilde_path;
use super::projection;
use super::state::{
    AppState, ErrorEnvelope, EventRecord, HealthEnvelope, WebhookOutcome, WebhookPayload,
};
use super::telemetry::log_line;
use super::webhook::{
    decide, extract_event_context, extract_header, load_secret, synthetic_delivery_id,
    verify_signature,
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
    let dispatch_config_path = expand_tilde_path(&cli.dispatch_config)?;
    let dispatch_config = match cli.dispatch_mode {
        DispatchMode::DryRun => None,
        DispatchMode::Exec => Some(load_dispatch_config(&dispatch_config_path)?),
    };

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

    let reconcile_state = state.clone();
    tokio::spawn(async move {
        run_reconcile_loop(reconcile_state, cli.reconcile_sec).await;
    });

    let queue_state = state.clone();
    tokio::spawn(async move {
        run_dispatch_queue_loop(queue_state, cli.heartbeat_sec).await;
    });

    if matches!(state.dispatch_mode, DispatchMode::Exec)
        && let Some(notifications) = state
            .dispatch_config
            .as_ref()
            .map(|cfg| cfg.notifications.clone())
        && notifications.enabled
    {
        let notification_state = state.clone();
        log_line(
            "notification_loop_start",
            json!({
                "poll_sec": notifications.poll_sec,
                "phases": notifications.phases.iter().map(|phase| phase.as_db_str()).collect::<Vec<_>>(),
                "app_name": notifications.app_name,
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

    let decision = decide(&event_type, record.action.as_deref(), context.as_ref());
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
                                let projection = projection::project_issue_runtime_state(
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

    let scanned_open = tokio::task::spawn_blocking(move || {
        let api = ForgejoClient::new(&cfg)?;
        api.list_issues(&cfg, &repo, "open", 100)
            .map(|items| items.len())
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
            "status": "ok",
        }),
    );
    Ok(())
}
