use std::fmt::Write as _;
use std::fs;

use anyhow::{Context, Result, anyhow};

use forgejo_agent::api::ForgejoClient;
use forgejo_agent::config::AgentConfig;

use super::cli::{Cli, PromptMode, PromptPreviewArgs};
use super::db;
use super::dispatch::render_fresh_preamble;
use super::dispatch_config::load_dispatch_config;
use super::dispatch_config_live::DispatchConfigHandle;
use super::paths::{expand_tilde_path, resolve_dispatch_config_path};
use super::reading_material;
use super::repo;
use super::state::AppState;
use super::template;

const PREVIEW_HISTORY_PLACEHOLDER: &str = "(prompt preview does not include conversation history)";
const PREVIEW_DELTA_PLACEHOLDER: &str = "(prompt preview does not include delta summary)";

fn truncate_preview_bytes(text: &str, byte_cap: usize) -> (String, bool) {
    if text.len() <= byte_cap {
        return (text.to_string(), false);
    }
    let mut end = byte_cap;
    while end > 0 && !text.is_char_boundary(end) {
        end -= 1;
    }
    (text[..end].to_string(), true)
}

fn apply_preview_caps(
    rendered: String,
    rows_truncated: bool,
    row_cap: usize,
    byte_cap: usize,
) -> String {
    let (mut capped, bytes_truncated) = truncate_preview_bytes(&rendered, byte_cap);
    if rows_truncated {
        write!(
            capped,
            "\n\n(note: preview row cap hit at {row_cap} events; raise --preview-row-cap for more)"
        )
        .expect("writing to String should not fail");
    }
    if bytes_truncated {
        write!(
            capped,
            "\n\n(note: preview byte cap hit at {byte_cap} bytes; raise --preview-byte-cap for more)"
        )
        .expect("writing to String should not fail");
    }
    capped
}

fn render_preview_issue_activity(
    state: &AppState,
    args: &PromptPreviewArgs,
    role_name: &str,
) -> Result<String> {
    let previous_event_cursor = if args.mode == PromptMode::Followup {
        db::issue_role_cursor_event_id(
            &state.db_path,
            &args.issue_ref.repo.to_string(),
            args.issue_ref.number,
            role_name,
        )?
    } else {
        None
    };
    let current_event_id = db::latest_event_id(&state.db_path)?;
    let query_limit = args.preview_row_cap.saturating_add(1);
    let mut rows = db::issue_delta_rows(
        &state.db_path,
        &args.issue_ref.repo.to_string(),
        args.issue_ref.number,
        previous_event_cursor,
        current_event_id,
        query_limit,
    )?;
    let rows_truncated = rows.len() > args.preview_row_cap;
    if rows_truncated {
        rows.truncate(args.preview_row_cap);
    }

    let rendered = if args.mode == PromptMode::Fresh {
        db::render_issue_history(&rows, rows.len().saturating_add(1))
    } else {
        db::summarize_issue_delta(&rows)
    };
    Ok(apply_preview_caps(
        rendered,
        rows_truncated,
        args.preview_row_cap,
        args.preview_byte_cap,
    ))
}

