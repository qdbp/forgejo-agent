use std::collections::hash_map::DefaultHasher;
use std::fs;
use std::hash::{Hash, Hasher};
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result, anyhow};
use chrono::{DateTime, TimeDelta, Utc};
use serde_json::json;

use forgejo_agent::config::AgentConfig;

use super::cli::{Cli, ScheduleTickArgs};
use super::db;
use super::dispatch_config::{
    DispatchConfig, DispatchTimerCatchUp, DispatchTimerConfig, DispatchTimerContextMode,
    load_dispatch_config,
};
use super::forgejoctl_cmd;
use super::lexicon::{DECISION_ACCEPTED, EVENT_SCHEDULE};
use super::paths::{expand_tilde_path, resolve_dispatch_config_path};
use super::state::{DecisionRecord, EventRecord};

#[derive(Debug, Clone, Copy)]
struct DueSlot {
    index: i64,
    scheduled_for: DateTime<Utc>,
}

#[derive(Debug, Clone)]
struct CreatedIssue {
    number: u64,
}

pub(super) fn schedule_tick_command(cli: &Cli, args: ScheduleTickArgs) -> Result<()> {
    let _cfg = AgentConfig::load(cli.config.clone(), cli.token_file.clone())?;
    let dispatch_config_path = resolve_dispatch_config_path(&cli.dispatch_config)?;
    let dispatch_config = load_dispatch_config(&dispatch_config_path)?;
    let db_path = expand_tilde_path(&cli.db_path)?;
    db::init_db(&db_path)?;

    let control_token = dispatch_config
        .control_plane
        .as_ref()
        .map(|control| control.token_file.as_path())
        .ok_or_else(|| {
            anyhow!("dispatch config missing [control_plane].token_file (required for schedule)")
        })?;
    let swarm_root = swarm_root_for_dispatch_config(&dispatch_config_path);
    let timer_filter = args
        .timer
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_ascii_lowercase);

    let mut matched_filter = timer_filter.is_none();
    for timer in &dispatch_config.timers {
        if !timer.enabled {
            continue;
        }
        if let Some(filter) = timer_filter.as_deref()
            && timer.id != *filter
        {
            continue;
        }
        matched_filter = true;
        tick_timer(
            &db_path,
            &dispatch_config,
            cli.config.as_deref(),
            control_token,
            swarm_root.as_path(),
            timer,
        )?;
    }

    if !matched_filter {
        return Err(anyhow!(
            "no enabled timer matched filter '{}'",
            timer_filter.unwrap_or_default()
        ));
    }
    Ok(())
}

fn tick_timer(
    db_path: &Path,
    dispatch_config: &DispatchConfig,
    config_override: Option<&Path>,
    control_token: &Path,
    swarm_root: &Path,
    timer: &DispatchTimerConfig,
) -> Result<()> {
    if db::timer_active_dispatch_count(db_path, timer.id.as_str())?
        >= u64::from(timer.schedule.max_inflight)
    {
        return Ok(());
    }

    let now = Utc::now();
    let Some(due) = next_due_slot(db_path, timer, now)? else {
        return Ok(());
    };
    if !db::claim_schedule_slot(
        db_path,
        timer.id.as_str(),
        due.index,
        due.scheduled_for.to_rfc3339().as_str(),
    )? {
        return Ok(());
    }

    let mut issue_created = false;
    let tick_result = (|| -> Result<()> {
        let cwd = resolve_timer_cwd(timer, swarm_root)?;
        fs::create_dir_all(&cwd)
            .with_context(|| format!("failed creating timer cwd {}", cwd.display()))?;
        db::upsert_timer_context_seed(
            db_path,
            timer.context.key.as_str(),
            timer.dispatch.target_role.as_str(),
            timer.target.repo.to_string().as_str(),
            timer.dispatch.principal.as_str(),
            cwd.to_string_lossy().as_ref(),
        )?;
        let resume_session_id = resolve_resume_session_id(db_path, timer)?;

        forgejoctl_cmd::run_forgejoctl(
            &dispatch_config.forgejoctl_bin,
            config_override,
            control_token,
            &["repo", "ensure", timer.target.repo.to_string().as_str()],
        )
        .with_context(|| format!("failed ensuring timer target repo {}", timer.target.repo))?;

        let title = render_run_issue_title(timer, due.scheduled_for);
        let body = render_run_issue_body(timer, due.scheduled_for)?;
        let issue = create_run_issue(
            dispatch_config,
            config_override,
            control_token,
            timer,
            title.as_str(),
            body.as_str(),
        )?;
        issue_created = true;

        let event = EventRecord {
            delivery_id: format!("schedule:{}:{}", timer.id, due.index),
            event_type: EVENT_SCHEDULE.to_string(),
            repo_full_name: timer.target.repo.to_string(),
            issue_number: Some(issue.number),
            source_issue_id: None,
            source_issue_anchor_at: None,
            action: Some("tick".to_string()),
            actor_login: Some("orchd".to_string()),
            event_text: Some(format!(
                "scheduled timer run {} slot {}",
                timer.id, due.index
            )),
            source_comment_id: None,
            source_created_at: Some(now.to_rfc3339()),
            raw_json: serde_json::to_string(&json!({
                "timer_id": timer.id,
                "slot_index": due.index,
                "scheduled_for": due.scheduled_for.to_rfc3339(),
                "issue_number": issue.number,
                "context_key": timer.context.key,
            }))?,
        };
        let event_id = db::insert_event(db_path, &event)?.ok_or_else(|| {
            anyhow!(
                "schedule event was already present unexpectedly: {}",
                event.delivery_id
            )
        })?;
        let decision = DecisionRecord {
            decision: DECISION_ACCEPTED.to_string(),
            reason_code: format!("scheduled:{}", timer.id),
            directive: Some(timer.dispatch.directive.clone()),
            target_role: Some(timer.dispatch.target_role.clone()),
            principal_login: Some(timer.dispatch.principal.clone()),
            schedule_timer_id: Some(timer.id.clone()),
            timer_context_key: Some(timer.context.key.clone()),
            resume_session_id,
            would_dispatch: true,
            decision_source: "scheduled_timer".to_string(),
            trigger_id: None,
            trigger_dedupe_key: None,
            trigger_apply_guardrails: false,
        };
        let decision_id = db::insert_decision(db_path, event_id, &event, &decision)?;
        db::annotate_schedule_claim(
            db_path,
            timer.id.as_str(),
            due.index,
            issue.number,
            event_id,
            decision_id,
        )?;
        Ok(())
    })();

    if tick_result.is_err() && !issue_created {
        let _ = db::delete_schedule_claim(db_path, timer.id.as_str(), due.index);
    }
    tick_result
}

