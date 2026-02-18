use std::collections::HashSet;
use std::process::Command;
use std::time::Duration as StdDuration;

use anyhow::{Context, Result, anyhow};
use serde_json::json;
use sha2::{Digest, Sha256};

use forgejo_agent::orchd_dispatch_core::DispatchNotificationPhase;

use super::db::{self, DispatchPhaseNotificationRow, ReplyNotificationRow};
use super::dispatch_config::DispatchNotificationsConfig;
use super::state::AppState;
use super::telemetry::log_line;

const MAX_NOTIFICATIONS_PER_PASS: u32 = 32;

#[derive(Clone, Copy, Debug)]
struct NotificationBaseline {
    dispatch_id: i64,
    event_id: i64,
}

const fn phase_summary(phase: DispatchNotificationPhase) -> &'static str {
    match phase {
        DispatchNotificationPhase::Started => "dispatch started",
        DispatchNotificationPhase::Completed => "dispatch completed",
        DispatchNotificationPhase::Failed => "dispatch failed",
        DispatchNotificationPhase::Blocked => "dispatch blocked",
    }
}

fn pastel_bgcolor(thread: &str) -> String {
    let key = if thread.is_empty() { "global" } else { thread };
    let digest = Sha256::digest(key.as_bytes());
    let red = 0x80_u8.saturating_add(digest[0] / 2);
    let green = 0x80_u8.saturating_add(digest[1] / 2);
    let blue = 0x80_u8.saturating_add(digest[2] / 2);
    format!("#{red:02x}{green:02x}{blue:02x}")
}

fn replacement_id(thread: &str) -> String {
    let key = if thread.is_empty() { "global" } else { thread };
    let digest = Sha256::digest(key.as_bytes());
    let raw = u32::from_be_bytes([digest[4], digest[5], digest[6], digest[7]]) & 0x7fff_ffff;
    raw.to_string()
}

fn thread_key(repo_full_name: &str, issue_number: u64) -> String {
    format!("{repo_full_name}#{issue_number}")
}

fn truncate_ellipsis(input: &str, max_chars: usize) -> String {
    let mut out = String::new();
    for (index, ch) in input.chars().enumerate() {
        if index >= max_chars {
            break;
        }
        out.push(ch);
    }
    if input.chars().nth(max_chars).is_some() {
        out.push('…');
    }
    out
}

fn first_non_empty_line(text: &str) -> String {
    text.lines()
        .map(str::trim)
        .find(|line| !line.is_empty())
        .unwrap_or("commented")
        .to_string()
}

fn send_notification(
    config: &DispatchNotificationsConfig,
    summary: &str,
    body: &str,
    thread: &str,
) -> Result<()> {
    let bgcolor = pastel_bgcolor(thread);
    let replace_id = replacement_id(thread);
    let status = Command::new(&config.notify_send_bin)
        .arg("-a")
        .arg(&config.app_name)
        .arg("-r")
        .arg(&replace_id)
        .arg("-h")
        .arg(format!("string:bgcolor:{bgcolor}"))
        .arg(summary)
        .arg(body)
        .status()
        .with_context(|| {
            format!(
                "failed to spawn notify command {}",
                config.notify_send_bin.display()
            )
        })?;
    if status.success() {
        Ok(())
    } else {
        Err(anyhow!(
            "notify command exited with status {status} for thread {thread}"
        ))
    }
}

fn dispatch_summary(row: &DispatchPhaseNotificationRow) -> String {
    phase_summary(row.phase).to_string()
}

fn dispatch_body(row: &DispatchPhaseNotificationRow) -> String {
    let mut lines = vec![
        format!("issue: {}#{}", row.repo_full_name, row.issue_number),
        format!("directive: {}", row.directive),
        format!("role: {}", row.target_role),
        format!("state: {}", row.status.as_db_str()),
    ];
    if let Some(reason_code) = row.reason_code.as_deref()
        && !reason_code.trim().is_empty()
    {
        lines.push(format!("reason: {reason_code}"));
    }
    lines.join("\n")
}

const fn reply_summary() -> &'static str {
    "Codex"
}

fn reply_body(row: &ReplyNotificationRow) -> String {
    let headline = truncate_ellipsis(first_non_empty_line(&row.event_text).as_str(), 200);
    format!(
        "{}#{} — {}: {}",
        row.repo_full_name, row.issue_number, row.actor_login, headline
    )
}

