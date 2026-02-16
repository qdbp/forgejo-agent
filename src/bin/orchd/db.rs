use std::fs;
use std::path::Path;
use std::time::Duration;

use anyhow::{Context, Result, anyhow};
use chrono::Utc;
use rusqlite::{Connection, OptionalExtension, Transaction, TransactionBehavior, params};
use tracing::info;

use forgejo_agent::orchd_dispatch_core::{
    DispatchBackendKind, DispatchEventKind, DispatchState, RunHandle, reduce_dispatch_state,
};

use super::lexicon::{
    DIRECTIVE_IMPL, EVENT_ISSUE_COMMENT, EVENT_ISSUES, directive_serializes_repo,
};
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
pub(super) struct QueuedDecision {
    pub(super) decision_id: i64,
    pub(super) event_id: i64,
    pub(super) record: EventRecord,
    pub(super) decision: DecisionRecord,
}

pub(super) fn init_db(db_path: &Path) -> Result<()> {
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
                let status: String = row.get(3)?;
                let status = DispatchState::parse_db(&status).ok_or_else(|| {
                    rusqlite::Error::FromSqlConversionFailure(
                        3,
                        rusqlite::types::Type::Text,
                        Box::new(std::io::Error::new(
                            std::io::ErrorKind::InvalidData,
                            format!("invalid dispatch status in db: {status}"),
                        )),
                    )
                })?;
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
        let status: String = row.get(3)?;
        let status = DispatchState::parse_db(&status).ok_or_else(|| {
            rusqlite::Error::FromSqlConversionFailure(
                3,
                rusqlite::types::Type::Text,
                Box::new(std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    format!("invalid dispatch status in db: {status}"),
                )),
            )
        })?;
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
}
