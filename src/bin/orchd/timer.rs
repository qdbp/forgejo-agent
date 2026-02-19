use std::collections::BTreeSet;

use anyhow::{Result, anyhow, bail};
use serde::Serialize;

use super::cli::{TimerResumeArgs, TimerSessionsArgs};
use super::db;
use super::issue::{normalized_role_filter, run_codex_resume};
use super::paths::expand_tilde_path;

fn normalize_timer_id(timer_id: &str) -> Result<String> {
    let normalized = timer_id.trim().to_ascii_lowercase();
    if normalized.is_empty() {
        bail!("timer id must be non-empty");
    }
    if normalized.chars().any(char::is_whitespace) {
        bail!("timer id must not contain whitespace");
    }
    Ok(normalized)
}

#[derive(Debug, Clone, Serialize)]
struct TimerSessionSummary {
    dispatch_id: i64,
    status: String,
    resumable: bool,
    target_role: String,
    codex_session_id: String,
}

fn timer_session_summaries(rows: &[db::ResumeDispatch]) -> Vec<TimerSessionSummary> {
    rows.iter()
        .map(|row| TimerSessionSummary {
            dispatch_id: row.id,
            status: row.status.as_db_str().to_string(),
            resumable: row.status.is_terminal(),
            target_role: row.target_role.clone(),
            codex_session_id: row.codex_session_id.clone().unwrap_or_default(),
        })
        .collect()
}

pub(super) fn timer_sessions_command(db_path_raw: &str, args: TimerSessionsArgs) -> Result<()> {
    let timer_id = normalize_timer_id(&args.timer_id)?;
    let db_path = expand_tilde_path(db_path_raw)?;
    let role_filter = normalized_role_filter(args.role.as_deref())?;
    let rows = db::list_timer_resume_dispatches(&db_path, &timer_id, role_filter.as_deref())?;
    let summaries = timer_session_summaries(&rows);

    if args.json {
        println!("{}", serde_json::to_string_pretty(&summaries)?);
        return Ok(());
    }

    println!(
        "{:<12} {:<16} {:<10} {:<18} {}",
        "dispatch_id", "status", "resumable", "role", "session_id"
    );
    for row in summaries {
        println!(
            "{:<12} {:<16} {:<10} {:<18} {}",
            row.dispatch_id, row.status, row.resumable, row.target_role, row.codex_session_id
        );
    }
    Ok(())
}

pub(super) fn timer_resume_command(db_path_raw: &str, args: TimerResumeArgs) -> Result<()> {
    let timer_id = normalize_timer_id(&args.timer_id)?;
    let db_path = expand_tilde_path(db_path_raw)?;
    if let Some(active) = db::latest_timer_active_dispatch(&db_path, &timer_id)? {
        bail!(
            "timer {timer_id} has in-flight dispatch {} ({})",
            active.id,
            active.status.as_db_str()
        );
    }
    let role_filter = normalized_role_filter(args.role.as_deref())?;
    let all_rows = db::list_timer_resume_dispatches(&db_path, &timer_id, role_filter.as_deref())?;
    if all_rows.is_empty() {
        bail!("timer {timer_id} has no associated codex_session_id");
    }

    let latest = if let Some(dispatch_id) = args.dispatch_id {
        all_rows
            .iter()
            .find(|row| row.id == dispatch_id)
            .cloned()
            .ok_or_else(|| {
                anyhow!(
                    "dispatch {} is not a resumable session for timer {}",
                    dispatch_id,
                    timer_id
                )
            })?
    } else {
        if role_filter.is_none() {
            let unique_roles = all_rows
                .iter()
                .map(|row| row.target_role.as_str())
                .collect::<BTreeSet<_>>();
            if unique_roles.len() > 1 {
                let roles = unique_roles.into_iter().collect::<Vec<_>>().join(", ");
                bail!(
                    "timer {} has sessions for multiple roles ({}); re-run with --role <role> or --dispatch-id <id> (hint: orchd obs timer sessions {})",
                    timer_id,
                    roles,
                    timer_id
                );
            }
        }
        all_rows
            .first()
            .cloned()
            .ok_or_else(|| anyhow!("timer {timer_id} has no associated codex_session_id"))?
    };

    if !latest.status.is_terminal() {
        bail!(
            "timer {timer_id} has non-terminal latest dispatch {} ({})",
            latest.id,
            latest.status.as_db_str()
        );
    }
    let session_id = latest
        .codex_session_id
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .ok_or_else(|| {
            anyhow!(
                "latest dispatch {} for timer {timer_id} has no codex_session_id",
                latest.id
            )
        })?
        .to_string();

    run_codex_resume(&latest.target_role, &session_id, &args.codex_resume_args)
}

#[cfg(test)]
mod tests {
    use std::path::{Path, PathBuf};

