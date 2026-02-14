#[allow(dead_code)]
#[path = "../api.rs"]
mod api;
#[allow(dead_code)]
#[path = "../config.rs"]
mod config;
#[allow(dead_code)]
#[path = "../types.rs"]
mod types;

use std::collections::HashMap;
use std::fs;
use std::fs::OpenOptions;
use std::io::Write as _;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::Duration as StdDuration;

use anyhow::{Context, Result, anyhow};
use axum::body::Bytes;
use axum::extract::State;
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use chrono::{DateTime, Duration as ChronoDuration, Utc};
use clap::{Parser, ValueEnum};
use hmac::{Hmac, Mac};
use rusqlite::{Connection, OptionalExtension, params};
use serde::{Deserialize, Serialize};
use serde_json::json;
use sha2::{Digest, Sha256};

use api::ForgejoClient;
use config::AgentConfig;
use types::{ApiIssue, IssueRef, RepoRef};

type HmacSha256 = Hmac<Sha256>;

#[derive(Parser, Debug)]
#[command(name = "orchd")]
#[command(about = "Dev-mode reactive orchestrator")]
struct Cli {
    #[arg(long)]
    config: Option<PathBuf>,
    #[arg(long = "token-file")]
    token_file: Option<PathBuf>,
    #[arg(long, default_value = "127.0.0.1:7878")]
    listen: String,
    #[arg(long, default_value = "~/.local/state/orchd-dev/orchd.sqlite")]
    db_path: String,
    #[arg(long = "webhook-secret-file")]
    webhook_secret_file: Option<String>,
    #[arg(long, default_value_t = 20)]
    heartbeat_sec: u64,
    #[arg(long, default_value_t = 60)]
    reconcile_sec: u64,
    #[arg(long)]
    reconcile_repo: Option<RepoRef>,
    #[arg(long, value_enum, default_value_t = DispatchMode::TmuxTui)]
    dispatch_mode: DispatchMode,
    #[arg(long, default_value = "config/orchd-dispatch.toml")]
    dispatch_config: String,
    #[arg(long)]
    no_comment_echo: bool,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, ValueEnum)]
enum DispatchMode {
    DryRun,
    TmuxExec,
    TmuxTui,
}

impl DispatchMode {
    const fn as_str(self) -> &'static str {
        match self {
            Self::DryRun => "dry-run",
            Self::TmuxExec => "tmux-exec",
            Self::TmuxTui => "tmux-tui",
        }
    }
}

#[derive(Clone, Debug)]
struct DispatchConfig {
    allowed_actors: Vec<String>,
    tmux: DispatchTmuxConfig,
    roles: HashMap<String, DispatchRoleConfig>,
    directives: HashMap<String, DispatchDirectiveConfig>,
    forgejoctl_bin: PathBuf,
}

#[derive(Clone, Debug)]
struct DispatchTmuxConfig {
    session: String,
    remain_on_exit: bool,
}

#[derive(Clone, Debug)]
struct DispatchRoleConfig {
    codex_bin: PathBuf,
    codex_role_arg: String,
    token_file: PathBuf,
    workdir: PathBuf,
}

#[derive(Clone, Debug)]
struct DispatchDirectiveConfig {
    role: String,
    prompt_file: PathBuf,
    timeout_sec: u64,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct DispatchConfigFile {
    version: u32,
    #[serde(default)]
    allowed_actors: Vec<String>,
    tmux: DispatchTmuxConfigFile,
    roles: HashMap<String, DispatchRoleConfigFile>,
    directives: HashMap<String, DispatchDirectiveConfigFile>,
    #[serde(default = "default_forgejoctl_bin")]
    forgejoctl_bin: String,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct DispatchTmuxConfigFile {
    session: String,
    #[serde(default = "default_tmux_remain_on_exit")]
    remain_on_exit: bool,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct DispatchRoleConfigFile {
    #[serde(default = "default_codex_bin")]
    codex_bin: String,
    codex_role_arg: Option<String>,
    token_file: String,
    workdir: String,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct DispatchDirectiveConfigFile {
    role: String,
    prompt_file: String,
    #[serde(default = "default_timeout_sec")]
    timeout_sec: u64,
}

const fn default_tmux_remain_on_exit() -> bool {
    true
}

fn default_codex_bin() -> String {
    "/home/main/forgejo-agent/bin/codex-role".to_string()
}

fn default_forgejoctl_bin() -> String {
    "/home/main/.local/bin/forgejoctl".to_string()
}

const fn default_timeout_sec() -> u64 {
    3600
}

const STARTING_DISPATCH_STALE_AFTER_SEC: i64 = 120;

#[derive(Debug, thiserror::Error)]
enum DispatchError {
    #[error("dispatch config not loaded")]
    ConfigNotLoaded,
    #[error("actor not allowed: {0}")]
    ActorNotAllowed(String),
    #[error("directive not configured: {0}")]
    DirectiveNotConfigured(String),
    #[error("role not configured: {0}")]
    RoleNotConfigured(String),
    #[error(
        "issue dispatch already in flight for {repo_full_name}#{issue_number} (dispatch {dispatch_id})"
    )]
    IssueDispatchInFlight {
        repo_full_name: String,
        issue_number: u64,
        dispatch_id: i64,
    },
    #[error("repo lock held at {0}")]
    RepoLocked(PathBuf),
    #[error("invalid issue ref: {0}")]
    InvalidIssueRef(String),
    #[error("io failure: {0}")]
    Io(String),
    #[error("tmux failure: {0}")]
    Tmux(String),
    #[error("issue fetch failure: {0}")]
    IssueFetch(String),
    #[error("db failure: {0}")]
    Db(String),
}

impl DispatchError {
    const fn reason_code(&self) -> &'static str {
        match self {
            Self::ConfigNotLoaded => "dispatch_config_missing",
            Self::ActorNotAllowed(_) => "actor_not_allowed",
            Self::DirectiveNotConfigured(_) => "directive_not_configured",
            Self::RoleNotConfigured(_) => "role_not_configured",
            Self::IssueDispatchInFlight { .. } => "issue_dispatch_in_flight",
            Self::RepoLocked(_) => "repo_locked",
            Self::InvalidIssueRef(_) => "invalid_issue_ref",
            Self::Io(_) => "io_failure",
            Self::Tmux(_) => "tmux_failure",
            Self::IssueFetch(_) => "issue_fetch_failure",
            Self::Db(_) => "db_failure",
        }
    }
}

#[derive(Debug)]
struct DispatchLaunch {
    dispatch_id: i64,
    tmux_locator: String,
    run_dir: PathBuf,
}

#[derive(Clone)]
struct CommentIdentity {
    forgejoctl_bin: PathBuf,
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
    tmux_session: Option<String>,
    tmux_window: Option<String>,
    lock_path: Option<String>,
}

struct TmuxRunScriptInputs<'a> {
    dispatch_id: i64,
    db_path: &'a Path,
    lock_path: &'a Path,
    run_dir: &'a Path,
    prompt_path: &'a Path,
    summary_path: &'a Path,
    completion_path: &'a Path,
    last_message_path: &'a Path,
    codex_log_path: &'a Path,
    marker_path: &'a Path,
    issue_ref_text: &'a str,
    forgejoctl_bin: &'a Path,
    token_file: &'a Path,
    workdir: &'a Path,
    codex_bin: &'a Path,
    codex_role_arg: &'a str,
    issue_session_id: Option<&'a str>,
    directive_name: &'a str,
    role_name: &'a str,
    tmux_locator: &'a str,
    timeout_sec: u64,
}

