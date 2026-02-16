use std::fs;
use std::path::Path;
use std::time::Duration;

use anyhow::{Context, Result, anyhow};
use chrono::Utc;
use rusqlite::{Connection, OptionalExtension, Transaction, TransactionBehavior, params};
use tracing::info;

use forgejo_agent::orchd_dispatch_core::{
    DispatchBackendKind, DispatchEventKind, DispatchNotificationPhase, DispatchState, RunHandle,
    reduce_dispatch_state,
};

use super::lexicon::{
    DIRECTIVE_IMPL, EVENT_ISSUE_COMMENT, EVENT_ISSUES, directive_serializes_repo,
};
use super::migrations;
use super::state::{DecisionRecord, EventRecord, IssueEventDeltaRow};

#[derive(Debug)]
pub(super) enum DispatchReservation {
    Started(i64),
    InFlightIssue(i64),
    InFlightRepo(i64),
}

#[derive(Debug)]
pub(super) struct InflightDispatch {
    pub(super) id: i64,
    pub(super) repo_full_name: String,
    pub(super) issue_number: u64,
    pub(super) status: DispatchState,
    pub(super) started_at: String,
    pub(super) backend_kind: Option<DispatchBackendKind>,
    pub(super) backend_ref: Option<String>,
    pub(super) lock_path: Option<String>,
}

#[derive(Debug, Clone)]
pub(super) struct ActiveIssueDispatch {
    pub(super) id: i64,
    pub(super) status: DispatchState,
}

#[derive(Debug, Clone)]
pub(super) struct IssueResumeDispatch {
    pub(super) id: i64,
    pub(super) status: DispatchState,
    pub(super) target_role: String,
    pub(super) codex_session_id: Option<String>,
}

#[derive(Debug, Clone)]
pub(super) struct QueuedDecision {
    pub(super) decision_id: i64,
    pub(super) event_id: i64,
    pub(super) record: EventRecord,
    pub(super) decision: DecisionRecord,
}

#[derive(Debug, Clone)]
pub(super) struct DispatchPhaseNotificationRow {
    pub(super) dedupe_key: String,
    pub(super) dispatch_id: i64,
    pub(super) repo_full_name: String,
    pub(super) issue_number: u64,
    pub(super) directive: String,
    pub(super) target_role: String,
    pub(super) status: DispatchState,
    pub(super) reason_code: Option<String>,
    pub(super) phase: DispatchNotificationPhase,
}

#[derive(Debug, Clone)]
pub(super) struct ReplyNotificationRow {
    pub(super) dedupe_key: String,
    pub(super) event_id: i64,
    pub(super) repo_full_name: String,
    pub(super) issue_number: u64,
    pub(super) actor_login: String,
    pub(super) event_text: String,
}

#[derive(Debug, Clone)]
pub(super) struct IssueTriggerGuardrailStats {
    pub(super) total: u64,
    pub(super) recent: u64,
    pub(super) last_created_at: Option<String>,
    pub(super) last_directive: Option<String>,
    pub(super) last_target_role: Option<String>,
}

pub(super) fn init_db(db_path: &Path) -> Result<()> {
    if let Some(parent) = db_path.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("failed to create db directory: {}", parent.display()))?;
    }
    let mut conn = open_db(db_path)?;
    migrations::apply_all(&mut conn)?;
    Ok(())
}

fn parse_dispatch_state_literal(raw: &str, column: usize) -> rusqlite::Result<DispatchState> {
    DispatchState::parse_db(raw).ok_or_else(|| {
        rusqlite::Error::FromSqlConversionFailure(
            column,
            rusqlite::types::Type::Text,
            Box::new(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                format!("invalid dispatch status in db: {raw}"),
            )),
        )
    })
}

fn parse_dispatch_notification_phase_literal(
    raw: &str,
    column: usize,
) -> rusqlite::Result<DispatchNotificationPhase> {
    DispatchNotificationPhase::parse_db(raw).ok_or_else(|| {
        rusqlite::Error::FromSqlConversionFailure(
            column,
            rusqlite::types::Type::Text,
            Box::new(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                format!("invalid dispatch notification phase in db: {raw}"),
            )),
        )
    })
}

pub(super) fn open_db(path: &Path) -> Result<Connection> {
    let conn =
        Connection::open(path).with_context(|| format!("failed to open db: {}", path.display()))?;
    conn.busy_timeout(Duration::from_secs(5))?;
    conn.execute_batch("PRAGMA journal_mode=WAL; PRAGMA synchronous=NORMAL;")?;
    Ok(conn)
}

