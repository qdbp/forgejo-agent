use std::time::Duration as StdDuration;

use anyhow::Result;
use notify_rust::Notification;
use serde_json::json;

use forgejo_agent::orchd_dispatch_core::DispatchNotificationPhase;

use super::db::{self, DispatchNotificationCandidate};
use super::dispatch_config::DispatchNotificationsConfig;
use super::state::AppState;
use super::telemetry::log_line;

const MAX_NOTIFICATIONS_PER_PASS: u32 = 32;

const fn phase_summary(phase: DispatchNotificationPhase) -> &'static str {
    match phase {
        DispatchNotificationPhase::Started => "dispatch started",
        DispatchNotificationPhase::Completed => "dispatch completed",
        DispatchNotificationPhase::Failed => "dispatch failed",
        DispatchNotificationPhase::Blocked => "dispatch blocked",
    }
}

fn build_notification_body(candidate: &DispatchNotificationCandidate) -> String {
    let mut lines = vec![
        format!(
            "issue: {}#{}",
            candidate.repo_full_name, candidate.issue_number
        ),
        format!("directive: {}", candidate.directive),
        format!("role: {}", candidate.target_role),
        format!("state: {}", candidate.status.as_db_str()),
    ];
    if let Some(reason_code) = candidate.reason_code.as_deref()
        && !reason_code.trim().is_empty()
    {
        lines.push(format!("reason: {reason_code}"));
    }
    lines.join("\n")
}

fn send_notification(
    config: &DispatchNotificationsConfig,
    candidate: &DispatchNotificationCandidate,
) -> Result<()> {
    Notification::new()
        .appname(&config.app_name)
        .summary(phase_summary(candidate.phase))
        .body(&build_notification_body(candidate))
        .show()?;
    Ok(())
}

fn notify_once(state: &AppState, config: &DispatchNotificationsConfig) -> Result<()> {
    let candidates = db::pending_dispatch_notifications(
        &state.db_path,
        config.phases.as_slice(),
        MAX_NOTIFICATIONS_PER_PASS,
    )?;
    for candidate in candidates {
        match send_notification(config, &candidate) {
            Ok(()) => match db::record_dispatch_notification(
                &state.db_path,
                candidate.dispatch_id,
                candidate.phase,
            ) {
                Ok(recorded) => {
                    if recorded {
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
                        "dispatch_notification_record_failed",
                        json!({
                            "dispatch_id": candidate.dispatch_id,
                            "phase": candidate.phase.as_db_str(),
                            "error": err.to_string(),
                        }),
                    );
                }
            },
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

pub(super) async fn run_notification_loop(state: AppState, config: DispatchNotificationsConfig) {
    let interval = StdDuration::from_secs(config.poll_sec.max(1));
    loop {
        if let Err(err) = notify_once(&state, &config) {
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