#[derive(Clone)]
struct AppState {
    db_path: PathBuf,
    webhook_secret: Option<Vec<u8>>,
    cfg: AgentConfig,
    reconcile_repo: RepoRef,
    comment_echo: bool,
    dispatch_mode: DispatchMode,
    dispatch_config: Option<DispatchConfig>,
}

#[derive(Debug, Deserialize)]
struct WebhookPayload {
    action: Option<String>,
    repository: Option<WebhookRepository>,
    issue: Option<WebhookIssue>,
    comment: Option<WebhookComment>,
    sender: Option<WebhookUser>,
}

#[derive(Debug, Deserialize)]
struct WebhookRepository {
    full_name: String,
}

#[derive(Debug, Deserialize)]
struct WebhookIssue {
    number: u64,
    #[serde(default)]
    body: Option<String>,
}

#[derive(Debug, Deserialize)]
struct WebhookComment {
    body: String,
    #[serde(default)]
    user: Option<WebhookUser>,
}

#[derive(Debug, Deserialize)]
struct WebhookUser {
    login: String,
}

#[derive(Debug, Clone)]
struct EventRecord {
    delivery_id: String,
    event_type: String,
    repo_full_name: String,
    issue_number: Option<u64>,
    action: Option<String>,
    actor_login: Option<String>,
    raw_json: String,
}

#[derive(Debug, Clone)]
struct EventContext {
    repo_full_name: String,
    repo_owner: String,
    issue_number: Option<u64>,
    actor_login: Option<String>,
    text: Option<String>,
}

#[derive(Debug, Clone)]
struct ParsedDirective {
    role: String,
    directive: String,
}

#[derive(Debug, Clone)]
struct DecisionRecord {
    decision: String,
    reason_code: String,
    directive: Option<String>,
    target_role: Option<String>,
    would_dispatch: bool,
}

#[derive(Debug, Serialize)]
struct WebhookOutcome {
    status: String,
    delivery_id: String,
    event_type: String,
    decision: String,
    reason_code: String,
    duplicate: bool,
}

#[derive(Debug, Serialize)]
struct ErrorEnvelope {
    error: String,
}

#[derive(Debug, Serialize)]
struct HealthEnvelope {
    status: &'static str,
}

fn main() {
    if let Err(err) = run_entry() {
        eprintln!("error: {err:#}");
        std::process::exit(1);
    }
}

fn run_entry() -> Result<()> {
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .context("failed to build tokio runtime")?;
    runtime.block_on(run())
}

async fn run() -> Result<()> {
    let cli = Cli::parse();
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
        cfg,
        reconcile_repo,
        comment_echo: !cli.no_comment_echo,
        dispatch_mode: cli.dispatch_mode,
        dispatch_config,
    };
    let mode_name = state.dispatch_mode.as_str();

    let heartbeat_state = state.clone();
    tokio::spawn(async move {
        run_heartbeat_loop(heartbeat_state, cli.heartbeat_sec).await;
    });

    let reconcile_state = state.clone();
    tokio::spawn(async move {
        run_reconcile_loop(reconcile_state, cli.reconcile_sec).await;
    });

    let app = Router::new()
        .route("/healthz", get(healthz_handler))
        .route("/webhook", post(webhook_handler))
        .with_state(state);

    let listen_addr: SocketAddr = cli
        .listen
        .parse()
        .with_context(|| format!("invalid --listen value: {}", cli.listen))?;
    let listener = tokio::net::TcpListener::bind(listen_addr)
        .await
        .with_context(|| format!("failed to bind {listen_addr}"))?;
    log_line(
        "startup",
        json!({
            "listen": listen_addr.to_string(),
            "heartbeat_sec": cli.heartbeat_sec,
            "reconcile_sec": cli.reconcile_sec,
            "mode": mode_name,
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
    Json(HealthEnvelope { status: "ok" })
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

    let decision = decide(&event_type, context.as_ref());
    let decision_id = insert_decision(&state.db_path, event_id, &record, &decision)?;

    let mut comment_posted = false;
    let mut comment_error: Option<String> = None;
    if decision.decision == "accepted" {
        if let Some(issue_number) = record.issue_number {
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
                            Ok(()) => {
                                comment_posted = true;
                            }
                            Err(err) => {
                                comment_error = Some(err.to_string());
                            }
                        }
                    }
                }
                DispatchMode::TmuxExec | DispatchMode::TmuxTui => {
                    let comment_identity = dispatch_comment_identity(state, &decision);
                    match dispatch_tmux(state.clone(), decision_id, &record, &decision).await {
                        Ok(launch) => {
                            if state.comment_echo {
                                let comment_body = format!(
                                    "orchd: dispatch started id={} directive={} role={} tmux={} run_dir={} delivery={}",
                                    launch.dispatch_id,
                                    decision.directive.as_deref().unwrap_or("-"),
                                    decision.target_role.as_deref().unwrap_or("-"),
                                    launch.tmux_locator,
                                    launch.run_dir.to_string_lossy(),
                                    record.delivery_id,
                                );
                                let post_result = if let Some(identity) = comment_identity.clone() {
                                    post_issue_comment_as_role(
                                        &record.repo_full_name,
                                        issue_number,
                                        comment_body,
                                        identity,
                                    )
                                    .await
                                } else {
                                    post_issue_comment(
                                        state.clone(),
                                        &record.repo_full_name,
                                        issue_number,
                                        comment_body,
                                    )
                                    .await
                                };
                                match post_result {
                                    Ok(()) => {
                                        comment_posted = true;
                                    }
                                    Err(err) => {
                                        comment_error = Some(err.to_string());
                                    }
                                }
                            }
                        }
                        Err(err) => {
                            let reason = err.reason_code();
                            let msg = err.to_string();
                            if state.comment_echo {
                                let comment_body = format!(
                                    "orchd: dispatch blocked reason={} error={} delivery={}",
                                    reason, msg, record.delivery_id
                                );
                                let post_result = if let Some(identity) = comment_identity.clone() {
                                    post_issue_comment_as_role(
                                        &record.repo_full_name,
                                        issue_number,
                                        comment_body,
                                        identity,
                                    )
                                    .await
                                } else {
                                    post_issue_comment(
                                        state.clone(),
                                        &record.repo_full_name,
                                        issue_number,
                                        comment_body,
                                    )
                                    .await
                                };
                                match post_result {
                                    Ok(()) => {
                                        comment_posted = true;
                                    }
                                    Err(post_err) => {
                                        comment_error = Some(post_err.to_string());
                                    }
                                }
                            }
                            if comment_error.is_none() {
                                comment_error = Some(format!("dispatch {reason}: {msg}"));
                            }
                        }
                    }
                }
            }
        } else {
            comment_error = Some("missing issue number".to_string());
        }
    }

    update_decision_comment_status(&state.db_path, decision_id, comment_posted, comment_error)?;

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
            "comment_posted": comment_posted,
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

