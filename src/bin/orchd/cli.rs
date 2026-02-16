use std::path::PathBuf;

use clap::{Args, Parser, Subcommand, ValueEnum};

use forgejo_agent::types::{IssueRef, RepoRef};

#[derive(Parser, Debug)]
#[command(name = "orchd")]
#[command(version = env!("FORGEJO_AGENT_BUILD"))]
#[command(about = "Dev-mode reactive orchestrator")]
pub(super) struct Cli {
    #[arg(long)]
    pub(super) config: Option<PathBuf>,
    #[arg(long = "token-file")]
    pub(super) token_file: Option<PathBuf>,
    #[arg(long, default_value = "127.0.0.1:7878")]
    pub(super) listen: String,
    #[arg(long, default_value = "~/.local/state/orchd-dev/orchd.sqlite")]
    pub(super) db_path: String,
    #[arg(long = "webhook-secret-file")]
    pub(super) webhook_secret_file: Option<String>,
    #[arg(long, default_value_t = 20)]
    pub(super) heartbeat_sec: u64,
    #[arg(long, default_value_t = 60)]
    pub(super) reconcile_sec: u64,
    #[arg(long)]
    pub(super) reconcile_repo: Option<RepoRef>,
    #[arg(long, value_enum, default_value_t = DispatchMode::Exec)]
    pub(super) dispatch_mode: DispatchMode,
    #[arg(long, value_enum, default_value_t = DispatchBackend::Systemd)]
    pub(super) dispatch_backend: DispatchBackend,
    #[arg(long, default_value = "config/orchd-dispatch.toml")]
    pub(super) dispatch_config: String,
    #[command(subcommand)]
    pub(super) command: Option<OrchdCommand>,
}

#[derive(Subcommand, Debug)]
pub(super) enum OrchdCommand {
    FinalizeDispatch(Box<FinalizeDispatchArgs>),
    #[command(subcommand)]
    Issue(IssueCommand),
}

#[derive(Subcommand, Debug)]
pub(super) enum IssueCommand {
    Resume(IssueResumeArgs),
}

#[derive(Args, Debug)]
pub(super) struct IssueResumeArgs {
    pub(super) repo: String,
    pub(super) issue_number: u64,
    #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
    pub(super) codex_resume_args: Vec<String>,
}

#[derive(Args, Debug)]
pub(super) struct FinalizeDispatchArgs {
    #[arg(long)]
    pub(super) db_path: PathBuf,
    #[arg(long)]
    pub(super) dispatch_id: i64,
    #[arg(long)]
    pub(super) status: String,
    #[arg(long = "reason-code")]
    pub(super) reason_code: String,
    #[arg(long = "exit-code")]
    pub(super) exit_code: i64,
    #[arg(long = "session-id", default_value = "")]
    pub(super) session_id: String,
    #[arg(long = "issue-ref")]
    pub(super) issue_ref: IssueRef,
    #[arg(long = "issue-title")]
    pub(super) issue_title: String,
    #[arg(long = "issue-url")]
    pub(super) issue_url: String,
    #[arg(long)]
    pub(super) directive: String,
    #[arg(long = "role-name")]
    pub(super) role_name: String,
    #[arg(long = "run-dir")]
    pub(super) run_dir: PathBuf,
    #[arg(long = "log-file")]
    pub(super) log_file: PathBuf,
    #[arg(long = "completion-file")]
    pub(super) completion_file: PathBuf,
    #[arg(long = "git-workdir")]
    pub(super) git_workdir: PathBuf,
    #[arg(long = "git-remote", default_value = "origin")]
    pub(super) git_remote: String,
    #[arg(long = "git-base", default_value = "main")]
    pub(super) git_base: String,
    #[arg(long = "git-branch", default_value = "")]
    pub(super) git_branch: String,
    #[arg(long = "forgejoctl-bin")]
    pub(super) forgejoctl_bin: PathBuf,
    #[arg(long = "forgejo-config")]
    pub(super) forgejo_config: Option<PathBuf>,
    #[arg(long = "token-file")]
    pub(super) token_file: PathBuf,
    #[arg(long = "principal-workdir")]
    pub(super) principal_workdir: Option<PathBuf>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, ValueEnum)]
pub(super) enum DispatchMode {
    DryRun,
    Exec,
}

impl DispatchMode {
    pub(super) const fn as_str(self) -> &'static str {
        match self {
            Self::DryRun => "dry-run",
            Self::Exec => "exec",
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, ValueEnum)]
pub(super) enum DispatchBackend {
    Systemd,
    Local,
}

impl DispatchBackend {
    pub(super) const fn as_str(self) -> &'static str {
        match self {
            Self::Systemd => "systemd",
            Self::Local => "local",
        }
    }
}
