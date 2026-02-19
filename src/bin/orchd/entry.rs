use anyhow::{Context, Result};
use clap::Parser;

use super::cli::{
    Cli, DeployCommand, IssueCommand, ObsCommand, OrchdCommand, PromptCommand, RoleCommand,
    ScheduleCommand, TimerCommand,
};
use super::deploy;
use super::finalize;
use super::issue;
use super::migrations;
use super::prompt;
use super::role;
use super::run_dispatch;
use super::schedule;
use super::server;
use super::telemetry::init_telemetry;
use super::timer;

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
        OrchdCommand::SchemaContract(args) => {
            let (latest, min_compatible) = migrations::schema_contract();
            if args.json {
                println!(
                    "{}",
                    serde_json::json!({
                        "latest": latest,
                        "min_compatible": min_compatible,
                    })
                );
            } else {
                println!("latest={latest}");
                println!("min_compatible={min_compatible}");
            }
            Ok(())
        }
        OrchdCommand::Obs(command) => run_obs_command(cli, command),
        OrchdCommand::Role(command) => run_role_command(cli, command),
        OrchdCommand::Schedule(command) => run_schedule_command(cli, command),
        OrchdCommand::Deploy(command) => run_deploy_command(cli, command),
    }
}

fn run_obs_command(cli: &Cli, command: ObsCommand) -> Result<()> {
    match command {
        ObsCommand::Prompt(command) => run_prompt_command(cli, command),
        ObsCommand::Issue(command) => run_issue_command(cli, command),
        ObsCommand::Timer(command) => run_timer_command(cli, command),
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

fn run_timer_command(cli: &Cli, command: TimerCommand) -> Result<()> {
    match command {
        TimerCommand::Sessions(args) => timer::timer_sessions_command(&cli.db_path, args),
        TimerCommand::Resume(args) => timer::timer_resume_command(&cli.db_path, args),
    }
}

fn run_role_command(cli: &Cli, command: RoleCommand) -> Result<()> {
    match command {
        RoleCommand::List(args) => role::role_list_command(cli, args),
        RoleCommand::Check(args) => role::role_check_command(cli, args),
        RoleCommand::Add(args) => role::role_add_command(cli, *args),
    }
}

fn run_schedule_command(cli: &Cli, command: ScheduleCommand) -> Result<()> {
    match command {
        ScheduleCommand::List(args) => schedule::schedule_list_command(cli, args),
        ScheduleCommand::Tick(args) => schedule::schedule_tick_command(cli, args),
    }
}

fn run_deploy_command(cli: &Cli, command: DeployCommand) -> Result<()> {
    match command {
        DeployCommand::Worker(args) => deploy::deploy_worker_command(cli, args),
    }
}