    use chrono::Utc;
    use forgejo_agent::orchd_dispatch_core::DispatchState;
    use rusqlite::params;

    use crate::orchd::lexicon::{DECISION_ACCEPTED, DIRECTIVE_REPLY, EVENT_ISSUES};

    use super::{TimerResumeArgs, timer_resume_command};
    use crate::orchd::db;

    fn temp_db_path(label: &str) -> PathBuf {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("clock drift")
            .as_nanos();
        std::env::temp_dir().join(format!(
            "forgejo-agent-timer-{label}-{}-{nanos}.sqlite",
            std::process::id()
        ))
    }

    fn seed_timer_dispatch(
        db_path: &Path,
        timer_id: &str,
        status: DispatchState,
        target_role: &str,
        codex_session_id: Option<&str>,
    ) {
        let conn = db::open_db(db_path).expect("open db");
        let now = Utc::now().to_rfc3339();
        conn.execute(
            r"
            INSERT INTO events (delivery_id, event_type, repo_full_name, issue_number, action, actor_login, raw_json, received_at)
            VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)
            ",
            params![
                format!("timer-event-{}", Utc::now().timestamp_nanos_opt().unwrap_or(0)),
                EVENT_ISSUES,
                "main/orchd-debug",
                11_i64,
                "opened",
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
            (event_id, repo_full_name, issue_number, actor_login, schedule_timer_id, directive, target_role, decision, reason_code, would_dispatch, created_at)
            VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)
            ",
            params![
                event_id,
                "main/orchd-debug",
                11_i64,
                "main",
                timer_id,
                DIRECTIVE_REPLY,
                target_role,
                DECISION_ACCEPTED,
                format!("scheduled:{timer_id}"),
                1_i64,
                Utc::now().to_rfc3339(),
            ],
        )
        .expect("insert decision");
        let decision_id = conn.last_insert_rowid();
        conn.execute(
            r"
            INSERT INTO dispatches
            (decision_id, repo_full_name, issue_number, actor_login, directive, target_role, status, codex_session_id, started_at, ended_at)
            VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)
            ",
            params![
                decision_id,
                "main/orchd-debug",
                11_i64,
                "main",
                DIRECTIVE_REPLY,
                target_role,
                status.as_db_str(),
                codex_session_id,
                Utc::now().to_rfc3339(),
                Utc::now().to_rfc3339(),
            ],
        )
        .expect("insert dispatch");
    }

    #[test]
    fn timer_resume_rejects_when_dispatch_in_flight() {
        let db_path = temp_db_path("lockout");
        db::init_db(&db_path).expect("db init");
        seed_timer_dispatch(
            &db_path,
            "doc-scrub",
            DispatchState::Running,
            "codex-orch",
            Some("session-running"),
        );
        let args = TimerResumeArgs {
            timer_id: "doc-scrub".to_string(),
            role: None,
            dispatch_id: None,
            codex_resume_args: Vec::new(),
        };
        let err = timer_resume_command(db_path.to_string_lossy().as_ref(), args)
            .expect_err("timer resume should lock out with active dispatch");
        let rendered = format!("{err:#}");
        assert!(rendered.contains("in-flight dispatch"));

        let _ = std::fs::remove_file(db_path);
    }

    #[test]
    fn timer_resume_rejects_ambiguous_roles_without_filter() {
        let db_path = temp_db_path("role-ambiguity");
        db::init_db(&db_path).expect("db init");
        seed_timer_dispatch(
            &db_path,
            "doc-scrub",
            DispatchState::Completed,
            "codex-orch",
            Some("session-orch"),
        );
        seed_timer_dispatch(
            &db_path,
            "doc-scrub",
            DispatchState::Completed,
            "codex-lead",
            Some("session-lead"),
        );
        let args = TimerResumeArgs {
            timer_id: "doc-scrub".to_string(),
            role: None,
            dispatch_id: None,
            codex_resume_args: Vec::new(),
        };
        let err = timer_resume_command(db_path.to_string_lossy().as_ref(), args)
            .expect_err("timer resume should reject ambiguous role selection");
        let rendered = format!("{err:#}");
        assert!(rendered.contains("multiple roles"));

        let _ = std::fs::remove_file(db_path);
    }

    #[test]
    fn timer_session_summary_marks_resumable_states() {
        let rows = vec![
            db::ResumeDispatch {
                id: 11,
                status: DispatchState::Completed,
                target_role: "codex-orch".to_string(),
                codex_session_id: Some("session-completed".to_string()),
            },
            db::ResumeDispatch {
                id: 12,
                status: DispatchState::Running,
                target_role: "codex-lead".to_string(),
                codex_session_id: Some("session-running".to_string()),
            },
        ];
        let summaries = super::timer_session_summaries(&rows);
        assert_eq!(summaries.len(), 2);
        assert!(summaries[0].resumable);
        assert!(!summaries[1].resumable);
    }
}
