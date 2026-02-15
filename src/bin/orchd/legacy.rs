use anyhow::{Context, Result};
use clap::Parser;

use super::cli::{Cli, OrchdCommand};
use super::finalize;
use super::server;
use super::telemetry::init_telemetry;

pub(super) fn run_entry() -> Result<()> {
    init_telemetry();
    let cli = Cli::parse();
    if let Some(command) = cli.command {
        // Subcommands are intentionally synchronous to avoid mixing reqwest::blocking
        // with a Tokio runtime.
        return run_command(command);
    }
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .context("failed to build tokio runtime")?;
    runtime.block_on(server::run_server(cli))
}

fn run_command(command: OrchdCommand) -> Result<()> {
    match command {
        OrchdCommand::FinalizeDispatch(args) => finalize::finalize_dispatch_command(args),
    }
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::path::{Path, PathBuf};

    use chrono::{Duration as ChronoDuration, Utc};
    use rusqlite::params;

    use crate::orchd::state::EventContext;
    use crate::orchd::webhook::{decide, parse_directive};
    use crate::orchd::{db, dispatch};
    use forgejo_agent::orchd_dispatch_core::DispatchState;

    fn inflight_dispatch(
        status: &str,
        started_at: String,
        tmux_session: Option<&str>,
    ) -> db::InflightDispatch {
        db::InflightDispatch {
            id: 1,
            status: status.to_string(),
            started_at,
            backend_kind: Some("tmux".to_string()),
            backend_ref: Some("codex-orch:rmain-orchd-debug-i1".to_string()),
            tmux_session: tmux_session.map(str::to_string),
            tmux_window: None,
            lock_path: None,
        }
    }

    #[test]
    fn starting_dispatch_is_not_stale_within_grace_period() {
        let started_at = (Utc::now() - ChronoDuration::seconds(5)).to_rfc3339();
        let dispatch = inflight_dispatch("starting", started_at, None);
        assert!(!dispatch::is_stale_starting_dispatch(
            &dispatch,
            "main/orchd-debug",
            1
        ));
    }

    #[test]
    fn starting_dispatch_with_invalid_timestamp_is_stale() {
        let dispatch = inflight_dispatch("starting", "invalid-timestamp".to_string(), None);
        assert!(dispatch::is_stale_starting_dispatch(
            &dispatch,
            "main/orchd-debug",
            1
        ));
    }

    #[test]
    fn starting_dispatch_without_tmux_session_is_stale_after_grace_period() {
        let started_at = (Utc::now()
            - ChronoDuration::seconds(dispatch::STARTING_DISPATCH_STALE_AFTER_SEC + 5))
        .to_rfc3339();
        let dispatch = inflight_dispatch("starting", started_at, None);
        assert!(dispatch::is_stale_starting_dispatch(
            &dispatch,
            "main/orchd-debug",
            1
        ));
    }

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
    ) -> db::DispatchReservation {
        let started_at = Utc::now().to_rfc3339();
        db::reserve_dispatch_starting(
            db_path,
            db::DispatchInsert {
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
        let conn = db::open_db(db_path).expect("open db");
        let now = Utc::now().to_rfc3339();
        conn.execute(
            r"
            INSERT INTO events (delivery_id, event_type, repo_full_name, issue_number, action, actor_login, raw_json, received_at)
            VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)
            ",
            params![
                format!("test-delivery-{}", Utc::now().timestamp_nanos_opt().unwrap_or(0)),
                "issue_comment",
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
                "poke",
                "codex-orch",
                "accepted",
                "explicit_directive",
                1_i64,
                Utc::now().to_rfc3339(),
            ],
        )
        .expect("insert decision");
        conn.last_insert_rowid()
    }

    fn dispatch_event_kinds(db_path: &Path, dispatch_id: i64) -> Vec<String> {
        let conn = db::open_db(db_path).expect("open db for event scan");
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
        db::init_db(&db_path).expect("db init");
        let first_decision_id = seed_decision_id(&db_path);
        let second_decision_id = seed_decision_id(&db_path);

        let first = reserve_sample_dispatch(&db_path, first_decision_id, 7, "poke");
        let first_id = match first {
            db::DispatchReservation::Started(id) => id,
            db::DispatchReservation::InFlightIssue(_)
            | db::DispatchReservation::InFlightRepo(_) => {
                panic!("expected first reservation to start")
            }
        };
        assert_eq!(
            dispatch_event_kinds(&db_path, first_id),
            vec!["mark_starting".to_string()]
        );

        let second = reserve_sample_dispatch(&db_path, second_decision_id, 7, "poke");
        match second {
            db::DispatchReservation::InFlightIssue(id) => assert_eq!(id, first_id),
            db::DispatchReservation::Started(_) => {
                panic!("expected second reservation to be blocked")
            }
            db::DispatchReservation::InFlightRepo(_) => {
                panic!("expected issue-level inflight, not repo-level inflight")
            }
        }

        let _ = fs::remove_file(db_path);
    }

    #[test]
    fn reserve_dispatch_blocks_second_inflight_impl_for_repo() {
        let db_path = temp_db_path("dispatch-reserve-repo");
        db::init_db(&db_path).expect("db init");
        let first_decision_id = seed_decision_id(&db_path);
        let second_decision_id = seed_decision_id(&db_path);

        let first = reserve_sample_dispatch(&db_path, first_decision_id, 7, "impl");
        let first_id = match first {
            db::DispatchReservation::Started(id) => id,
            db::DispatchReservation::InFlightIssue(_)
            | db::DispatchReservation::InFlightRepo(_) => {
                panic!("expected first reservation to start")
            }
        };

        let second = reserve_sample_dispatch(&db_path, second_decision_id, 8, "impl");
        match second {
            db::DispatchReservation::InFlightRepo(id) => assert_eq!(id, first_id),
            db::DispatchReservation::Started(_) => {
                panic!("expected repo-level inflight block")
            }
            db::DispatchReservation::InFlightIssue(_) => {
                panic!("expected repo-level inflight, not issue-level inflight")
            }
        }

        let _ = fs::remove_file(db_path);
    }

    #[test]
    fn stale_autoheal_records_heal_event() {
        let db_path = temp_db_path("dispatch-autoheal");
        db::init_db(&db_path).expect("db init");
        let decision_id = seed_decision_id(&db_path);
        let started_id = match reserve_sample_dispatch(&db_path, decision_id, 9, "poke") {
            db::DispatchReservation::Started(id) => id,
            db::DispatchReservation::InFlightIssue(_)
            | db::DispatchReservation::InFlightRepo(_) => {
                panic!("expected started dispatch")
            }
        };
        db::mark_dispatch_failed_runtime(
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

        let conn = db::open_db(&db_path).expect("open db");
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
    fn owner_comment_without_directive_is_ignored() {
        let context = EventContext {
            repo_full_name: "main/orchd-debug".to_string(),
            issue_number: Some(1),
            actor_login: Some("main".to_string()),
            text: Some("just checking in".to_string()),
            source_comment_id: None,
            source_created_at: None,
        };
        let decision = decide("issue_comment", Some(&context));
        assert_eq!(decision.decision, "ignored");
        assert_eq!(decision.reason_code, "no_directive");
        assert!(!decision.would_dispatch);
    }

    #[test]
    fn explicit_directive_is_still_accepted() {
        let context = EventContext {
            repo_full_name: "main/orchd-debug".to_string(),
            issue_number: Some(1),
            actor_login: Some("main".to_string()),
            text: Some("@codex-orch poke".to_string()),
            source_comment_id: None,
            source_created_at: None,
        };
        let decision = decide("issue_comment", Some(&context));
        assert_eq!(decision.decision, "accepted");
        assert_eq!(decision.reason_code, "explicit_directive");
        assert_eq!(decision.directive.as_deref(), Some("poke"));
        assert_eq!(decision.target_role.as_deref(), Some("codex-orch"));
        assert!(decision.would_dispatch);
    }

    #[test]
    fn codex_alias_maps_to_orch_role() {
        let parsed = parse_directive("@codex poke").expect("directive should parse");
        assert_eq!(parsed.role, "codex-orch");
        assert_eq!(parsed.directive, "poke");
    }
}