async fn post_issue_comment_as_role(
    repo_full_name: &str,
    issue_number: u64,
    body: String,
    identity: CommentIdentity,
) -> Result<()> {
    let issue_ref = format!("{repo_full_name}#{issue_number}");
    tokio::task::spawn_blocking(move || -> Result<()> {
        let mut child = Command::new(&identity.forgejoctl_bin)
            .args([
                "--token-file",
                &identity.token_file.to_string_lossy(),
                "issue",
                "comment",
                &issue_ref,
                "--body-stdin",
            ])
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .spawn()
            .with_context(|| {
                format!(
                    "failed to spawn forgejoctl comment command: {}",
                    identity.forgejoctl_bin.display()
                )
            })?;

        if let Some(stdin) = child.stdin.as_mut() {
            stdin
                .write_all(body.as_bytes())
                .context("failed writing comment body to forgejoctl stdin")?;
        }

        let output = child
            .wait_with_output()
            .context("failed waiting on forgejoctl comment command")?;
        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            return Err(anyhow!(
                "forgejoctl comment failed for {issue_ref}: {}",
                stderr.trim()
            ));
        }
        Ok(())
    })
    .await
    .context("comment task join failure")??;
    Ok(())
}

fn extract_event_context(event_type: &str, payload: &WebhookPayload) -> Option<EventContext> {
    let repo_full_name = payload.repository.as_ref()?.full_name.clone();
    let repo_owner = repo_full_name
        .split_once('/')
        .map_or_else(|| "<unknown>".to_string(), |(owner, _)| owner.to_string());
    let issue_number = payload.issue.as_ref().map(|issue| issue.number);

    let actor_login = payload
        .sender
        .as_ref()
        .map(|sender| sender.login.clone())
        .or_else(|| {
            payload
                .comment
                .as_ref()
                .and_then(|comment| comment.user.as_ref().map(|user| user.login.clone()))
        });

    let text = match event_type {
        "issue_comment" => payload.comment.as_ref().map(|comment| comment.body.clone()),
        "issues" => payload.issue.as_ref().and_then(|issue| issue.body.clone()),
        _ => None,
    };

    Some(EventContext {
        repo_full_name,
        repo_owner,
        issue_number,
        actor_login,
        text,
    })
}

fn decide(event_type: &str, context: Option<&EventContext>) -> DecisionRecord {
    let Some(context) = context else {
        return DecisionRecord {
            decision: "ignored".to_string(),
            reason_code: "missing_context".to_string(),
            directive: None,
            target_role: None,
            would_dispatch: false,
        };
    };

    if event_type == "issue_comment" && context.text.as_deref().is_some_and(is_orchd_echo_comment) {
        return DecisionRecord {
            decision: "ignored".to_string(),
            reason_code: "orchd_echo_comment".to_string(),
            directive: None,
            target_role: None,
            would_dispatch: false,
        };
    }

    if let Some(text) = context.text.as_deref()
        && let Some(parsed) = parse_directive(text)
    {
        return DecisionRecord {
            decision: "accepted".to_string(),
            reason_code: "explicit_directive".to_string(),
            directive: Some(parsed.directive),
            target_role: Some(parsed.role),
            would_dispatch: true,
        };
    }

    if event_type == "issue_comment"
        && context.actor_login.as_deref() == Some(context.repo_owner.as_str())
    {
        return DecisionRecord {
            decision: "accepted".to_string(),
            reason_code: "owner_default_poke".to_string(),
            directive: Some("poke".to_string()),
            target_role: Some("codex-orch".to_string()),
            would_dispatch: true,
        };
    }

    DecisionRecord {
        decision: "ignored".to_string(),
        reason_code: "no_directive".to_string(),
        directive: None,
        target_role: None,
        would_dispatch: false,
    }
}

fn parse_directive(text: &str) -> Option<ParsedDirective> {
    text.lines().find_map(parse_directive_line)
}

fn parse_directive_line(line: &str) -> Option<ParsedDirective> {
    let mut parts = line.split_whitespace();
    let role_token = parts.next()?;
    let directive_token = parts.next()?;
    if parts.next().is_some() {
        return None;
    }

    let role_token = role_token
        .trim_matches(|ch: char| [',', ';', ':'].contains(&ch))
        .trim_start_matches('@')
        .to_ascii_lowercase();
    let directive = directive_token
        .trim_matches(|ch: char| [',', ';', ':', '.'].contains(&ch))
        .to_ascii_lowercase();

    let role = if role_token == "codex" {
        "codex-dev".to_string()
    } else if role_token.starts_with("codex-") {
        role_token
    } else {
        return None;
    };

    if !matches!(directive.as_str(), "design" | "impl" | "poke") {
        return None;
    }

    Some(ParsedDirective { role, directive })
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
        token_file: role.token_file.clone(),
    })
}

fn is_orchd_echo_comment(text: &str) -> bool {
    text.trim_start().starts_with("orchd:")
}

fn load_dispatch_config(path: &Path) -> Result<DispatchConfig> {
    let raw_text = fs::read_to_string(path)
        .with_context(|| format!("failed to read dispatch config: {}", path.display()))?;
    let raw: DispatchConfigFile =
        toml::from_str(&raw_text).with_context(|| format!("invalid TOML: {}", path.display()))?;

    if raw.version != 1 {
        return Err(anyhow!(
            "unsupported dispatch config version {} in {}",
            raw.version,
            path.display()
        ));
    }
    if raw.allowed_actors.is_empty() {
        return Err(anyhow!(
            "dispatch config {} has empty allowed_actors",
            path.display()
        ));
    }

    let base_dir = path
        .parent()
        .map(PathBuf::from)
        .ok_or_else(|| anyhow!("dispatch config has no parent: {}", path.display()))?;

    let mut roles = HashMap::new();
    for (role_name, role) in raw.roles {
        let codex_role_arg = role.codex_role_arg.unwrap_or_else(|| {
            role_name
                .strip_prefix("codex-")
                .unwrap_or(role_name.as_str())
                .to_string()
        });
        roles.insert(
            role_name,
            DispatchRoleConfig {
                codex_bin: resolve_config_path(&base_dir, &role.codex_bin)?,
                codex_role_arg,
                token_file: resolve_config_path(&base_dir, &role.token_file)?,
                workdir: resolve_config_path(&base_dir, &role.workdir)?,
            },
        );
    }

    let mut directives = HashMap::new();
    for (directive_name, directive) in raw.directives {
        directives.insert(
            directive_name,
            DispatchDirectiveConfig {
                role: directive.role,
                prompt_file: resolve_config_path(&base_dir, &directive.prompt_file)?,
                timeout_sec: directive.timeout_sec.max(30),
            },
        );
    }

    Ok(DispatchConfig {
        allowed_actors: raw
            .allowed_actors
            .into_iter()
            .map(|actor| actor.to_ascii_lowercase())
            .collect(),
        tmux: DispatchTmuxConfig {
            session: raw.tmux.session,
            remain_on_exit: raw.tmux.remain_on_exit,
        },
        roles,
        directives,
        forgejoctl_bin: resolve_config_path(&base_dir, &raw.forgejoctl_bin)?,
    })
}

