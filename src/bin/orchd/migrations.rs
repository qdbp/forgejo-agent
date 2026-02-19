use anyhow::{Context, Result, anyhow};
use chrono::Utc;
use rusqlite::{Connection, OptionalExtension, TransactionBehavior, params};

#[derive(Clone, Copy)]
struct Migration {
    version: i64,
    name: &'static str,
    apply: fn(&mut Connection) -> Result<()>,
}

pub(super) const LATEST_SCHEMA_VERSION: i64 = 13;
pub(super) const MIN_COMPAT_SCHEMA_VERSION: i64 = 1;

const MIGRATIONS: &[Migration] = &[
    Migration {
        version: 1,
        name: "bootstrap_core_tables",
        apply: migration_0001_bootstrap_core_tables,
    },
    Migration {
        version: 2,
        name: "add_late_columns",
        apply: migration_0002_add_late_columns,
    },
    Migration {
        version: 3,
        name: "drop_legacy_tmux_dispatch_columns",
        apply: migration_0003_drop_legacy_tmux_dispatch_columns,
    },
    Migration {
        version: 4,
        name: "add_notification_deliveries_table",
        apply: migration_0004_add_notification_deliveries_table,
    },
    Migration {
        version: 5,
        name: "ensure_notification_deliveries_table",
        apply: migration_0005_ensure_notification_deliveries_table,
    },
    Migration {
        version: 6,
        name: "ensure_trigger_dispatch_dedupe_table",
        apply: migration_0006_ensure_trigger_dispatch_dedupe_table,
    },
    Migration {
        version: 7,
        name: "canonicalize_poke_directive_to_reply",
        apply: migration_0007_canonicalize_poke_directive_to_reply,
    },
    Migration {
        version: 8,
        name: "add_dispatch_principal_logins",
        apply: migration_0008_add_dispatch_principal_logins,
    },
    Migration {
        version: 9,
        name: "add_timer_schedule_context_tables",
        apply: migration_0009_add_timer_schedule_context_tables,
    },
    Migration {
        version: 10,
        name: "add_prune_support_indexes",
        apply: migration_0010_add_prune_support_indexes,
    },
    Migration {
        version: 11,
        name: "ensure_timer_schedule_context_tables_compat",
        apply: migration_0011_ensure_timer_schedule_context_tables_compat,
    },
    Migration {
        version: 12,
        name: "add_deploy_jobs_table",
        apply: migration_0012_add_deploy_jobs_table,
    },
    Migration {
        version: 13,
        name: "ensure_deploy_v2_tables",
        apply: migration_0013_ensure_deploy_v2_tables,
    },
];

pub(super) fn apply_all(conn: &mut Connection) -> Result<()> {
    ensure_schema_migrations_table(conn)?;
    let mut applied = applied_versions(conn)?;
    for migration in MIGRATIONS {
        if applied.contains(&migration.version) {
            continue;
        }
        (migration.apply)(conn).with_context(|| {
            format!(
                "failed applying migration {} ({})",
                migration.version, migration.name
            )
        })?;
        record_migration(conn, migration.version, migration.name)?;
        applied.push(migration.version);
    }

    let current = current_schema_version(conn)?;
    if current != LATEST_SCHEMA_VERSION {
        return Err(anyhow!(
            "migration runner ended at version {current}, expected {LATEST_SCHEMA_VERSION}"
        ));
    }
    Ok(())
}

pub(super) const fn schema_contract() -> (i64, i64) {
    (LATEST_SCHEMA_VERSION, MIN_COMPAT_SCHEMA_VERSION)
}

fn ensure_schema_migrations_table(conn: &Connection) -> Result<()> {
    conn.execute_batch(
        r"
        CREATE TABLE IF NOT EXISTS schema_migrations (
            version INTEGER PRIMARY KEY,
            name TEXT NOT NULL,
            applied_at TEXT NOT NULL
        );
        ",
    )?;
    Ok(())
}

fn applied_versions(conn: &Connection) -> Result<Vec<i64>> {
    let mut stmt = conn.prepare("SELECT version FROM schema_migrations ORDER BY version ASC")?;
    let rows = stmt.query_map([], |row| row.get::<_, i64>(0))?;
    rows.collect::<rusqlite::Result<Vec<_>>>()
        .map_err(Into::into)
}

