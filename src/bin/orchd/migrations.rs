use anyhow::{Context, Result, anyhow};
use chrono::Utc;
use rusqlite::{Connection, OptionalExtension, TransactionBehavior, params};

#[derive(Clone, Copy)]
struct Migration {
    version: i64,
    name: &'static str,
    apply: fn(&mut Connection) -> Result<()>,
}

const LATEST_SCHEMA_VERSION: i64 = 3;

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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn apply_all_creates_latest_schema_on_new_db() {
        let mut conn = Connection::open_in_memory().expect("open in-memory db");
        apply_all(&mut conn).expect("apply all migrations");
        assert!(!table_has_column(&conn, "dispatches", "tmux_session").expect("pragma"));
        assert!(!table_has_column(&conn, "dispatches", "tmux_window").expect("pragma"));
        assert!(table_has_column(&conn, "dispatches", "backend_kind").expect("pragma"));
        assert!(table_has_column(&conn, "dispatches", "backend_ref").expect("pragma"));
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

        assert!(!table_has_column(&conn, "dispatches", "tmux_session").expect("pragma"));
        assert!(!table_has_column(&conn, "dispatches", "tmux_window").expect("pragma"));

        let (kind, backend_ref): (Option<String>, Option<String>) = conn
            .query_row(
                "SELECT backend_kind, backend_ref FROM dispatches WHERE id = 1",
                [],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .expect("query migrated dispatch");
        assert_eq!(kind.as_deref(), Some("tmux"));
        assert_eq!(
            backend_ref.as_deref(),
            Some("codex-orch:rmain-forgejo-work-i1")
        );
        assert_eq!(
            current_schema_version(&conn).expect("schema version query"),
            LATEST_SCHEMA_VERSION
        );
    }
}