fn resolve_config_path(base_dir: &Path, raw: &str) -> Result<PathBuf> {
    let expanded = expand_tilde_path(raw)?;
    if expanded.is_absolute() {
        Ok(expanded)
    } else {
        Ok(base_dir.join(expanded))
    }
}

fn shell_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\"'\"'"))
}

fn render_prompt(template: &str, values: &[(&str, String)]) -> String {
    let mut text = template.to_string();
    for (key, value) in values {
        let token = format!("{{{{{key}}}}}");
        text = text.replace(&token, value);
    }
    text
}

fn tmux_repo_slug(repo_full_name: &str) -> String {
    let mut slug = String::with_capacity(repo_full_name.len());
    let mut last_dash = false;
    for ch in repo_full_name.chars() {
        if ch.is_ascii_alphanumeric() {
            slug.push(ch.to_ascii_lowercase());
            last_dash = false;
        } else if !last_dash {
            slug.push('-');
            last_dash = true;
        }
    }
    let trimmed = slug.trim_matches('-');
    let normalized = if trimmed.is_empty() { "repo" } else { trimmed };
    normalized.chars().take(24).collect()
}

fn issue_tmux_window_name(repo_full_name: &str, issue_number: u64) -> String {
    let repo_slug = tmux_repo_slug(repo_full_name);
    format!("r{repo_slug}-i{issue_number}")
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

fn acquire_repo_lock(db_path: &Path, repo_full_name: &str) -> Result<PathBuf, DispatchError> {
    let slug = repo_full_name.replace('/', "__");
    let lock_path = lock_root(db_path)?.join(format!("{slug}.lock"));
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&lock_path)
        .map_err(|err| {
            if err.kind() == std::io::ErrorKind::AlreadyExists {
                DispatchError::RepoLocked(lock_path.clone())
            } else {
                DispatchError::Io(format!(
                    "failed to create lock {}: {err}",
                    lock_path.display()
                ))
            }
        })?;
    writeln!(file, "repo={repo_full_name}")
        .and_then(|()| writeln!(file, "created_at={}", Utc::now().to_rfc3339()))
        .map_err(|err| DispatchError::Io(format!("failed writing lock metadata: {err}")))?;
    Ok(lock_path)
}

fn tmux_has_session(session: &str) -> Result<bool, DispatchError> {
    let status = Command::new("tmux")
        .args(["has-session", "-t", session])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map_err(|err| DispatchError::Tmux(format!("failed checking tmux session: {err}")))?;
    Ok(status.success())
}

fn tmux_set_remain_on_exit(session: &str, enabled: bool) -> Result<(), DispatchError> {
    let flag = if enabled { "on" } else { "off" };
    let status = Command::new("tmux")
        .args(["set-option", "-t", session, "remain-on-exit", flag])
        .status()
        .map_err(|err| DispatchError::Tmux(format!("failed setting remain-on-exit: {err}")))?;
    if !status.success() {
        return Err(DispatchError::Tmux(format!(
            "tmux set-option failed for session {session}"
        )));
    }
    Ok(())
}

fn tmux_has_window(session: &str, window: &str) -> Result<bool, DispatchError> {
    let output = Command::new("tmux")
        .args(["list-windows", "-t", session, "-F", "#{window_name}"])
        .output()
        .map_err(|err| DispatchError::Tmux(format!("failed listing tmux windows: {err}")))?;
    if !output.status.success() {
        return Err(DispatchError::Tmux(format!(
            "tmux list-windows failed for session {session}"
        )));
    }
    let target = window.trim();
    let windows = String::from_utf8_lossy(&output.stdout);
    Ok(windows.lines().any(|name| name.trim() == target))
}

fn tmux_window_has_live_pane(session: &str, window: &str) -> Result<bool, DispatchError> {
    let target = format!("{session}:{window}");
    let output = Command::new("tmux")
        .args(["list-panes", "-t", &target, "-F", "#{pane_dead}"])
        .output()
        .map_err(|err| DispatchError::Tmux(format!("failed listing tmux panes: {err}")))?;
    if !output.status.success() {
        return Ok(false);
    }
    let pane_states = String::from_utf8_lossy(&output.stdout);
    Ok(pane_states.lines().any(|line| line.trim() == "0"))
}

fn tmux_spawn_or_respawn_window(
    session: &str,
    window: &str,
    script_path: &Path,
    remain_on_exit: bool,
) -> Result<(), DispatchError> {
    let cmd = format!("bash {}", shell_quote(&script_path.to_string_lossy()));
    if tmux_has_session(session)? {
        if tmux_has_window(session, window)? {
            let status = Command::new("tmux")
                .args([
                    "respawn-window",
                    "-k",
                    "-t",
                    &format!("{session}:{window}"),
                    &cmd,
                ])
                .status()
                .map_err(|err| {
                    DispatchError::Tmux(format!("failed respawning tmux window: {err}"))
                })?;
            if !status.success() {
                return Err(DispatchError::Tmux(format!(
                    "tmux respawn-window failed for {session}:{window}"
                )));
            }
        } else {
            let status = Command::new("tmux")
                .args([
                    "new-window",
                    "-t",
                    &format!("{session}:"),
                    "-n",
                    window,
                    &cmd,
                ])
                .status()
                .map_err(|err| {
                    DispatchError::Tmux(format!("failed creating tmux window: {err}"))
                })?;
            if !status.success() {
                return Err(DispatchError::Tmux(format!(
                    "tmux new-window failed for {session}:{window}"
                )));
            }
        }
    } else {
        let status = Command::new("tmux")
            .args(["new-session", "-d", "-s", session, "-n", window, &cmd])
            .status()
            .map_err(|err| DispatchError::Tmux(format!("failed creating tmux session: {err}")))?;
        if !status.success() {
            return Err(DispatchError::Tmux(format!(
                "tmux new-session failed for {session}:{window}"
            )));
        }
    }
    tmux_set_remain_on_exit(session, remain_on_exit)?;
    Ok(())
}