fn create_run_issue(
    dispatch_config: &DispatchConfig,
    config_override: Option<&Path>,
    token_file: &Path,
    timer: &DispatchTimerConfig,
    title: &str,
    body: &str,
) -> Result<CreatedIssue> {
    let mut args = vec![
        "issue".to_string(),
        "create".to_string(),
        timer.target.repo.to_string(),
        "--title".to_string(),
        title.to_string(),
        "--body".to_string(),
        body.to_string(),
        "--workflow".to_string(),
        timer.run_issue.workflow.clone(),
        "--json".to_string(),
    ];
    for label in &timer.run_issue.labels {
        args.push("--label".to_string());
        args.push(label.clone());
    }
    let output = run_forgejoctl_with_output(
        &dispatch_config.forgejoctl_bin,
        config_override,
        token_file,
        args.as_slice(),
    )?;
    let payload: serde_json::Value = serde_json::from_slice(&output)
        .context("failed parsing timer issue create response as JSON")?;
    let issue_number = payload
        .get("number")
        .and_then(serde_json::Value::as_u64)
        .ok_or_else(|| anyhow!("timer issue create response missing numeric 'number'"))?;
    Ok(CreatedIssue {
        number: issue_number,
    })
}

fn run_forgejoctl_with_output(
    forgejoctl_bin: &Path,
    config_override: Option<&Path>,
    token_file: &Path,
    args: &[String],
) -> Result<Vec<u8>> {
    let mut cmd = Command::new(forgejoctl_bin);
    if let Some(config_override) = config_override {
        cmd.arg("--config").arg(config_override);
    }
    let output = cmd
        .arg("--token-file")
        .arg(token_file)
        .args(args)
        .output()
        .with_context(|| format!("failed invoking forgejoctl {}", forgejoctl_bin.display()))?;
    if output.status.success() {
        Ok(output.stdout)
    } else {
        Err(anyhow!(
            "forgejoctl command failed (exit={:?}) args={args:?} stderr={}",
            output.status.code(),
            String::from_utf8_lossy(&output.stderr).trim()
        ))
    }
}

fn render_run_issue_title(timer: &DispatchTimerConfig, scheduled_for: DateTime<Utc>) -> String {
    let date = scheduled_for.format("%Y-%m-%d").to_string();
    timer
        .run_issue
        .title_template
        .replace("{{date}}", date.as_str())
        .replace("{{timer_id}}", timer.id.as_str())
        .replace("{{scheduled_for}}", scheduled_for.to_rfc3339().as_str())
}

