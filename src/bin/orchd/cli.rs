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
    #[arg(long, default_value_t = 5)]
    pub(super) dispatch_config_reload_sec: u64,
    #[arg(long, default_value_t = false)]
    pub(super) skip_startup_role_check: bool,
    #[command(subcommand)]
    pub(super) command: Option<OrchdCommand>,
}

#[derive(Subcommand, Debug)]
pub(super) enum OrchdCommand {
    FinalizeDispatch(Box<FinalizeDispatchArgs>),
    RunDispatch(RunDispatchArgs),
    #[command(subcommand)]
    Obs(ObsCommand),
    #[command(subcommand)]
    Role(RoleCommand),
    #[command(subcommand)]
    Schedule(ScheduleCommand),
}

#[derive(Subcommand, Debug)]
pub(super) enum ObsCommand {
    #[command(subcommand)]
    Prompt(PromptCommand),
    #[command(subcommand)]
    Issue(IssueCommand),
    #[command(subcommand)]
    Timer(TimerCommand),
}

#[derive(Subcommand, Debug)]
pub(super) enum PromptCommand {
    Preview(PromptPreviewArgs),
}

#[derive(Subcommand, Debug)]
pub(super) enum IssueCommand {
    Sessions(IssueSessionsArgs),
    Resume(IssueResumeArgs),
}

#[derive(Subcommand, Debug)]
pub(super) enum TimerCommand {
    Resume(TimerResumeArgs),
}

#[derive(Subcommand, Debug)]
pub(super) enum RoleCommand {
    List(RoleListArgs),
    Check(RoleCheckArgs),
    Add(Box<RoleAddArgs>),
}

#[derive(Subcommand, Debug)]
pub(super) enum ScheduleCommand {
    Tick(ScheduleTickArgs),
}

#[derive(Args, Debug)]
pub(super) struct ScheduleTickArgs {
    #[arg(long)]
    pub(super) timer: Option<String>,
}

#[derive(Args, Debug)]
pub(super) struct IssueSessionsArgs {
    pub(super) repo: String,
    pub(super) issue_number: u64,
    #[arg(long)]
    pub(super) role: Option<String>,
    #[arg(long)]
    pub(super) json: bool,
}

#[derive(Args, Debug)]
pub(super) struct IssueResumeArgs {
    pub(super) repo: String,
    pub(super) issue_number: u64,
    #[arg(long)]
    pub(super) role: Option<String>,
    #[arg(long = "dispatch-id")]
    pub(super) dispatch_id: Option<i64>,
    #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
    pub(super) codex_resume_args: Vec<String>,
}

#[derive(Args, Debug)]
pub(super) struct TimerResumeArgs {
    pub(super) timer_id: String,
    #[arg(long)]
    pub(super) role: Option<String>,
    #[arg(long = "dispatch-id")]
    pub(super) dispatch_id: Option<i64>,
    #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
    pub(super) codex_resume_args: Vec<String>,
}

#[derive(Args, Debug)]
pub(super) struct RoleListArgs {
    #[arg(long)]
    pub(super) json: bool,
}

#[derive(Args, Debug)]
pub(super) struct RoleCheckArgs {
    #[arg(long)]
    pub(super) role: Option<String>,
    #[arg(long)]
    pub(super) json: bool,
    /// Skip Forgejo API calls; only validate local invariants (role cards, token files, perms).
    #[arg(long)]
    pub(super) offline: bool,
}

#[derive(Args, Debug)]
#[allow(clippy::struct_excessive_bools)]
pub(super) struct RoleAddArgs {
    #[arg(long)]
    pub(super) role: String,
    #[arg(long)]
    pub(super) rank: String,
    #[arg(long = "forgejo-login")]
    pub(super) forgejo_login: String,
    #[arg(long = "codex-role-arg")]
    pub(super) codex_role_arg: Option<String>,
    #[arg(long = "token-file")]
    pub(super) token_file: Option<PathBuf>,
    #[arg(long = "codex-bin")]
    pub(super) codex_bin: Option<PathBuf>,
    #[arg(long = "can-dispatch")]
    pub(super) can_dispatch: bool,
    #[arg(long = "create-user")]
    pub(super) create_user: bool,
    #[arg(long = "rotate-token")]
    pub(super) rotate_token: bool,
    #[arg(long = "admin-token-file")]
    pub(super) admin_token_file: Option<PathBuf>,
    #[arg(long = "scream-repo", default_value = "main/forgejo-work")]
    pub(super) scream_repo: RepoRef,
    #[arg(long = "scream-permission", default_value = "write")]
    pub(super) scream_permission: String,
    #[arg(long)]
    pub(super) dry_run: bool,
    #[arg(long)]
    pub(super) json: bool,
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

    // Optional sidecar checkout that must land before the primary repo (used for paired
    // forgejo-agent + swarm updates). When present, all fields should be populated.
    #[arg(long = "sidecar-repo")]
    pub(super) sidecar_repo: Option<RepoRef>,
    #[arg(long = "sidecar-git-workdir")]
    pub(super) sidecar_git_workdir: Option<PathBuf>,
    #[arg(long = "sidecar-git-remote")]
    pub(super) sidecar_git_remote: Option<String>,
    #[arg(long = "sidecar-git-base")]
    pub(super) sidecar_git_base: Option<String>,
    #[arg(long = "sidecar-git-branch")]
    pub(super) sidecar_git_branch: Option<String>,
    #[arg(long = "sidecar-principal-workdir")]
    pub(super) sidecar_principal_workdir: Option<PathBuf>,
}

#[derive(Args, Debug)]
pub(super) struct RunDispatchArgs {
    #[arg(long)]
    pub(super) spec: PathBuf,
}

#[derive(Args, Debug)]
pub(super) struct PromptPreviewArgs {
    pub(super) issue_ref: IssueRef,
    #[arg(long)]
    pub(super) role: String,
    #[arg(long)]
    pub(super) directive: String,
    #[arg(long, value_enum, default_value_t = PromptMode::Fresh)]
    pub(super) mode: PromptMode,
    #[arg(long)]
    pub(super) with_history: bool,
    #[arg(long)]
    pub(super) with_delta: bool,
    #[arg(long = "preview-row-cap", default_value_t = 120)]
    pub(super) preview_row_cap: usize,
    #[arg(long = "preview-byte-cap", default_value_t = 12_000)]
    pub(super) preview_byte_cap: usize,
    #[arg(long)]
    pub(super) json: bool,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, ValueEnum)]
pub(super) enum PromptMode {
    Fresh,
    Followup,
}

impl PromptMode {
    pub(super) const fn as_str(self) -> &'static str {
        match self {
            Self::Fresh => "fresh",
            Self::Followup => "followup",
        }
    }
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