fn build_tmux_exec_run_script(inputs: &TmuxRunScriptInputs<'_>) -> String {
    format!(
        r#"#!/usr/bin/env bash
set -euo pipefail

DISPATCH_ID={dispatch_id}
DB_PATH={db_path}
LOCK_PATH={lock_path}
RUN_DIR={run_dir}
PROMPT_FILE={prompt_file}
SUMMARY_FILE={summary_file}
COMPLETION_FILE={completion_file}
LAST_MESSAGE_FILE={last_message_file}
CODEX_LOG_FILE={codex_log_file}
MARKER_FILE={marker_file}
ISSUE_REF={issue_ref}
FORGEJOCTL_BIN={forgejoctl_bin}
TOKEN_FILE={token_file}
WORKDIR={workdir}
CODEX_BIN={codex_bin}
CODEX_ROLE_ARG={codex_role_arg}
ISSUE_SESSION_ID={issue_session_id}
DIRECTIVE={directive}
ROLE_NAME={role_name}
TMUX_LOCATOR={tmux_locator}
TIMEOUT_SEC={timeout_sec}

cleanup() {{
  rm -f "$LOCK_PATH"
}}
trap cleanup EXIT

touch "$MARKER_FILE"
cd "$WORKDIR"
: > "$CODEX_LOG_FILE"

run_codex_fresh() {{
  cat "$PROMPT_FILE" \
    | timeout --preserve-status "$TIMEOUT_SEC" "$CODEX_BIN" "$CODEX_ROLE_ARG" --no-alt-screen exec --skip-git-repo-check -o "$LAST_MESSAGE_FILE" - \
      2>&1 | tee -a "$CODEX_LOG_FILE"
}}

set +e
if [[ -n "$ISSUE_SESSION_ID" ]]; then
  cat "$PROMPT_FILE" \
    | timeout --preserve-status "$TIMEOUT_SEC" "$CODEX_BIN" "$CODEX_ROLE_ARG" --no-alt-screen exec -o "$LAST_MESSAGE_FILE" resume --skip-git-repo-check "$ISSUE_SESSION_ID" - \
      2>&1 | tee -a "$CODEX_LOG_FILE"
  exit_code=$?
  if [[ "$exit_code" -ne 0 && "$exit_code" -ne 124 ]]; then
    echo "orchd: resume failed for issue session $ISSUE_SESSION_ID, falling back to fresh exec" | tee -a "$CODEX_LOG_FILE"
    run_codex_fresh
    exit_code=$?
  fi
else
  run_codex_fresh
  exit_code=$?
fi
set -e

session_id="$(sed -n 's/^session id: //p' "$CODEX_LOG_FILE" | tail -n 1)"
if [[ -z "$session_id" ]]; then
  session_id="$(find "$HOME/.codex/sessions" -type f -name '*.jsonl' -newer "$MARKER_FILE" 2>/dev/null | sort | tail -n 1 | sed -n 's#.*-\([0-9a-fA-F-]\{{36\}}\)\.jsonl#\1#p')"
fi

if [[ -s "$LAST_MESSAGE_FILE" ]]; then
  head -n 120 "$LAST_MESSAGE_FILE" > "$SUMMARY_FILE"
else
  echo "(no final assistant message)" > "$SUMMARY_FILE"
fi

if [[ "$exit_code" -eq 0 ]]; then
  status="completed"
  reason_code="completed"
elif [[ "$exit_code" -eq 124 ]]; then
  status="timed_out"
  reason_code="timeout"
else
  status="failed_runtime"
  reason_code="codex_exit_nonzero"
fi

if [[ -n "$session_id" ]]; then
  session_sql="'${{session_id//\'/\'\'}}'"
else
  session_sql="NULL"
fi
ended_at="$(date -u +%Y-%m-%dT%H:%M:%SZ)"
sqlite3 "$DB_PATH" "UPDATE dispatches SET status='$status', reason_code='$reason_code', codex_session_id=$session_sql, exit_code=$exit_code, ended_at='$ended_at' WHERE id=$DISPATCH_ID;"

{{
  echo "orchd: dispatch completed id=$DISPATCH_ID status=$status reason=$reason_code"
  echo "directive=$DIRECTIVE role=$ROLE_NAME"
  echo "tmux=$TMUX_LOCATOR"
  echo "codex_session_id=${{session_id:-unknown}}"
  echo "run_dir=$RUN_DIR"
  echo "log=$CODEX_LOG_FILE"
  echo
  echo '```markdown'
  cat "$SUMMARY_FILE"
  echo '```'
}} > "$COMPLETION_FILE"

"$FORGEJOCTL_BIN" --token-file "$TOKEN_FILE" issue comment "$ISSUE_REF" --body-file "$COMPLETION_FILE" || true
"#,
        dispatch_id = inputs.dispatch_id,
        db_path = shell_quote(&inputs.db_path.to_string_lossy()),
        lock_path = shell_quote(&inputs.lock_path.to_string_lossy()),
        run_dir = shell_quote(&inputs.run_dir.to_string_lossy()),
        prompt_file = shell_quote(&inputs.prompt_path.to_string_lossy()),
        summary_file = shell_quote(&inputs.summary_path.to_string_lossy()),
        completion_file = shell_quote(&inputs.completion_path.to_string_lossy()),
        last_message_file = shell_quote(&inputs.last_message_path.to_string_lossy()),
        codex_log_file = shell_quote(&inputs.codex_log_path.to_string_lossy()),
        marker_file = shell_quote(&inputs.marker_path.to_string_lossy()),
        issue_ref = shell_quote(inputs.issue_ref_text),
        forgejoctl_bin = shell_quote(&inputs.forgejoctl_bin.to_string_lossy()),
        token_file = shell_quote(&inputs.token_file.to_string_lossy()),
        workdir = shell_quote(&inputs.workdir.to_string_lossy()),
        codex_bin = shell_quote(&inputs.codex_bin.to_string_lossy()),
        codex_role_arg = shell_quote(inputs.codex_role_arg),
        issue_session_id = shell_quote(inputs.issue_session_id.unwrap_or("")),
        directive = shell_quote(inputs.directive_name),
        role_name = shell_quote(inputs.role_name),
        tmux_locator = shell_quote(inputs.tmux_locator),
        timeout_sec = inputs.timeout_sec,
    )
}