fn notify_dispatch_phases(
    state: &AppState,
    config: &DispatchNotificationsConfig,
    baseline: NotificationBaseline,
    suppress_threads: &HashSet<String>,
) -> Result<()> {
    let candidates = db::pending_dispatch_phase_notifications(
        &state.db_path,
        config.phases.as_slice(),
        baseline.dispatch_id,
        MAX_NOTIFICATIONS_PER_PASS,
    )?;
    for candidate in candidates {
        let thread = thread_key(&candidate.repo_full_name, candidate.issue_number);
        if suppress_threads.contains(&thread) {
            let inserted = db::record_notification_delivery(
                &state.db_path,
                &candidate.dedupe_key,
                "dispatch_phase_suppressed",
            )?;
            if inserted {
                log_line(
                    "dispatch_notification_suppressed",
                    json!({
                        "dispatch_id": candidate.dispatch_id,
                        "phase": candidate.phase.as_db_str(),
                        "repo": candidate.repo_full_name,
                        "issue_number": candidate.issue_number,
                        "directive": candidate.directive,
                        "target_role": candidate.target_role,
                    }),
                );
            }
            continue;
        }
        let summary = dispatch_summary(&candidate);
        let body = dispatch_body(&candidate);
        match send_notification(config, &summary, &body, &thread) {
            Ok(()) => {
                let inserted = db::record_notification_delivery(
                    &state.db_path,
                    &candidate.dedupe_key,
                    "dispatch_phase",
                )?;
                if inserted {
                    log_line(
                        "dispatch_notification_sent",
                        json!({
                            "dispatch_id": candidate.dispatch_id,
                            "phase": candidate.phase.as_db_str(),
                            "repo": candidate.repo_full_name,
                            "issue_number": candidate.issue_number,
                            "directive": candidate.directive,
                            "target_role": candidate.target_role,
                        }),
                    );
                }
            }
            Err(err) => {
                log_line(
                    "dispatch_notification_failed",
                    json!({
                        "dispatch_id": candidate.dispatch_id,
                        "phase": candidate.phase.as_db_str(),
                        "repo": candidate.repo_full_name,
                        "issue_number": candidate.issue_number,
                        "error": err.to_string(),
                    }),
                );
            }
        }
    }
    Ok(())
}

fn notify_replies(
    state: &AppState,
    config: &DispatchNotificationsConfig,
    baseline: NotificationBaseline,
) -> Result<HashSet<String>> {
    let mut notified_threads = HashSet::new();
    let candidates = db::pending_reply_notifications(
        &state.db_path,
        &config.watch_login,
        baseline.event_id,
        MAX_NOTIFICATIONS_PER_PASS,
    )?;
    for candidate in candidates {
        let thread = thread_key(&candidate.repo_full_name, candidate.issue_number);
        let summary = reply_summary();
        let body = reply_body(&candidate);
        match send_notification(config, summary, &body, &thread) {
            Ok(()) => {
                notified_threads.insert(thread.clone());
                let inserted = db::record_notification_delivery(
                    &state.db_path,
                    &candidate.dedupe_key,
                    "reply",
                )?;
                if inserted {
                    log_line(
                        "reply_notification_sent",
                        json!({
                            "event_id": candidate.event_id,
                            "repo": candidate.repo_full_name,
                            "issue_number": candidate.issue_number,
                            "actor_login": candidate.actor_login,
                        }),
                    );
                }
            }
            Err(err) => {
                log_line(
                    "reply_notification_failed",
                    json!({
                        "event_id": candidate.event_id,
                        "repo": candidate.repo_full_name,
                        "issue_number": candidate.issue_number,
                        "actor_login": candidate.actor_login,
                        "error": err.to_string(),
                    }),
                );
            }
        }
    }
    Ok(notified_threads)
}

fn notify_once(
    state: &AppState,
    config: &DispatchNotificationsConfig,
    baseline: NotificationBaseline,
) -> Result<()> {
    // Prefer reply notifications over dispatch phase notifications to avoid double-notifying the
    // operator when a dispatch both updates state and posts a comment.
    let reply_threads = notify_replies(state, config, baseline)?;
    notify_dispatch_phases(state, config, baseline, &reply_threads)?;
    Ok(())
}

fn load_baseline(state: &AppState) -> NotificationBaseline {
    let dispatch_id = match db::latest_dispatch_id(&state.db_path) {
        Ok(value) => value,
        Err(err) => {
            log_line(
                "notification_baseline_dispatch_error",
                json!({
                    "error": err.to_string(),
                }),
            );
            0
        }
    };
    let event_id = match db::latest_event_id(&state.db_path) {
        Ok(value) => value,
        Err(err) => {
            log_line(
                "notification_baseline_event_error",
                json!({
                    "error": err.to_string(),
                }),
            );
            0
        }
    };
    NotificationBaseline {
        dispatch_id,
        event_id,
    }
}

pub(super) async fn run_notification_loop(state: AppState, config: DispatchNotificationsConfig) {
    let interval = StdDuration::from_secs(config.poll_sec.max(1));
    let baseline = load_baseline(&state);
    loop {
        if let Err(err) = notify_once(&state, &config, baseline) {
            log_line(
                "notification_loop_error",
                json!({
                    "error": err.to_string(),
                }),
            );
        }
        tokio::time::sleep(interval).await;
    }
}