pub(super) fn upsert_repo_seen(db_path: &Path, repo_full_name: &str) -> Result<()> {
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

pub(super) fn repo_labels_ensured_at(
    db_path: &Path,
    repo_full_name: &str,
) -> Result<Option<String>> {
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

pub(super) fn update_repo_labels_ensured(
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

pub(super) fn update_repo_local_path(
    db_path: &Path,
    repo_full_name: &str,
    local_path: &Path,
) -> Result<()> {
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

pub(super) fn insert_event(db_path: &Path, event: &EventRecord) -> Result<Option<i64>> {
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

pub(super) fn insert_decision(
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

pub(super) fn claim_trigger_dispatch_dedupe(
    db_path: &Path,
    event_id: i64,
    trigger_id: &str,
    dedupe_key: &str,
    repo_full_name: &str,
    issue_number: Option<u64>,
) -> Result<bool> {
    let conn = open_db(db_path)?;
    let inserted = conn.execute(
        r"
        INSERT INTO trigger_dispatch_dedupes
            (dedupe_key, trigger_id, event_id, repo_full_name, issue_number, created_at)
        VALUES
            (?1, ?2, ?3, ?4, ?5, ?6)
        ",
        params![
            dedupe_key,
            trigger_id,
            event_id,
            repo_full_name,
            issue_number.and_then(|value| i64::try_from(value).ok()),
            Utc::now().to_rfc3339(),
        ],
    );

    match inserted {
        Ok(_) => Ok(true),
        Err(err) => {
            let duplicate = matches!(
                err,
                rusqlite::Error::SqliteFailure(sqlite_err, _)
                    if sqlite_err.extended_code == 2067
            );
            if duplicate {
                Ok(false)
            } else {
                Err(err.into())
            }
        }
    }
}

pub(super) fn issue_trigger_guardrail_stats(
    db_path: &Path,
    repo_full_name: &str,
    issue_number: u64,
    since_created_at: &str,
) -> Result<IssueTriggerGuardrailStats> {
    let conn = open_db(db_path)?;
    let issue_number = i64::try_from(issue_number)?;
    let trigger_filter = r"
        would_dispatch = 1
        AND decision = 'accepted'
        AND (
            reason_code = 'assignee_reply'
            OR reason_code LIKE 'registered_trigger:%'
        )
    ";

    let total: u64 = conn.query_row(
        &format!(
            r"
            SELECT COUNT(*)
            FROM decisions
            WHERE repo_full_name = ?1
              AND issue_number = ?2
              AND {trigger_filter}
            "
        ),
        params![repo_full_name, issue_number],
        |row| row.get(0),
    )?;

    let recent: u64 = conn.query_row(
        &format!(
            r"
            SELECT COUNT(*)
            FROM decisions
            WHERE repo_full_name = ?1
              AND issue_number = ?2
              AND {trigger_filter}
              AND created_at >= ?3
            "
        ),
        params![repo_full_name, issue_number, since_created_at],
        |row| row.get(0),
    )?;

    let latest = conn
        .query_row(
            &format!(
                r"
                SELECT created_at, directive, target_role
                FROM decisions
                WHERE repo_full_name = ?1
                  AND issue_number = ?2
                  AND {trigger_filter}
                ORDER BY id DESC
                LIMIT 1
                "
            ),
            params![repo_full_name, issue_number],
            |row| {
                Ok((
                    row.get::<_, Option<String>>(0)?,
                    row.get::<_, Option<String>>(1)?,
                    row.get::<_, Option<String>>(2)?,
                ))
            },
        )
        .optional()?;

    let (last_created_at, last_directive, last_target_role) = latest.unwrap_or((None, None, None));

    Ok(IssueTriggerGuardrailStats {
        total,
        recent,
        last_created_at,
        last_directive,
        last_target_role,
    })
}

pub(super) fn update_decision_comment_status(
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

pub(super) fn issue_role_cursor_event_id(
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

pub(super) fn upsert_issue_role_cursor_event_id(
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

pub(super) fn issue_delta_rows(
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
          AND event_type IN (?5, ?6)
          AND event_text IS NOT NULL
          AND event_text != ''
        ORDER BY id ASC
        LIMIT 200
        ",
    )?;
    let rows = stmt
        .query_map(
            params![
                repo_full_name,
                issue_number,
                start_event_id,
                up_to_event_id,
                EVENT_ISSUE_COMMENT,
                EVENT_ISSUES
            ],
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

pub(super) fn summarize_issue_delta(rows: &[IssueEventDeltaRow]) -> String {
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

#[derive(Debug, Clone, Copy)]
pub(super) struct DispatchInsert<'a> {
    pub(super) decision_id: i64,
    pub(super) repo_full_name: &'a str,
    pub(super) issue_number: u64,
    pub(super) actor_login: Option<&'a str>,
    pub(super) directive: &'a str,
    pub(super) target_role: &'a str,
    pub(super) started_at: &'a str,
}

pub(super) fn reserve_dispatch_starting(
    db_path: &Path,
    insert: DispatchInsert<'_>,
) -> Result<DispatchReservation> {
    let mut conn = open_db(db_path)?;
    let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
    let issue_number = i64::try_from(insert.issue_number)?;
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
                insert.repo_full_name,
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

    if directive_serializes_repo(insert.directive) {
        let repo_inflight = tx
            .query_row(
                r"
                SELECT id
                FROM dispatches
                WHERE repo_full_name = ?1
                  AND directive = ?2
                  AND status IN (?3, ?4)
                ORDER BY id DESC
                LIMIT 1
                ",
                params![
                    insert.repo_full_name,
                    insert.directive,
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
        (decision_id, repo_full_name, issue_number, actor_login, directive, target_role, status, started_at)
        VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)
        ",
        params![
            insert.decision_id,
            insert.repo_full_name,
            issue_number,
            insert.actor_login,
            insert.directive,
            insert.target_role,
            DispatchState::Starting.as_db_str(),
            insert.started_at,
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

pub(super) fn update_dispatch_running(
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
    let rows = tx.execute(
        r"
        UPDATE dispatches
        SET status = ?2,
            backend_kind = ?3,
            backend_ref = ?4,
            run_dir = ?5,
            lock_path = ?6
        WHERE id = ?1
          AND status = ?7
        ",
        params![
            dispatch_id,
            plan.next_state.as_db_str(),
            run_handle.backend_kind.as_db_str(),
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

pub(super) fn update_dispatch_failed_start(
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

pub(super) fn update_dispatch_terminal(
    db_path: &Path,
    dispatch_id: i64,
    event_kind: DispatchEventKind,
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
    let next_state = reduce_dispatch_state(current_state, event_kind).map_err(|err| {
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
        event_kind,
        Some(&current_status),
        next_state.as_db_str(),
        Some(reason_code),
        None,
    )?;
    tx.commit()?;
    Ok(true)
}

pub(super) fn latest_issue_inflight_dispatch(
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
            SELECT id, repo_full_name, issue_number, status, started_at, backend_kind, backend_ref, lock_path
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
                let status_raw: String = row.get(3)?;
                let status = parse_dispatch_state_literal(&status_raw, 3)?;
                let backend_kind_raw: Option<String> = row.get(5)?;
                let backend_kind = backend_kind_raw
                    .as_deref()
                    .and_then(DispatchBackendKind::parse_db);
                let issue_number_raw: i64 = row.get(2)?;
                let issue_number = u64::try_from(issue_number_raw).map_err(|_| {
                    rusqlite::Error::FromSqlConversionFailure(
                        2,
                        rusqlite::types::Type::Integer,
                        Box::new(std::io::Error::new(
                            std::io::ErrorKind::InvalidData,
                            format!("invalid issue number in db: {issue_number_raw}"),
                        )),
                    )
                })?;
                Ok(InflightDispatch {
                    id: row.get(0)?,
                    repo_full_name: row.get(1)?,
                    issue_number,
                    status,
                    started_at: row.get(4)?,
                    backend_kind,
                    backend_ref: row.get(6)?,
                    lock_path: row.get(7)?,
                })
            },
        )
        .optional()?;
    Ok(dispatch)
}

pub(super) fn list_inflight_dispatches(db_path: &Path) -> Result<Vec<InflightDispatch>> {
    let conn = open_db(db_path)?;
    let starting_status = DispatchState::Starting.as_db_str();
    let running_status = DispatchState::Running.as_db_str();
    let mut stmt = conn.prepare(
        r"
        SELECT id, repo_full_name, issue_number, status, started_at, backend_kind, backend_ref, lock_path
        FROM dispatches
        WHERE status IN (?1, ?2)
        ORDER BY id ASC
        ",
    )?;
    let rows = stmt.query_map(params![starting_status, running_status], |row| {
        let status_raw: String = row.get(3)?;
        let status = parse_dispatch_state_literal(&status_raw, 3)?;
        let backend_kind_raw: Option<String> = row.get(5)?;
        let backend_kind = backend_kind_raw
            .as_deref()
            .and_then(DispatchBackendKind::parse_db);
        let issue_number_raw: i64 = row.get(2)?;
        let issue_number = u64::try_from(issue_number_raw).map_err(|_| {
            rusqlite::Error::FromSqlConversionFailure(
                2,
                rusqlite::types::Type::Integer,
                Box::new(std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    format!("invalid issue number in db: {issue_number_raw}"),
                )),
            )
        })?;
        Ok(InflightDispatch {
            id: row.get(0)?,
            repo_full_name: row.get(1)?,
            issue_number,
            status,
            started_at: row.get(4)?,
            backend_kind,
            backend_ref: row.get(6)?,
            lock_path: row.get(7)?,
        })
    })?;
    rows.collect::<std::result::Result<Vec<_>, _>>()
        .map_err(Into::into)
}

pub(super) fn latest_issue_active_dispatch(
    db_path: &Path,
    repo_full_name: &str,
    issue_number: u64,
) -> Result<Option<ActiveIssueDispatch>> {
    let conn = open_db(db_path)?;
    let dispatch = conn
        .query_row(
            r"
            SELECT id, status
            FROM dispatches
            WHERE repo_full_name = ?1
              AND issue_number = ?2
              AND status IN (?3, ?4, ?5, ?6)
            ORDER BY id DESC
            LIMIT 1
            ",
            params![
                repo_full_name,
                i64::try_from(issue_number)?,
                DispatchState::Queued.as_db_str(),
                DispatchState::Launching.as_db_str(),
                DispatchState::Starting.as_db_str(),
                DispatchState::Running.as_db_str(),
            ],
            |row| {
                let status_raw: String = row.get(1)?;
                let status = parse_dispatch_state_literal(&status_raw, 1)?;
                Ok(ActiveIssueDispatch {
                    id: row.get(0)?,
                    status,
                })
            },
        )
        .optional()?;
    Ok(dispatch)
}

pub(super) fn latest_issue_resume_dispatch(
    db_path: &Path,
    repo_full_name: &str,
    issue_number: u64,
) -> Result<Option<IssueResumeDispatch>> {
    let conn = open_db(db_path)?;
    let dispatch = conn
        .query_row(
            r"
            SELECT id, status, target_role, codex_session_id
            FROM dispatches
            WHERE repo_full_name = ?1
              AND issue_number = ?2
              AND codex_session_id IS NOT NULL
              AND codex_session_id != ''
            ORDER BY id DESC
            LIMIT 1
            ",
            params![repo_full_name, i64::try_from(issue_number)?],
            |row| {
                let status_raw: String = row.get(1)?;
                let status = parse_dispatch_state_literal(&status_raw, 1)?;
                Ok(IssueResumeDispatch {
                    id: row.get(0)?,
                    status,
                    target_role: row.get(2)?,
                    codex_session_id: row.get(3)?,
                })
            },
        )
        .optional()?;
    Ok(dispatch)
}

pub(super) fn latest_repo_inflight_impl_dispatch_id(
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
          AND directive = ?2
          AND status IN (?3, ?4)
        ORDER BY id DESC
        LIMIT 1
        ",
        params![
            repo_full_name,
            DIRECTIVE_IMPL,
            starting_status,
            running_status
        ],
        |row| row.get::<_, i64>(0),
    )
    .optional()
    .map_err(Into::into)
}

pub(super) fn mark_dispatch_failed_runtime(
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

pub(super) fn latest_issue_codex_session_id(
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

pub(super) fn queued_impl_decisions(db_path: &Path, limit: u32) -> Result<Vec<QueuedDecision>> {
    let conn = open_db(db_path)?;
    let mut stmt = conn.prepare(
        r"
        WITH latest AS (
            SELECT repo_full_name, issue_number, target_role, MAX(id) AS decision_id
            FROM decisions
            WHERE would_dispatch = 1
              AND directive = ?1
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
        LIMIT ?2
        ",
    )?;
    let rows = stmt
        .query_map(params![DIRECTIVE_IMPL, i64::from(limit)], |row| {
            let decision_id: i64 = row.get(0)?;
            let event_id: i64 = row.get(1)?;
            let record = EventRecord {
                delivery_id: row.get(2)?,
                event_type: row.get(3)?,
                repo_full_name: row.get(4)?,
                issue_number: row
                    .get::<_, Option<i64>>(5)?
                    .and_then(|n| u64::try_from(n).ok()),
                source_issue_id: None,
                source_issue_anchor_at: None,
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
                decision_source: "db".to_string(),
                trigger_id: None,
                trigger_dedupe_key: None,
                trigger_apply_guardrails: false,
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

pub(super) fn record_notification_delivery(
    db_path: &Path,
    dedupe_key: &str,
    category: &str,
) -> Result<bool> {
    let conn = open_db(db_path)?;
    let now = Utc::now().to_rfc3339();
    let rows = conn.execute(
        r"
        INSERT OR IGNORE INTO notification_deliveries (dedupe_key, category, sent_at)
        VALUES (?1, ?2, ?3)
        ",
        params![dedupe_key, category, now],
    )?;
    Ok(rows > 0)
}

pub(super) fn pending_dispatch_phase_notifications(
    db_path: &Path,
    enabled_phases: &[DispatchNotificationPhase],
    after_dispatch_id: i64,
    limit: u32,
) -> Result<Vec<DispatchPhaseNotificationRow>> {
    if enabled_phases.is_empty() || limit == 0 {
        return Ok(Vec::new());
    }
    let on_started = enabled_phases.contains(&DispatchNotificationPhase::Started);
    let on_completed = enabled_phases.contains(&DispatchNotificationPhase::Completed);
    let on_failed = enabled_phases.contains(&DispatchNotificationPhase::Failed);
    let on_blocked = enabled_phases.contains(&DispatchNotificationPhase::Blocked);

    let conn = open_db(db_path)?;
    let mut stmt = conn.prepare(
        r"
        SELECT
            d.id,
            d.repo_full_name,
            d.issue_number,
            d.directive,
            d.target_role,
            d.status,
            d.reason_code,
            CASE
                WHEN d.status = 'running' THEN 'started'
                WHEN d.status = 'completed' THEN 'completed'
                WHEN d.status = 'blocked' THEN 'blocked'
                ELSE 'failed'
            END AS phase
        FROM dispatches d
        WHERE (
            (?1 = 1 AND d.status = 'running')
            OR (?2 = 1 AND d.status = 'completed')
            OR (?3 = 1 AND d.status = 'blocked')
            OR (?4 = 1 AND d.status IN ('failed_start', 'failed_runtime', 'timed_out', 'canceled'))
        )
          AND d.id > ?5
          AND NOT EXISTS (
              SELECT 1
              FROM notification_deliveries n
              WHERE n.dedupe_key =
                  'dispatch:' || d.id || ':' ||
                  CASE
                      WHEN d.status = 'running' THEN 'started'
                      WHEN d.status = 'completed' THEN 'completed'
                      WHEN d.status = 'blocked' THEN 'blocked'
                      ELSE 'failed'
                  END
          )
        ORDER BY d.id ASC
        LIMIT ?6
        ",
    )?;

    let rows = stmt
        .query_map(
            params![
                i64::from(on_started),
                i64::from(on_completed),
                i64::from(on_blocked),
                i64::from(on_failed),
                after_dispatch_id,
                i64::from(limit)
            ],
            |row| {
                let dispatch_id: i64 = row.get(0)?;
                let repo_full_name: String = row.get(1)?;
                let issue_number_i64: i64 = row.get(2)?;
                let issue_number = u64::try_from(issue_number_i64).map_err(|_| {
                    rusqlite::Error::FromSqlConversionFailure(
                        2,
                        rusqlite::types::Type::Integer,
                        Box::new(std::io::Error::new(
                            std::io::ErrorKind::InvalidData,
                            format!("invalid issue number in dispatch row: {issue_number_i64}"),
                        )),
                    )
                })?;
                let status_raw: String = row.get(5)?;
                let phase_raw: String = row.get(7)?;
                let status = parse_dispatch_state_literal(&status_raw, 5)?;
                let phase = parse_dispatch_notification_phase_literal(&phase_raw, 7)?;
                Ok(DispatchPhaseNotificationRow {
                    dedupe_key: format!("dispatch:{dispatch_id}:{}", phase.as_db_str()),
                    dispatch_id,
                    repo_full_name,
                    issue_number,
                    directive: row.get(3)?,
                    target_role: row.get(4)?,
                    status,
                    reason_code: row.get(6)?,
                    phase,
                })
            },
        )?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(rows)
}

pub(super) fn pending_reply_notifications(
    db_path: &Path,
    watch_login: &str,
    after_event_id: i64,
    limit: u32,
) -> Result<Vec<ReplyNotificationRow>> {
    if watch_login.trim().is_empty() || limit == 0 {
        return Ok(Vec::new());
    }
    let conn = open_db(db_path)?;
    let watch_login = watch_login.to_ascii_lowercase();
    let mention_pattern = format!("%@{}%", watch_login);
    let mut stmt = conn.prepare(
        r"
        SELECT
            e.id,
            e.repo_full_name,
            e.issue_number,
            e.actor_login,
            e.event_text
        FROM events e
        WHERE e.event_type = ?1
          AND e.action = 'created'
          AND e.issue_number IS NOT NULL
          AND e.actor_login IS NOT NULL
          AND e.id > ?5
          AND lower(e.actor_login) GLOB 'codex-*'
          AND lower(e.actor_login) != ?2
          AND (
              EXISTS (
                  SELECT 1
                  FROM events io
                  WHERE io.repo_full_name = e.repo_full_name
                    AND io.issue_number = e.issue_number
                    AND io.event_type = ?3
                    AND io.action = 'opened'
                    AND lower(COALESCE(io.actor_login, '')) = ?2
              )
              OR lower(COALESCE(e.event_text, '')) LIKE ?4
              OR EXISTS (
                  SELECT 1
                  FROM events im
                  WHERE im.repo_full_name = e.repo_full_name
                    AND im.issue_number = e.issue_number
                    AND im.event_type = ?3
                    AND lower(COALESCE(im.event_text, '')) LIKE ?4
              )
          )
          AND NOT EXISTS (
              SELECT 1
              FROM notification_deliveries n
              WHERE n.dedupe_key = 'reply:' || e.id
          )
        ORDER BY e.id ASC
        LIMIT ?6
        ",
    )?;
    let rows = stmt
        .query_map(
            params![
                EVENT_ISSUE_COMMENT,
                watch_login,
                EVENT_ISSUES,
                mention_pattern,
                after_event_id,
                i64::from(limit),
            ],
            |row| {
                let event_id: i64 = row.get(0)?;
                let issue_number_i64: i64 = row.get(2)?;
                let issue_number = u64::try_from(issue_number_i64).map_err(|_| {
                    rusqlite::Error::FromSqlConversionFailure(
                        2,
                        rusqlite::types::Type::Integer,
                        Box::new(std::io::Error::new(
                            std::io::ErrorKind::InvalidData,
                            format!("invalid issue number in event row: {issue_number_i64}"),
                        )),
                    )
                })?;
                Ok(ReplyNotificationRow {
                    dedupe_key: format!("reply:{event_id}"),
                    event_id,
                    repo_full_name: row.get(1)?,
                    issue_number,
                    actor_login: row.get(3)?,
                    event_text: row.get::<_, Option<String>>(4)?.unwrap_or_default(),
                })
            },
        )?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(rows)
}

pub(super) fn latest_dispatch_id(db_path: &Path) -> Result<i64> {
    let conn = open_db(db_path)?;
    let latest = conn
        .query_row("SELECT MAX(id) FROM dispatches", [], |row| {
            row.get::<_, Option<i64>>(0)
        })
        .optional()?
        .flatten()
        .unwrap_or(0);
    Ok(latest)
}

pub(super) fn latest_event_id(db_path: &Path) -> Result<i64> {
    let conn = open_db(db_path)?;
    let latest = conn
        .query_row("SELECT MAX(id) FROM events", [], |row| {
            row.get::<_, Option<i64>>(0)
        })
        .optional()?
        .flatten()
        .unwrap_or(0);
    Ok(latest)
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::path::{Path, PathBuf};

    use chrono::Utc;
    use forgejo_agent::orchd_dispatch_core::DispatchState;
    use rusqlite::params;

    use crate::orchd::lexicon::{
        DECISION_ACCEPTED, DIRECTIVE_IMPL, DIRECTIVE_POKE, EVENT_ISSUE_COMMENT,
    };

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

    fn reserve_sample_dispatch(
        db_path: &Path,
        decision_id: i64,
        issue_number: u64,
        directive: &str,
    ) -> super::DispatchReservation {
        let started_at = Utc::now().to_rfc3339();
        super::reserve_dispatch_starting(
            db_path,
            super::DispatchInsert {
                decision_id,
                repo_full_name: "main/orchd-debug",
                issue_number,
                actor_login: Some("main"),
                directive,
                target_role: "codex-orch",
                started_at: &started_at,
            },
        )
        .expect("reserve dispatch")
    }

    fn insert_dispatch_row(
        db_path: &Path,
        decision_id: i64,
        issue_number: u64,
        status: DispatchState,
        target_role: &str,
        codex_session_id: Option<&str>,
    ) -> i64 {
        let conn = super::open_db(db_path).expect("open db");
        conn.execute(
            r"
            INSERT INTO dispatches
            (decision_id, repo_full_name, issue_number, actor_login, directive, target_role, status, codex_session_id, started_at, ended_at)
            VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)
            ",
            params![
                decision_id,
                "main/orchd-debug",
                i64::try_from(issue_number).expect("issue number fits i64"),
                "main",
                DIRECTIVE_POKE,
                target_role,
                status.as_db_str(),
                codex_session_id,
                Utc::now().to_rfc3339(),
                Utc::now().to_rfc3339(),
            ],
        )
        .expect("insert dispatch");
        conn.last_insert_rowid()
    }

    fn seed_decision_id(db_path: &Path) -> i64 {
        let conn = super::open_db(db_path).expect("open db");
        let now = Utc::now().to_rfc3339();
        conn.execute(
            r"
            INSERT INTO events (delivery_id, event_type, repo_full_name, issue_number, action, actor_login, raw_json, received_at)
            VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)
            ",
            params![
                format!("test-delivery-{}", Utc::now().timestamp_nanos_opt().unwrap_or(0)),
                EVENT_ISSUE_COMMENT,
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
                DIRECTIVE_POKE,
                "codex-orch",
                DECISION_ACCEPTED,
                "explicit_directive",
                1_i64,
                Utc::now().to_rfc3339(),
            ],
        )
        .expect("insert decision");
        conn.last_insert_rowid()
    }

    fn dispatch_event_kinds(db_path: &Path, dispatch_id: i64) -> Vec<String> {
        let conn = super::open_db(db_path).expect("open db for event scan");
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
        super::init_db(&db_path).expect("db init");
        let first_decision_id = seed_decision_id(&db_path);
        let second_decision_id = seed_decision_id(&db_path);

        let first = reserve_sample_dispatch(&db_path, first_decision_id, 7, DIRECTIVE_POKE);
        let first_id = match first {
            super::DispatchReservation::Started(id) => id,
            super::DispatchReservation::InFlightIssue(_)
            | super::DispatchReservation::InFlightRepo(_) => {
                panic!("expected first reservation to start")
            }
        };
        assert_eq!(
            dispatch_event_kinds(&db_path, first_id),
            vec!["mark_starting".to_string()]
        );

        let second = reserve_sample_dispatch(&db_path, second_decision_id, 7, DIRECTIVE_POKE);
        match second {
            super::DispatchReservation::InFlightIssue(id) => assert_eq!(id, first_id),
            super::DispatchReservation::Started(_) => {
                panic!("expected second reservation to be blocked")
            }
            super::DispatchReservation::InFlightRepo(_) => {
                panic!("expected issue-level inflight, not repo-level inflight")
            }
        }

        let _ = fs::remove_file(db_path);
    }

    #[test]
    fn reserve_dispatch_blocks_second_inflight_impl_for_repo() {
        let db_path = temp_db_path("dispatch-reserve-repo");
        super::init_db(&db_path).expect("db init");
        let first_decision_id = seed_decision_id(&db_path);
        let second_decision_id = seed_decision_id(&db_path);

        let first = reserve_sample_dispatch(&db_path, first_decision_id, 7, DIRECTIVE_IMPL);
        let first_id = match first {
            super::DispatchReservation::Started(id) => id,
            super::DispatchReservation::InFlightIssue(_)
            | super::DispatchReservation::InFlightRepo(_) => {
                panic!("expected first reservation to start")
            }
        };

        let second = reserve_sample_dispatch(&db_path, second_decision_id, 8, DIRECTIVE_IMPL);
        match second {
            super::DispatchReservation::InFlightRepo(id) => assert_eq!(id, first_id),
            super::DispatchReservation::Started(_) => {
                panic!("expected repo-level inflight block")
            }
            super::DispatchReservation::InFlightIssue(_) => {
                panic!("expected repo-level inflight, not issue-level inflight")
            }
        }

        let _ = fs::remove_file(db_path);
    }

    #[test]
    fn stale_autoheal_records_heal_event() {
        let db_path = temp_db_path("dispatch-autoheal");
        super::init_db(&db_path).expect("db init");
        let decision_id = seed_decision_id(&db_path);
        let started_id = match reserve_sample_dispatch(&db_path, decision_id, 9, DIRECTIVE_POKE) {
            super::DispatchReservation::Started(id) => id,
            super::DispatchReservation::InFlightIssue(_)
            | super::DispatchReservation::InFlightRepo(_) => {
                panic!("expected started dispatch")
            }
        };
        super::mark_dispatch_failed_runtime(
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

        let conn = super::open_db(&db_path).expect("open db");
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
    fn latest_issue_active_dispatch_finds_non_terminal_row() {
        let db_path = temp_db_path("issue-active");
        super::init_db(&db_path).expect("db init");
        let decision_id = seed_decision_id(&db_path);
        let _completed = insert_dispatch_row(
            &db_path,
            decision_id,
            12,
            DispatchState::Completed,
            "codex-orch",
            Some("019c63c6-d558-7a63-b126-0441644aa84c"),
        );
        let active_id = insert_dispatch_row(
            &db_path,
            decision_id,
            12,
            DispatchState::Running,
            "codex-orch",
            Some("019c63c6-d558-7a63-b126-0441644aa84c"),
        );

        let active = super::latest_issue_active_dispatch(&db_path, "main/orchd-debug", 12)
            .expect("query active dispatch")
            .expect("active dispatch present");
        assert_eq!(active.id, active_id);
        assert_eq!(active.status, DispatchState::Running);

        let _ = fs::remove_file(db_path);
    }

    #[test]
    fn latest_issue_resume_dispatch_returns_latest_session_row() {
        let db_path = temp_db_path("issue-resume-row");
        super::init_db(&db_path).expect("db init");
        let decision_id = seed_decision_id(&db_path);
        let first_id = insert_dispatch_row(
            &db_path,
            decision_id,
            15,
            DispatchState::Completed,
            "codex-orch",
            Some("session-old"),
        );
        let second_id = insert_dispatch_row(
            &db_path,
            decision_id,
            15,
            DispatchState::FailedRuntime,
            "codex-orch",
            None,
        );

        let latest = super::latest_issue_resume_dispatch(&db_path, "main/orchd-debug", 15)
            .expect("query latest dispatch")
            .expect("latest dispatch present");
        assert_eq!(second_id, first_id + 1);
        assert_eq!(latest.id, first_id);
        assert_eq!(latest.status, DispatchState::Completed);
        assert_eq!(latest.target_role, "codex-orch");
        assert_eq!(latest.codex_session_id.as_deref(), Some("session-old"));

        let _ = fs::remove_file(db_path);
    }

    #[test]
    fn trigger_dedupe_claim_is_idempotent() {
        let db_path = temp_db_path("trigger-dedupe");
        super::init_db(&db_path).expect("db init");
        let conn = super::open_db(&db_path).expect("open db");
        let now = Utc::now().to_rfc3339();
        conn.execute(
            r"
            INSERT INTO events (delivery_id, event_type, repo_full_name, issue_number, action, actor_login, raw_json, received_at)
            VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)
            ",
            params![
                "dedupe-delivery-1",
                EVENT_ISSUE_COMMENT,
                "main/orchd-debug",
                26_i64,
                "created",
                "main",
                "{}",
                now
            ],
        )
        .expect("insert event");
        let event_id = conn.last_insert_rowid();

        let first = super::claim_trigger_dispatch_dedupe(
            &db_path,
            event_id,
            "legacy.assignee.reply",
            "dedupe-key-1",
            "main/orchd-debug",
            Some(26),
        )
        .expect("first dedupe claim");
        let second = super::claim_trigger_dispatch_dedupe(
            &db_path,
            event_id,
            "legacy.assignee.reply",
            "dedupe-key-1",
            "main/orchd-debug",
            Some(26),
        )
        .expect("second dedupe claim");
        assert!(first);
        assert!(!second);

        let _ = fs::remove_file(db_path);
    }

    #[test]
    fn issue_trigger_guardrail_stats_track_triggered_decisions_only() {
        let db_path = temp_db_path("trigger-guardrail-stats");
        super::init_db(&db_path).expect("db init");
        let conn = super::open_db(&db_path).expect("open db");

        let insert_decision = |delivery: &str, reason: &str, directive: &str, role: &str| {
            let now = Utc::now().to_rfc3339();
            conn.execute(
                r"
                INSERT INTO events (delivery_id, event_type, repo_full_name, issue_number, action, actor_login, raw_json, received_at)
                VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)
                ",
                params![
                    delivery,
                    EVENT_ISSUE_COMMENT,
                    "main/orchd-debug",
                    41_i64,
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
                    41_i64,
                    "main",
                    directive,
                    role,
                    DECISION_ACCEPTED,
                    reason,
                    1_i64,
                    Utc::now().to_rfc3339(),
                ],
            )
            .expect("insert decision");
        };

        insert_decision("trigger-a", "assignee_reply", "reply", "codex-orch");
        insert_decision(
            "trigger-b",
            "registered_trigger:closed_debrief",
            "poke",
            "codex-orch",
        );
        insert_decision("explicit-c", "explicit_directive", "poke", "codex-orch");

        let stats = super::issue_trigger_guardrail_stats(
            &db_path,
            "main/orchd-debug",
            41,
            "1970-01-01T00:00:00+00:00",
        )
        .expect("guardrail stats");

        assert_eq!(stats.total, 2);
        assert_eq!(stats.recent, 2);
        assert_eq!(stats.last_directive.as_deref(), Some("poke"));
        assert_eq!(stats.last_target_role.as_deref(), Some("codex-orch"));

        let _ = fs::remove_file(db_path);
    }
}