fn build_tmux_tui_run_script(
    inputs: &TmuxRunScriptInputs<'_>,
    bootstrap_prompt_path: &Path,
    session_jsonl_path: &Path,
) -> String {
    format!(
        r#"#!/usr/bin/env bash
set -euo pipefail

DISPATCH_ID={dispatch_id}
DB_PATH={db_path}
LOCK_PATH={lock_path}
RUN_DIR={run_dir}
PROMPT_FILE={prompt_file}
BOOTSTRAP_PROMPT_FILE={bootstrap_prompt_file}
SESSION_JSONL_FILE={session_jsonl_file}
SUMMARY_FILE={summary_file}
COMPLETION_FILE={completion_file}
LAST_MESSAGE_FILE={last_message_file}
CODEX_LOG_FILE={codex_log_file}
MARKER_FILE={marker_file}
ISSUE_REF={issue_ref}
FORGEJOCTL_BIN={forgejoctl_bin}
TOKEN_FILE={token_file}
WORKDIR={workdir}
CODEX_BIN={codex_bin}
CODEX_ROLE_ARG={codex_role_arg}
ISSUE_SESSION_ID={issue_session_id}
DIRECTIVE={directive}
ROLE_NAME={role_name}
TMUX_LOCATOR={tmux_locator}
TIMEOUT_SEC={timeout_sec}

cleanup() {{
  rm -f "$LOCK_PATH"
}}
trap cleanup EXIT

touch "$MARKER_FILE"
cd "$WORKDIR"
: > "$CODEX_LOG_FILE"

bootstrap_prompt="$(cat "$BOOTSTRAP_PROMPT_FILE")"

set +e
if [[ -n "$ISSUE_SESSION_ID" ]]; then
  timeout --preserve-status "$TIMEOUT_SEC" "$CODEX_BIN" "$CODEX_ROLE_ARG" resume "$ISSUE_SESSION_ID" "$bootstrap_prompt"
  exit_code=$?
else
  timeout --preserve-status "$TIMEOUT_SEC" "$CODEX_BIN" "$CODEX_ROLE_ARG" --cd "$WORKDIR" "$bootstrap_prompt"
  exit_code=$?
fi
set -e

session_id="$ISSUE_SESSION_ID"
if [[ -z "$session_id" ]]; then
  session_id="$(find "$HOME/.codex/sessions" -type f -name '*.jsonl' -newer "$MARKER_FILE" 2>/dev/null | sort | tail -n 1 | sed -n 's#.*-\([0-9a-fA-F-]\{{36\}}\)\.jsonl#\1#p')"
fi

session_jsonl=""
if [[ -n "$session_id" ]]; then
  session_jsonl="$(find "$HOME/.codex/sessions" -type f -name "*-${{session_id}}.jsonl" 2>/dev/null | sort | tail -n 1)"
fi
if [[ -z "$session_jsonl" ]]; then
  session_jsonl="$(find "$HOME/.codex/sessions" -type f -name '*.jsonl' -newer "$MARKER_FILE" 2>/dev/null | sort | tail -n 1)"
fi
printf '%s\n' "$session_jsonl" > "$SESSION_JSONL_FILE"

found_final_answer="0"
if [[ -n "$session_jsonl" && -r "$session_jsonl" ]]; then
  parse_result="$(python3 - "$session_jsonl" "$SUMMARY_FILE" <<'PY'
import json
import sys

session_path = sys.argv[1]
summary_path = sys.argv[2]
found = False
last_text = None

with open(session_path, "r", encoding="utf-8", errors="replace") as handle:
    for raw_line in handle:
        line = raw_line.strip()
        if not line:
            continue
        try:
            entry = json.loads(line)
        except Exception:
            continue
        if entry.get("type") != "response_item":
            continue
        payload = entry.get("payload") or {{}}
        if payload.get("type") != "message":
            continue
        if payload.get("role") != "assistant":
            continue
        if payload.get("phase") != "final_answer":
            continue
        found = True
        text_parts = []
        for item in payload.get("content") or []:
            if not isinstance(item, dict):
                continue
            text = item.get("text")
            if isinstance(text, str) and text.strip():
                text_parts.append(text.strip())
        if text_parts:
            last_text = "\n\n".join(text_parts)

if found and last_text is None:
    summary = "(final_answer detected, but output_text extraction was empty)"
elif found:
    summary = last_text
else:
    summary = "(no final_answer event found in session jsonl)"

lines = summary.splitlines()
with open(summary_path, "w", encoding="utf-8") as handle:
    if lines:
        handle.write("\n".join(lines[:120]) + "\n")
    else:
        handle.write("\n")

print("FOUND_FINAL_ANSWER=1" if found else "FOUND_FINAL_ANSWER=0")
PY
)"
  if [[ "$parse_result" == *"FOUND_FINAL_ANSWER=1"* ]]; then
    found_final_answer="1"
  fi
else
  echo "(session jsonl not found)" > "$SUMMARY_FILE"
fi

if [[ "$found_final_answer" == "1" ]]; then
  status="completed"
  reason_code="completed_final_answer"
elif [[ "$exit_code" -eq 124 ]]; then
  status="timed_out"
  reason_code="timeout"
elif [[ "$exit_code" -eq 0 ]]; then
  status="stopped_no_final_answer"
  reason_code="no_final_answer"
else
  status="failed_runtime"
  reason_code="codex_exit_nonzero"
fi

if [[ -n "$session_id" ]]; then
  session_sql="'${{session_id//\'/\'\'}}'"
else
  session_sql="NULL"
fi
ended_at="$(date -u +%Y-%m-%dT%H:%M:%SZ)"
sqlite3 "$DB_PATH" "UPDATE dispatches SET status='$status', reason_code='$reason_code', codex_session_id=$session_sql, exit_code=$exit_code, ended_at='$ended_at' WHERE id=$DISPATCH_ID;"

{{
  echo "orchd: dispatch completed id=$DISPATCH_ID status=$status reason=$reason_code"
  echo "directive=$DIRECTIVE role=$ROLE_NAME"
  echo "tmux=$TMUX_LOCATOR"
  echo "codex_session_id=${{session_id:-unknown}}"
  echo "session_jsonl=${{session_jsonl:-unknown}}"
  echo "run_dir=$RUN_DIR"
  echo "log=$CODEX_LOG_FILE"
  echo
  echo '```markdown'
  cat "$SUMMARY_FILE"
  echo '```'
}} > "$COMPLETION_FILE"

{{
  echo "mode=tmux-tui"
  echo "session_id=${{session_id:-unknown}}"
  echo "session_jsonl=${{session_jsonl:-unknown}}"
  echo "found_final_answer=$found_final_answer"
  echo "exit_code=$exit_code"
}} > "$CODEX_LOG_FILE"

"$FORGEJOCTL_BIN" --token-file "$TOKEN_FILE" issue comment "$ISSUE_REF" --body-file "$COMPLETION_FILE" || true
"#,
        dispatch_id = inputs.dispatch_id,
        db_path = shell_quote(&inputs.db_path.to_string_lossy()),
        lock_path = shell_quote(&inputs.lock_path.to_string_lossy()),
        run_dir = shell_quote(&inputs.run_dir.to_string_lossy()),
        prompt_file = shell_quote(&inputs.prompt_path.to_string_lossy()),
        bootstrap_prompt_file = shell_quote(&bootstrap_prompt_path.to_string_lossy()),
        session_jsonl_file = shell_quote(&session_jsonl_path.to_string_lossy()),
        summary_file = shell_quote(&inputs.summary_path.to_string_lossy()),
        completion_file = shell_quote(&inputs.completion_path.to_string_lossy()),
        last_message_file = shell_quote(&inputs.last_message_path.to_string_lossy()),
        codex_log_file = shell_quote(&inputs.codex_log_path.to_string_lossy()),
        marker_file = shell_quote(&inputs.marker_path.to_string_lossy()),
        issue_ref = shell_quote(inputs.issue_ref_text),
        forgejoctl_bin = shell_quote(&inputs.forgejoctl_bin.to_string_lossy()),
        token_file = shell_quote(&inputs.token_file.to_string_lossy()),
        workdir = shell_quote(&inputs.workdir.to_string_lossy()),
        codex_bin = shell_quote(&inputs.codex_bin.to_string_lossy()),
        codex_role_arg = shell_quote(inputs.codex_role_arg),
        issue_session_id = shell_quote(inputs.issue_session_id.unwrap_or("")),
        directive = shell_quote(inputs.directive_name),
        role_name = shell_quote(inputs.role_name),
        tmux_locator = shell_quote(inputs.tmux_locator),
        timeout_sec = inputs.timeout_sec,
    )
}

