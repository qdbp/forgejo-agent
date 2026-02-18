use anyhow::{Context, Result};
use clap::Parser;

use super::cli::{Cli, IssueCommand, OrchdCommand, PromptCommand, RoleCommand};
use super::finalize;
use super::issue;
use super::prompt;
use super::role;
use super::run_dispatch;
use super::server;
use super::telemetry::init_telemetry;

pub(super) fn run_entry() -> Result<()> {
    init_telemetry();
    let mut cli = Cli::parse();
    if let Some(command) = cli.command.take() {
        // Subcommands are intentionally synchronous to avoid mixing reqwest::blocking
        // with a Tokio runtime.
        return run_command(&cli, command);
    }
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .context("failed to build tokio runtime")?;
    runtime.block_on(server::run_server(cli))
}

fn run_command(cli: &Cli, command: OrchdCommand) -> Result<()> {
    match command {
        OrchdCommand::FinalizeDispatch(args) => finalize::finalize_dispatch_command(*args),
        OrchdCommand::RunDispatch(args) => run_dispatch::run_dispatch_command(args),
        OrchdCommand::Prompt(command) => run_prompt_command(cli, command),
        OrchdCommand::Issue(command) => run_issue_command(cli, command),
        OrchdCommand::Role(command) => run_role_command(cli, command),
    }
}

fn run_prompt_command(cli: &Cli, command: PromptCommand) -> Result<()> {
    match command {
        PromptCommand::Preview(args) => prompt::prompt_preview_command(cli, args),
    }
}

fn run_issue_command(cli: &Cli, command: IssueCommand) -> Result<()> {
    match command {
        IssueCommand::Sessions(args) => issue::issue_sessions_command(&cli.db_path, args),
        IssueCommand::Resume(args) => issue::issue_resume_command(&cli.db_path, args),
    }
}

fn run_role_command(cli: &Cli, command: RoleCommand) -> Result<()> {
    match command {
        RoleCommand::List(args) => role::role_list_command(cli, args),
        RoleCommand::Check(args) => role::role_check_command(cli, args),
        RoleCommand::Add(args) => role::role_add_command(cli, *args),
    }
}
