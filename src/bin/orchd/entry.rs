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