pub(super) fn prompt_preview_command(cli: &Cli, args: PromptPreviewArgs) -> Result<()> {
    let dispatch_config_path = resolve_dispatch_config_path(&cli.dispatch_config)?;
    let dispatch_config = load_dispatch_config(&dispatch_config_path)?;

    let role_name = args.role.trim().to_ascii_lowercase();
    let directive_name = args.directive.trim().to_ascii_lowercase();
    let role_cfg = dispatch_config
        .roles
        .get(&role_name)
        .ok_or_else(|| anyhow!("role not configured: {role_name}"))?;
    let directive_cfg = dispatch_config
        .directives
        .get(&directive_name)
        .ok_or_else(|| anyhow!("directive not configured: {directive_name}"))?;

    let cfg = AgentConfig::load(cli.config.clone(), cli.token_file.clone())?;
    let api = ForgejoClient::new(&cfg)?;
    let issue = api.get_issue(&cfg, &args.issue_ref)?;
    let issue_title = issue.title;
    let issue_body = issue.body.unwrap_or_default();

    let db_path = expand_tilde_path(&cli.db_path)?;
    let reconcile_repo = args.issue_ref.repo.clone();
    let state = AppState {
        db_path,
        webhook_secret: None,
        webhook_url: String::new(),
        cfg,
        forgejo_config_file: None,
        reconcile_repo,
        dispatch_mode: cli.dispatch_mode,
        dispatch_backend: cli.dispatch_backend,
        dispatch_config: DispatchConfigHandle::Disabled,
    };

    let repo_full_name = args.issue_ref.repo.to_string();
    let repo_root = repo::ensure_repo_checkout(&state, role_cfg, &repo_full_name)?;

    let prompt_mode = args.mode.as_str();

    let orders_template = fs::read_to_string(&directive_cfg.prompt_file).with_context(|| {
        format!(
            "failed reading prompt {}",
            directive_cfg.prompt_file.display()
        )
    })?;
    let orders_md = template::render_prompt(&orders_template, &[])?;

    let preamble_md = if args.mode == PromptMode::Fresh {
        render_fresh_preamble(
            &dispatch_config.prompt_envelopes,
            &dispatch_config.rank_acl,
            &role_name,
        )?
    } else {
        String::new()
    };

    let issue_ref = args.issue_ref.to_string();
    let turn_type = match args.mode {
        PromptMode::Fresh => "first turn in this issue",
        PromptMode::Followup => "follow-up turn in an existing issue session",
    };
    let dispatch_md = template::render_prompt_file(
        &dispatch_config.prompt_envelopes.turn_context_file,
        &[
            ("actor", "preview"),
            ("issue_ref", &issue_ref),
            ("turn_type", turn_type),
            ("trigger", "manual prompt preview"),
        ],
        "turn context",
    )?;

    let issue_md = match args.mode {
        PromptMode::Fresh => {
            let issue_history = if args.with_history {
                render_preview_issue_activity(&state, &args, &role_name)?
            } else {
                PREVIEW_HISTORY_PLACEHOLDER.to_string()
            };
            template::render_prompt_file(
                &dispatch_config.prompt_envelopes.issue_fresh_file,
                &[
                    ("issue_title", issue_title.as_str()),
                    (
                        "issue_body",
                        if issue_body.trim().is_empty() {
                            "(empty)"
                        } else {
                            issue_body.as_str()
                        },
                    ),
                    ("issue_history", issue_history.as_str()),
                ],
                "issue fresh",
            )?
        }
        PromptMode::Followup => {
            let issue_delta = if args.with_delta {
                render_preview_issue_activity(&state, &args, &role_name)?
            } else {
                PREVIEW_DELTA_PLACEHOLDER.to_string()
            };
            template::render_prompt_file(
                &dispatch_config.prompt_envelopes.issue_followup_file,
                &[
                    ("issue_title", issue_title.as_str()),
                    ("issue_delta", issue_delta.as_str()),
                ],
                "issue followup",
            )?
        }
    };

    let reading_outcome = reading_material::build_reading_material(
        &dispatch_config.reading_material,
        &role_name,
        &directive_name,
        prompt_mode,
        &repo_root,
        &dispatch_config.repo_bindings,
    );
    if args.json {
        println!(
            "{}",
            serde_json::to_string_pretty(&reading_outcome.doc_plan)?
        );
        return Ok(());
    }

    let reading_material_md = reading_outcome.markdown;
    let envelope_path = match args.mode {
        PromptMode::Fresh => &dispatch_config.prompt_envelopes.fresh_envelope,
        PromptMode::Followup => &dispatch_config.prompt_envelopes.followup_envelope,
    };
    let envelope_template = fs::read_to_string(envelope_path)
        .with_context(|| format!("failed reading envelope {}", envelope_path.display()))?;
    let prompt = template::render_prompt(
        &envelope_template,
        &[
            ("preamble_md", &preamble_md),
            ("dispatch_md", &dispatch_md),
            ("reading_material_md", &reading_material_md),
            ("orders_md", &orders_md),
            ("issue_md", &issue_md),
        ],
    )?;
    print!("{prompt}");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{apply_preview_caps, truncate_preview_bytes};

    #[test]
    fn truncate_preview_bytes_obeys_utf8_boundaries() {
        let (truncated, clipped) = truncate_preview_bytes("ab\u{00e9}", 3);
        assert!(clipped);
        assert_eq!(truncated, "ab");
    }

    #[test]
    fn apply_preview_caps_appends_truncation_notes() {
        let rendered = "0123456789".to_string();
        let capped = apply_preview_caps(rendered, true, 3, 5);
        assert!(capped.starts_with("01234"));
        assert!(capped.contains("preview row cap hit at 3 events"));
        assert!(capped.contains("preview byte cap hit at 5 bytes"));
    }

    #[test]
    fn apply_preview_caps_keeps_short_text_unchanged() {
        let rendered = "short text".to_string();
        assert_eq!(
            apply_preview_caps(rendered.clone(), false, 10, 64),
            rendered
        );
    }
}