fn render_run_issue_body(
    timer: &DispatchTimerConfig,
    scheduled_for: DateTime<Utc>,
) -> Result<String> {
    let standing_order = fs::read_to_string(&timer.run_issue.body_file).with_context(|| {
        format!(
            "failed reading timer standing order body {}",
            timer.run_issue.body_file.display()
        )
    })?;
    let mut body = String::new();
    body.push_str("scheduled timer run\n\n");
    body.push_str(format!("- timer_id: {}\n", timer.id).as_str());
    body.push_str(format!("- scheduled_for: {}\n", scheduled_for.to_rfc3339()).as_str());
    body.push_str(format!("- context_key: {}\n", timer.context.key).as_str());
    body.push_str(format!("- role: {}\n\n", timer.dispatch.target_role).as_str());
    body.push_str(standing_order.as_str());
    Ok(body)
}

fn swarm_root_for_dispatch_config(dispatch_config_path: &Path) -> PathBuf {
    let config_dir = dispatch_config_path
        .parent()
        .map(Path::to_path_buf)
        .unwrap_or_else(|| PathBuf::from("."));
    config_dir
        .parent()
        .map(Path::to_path_buf)
        .unwrap_or(config_dir)
}

fn resolve_timer_cwd(timer: &DispatchTimerConfig, swarm_root: &Path) -> Result<PathBuf> {
    if let Some(cwd) = timer.workspace.cwd.as_deref() {
        let expanded = expand_tilde_path(cwd)?;
        if expanded.is_absolute() {
            return Ok(expanded);
        }
        return Ok(swarm_root.join(expanded));
    }
    Ok(swarm_root
        .join("offices")
        .join(timer.dispatch.target_role.as_str()))
}

fn resolve_resume_session_id(
    db_path: &Path,
    timer: &DispatchTimerConfig,
) -> Result<Option<String>> {
    let Some(existing) = db::timer_context(db_path, timer.context.key.as_str())? else {
        return Ok(None);
    };
    if matches!(timer.context.mode, DispatchTimerContextMode::Fresh) {
        db::reset_timer_context_state(db_path, timer.context.key.as_str())?;
        return Ok(None);
    }
    let should_reset = should_reset_timer_context(timer, &existing);
    if should_reset {
        db::reset_timer_context_state(db_path, timer.context.key.as_str())?;
        return Ok(None);
    }
    Ok(existing
        .codex_session_id
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(ToOwned::to_owned))
}

fn should_reset_timer_context(timer: &DispatchTimerConfig, row: &db::TimerContextRow) -> bool {
    let Some(session_id) = row
        .codex_session_id
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
    else {
        return false;
    };
    if session_id.is_empty() {
        return false;
    }
    if let Some(last_pct) = row.last_context_pct
        && last_pct <= timer.context.min_context_pct
    {
        return true;
    }
    if row.prompt_bytes_total >= timer.context.fallback_max_prompt_bytes {
        return true;
    }
    if let Some(max_runs) = timer.context.max_runs
        && row.run_count >= max_runs
    {
        return true;
    }
    if let Some(max_age_sec) = timer.context.max_age_sec
        && let Ok(updated_at) = DateTime::parse_from_rfc3339(&row.updated_at)
    {
        let age = Utc::now().signed_duration_since(updated_at.with_timezone(&Utc));
        if age >= TimeDelta::seconds(i64::try_from(max_age_sec).unwrap_or(i64::MAX)) {
            return true;
        }
    }
    if timer.context.reset_on_failure
        && row
            .last_status
            .as_deref()
            .is_some_and(|status| status != "completed")
    {
        return true;
    }
    false
}

fn next_due_slot(
    db_path: &Path,
    timer: &DispatchTimerConfig,
    now: DateTime<Utc>,
) -> Result<Option<DueSlot>> {
    let start_at = DateTime::parse_from_rfc3339(timer.schedule.start_at.as_str())
        .with_context(|| format!("timer '{}' has invalid schedule.start_at", timer.id))?
        .with_timezone(&Utc);
    if now < start_at {
        return Ok(None);
    }
    let interval_sec = i64::try_from(timer.schedule.interval_sec).unwrap_or(i64::MAX);
    if interval_sec <= 0 {
        return Ok(None);
    }
    let elapsed_sec = now.signed_duration_since(start_at).num_seconds();
    let current_slot = elapsed_sec.div_euclid(interval_sec);
    let next_slot = db::latest_schedule_slot_index(db_path, timer.id.as_str())?
        .map(|slot| slot + 1)
        .unwrap_or(0);
    if next_slot > current_slot {
        return Ok(None);
    }

    let resolve_due = |slot: i64| {
        let due = slot_due_at(
            timer.id.as_str(),
            start_at,
            interval_sec,
            slot,
            timer.schedule.jitter_sec,
        );
        (due <= now).then_some(DueSlot {
            index: slot,
            scheduled_for: due,
        })
    };

    match timer.schedule.catch_up {
        DispatchTimerCatchUp::Skip => Ok(resolve_due(current_slot)),
        DispatchTimerCatchUp::Coalesce => {
            let mut slot = current_slot;
            while slot >= next_slot {
                if let Some(due) = resolve_due(slot) {
                    return Ok(Some(due));
                }
                if slot == 0 {
                    break;
                }
                slot -= 1;
            }
            Ok(None)
        }
    }
}

