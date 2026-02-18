use std::fs;

use anyhow::{Context, Result, anyhow};

use forgejo_agent::api::ForgejoClient;
use forgejo_agent::config::AgentConfig;

use super::cli::{Cli, PromptMode, PromptPreviewArgs};
use super::dispatch::render_fresh_preamble;
use super::dispatch_config::load_dispatch_config;
use super::paths::{expand_tilde_path, resolve_dispatch_config_path};
use super::reading_material;
use super::repo;
use super::state::AppState;
use super::template;

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
        dispatch_config: None,
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
        PromptMode::Fresh => template::render_prompt_file(
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
                (
                    "issue_history",
                    "(prompt preview does not include conversation history)",
                ),
            ],
            "issue fresh",
        )?,
        PromptMode::Followup => template::render_prompt_file(
            &dispatch_config.prompt_envelopes.issue_followup_file,
            &[
                ("issue_title", issue_title.as_str()),
                (
                    "issue_delta",
                    "(prompt preview does not include delta summary)",
                ),
            ],
            "issue followup",
        )?,
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