async fn dispatch_tmux(
    state: AppState,
    decision_id: i64,
    record: &EventRecord,
    decision: &DecisionRecord,
) -> Result<DispatchLaunch, DispatchError> {
    let dispatch_config = state
        .dispatch_config
        .as_ref()
        .ok_or(DispatchError::ConfigNotLoaded)?;
    let actor = record
        .actor_login
        .clone()
        .unwrap_or_default()
        .to_ascii_lowercase();
    if !dispatch_config
        .allowed_actors
        .iter()
        .any(|allowed| allowed == &actor)
    {
        return Err(DispatchError::ActorNotAllowed(actor));
    }

    let directive_name = decision
        .directive
        .as_deref()
        .ok_or_else(|| DispatchError::DirectiveNotConfigured("<none>".to_string()))?;
    let directive = dispatch_config
        .directives
        .get(directive_name)
        .ok_or_else(|| DispatchError::DirectiveNotConfigured(directive_name.to_string()))?;
    let role = dispatch_config
        .roles
        .get(&directive.role)
        .ok_or_else(|| DispatchError::RoleNotConfigured(directive.role.clone()))?;

    let issue_number = record
        .issue_number
        .ok_or_else(|| DispatchError::InvalidIssueRef(record.repo_full_name.clone()))?;
    if let Some(dispatch_id) = find_issue_inflight_dispatch_with_healing(
        &state.db_path,
        &record.repo_full_name,
        issue_number,
    )
    .map_err(|err| DispatchError::Db(err.to_string()))?
    {
        return Err(DispatchError::IssueDispatchInFlight {
            repo_full_name: record.repo_full_name.clone(),
            issue_number,
            dispatch_id,
        });
    }
    let issue_session_id =
        latest_issue_codex_session_id(&state.db_path, &record.repo_full_name, issue_number)
            .map_err(|err| DispatchError::Db(err.to_string()))?;

    let lock_path = acquire_repo_lock(&state.db_path, &record.repo_full_name)?;

    let repo = RepoRef::parse(&record.repo_full_name)
        .map_err(|_| DispatchError::InvalidIssueRef(record.repo_full_name.clone()))?;
    let issue_ref = IssueRef {
        repo,
        number: issue_number,
    };
    let issue = fetch_issue(state.clone(), issue_ref.clone()).await?;

    let now = Utc::now().to_rfc3339();
    let dispatch_id = insert_dispatch_starting(
        &state.db_path,
        &DispatchInsert {
            decision_id,
            repo_full_name: record.repo_full_name.clone(),
            issue_number,
            actor_login: record.actor_login.clone(),
            directive: directive_name.to_string(),
            target_role: directive.role.clone(),
            started_at: now,
        },
    )
    .map_err(|err| DispatchError::Db(err.to_string()))?;

    let tmux_window = issue_tmux_window_name(&record.repo_full_name, issue_number);
    let run_dir = run_root(&state.db_path)?.join(format!("dispatch-{dispatch_id}"));
    fs::create_dir_all(&run_dir)
        .map_err(|err| DispatchError::Io(format!("failed to create run dir: {err}")))?;

    let template = fs::read_to_string(&directive.prompt_file).map_err(|err| {
        DispatchError::Io(format!(
            "failed reading prompt {}: {err}",
            directive.prompt_file.display()
        ))
    })?;
    let issue_body = issue.body.unwrap_or_default();
    let prompt = render_prompt(
        &template,
        &[
            ("issue_ref", issue_ref.to_string()),
            ("repo", record.repo_full_name.clone()),
            ("issue_number", issue_number.to_string()),
            ("directive", directive_name.to_string()),
            ("target_role", directive.role.clone()),
            ("actor", actor),
            ("issue_title", issue.title),
            ("issue_body", issue_body),
            ("issue_url", issue.html_url),
            ("event_type", record.event_type.clone()),
            ("delivery_id", record.delivery_id.clone()),
        ],
    );

    let prompt_path = run_dir.join("prompt.md");
    fs::write(&prompt_path, prompt)
        .map_err(|err| DispatchError::Io(format!("failed writing prompt: {err}")))?;

    let script_path = run_dir.join("run.sh");
    let summary_path = run_dir.join("summary.md");
    let completion_path = run_dir.join("completion.md");
    let last_message_path = run_dir.join("last_message.md");
    let codex_log_path = run_dir.join("codex.log");
    let marker_path = run_dir.join("start.marker");
    let issue_ref_text = format!("{}#{}", record.repo_full_name, issue_number);
    let tmux_locator = format!("{}:{}", dispatch_config.tmux.session, tmux_window);

    let script_inputs = TmuxRunScriptInputs {
        dispatch_id,
        db_path: &state.db_path,
        lock_path: &lock_path,
        run_dir: &run_dir,
        prompt_path: &prompt_path,
        summary_path: &summary_path,
        completion_path: &completion_path,
        last_message_path: &last_message_path,
        codex_log_path: &codex_log_path,
        marker_path: &marker_path,
        issue_ref_text: &issue_ref_text,
        forgejoctl_bin: &dispatch_config.forgejoctl_bin,
        token_file: &role.token_file,
        workdir: &role.workdir,
        codex_bin: &role.codex_bin,
        codex_role_arg: &role.codex_role_arg,
        issue_session_id: issue_session_id.as_deref(),
        directive_name,
        role_name: &directive.role,
        tmux_locator: &tmux_locator,
        timeout_sec: directive.timeout_sec,
    };

    let script = match state.dispatch_mode {
        DispatchMode::TmuxExec => build_tmux_exec_run_script(&script_inputs),
        DispatchMode::TmuxTui => {
            let bootstrap_prompt_path = run_dir.join("bootstrap_prompt.md");
            let bootstrap_prompt = format!(
                "You are codex-orch running under orchd dispatch.\n\nBefore taking any action, read and follow the full task instructions in this file:\n{}\n\nTreat that file as canonical for this dispatch.",
                prompt_path.display()
            );
            fs::write(&bootstrap_prompt_path, bootstrap_prompt).map_err(|err| {
                DispatchError::Io(format!("failed writing bootstrap prompt: {err}"))
            })?;
            let session_jsonl_path = run_dir.join("session.jsonl.path");
            build_tmux_tui_run_script(&script_inputs, &bootstrap_prompt_path, &session_jsonl_path)
        }
        DispatchMode::DryRun => {
            return Err(DispatchError::ConfigNotLoaded);
        }
    };

    fs::write(&script_path, script)
        .map_err(|err| DispatchError::Io(format!("failed writing run script: {err}")))?;

    let spawn_result = tmux_spawn_or_respawn_window(
        &dispatch_config.tmux.session,
        &tmux_window,
        &script_path,
        dispatch_config.tmux.remain_on_exit,
    );
    if let Err(err) = spawn_result {
        let _ = update_dispatch_failed_start(
            &state.db_path,
            dispatch_id,
            err.reason_code(),
            &err.to_string(),
        );
        let _ = fs::remove_file(&lock_path);
        return Err(err);
    }

    update_dispatch_running(
        &state.db_path,
        dispatch_id,
        &dispatch_config.tmux.session,
        &tmux_window,
        &run_dir,
        &lock_path,
    )
    .map_err(|err| DispatchError::Db(err.to_string()))?;

    Ok(DispatchLaunch {
        dispatch_id,
        tmux_locator,
        run_dir,
    })
}

fn verify_signature(secret: Option<&[u8]>, headers: &HeaderMap, body: &[u8]) -> Result<()> {
    let Some(secret) = secret else {
        return Ok(());
    };

    let signature = extract_header(headers, &["x-forgejo-signature", "x-gitea-signature"])
        .ok_or_else(|| anyhow!("missing webhook signature header"))?;
    let signature = signature.trim_start_matches("sha256=");
    let provided = hex::decode(signature).context("signature is not valid hex")?;

    let mut mac = HmacSha256::new_from_slice(secret).context("invalid webhook secret")?;
    mac.update(body);
    mac.verify_slice(&provided)
        .map_err(|_| anyhow!("webhook signature verification failed"))?;
    Ok(())
}