fn slot_due_at(
    timer_id: &str,
    start_at: DateTime<Utc>,
    interval_sec: i64,
    slot_index: i64,
    jitter_sec: u64,
) -> DateTime<Utc> {
    let base = start_at + TimeDelta::seconds(interval_sec.saturating_mul(slot_index));
    if jitter_sec == 0 {
        return base;
    }
    let mut hasher = DefaultHasher::new();
    timer_id.hash(&mut hasher);
    slot_index.hash(&mut hasher);
    let jitter = hasher.finish() % (jitter_sec + 1);
    base + TimeDelta::seconds(i64::try_from(jitter).unwrap_or(i64::MAX))
}

#[cfg(test)]
mod tests {
    use chrono::TimeZone;

    use super::*;
    use crate::orchd::dispatch_config::{
        DispatchTimerContextConfig, DispatchTimerDispatchConfig, DispatchTimerRunIssueConfig,
        DispatchTimerScheduleConfig, DispatchTimerTargetConfig, DispatchTimerWorkspaceConfig,
    };
    use forgejo_agent::types::RepoRef;

    fn sample_timer(catch_up: DispatchTimerCatchUp) -> DispatchTimerConfig {
        DispatchTimerConfig {
            id: "doc_scrub".to_string(),
            enabled: true,
            schedule: DispatchTimerScheduleConfig {
                interval_sec: 3600,
                start_at: "2026-02-18T00:00:00Z".to_string(),
                jitter_sec: 0,
                catch_up,
                max_inflight: 1,
            },
            target: DispatchTimerTargetConfig {
                repo: RepoRef::new("main", "forgejo-agent"),
            },
            run_issue: DispatchTimerRunIssueConfig {
                title_template: "doc scrub {{date}}".to_string(),
                body_file: PathBuf::from("/tmp/doc.md"),
                workflow: "ready".to_string(),
                labels: Vec::new(),
            },
            dispatch: DispatchTimerDispatchConfig {
                directive: "investigate".to_string(),
                target_role: "codex-lead".to_string(),
                principal: "codex-orch".to_string(),
            },
            context: DispatchTimerContextConfig {
                mode: DispatchTimerContextMode::Reuse,
                key: "shared".to_string(),
                max_age_sec: None,
                max_runs: None,
                reset_on_failure: true,
                min_context_pct: 50,
                fallback_max_prompt_bytes: 2_000_000,
            },
            workspace: DispatchTimerWorkspaceConfig { cwd: None },
        }
    }

    #[test]
    fn title_template_expands_known_tokens() {
        let timer = sample_timer(DispatchTimerCatchUp::Coalesce);
        let scheduled_for = Utc
            .with_ymd_and_hms(2026, 2, 19, 3, 0, 0)
            .single()
            .expect("time");
        let title = render_run_issue_title(&timer, scheduled_for);
        assert_eq!(title, "doc scrub 2026-02-19");
    }

    #[test]
    fn coalesce_uses_latest_due_slot() {
        let timer = sample_timer(DispatchTimerCatchUp::Coalesce);
        let temp = tempfile::tempdir().expect("tmp");
        let db_path = temp.path().join("orchd.sqlite");
        db::init_db(&db_path).expect("db init");
        let now = Utc
            .with_ymd_and_hms(2026, 2, 18, 5, 15, 0)
            .single()
            .expect("time");
        let due = next_due_slot(&db_path, &timer, now)
            .expect("due")
            .expect("slot");
        assert_eq!(due.index, 5);
    }

    #[test]
    fn skip_uses_current_slot_only() {
        let timer = sample_timer(DispatchTimerCatchUp::Skip);
        let temp = tempfile::tempdir().expect("tmp");
        let db_path = temp.path().join("orchd.sqlite");
        db::init_db(&db_path).expect("db init");
        let now = Utc
            .with_ymd_and_hms(2026, 2, 18, 5, 15, 0)
            .single()
            .expect("time");
        let due = next_due_slot(&db_path, &timer, now)
            .expect("due")
            .expect("slot");
        assert_eq!(due.index, 5);
    }
}