fn current_schema_version(conn: &Connection) -> Result<i64> {
    let version = conn
        .query_row("SELECT MAX(version) FROM schema_migrations", [], |row| {
            row.get::<_, Option<i64>>(0)
        })
        .optional()?
        .flatten()
        .unwrap_or(0);
    Ok(version)
}

fn record_migration(conn: &Connection, version: i64, name: &str) -> Result<()> {
    conn.execute(
        "INSERT INTO schema_migrations(version, name, applied_at) VALUES(?1, ?2, ?3)",
        params![version, name, Utc::now().to_rfc3339()],
    )?;
    Ok(())
}

fn migration_0001_bootstrap_core_tables(conn: &mut Connection) -> Result<()> {
    let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
    tx.execute_batch(
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
    tx.commit()?;
    Ok(())
}

fn migration_0002_add_late_columns(conn: &mut Connection) -> Result<()> {
    let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
    ensure_column_exists_tx(&tx, "events", "event_text", "TEXT")?;
    ensure_column_exists_tx(&tx, "events", "source_comment_id", "INTEGER")?;
    ensure_column_exists_tx(&tx, "events", "source_created_at", "TEXT")?;
    ensure_column_exists_tx(&tx, "dispatches", "backend_kind", "TEXT")?;
    ensure_column_exists_tx(&tx, "dispatches", "backend_ref", "TEXT")?;
    tx.commit()?;
    Ok(())
}

fn migration_0003_drop_legacy_tmux_dispatch_columns(conn: &mut Connection) -> Result<()> {
    let has_tmux_session = table_has_column(conn, "dispatches", "tmux_session")?;
    let has_tmux_window = table_has_column(conn, "dispatches", "tmux_window")?;
    if !has_tmux_session && !has_tmux_window {
        return Ok(());
    }

    let tmux_session_expr = if has_tmux_session {
        "tmux_session"
    } else {
        "NULL"
    };
    let tmux_window_expr = if has_tmux_window {
        "tmux_window"
    } else {
        "NULL"
    };

    let copy_sql = format!(
        r"
        INSERT INTO dispatches_new (
            id, decision_id, repo_full_name, issue_number, actor_login, directive, target_role, status,
            backend_kind, backend_ref, reason_code, error_text, run_dir, lock_path,
            codex_session_id, exit_code, started_at, ended_at
        )
        SELECT
            id, decision_id, repo_full_name, issue_number, actor_login, directive, target_role, status,
            CASE
                WHEN backend_kind IS NOT NULL AND trim(backend_kind) <> '' THEN backend_kind
                WHEN {tmux_session_expr} IS NOT NULL OR {tmux_window_expr} IS NOT NULL THEN 'tmux'
                ELSE NULL
            END,
            CASE
                WHEN backend_ref IS NOT NULL AND trim(backend_ref) <> '' THEN backend_ref
                WHEN {tmux_session_expr} IS NOT NULL AND {tmux_window_expr} IS NOT NULL THEN {tmux_session_expr} || ':' || {tmux_window_expr}
                ELSE NULL
            END,
            reason_code, error_text, run_dir, lock_path, codex_session_id, exit_code, started_at, ended_at
        FROM dispatches;
        "
    );

    let foreign_keys_were_on = foreign_keys_enabled(conn)?;
    if foreign_keys_were_on {
        conn.execute_batch("PRAGMA foreign_keys = OFF;")?;
    }

    let migration_result = (|| -> Result<()> {
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        tx.execute_batch(
            r"
            CREATE TABLE dispatches_new (
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
            ",
        )?;
        tx.execute_batch(&copy_sql)?;
        tx.execute_batch(
            r"
            DROP TABLE dispatches;
            ALTER TABLE dispatches_new RENAME TO dispatches;
            CREATE INDEX IF NOT EXISTS idx_dispatches_repo_status
                ON dispatches (repo_full_name, status);
            CREATE INDEX IF NOT EXISTS idx_dispatches_repo_issue
                ON dispatches (repo_full_name, issue_number, id DESC);
            ",
        )?;
        tx.commit()?;
        Ok(())
    })();

    if foreign_keys_were_on {
        conn.execute_batch("PRAGMA foreign_keys = ON;")?;
    }

    migration_result
}

fn migration_0004_add_notification_deliveries_table(conn: &mut Connection) -> Result<()> {
    migration_0005_ensure_notification_deliveries_table(conn)
}

fn migration_0005_ensure_notification_deliveries_table(conn: &mut Connection) -> Result<()> {
    let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
    tx.execute_batch(
        r"
        CREATE TABLE IF NOT EXISTS notification_deliveries (
            dedupe_key TEXT PRIMARY KEY,
            category TEXT NOT NULL,
            sent_at TEXT NOT NULL
        );
        CREATE INDEX IF NOT EXISTS idx_notification_deliveries_category_sent
            ON notification_deliveries (category, sent_at DESC);
        ",
    )?;
    tx.commit()?;
    Ok(())
}

fn migration_0006_ensure_trigger_dispatch_dedupe_table(conn: &mut Connection) -> Result<()> {
    let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
    tx.execute_batch(
        r"
        CREATE TABLE IF NOT EXISTS trigger_dispatch_dedupes (
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            dedupe_key TEXT NOT NULL UNIQUE,
            trigger_id TEXT NOT NULL,
            event_id INTEGER NOT NULL,
            repo_full_name TEXT NOT NULL,
            issue_number INTEGER,
            created_at TEXT NOT NULL,
            FOREIGN KEY(event_id) REFERENCES events(id)
        );
        CREATE INDEX IF NOT EXISTS idx_trigger_dispatch_dedupe_issue
            ON trigger_dispatch_dedupes (repo_full_name, issue_number, id DESC);
        ",
    )?;
    tx.commit()?;
    Ok(())
}

fn ensure_column_exists_tx(
    tx: &rusqlite::Transaction<'_>,
    table_name: &str,
    column_name: &str,
    column_type: &str,
) -> Result<()> {
    if table_has_column_tx(tx, table_name, column_name)? {
        return Ok(());
    }
    let alter = format!("ALTER TABLE {table_name} ADD COLUMN {column_name} {column_type}");
    tx.execute(&alter, [])?;
    Ok(())
}

fn table_has_column(conn: &Connection, table_name: &str, column_name: &str) -> Result<bool> {
    let pragma = format!("PRAGMA table_info({table_name})");
    let mut stmt = conn.prepare(&pragma)?;
    let names = stmt
        .query_map([], |row| row.get::<_, String>(1))?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(names.iter().any(|name| name == column_name))
}

fn table_has_column_tx(
    tx: &rusqlite::Transaction<'_>,
    table_name: &str,
    column_name: &str,
) -> Result<bool> {
    let pragma = format!("PRAGMA table_info({table_name})");
    let mut stmt = tx.prepare(&pragma)?;
    let names = stmt
        .query_map([], |row| row.get::<_, String>(1))?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(names.iter().any(|name| name == column_name))
}

fn foreign_keys_enabled(conn: &Connection) -> Result<bool> {
    let enabled: i64 = conn.query_row("PRAGMA foreign_keys", [], |row| row.get(0))?;
    Ok(enabled != 0)
}

fn table_exists(conn: &Connection, table_name: &str) -> Result<bool> {
    let exists = conn
        .query_row(
            "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type = 'table' AND name = ?1)",
            params![table_name],
            |row| row.get::<_, i64>(0),
        )
        .optional()?
        .unwrap_or(0);
    Ok(exists != 0)
}

fn migration_0007_canonicalize_poke_directive_to_reply(conn: &mut Connection) -> Result<()> {
    let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
    if table_exists(&tx, "decisions")? {
        tx.execute(
            "UPDATE decisions SET directive = 'reply' WHERE directive = 'poke'",
            [],
        )?;
    }
    if table_exists(&tx, "dispatches")? {
        tx.execute(
            "UPDATE dispatches SET directive = 'reply' WHERE directive = 'poke'",
            [],
        )?;
    }
    tx.commit()?;
    Ok(())
}

fn migration_0008_add_dispatch_principal_logins(conn: &mut Connection) -> Result<()> {
    let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
    if table_exists(&tx, "decisions")? {
        ensure_column_exists_tx(&tx, "decisions", "principal_login", "TEXT")?;
        tx.execute(
            "UPDATE decisions SET principal_login = actor_login WHERE principal_login IS NULL AND actor_login IS NOT NULL",
            [],
        )?;
    }
    if table_exists(&tx, "dispatches")? {
        ensure_column_exists_tx(&tx, "dispatches", "principal_login", "TEXT")?;
        tx.execute(
            "UPDATE dispatches SET principal_login = actor_login WHERE principal_login IS NULL AND actor_login IS NOT NULL",
            [],
        )?;
    }
    tx.commit()?;
    Ok(())
}

fn migration_0009_add_timer_schedule_context_tables(conn: &mut Connection) -> Result<()> {
    ensure_timer_schedule_context_schema(conn)
}

fn migration_0010_add_prune_support_indexes(conn: &mut Connection) -> Result<()> {
    let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
    if table_exists(&tx, "events")? {
        ensure_column_exists_tx(&tx, "events", "payload_sha256", "TEXT NOT NULL DEFAULT ''")?;
        tx.execute_batch(
            r"
            CREATE INDEX IF NOT EXISTS idx_events_received_at
                ON events (received_at);
            ",
        )?;
    }
    if table_exists(&tx, "decisions")? {
        tx.execute_batch(
            r"
            CREATE INDEX IF NOT EXISTS idx_decisions_event_id
                ON decisions (event_id);
            ",
        )?;
    }
    if table_exists(&tx, "dispatches")? {
        tx.execute_batch(
            r"
            CREATE INDEX IF NOT EXISTS idx_dispatches_decision_id
                ON dispatches (decision_id);
            ",
        )?;
    }
    if table_exists(&tx, "heartbeats")? {
        tx.execute_batch(
            r"
            CREATE INDEX IF NOT EXISTS idx_heartbeats_recorded_at
                ON heartbeats (recorded_at);
            ",
        )?;
    }
    if table_exists(&tx, "reconciles")? {
        tx.execute_batch(
            r"
            CREATE INDEX IF NOT EXISTS idx_reconciles_recorded_at
                ON reconciles (recorded_at);
            ",
        )?;
    }
    if table_exists(&tx, "trigger_dispatch_dedupes")? {
        tx.execute_batch(
            r"
            CREATE INDEX IF NOT EXISTS idx_trigger_dispatch_dedupes_event_id
                ON trigger_dispatch_dedupes (event_id);
            ",
        )?;
    }
    tx.commit()?;
    Ok(())
}

fn migration_0011_ensure_timer_schedule_context_tables_compat(conn: &mut Connection) -> Result<()> {
    ensure_timer_schedule_context_schema(conn)
}

fn migration_0012_add_deploy_jobs_table(conn: &mut Connection) -> Result<()> {
    let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
    tx.execute_batch(
        r"
        CREATE TABLE IF NOT EXISTS deploy_jobs (
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            repo_full_name TEXT NOT NULL,
            target_branch TEXT NOT NULL,
            target_sha TEXT NOT NULL,
            source_event_id INTEGER,
            source_delivery_id TEXT,
            source_actor_login TEXT,
            status TEXT NOT NULL,
            reason_code TEXT,
            error_text TEXT,
            checkout_path TEXT,
            log_path TEXT,
            incident_issue_number INTEGER,
            rollback_status TEXT,
            attempt_count INTEGER NOT NULL DEFAULT 0,
            queued_at TEXT NOT NULL,
            started_at TEXT,
            ended_at TEXT,
            FOREIGN KEY(source_event_id) REFERENCES events(id),
            UNIQUE(repo_full_name, target_branch, target_sha)
        );
        CREATE INDEX IF NOT EXISTS idx_deploy_jobs_status_id
            ON deploy_jobs (status, id);
        CREATE INDEX IF NOT EXISTS idx_deploy_jobs_repo_branch_id
            ON deploy_jobs (repo_full_name, target_branch, id DESC);
        ",
    )?;
    tx.commit()?;
    Ok(())
}

fn migration_0013_ensure_deploy_v2_tables(conn: &mut Connection) -> Result<()> {
    let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
    if table_exists(&tx, "deploy_jobs")? {
        ensure_column_exists_tx(&tx, "deploy_jobs", "superseded_by_job_id", "INTEGER")?;
        ensure_column_exists_tx(&tx, "deploy_jobs", "worker_identity", "TEXT")?;
    }
    tx.execute_batch(
        r"
        CREATE TABLE IF NOT EXISTS deploy_runs (
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            deploy_job_id INTEGER NOT NULL,
            repo_full_name TEXT NOT NULL,
            target_branch TEXT NOT NULL,
            target_sha TEXT NOT NULL,
            phase TEXT NOT NULL,
            status TEXT NOT NULL,
            reason_code TEXT,
            detail_json TEXT,
            started_at TEXT NOT NULL,
            ended_at TEXT,
            FOREIGN KEY(deploy_job_id) REFERENCES deploy_jobs(id)
        );
        CREATE INDEX IF NOT EXISTS idx_deploy_runs_job_id
            ON deploy_runs (deploy_job_id, id DESC);
        CREATE INDEX IF NOT EXISTS idx_deploy_runs_status_id
            ON deploy_runs (status, id DESC);

        CREATE TABLE IF NOT EXISTS deploy_releases (
            repo_full_name TEXT NOT NULL,
            target_branch TEXT NOT NULL,
            active_sha TEXT,
            previous_sha TEXT,
            updated_at TEXT NOT NULL,
            PRIMARY KEY(repo_full_name, target_branch)
        );
        ",
    )?;
    tx.commit()?;
    Ok(())
}

fn ensure_timer_schedule_context_schema(conn: &mut Connection) -> Result<()> {
    let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
    if table_exists(&tx, "decisions")? {
        ensure_column_exists_tx(&tx, "decisions", "schedule_timer_id", "TEXT")?;
        ensure_column_exists_tx(&tx, "decisions", "timer_context_key", "TEXT")?;
        ensure_column_exists_tx(&tx, "decisions", "resume_session_id", "TEXT")?;
    }
    tx.execute_batch(
        r"
        CREATE TABLE IF NOT EXISTS schedule_claims (
            timer_id TEXT NOT NULL,
            slot_index INTEGER NOT NULL,
            scheduled_for TEXT NOT NULL,
            issue_number INTEGER,
            event_id INTEGER,
            decision_id INTEGER,
            created_at TEXT NOT NULL,
            PRIMARY KEY (timer_id, slot_index)
        );
        CREATE INDEX IF NOT EXISTS idx_schedule_claims_timer_scheduled
            ON schedule_claims (timer_id, scheduled_for DESC);

        CREATE TABLE IF NOT EXISTS timer_contexts (
            context_key TEXT PRIMARY KEY,
            role_name TEXT NOT NULL,
            repo_full_name TEXT NOT NULL,
            principal_login TEXT NOT NULL,
            cwd TEXT NOT NULL,
            codex_session_id TEXT,
            run_count INTEGER NOT NULL DEFAULT 0,
            prompt_bytes_total INTEGER NOT NULL DEFAULT 0,
            last_context_pct INTEGER,
            last_status TEXT,
            updated_at TEXT NOT NULL
        );
        CREATE INDEX IF NOT EXISTS idx_timer_contexts_role_repo
            ON timer_contexts (role_name, repo_full_name, updated_at DESC);
        ",
    )?;
    tx.commit()?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn apply_all_creates_latest_schema_on_new_db() {
        let mut conn = Connection::open_in_memory().expect("open in-memory db");
        apply_all(&mut conn).expect("apply all migrations");
        assert!(table_exists(&conn, "notification_deliveries").expect("table exists"));
        assert!(table_exists(&conn, "trigger_dispatch_dedupes").expect("table exists"));
        assert!(table_exists(&conn, "schedule_claims").expect("table exists"));
        assert!(table_exists(&conn, "timer_contexts").expect("table exists"));
        assert!(table_exists(&conn, "deploy_jobs").expect("table exists"));
        assert!(table_exists(&conn, "deploy_runs").expect("table exists"));
        assert!(table_exists(&conn, "deploy_releases").expect("table exists"));
        assert!(!table_has_column(&conn, "dispatches", "tmux_session").expect("pragma"));
        assert!(!table_has_column(&conn, "dispatches", "tmux_window").expect("pragma"));
        assert!(table_has_column(&conn, "dispatches", "backend_kind").expect("pragma"));
        assert!(table_has_column(&conn, "dispatches", "backend_ref").expect("pragma"));
        assert!(table_has_column(&conn, "decisions", "principal_login").expect("pragma"));
        assert!(table_has_column(&conn, "decisions", "schedule_timer_id").expect("pragma"));
        assert!(table_has_column(&conn, "decisions", "timer_context_key").expect("pragma"));
        assert!(table_has_column(&conn, "decisions", "resume_session_id").expect("pragma"));
        assert!(table_has_column(&conn, "dispatches", "principal_login").expect("pragma"));
        assert!(table_has_column(&conn, "trigger_dispatch_dedupes", "dedupe_key").expect("pragma"));
        assert_eq!(
            current_schema_version(&conn).expect("schema version query"),
            LATEST_SCHEMA_VERSION
        );
    }

    #[test]
    fn migration_rebuilds_legacy_dispatches_and_keeps_rows() {
        let mut conn = Connection::open_in_memory().expect("open in-memory db");
        conn.execute_batch(
            r"
            CREATE TABLE schema_migrations (
                version INTEGER PRIMARY KEY,
                name TEXT NOT NULL,
                applied_at TEXT NOT NULL
            );
            INSERT INTO schema_migrations(version, name, applied_at) VALUES
                (1, 'bootstrap_core_tables', '2026-01-01T00:00:00Z'),
                (2, 'add_late_columns', '2026-01-01T00:00:01Z');
            CREATE TABLE decisions (
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
                created_at TEXT NOT NULL
            );
            INSERT INTO decisions(event_id, repo_full_name, issue_number, actor_login, directive, target_role, decision, reason_code, would_dispatch, created_at)
            VALUES (1, 'main/forgejo-work', 1, 'main', 'poke', 'codex-orch', 'accepted', 'explicit_directive', 1, '2026-01-01T00:00:00Z');
            CREATE TABLE dispatches (
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
                backend_kind TEXT,
                backend_ref TEXT,
                FOREIGN KEY(decision_id) REFERENCES decisions(id)
            );
            INSERT INTO dispatches(
                decision_id, repo_full_name, issue_number, actor_login, directive, target_role, status,
                reason_code, error_text, tmux_session, tmux_window, run_dir, lock_path, codex_session_id, exit_code, started_at, ended_at, backend_kind, backend_ref
            ) VALUES (
                1, 'main/forgejo-work', 1, 'main', 'poke', 'codex-orch', 'completed',
                'completed', NULL, 'codex-orch', 'rmain-forgejo-work-i1', '/tmp/run', '/tmp/lock', 'abc', 0, '2026-01-01T00:00:00Z', '2026-01-01T00:01:00Z', NULL, NULL
            );
            ",
        )
        .expect("seed legacy schema");

        apply_all(&mut conn).expect("apply all migrations");

        assert!(table_exists(&conn, "notification_deliveries").expect("table exists"));
        assert!(table_exists(&conn, "trigger_dispatch_dedupes").expect("table exists"));
        assert!(table_exists(&conn, "schedule_claims").expect("table exists"));
        assert!(table_exists(&conn, "timer_contexts").expect("table exists"));
        assert!(table_exists(&conn, "deploy_jobs").expect("table exists"));
        assert!(table_exists(&conn, "deploy_runs").expect("table exists"));
        assert!(table_exists(&conn, "deploy_releases").expect("table exists"));
        assert!(!table_has_column(&conn, "dispatches", "tmux_session").expect("pragma"));
        assert!(!table_has_column(&conn, "dispatches", "tmux_window").expect("pragma"));
        assert!(table_has_column(&conn, "trigger_dispatch_dedupes", "dedupe_key").expect("pragma"));
        assert!(table_has_column(&conn, "decisions", "schedule_timer_id").expect("pragma"));
        assert!(table_has_column(&conn, "decisions", "timer_context_key").expect("pragma"));
        assert!(table_has_column(&conn, "decisions", "resume_session_id").expect("pragma"));

        let (kind, backend_ref, directive, principal): (Option<String>, Option<String>, String, Option<String>) = conn
            .query_row(
                "SELECT backend_kind, backend_ref, directive, principal_login FROM dispatches WHERE id = 1",
                [],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
            )
            .expect("query migrated dispatch");
        assert_eq!(kind.as_deref(), Some("tmux"));
        assert_eq!(
            backend_ref.as_deref(),
            Some("codex-orch:rmain-forgejo-work-i1")
        );
        assert_eq!(directive.as_str(), "reply");
        assert_eq!(principal.as_deref(), Some("main"));
        assert_eq!(
            current_schema_version(&conn).expect("schema version query"),
            LATEST_SCHEMA_VERSION
        );
    }

    #[test]
    fn migration_compat_heals_timer_schema_when_db_already_has_legacy_v10() {
        let mut conn = Connection::open_in_memory().expect("open in-memory db");
        conn.execute_batch(
            r"
            CREATE TABLE schema_migrations (
                version INTEGER PRIMARY KEY,
                name TEXT NOT NULL,
                applied_at TEXT NOT NULL
            );
            INSERT INTO schema_migrations(version, name, applied_at) VALUES
                (1, 'bootstrap_core_tables', '2026-01-01T00:00:00Z'),
                (2, 'add_late_columns', '2026-01-01T00:00:01Z'),
                (3, 'drop_legacy_tmux_dispatch_columns', '2026-01-01T00:00:02Z'),
                (4, 'add_notification_deliveries_table', '2026-01-01T00:00:03Z'),
                (5, 'ensure_notification_deliveries_table', '2026-01-01T00:00:04Z'),
                (6, 'ensure_trigger_dispatch_dedupe_table', '2026-01-01T00:00:05Z'),
                (7, 'canonicalize_poke_directive_to_reply', '2026-01-01T00:00:06Z'),
                (8, 'add_dispatch_principal_logins', '2026-01-01T00:00:07Z'),
                (9, 'add_events_payload_sha256', '2026-01-01T00:00:08Z'),
                (10, 'add_prune_support_indexes', '2026-01-01T00:00:09Z');
            CREATE TABLE decisions (
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
                principal_login TEXT
            );
            CREATE TABLE events (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                delivery_id TEXT NOT NULL,
                event_type TEXT NOT NULL,
                repo_full_name TEXT NOT NULL,
                issue_number INTEGER,
                action TEXT,
                actor_login TEXT,
                raw_json TEXT NOT NULL,
                received_at TEXT NOT NULL,
                event_text TEXT,
                source_comment_id INTEGER,
                source_created_at TEXT,
                payload_sha256 TEXT NOT NULL DEFAULT ''
            );
            ",
        )
        .expect("seed legacy v10 schema");

        apply_all(&mut conn).expect("apply all migrations");
        assert!(table_has_column(&conn, "decisions", "schedule_timer_id").expect("pragma"));
        assert!(table_has_column(&conn, "decisions", "timer_context_key").expect("pragma"));
        assert!(table_has_column(&conn, "decisions", "resume_session_id").expect("pragma"));
        assert!(table_exists(&conn, "schedule_claims").expect("table exists"));
        assert!(table_exists(&conn, "timer_contexts").expect("table exists"));
        assert!(table_exists(&conn, "deploy_jobs").expect("table exists"));
        assert!(table_exists(&conn, "deploy_runs").expect("table exists"));
        assert!(table_exists(&conn, "deploy_releases").expect("table exists"));
        assert_eq!(
            current_schema_version(&conn).expect("schema version query"),
            LATEST_SCHEMA_VERSION
        );
    }

    #[test]
    fn apply_all_upgrades_notification_only_schema_to_include_trigger_dedupes() {
        let mut conn = Connection::open_in_memory().expect("open in-memory db");
        conn.execute_batch(
            r"
            CREATE TABLE schema_migrations (
                version INTEGER PRIMARY KEY,
                name TEXT NOT NULL,
                applied_at TEXT NOT NULL
            );
            INSERT INTO schema_migrations(version, name, applied_at) VALUES
                (1, 'bootstrap_core_tables', '2026-01-01T00:00:00Z'),
                (2, 'add_late_columns', '2026-01-01T00:00:01Z'),
                (3, 'drop_legacy_tmux_dispatch_columns', '2026-01-01T00:00:02Z'),
                (4, 'add_notification_deliveries_table', '2026-01-01T00:00:03Z'),
                (5, 'ensure_notification_deliveries_table', '2026-01-01T00:00:04Z');
            CREATE TABLE notification_deliveries (
                dedupe_key TEXT PRIMARY KEY,
                category TEXT NOT NULL,
                sent_at TEXT NOT NULL
            );
            ",
        )
        .expect("seed notification-only schema");

        apply_all(&mut conn).expect("apply all migrations");

        assert!(table_exists(&conn, "notification_deliveries").expect("table exists"));
        assert!(table_exists(&conn, "trigger_dispatch_dedupes").expect("table exists"));
        assert!(table_exists(&conn, "schedule_claims").expect("table exists"));
        assert!(table_exists(&conn, "timer_contexts").expect("table exists"));
        assert!(table_exists(&conn, "deploy_jobs").expect("table exists"));
        assert!(table_exists(&conn, "deploy_runs").expect("table exists"));
        assert!(table_exists(&conn, "deploy_releases").expect("table exists"));
        assert_eq!(
            current_schema_version(&conn).expect("schema version query"),
            LATEST_SCHEMA_VERSION
        );
    }
}