fn extract_header(headers: &HeaderMap, names: &[&str]) -> Option<String> {
    names.iter().find_map(|name| {
        headers
            .get(*name)
            .and_then(|value| value.to_str().ok())
            .map(ToOwned::to_owned)
    })
}

fn synthetic_delivery_id(body: &[u8]) -> String {
    let hash = Sha256::digest(body);
    let hash_hex = hex::encode(hash);
    format!(
        "synthetic-{}-{}",
        Utc::now().timestamp_micros(),
        &hash_hex[..12]
    )
}

fn expand_tilde_path(path: &str) -> Result<PathBuf> {
    if path == "~" {
        return std::env::var("HOME")
            .map(PathBuf::from)
            .map_err(|_| anyhow!("HOME is not set"));
    }
    if let Some(rest) = path.strip_prefix("~/") {
        return std::env::var("HOME")
            .map(PathBuf::from)
            .map(|home| home.join(rest))
            .map_err(|_| anyhow!("HOME is not set"));
    }
    Ok(PathBuf::from(path))
}

fn load_secret(secret_file: Option<&str>) -> Result<Option<Vec<u8>>> {
    let Some(secret_file) = secret_file else {
        return Ok(None);
    };
    let path = expand_tilde_path(secret_file)?;
    let secret = fs::read_to_string(&path)
        .with_context(|| format!("failed to read webhook secret file: {}", path.display()))?;
    let secret = secret.trim().as_bytes().to_vec();
    if secret.is_empty() {
        return Err(anyhow!("webhook secret file is empty: {}", path.display()));
    }
    Ok(Some(secret))
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
        ",
    )?;
    Ok(())
}

fn open_db(path: &Path) -> Result<Connection> {
    let conn =
        Connection::open(path).with_context(|| format!("failed to open db: {}", path.display()))?;
    conn.busy_timeout(StdDuration::from_secs(5))?;
    conn.execute_batch("PRAGMA journal_mode=WAL; PRAGMA synchronous=NORMAL;")?;
    Ok(conn)
}

fn insert_event(db_path: &Path, event: &EventRecord) -> Result<Option<i64>> {
    let conn = open_db(db_path)?;
    let now = Utc::now().to_rfc3339();
    let inserted = conn.execute(
        r"
        INSERT INTO events (delivery_id, event_type, repo_full_name, issue_number, action, actor_login, raw_json, received_at)
        VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)
        ",
        params![
            event.delivery_id,
            event.event_type,
            event.repo_full_name,
            event.issue_number,
            event.action,
            event.actor_login,
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

fn insert_dispatch_starting(db_path: &Path, dispatch: &DispatchInsert) -> Result<i64> {
    let conn = open_db(db_path)?;
    conn.execute(
        r"
        INSERT INTO dispatches
        (decision_id, repo_full_name, issue_number, actor_login, directive, target_role, status, started_at, tmux_session)
        VALUES (?1, ?2, ?3, ?4, ?5, ?6, 'starting', ?7, NULL)
        ",
        params![
            dispatch.decision_id,
            dispatch.repo_full_name.as_str(),
            i64::try_from(dispatch.issue_number)?,
            dispatch.actor_login.as_deref(),
            dispatch.directive.as_str(),
            dispatch.target_role.as_str(),
            dispatch.started_at.as_str(),
        ],
    )?;
    Ok(conn.last_insert_rowid())
}

fn update_dispatch_running(
    db_path: &Path,
    dispatch_id: i64,
    tmux_session: &str,
    tmux_window: &str,
    run_dir: &Path,
    lock_path: &Path,
) -> Result<()> {
    let conn = open_db(db_path)?;
    conn.execute(
        r"
        UPDATE dispatches
        SET status = 'running',
            tmux_session = ?2,
            tmux_window = ?3,
            run_dir = ?4,
            lock_path = ?5
        WHERE id = ?1
        ",
        params![
            dispatch_id,
            tmux_session,
            tmux_window,
            run_dir.to_string_lossy(),
            lock_path.to_string_lossy(),
        ],
    )?;
    Ok(())
}

fn update_dispatch_failed_start(
    db_path: &Path,
    dispatch_id: i64,
    reason_code: &str,
    error_text: &str,
) -> Result<()> {
    let conn = open_db(db_path)?;
    let now = Utc::now().to_rfc3339();
    conn.execute(
        r"
        UPDATE dispatches
        SET status = 'failed_start',
            reason_code = ?2,
            error_text = ?3,
            ended_at = ?4
        WHERE id = ?1
        ",
        params![dispatch_id, reason_code, error_text, now],
    )?;
    Ok(())
}

fn latest_issue_inflight_dispatch(
    db_path: &Path,
    repo_full_name: &str,
    issue_number: u64,
) -> Result<Option<InflightDispatch>> {
    let conn = open_db(db_path)?;
    let dispatch = conn
        .query_row(
            r"
            SELECT id, status, started_at, tmux_session, tmux_window, lock_path
            FROM dispatches
            WHERE repo_full_name = ?1
              AND issue_number = ?2
              AND status IN ('starting', 'running')
            ORDER BY id DESC
            LIMIT 1
            ",
            params![repo_full_name, i64::try_from(issue_number)?],
            |row| {
                Ok(InflightDispatch {
                    id: row.get(0)?,
                    status: row.get(1)?,
                    started_at: row.get(2)?,
                    tmux_session: row.get(3)?,
                    tmux_window: row.get(4)?,
                    lock_path: row.get(5)?,
                })
            },
        )
        .optional()?;
    Ok(dispatch)
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
    let Some(session) = dispatch.tmux_session.as_deref() else {
        return true;
    };
    let window = dispatch
        .tmux_window
        .clone()
        .unwrap_or_else(|| issue_tmux_window_name(repo_full_name, issue_number));
    match tmux_window_has_live_pane(session, &window) {
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
    match dispatch.status.as_str() {
        "running" => {
            let Some(session) = dispatch.tmux_session.as_deref() else {
                return true;
            };
            let Some(window) = dispatch.tmux_window.as_deref() else {
                return true;
            };
            match tmux_window_has_live_pane(session, window) {
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
        "starting" => is_stale_starting_dispatch(dispatch, repo_full_name, issue_number),
        _ => false,
    }
}

fn mark_dispatch_failed_runtime(
    db_path: &Path,
    dispatch_id: i64,
    reason_code: &str,
    error_text: &str,
) -> Result<()> {
    let conn = open_db(db_path)?;
    let ended_at = Utc::now().to_rfc3339();
    conn.execute(
        r"
        UPDATE dispatches
        SET status = 'failed_runtime',
            reason_code = ?2,
            error_text = ?3,
            ended_at = ?4
        WHERE id = ?1
          AND status IN ('starting', 'running')
        ",
        params![dispatch_id, reason_code, error_text, ended_at],
    )?;
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

fn log_line(event: &str, payload: serde_json::Value) {
    let line = json!({
        "ts": Utc::now().to_rfc3339(),
        "event": event,
        "data": payload,
    });
    println!("{line}");
}

#[cfg(test)]
mod tests {
    use super::*;

    fn inflight_dispatch(
        status: &str,
        started_at: String,
        tmux_session: Option<&str>,
    ) -> InflightDispatch {
        InflightDispatch {
            id: 1,
            status: status.to_string(),
            started_at,
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
}
