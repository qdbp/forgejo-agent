use std::collections::HashSet;
use std::fmt::Write as _;
use std::fs;
use std::io::{self, Read};
use std::path::PathBuf;
use std::process::Command;
use std::thread;
use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail};
use chrono::{Duration as ChronoDuration, Utc};
use clap::{Args, Parser, Subcommand};

use forgejo_agent::api::ForgejoClient;
use forgejo_agent::config::AgentConfig;
use forgejo_agent::policy;
use forgejo_agent::policy::{
    ORCHD_CONTROL_LABELS, ORCHD_STATE_LABELS, OTHER_LABELS, STATE_LABEL_COLOR,
    is_orchd_failure_label, orchd_failure_label,
};
use forgejo_agent::types::{
    ApiIssue, IssueRef, ListState, OpenState, OrchdRuntimeState, RepoRef, WorkflowState,
};

#[derive(Parser, Debug)]
#[command(name = "forgejo-agent")]
#[command(version = env!("FORGEJO_AGENT_BUILD"))]
#[command(about = "Typed Forgejo control plane for agent swarms")]
struct Cli {
    #[arg(long)]
    config: Option<PathBuf>,
    #[arg(long = "token-file")]
    token_file: Option<PathBuf>,
    #[command(subcommand)]
    command: TopCommand,
}

#[derive(Subcommand, Debug)]
enum TopCommand {
    Repo {
        #[command(subcommand)]
        command: RepoCommand,
    },
    Issue {
        #[command(subcommand)]
        command: IssueCommand,
    },
    Worker {
        #[command(subcommand)]
        command: WorkerCommand,
    },
    Whoami {
        #[arg(long)]
        json: bool,
    },
}

#[derive(Subcommand, Debug)]
enum RepoCommand {
    Ensure(RepoEnsureArgs),
}

#[derive(Args, Debug)]
struct RepoEnsureArgs {
    repo: Option<RepoRef>,
}

#[derive(Subcommand, Debug)]
enum IssueCommand {
    List(IssueListArgs),
    Show(IssueShowArgs),
    Create(IssueCreateArgs),
    Edit(IssueEditArgs),
    Comment(IssueCommentArgs),
    OrchdState(IssueOrchdStateArgs),
    Transition(IssueTransitionArgs),
    Assign(IssueAssignArgs),
    Claim(IssueClaimArgs),
    Release(IssueReleaseArgs),
    Blocker(IssueBlockerArgs),
    Close(IssueCloseArgs),
    Reopen(IssueReopenArgs),
}

#[derive(Args, Debug)]
struct IssueListArgs {
    repo: Option<RepoRef>,
    #[arg(long, default_value = "open")]
    state: ListState,
    #[arg(long, default_value_t = 100)]
    limit: u32,
    #[arg(long = "label")]
    labels: Vec<String>,
    #[arg(long)]
    json: bool,
}

#[derive(Args, Debug)]
struct IssueShowArgs {
    issue: IssueRef,
    #[arg(long)]
    json: bool,
}

#[derive(Args, Debug)]
struct IssueCreateArgs {
    repo: Option<RepoRef>,
    #[arg(long)]
    title: String,
    #[arg(long, conflicts_with_all = ["body_file", "body_stdin"])]
    body: Option<String>,
    #[arg(long = "body-file", conflicts_with_all = ["body", "body_stdin"])]
    body_file: Option<PathBuf>,
    #[arg(long = "body-stdin", conflicts_with_all = ["body", "body_file"])]
    body_stdin: bool,
    #[arg(long = "label")]
    labels: Vec<String>,
    #[arg(long, default_value = "triage")]
    workflow: WorkflowState,
    #[arg(long)]
    json: bool,
}

#[derive(Args, Debug)]
struct IssueEditArgs {
    issue: IssueRef,
    #[arg(long)]
    title: Option<String>,
    #[arg(long, conflicts_with_all = ["body_file", "body_stdin"])]
    body: Option<String>,
    #[arg(long = "body-file", conflicts_with_all = ["body", "body_stdin"])]
    body_file: Option<PathBuf>,
    #[arg(long = "body-stdin", conflicts_with_all = ["body", "body_file"])]
    body_stdin: bool,
    #[arg(long)]
    json: bool,
}

#[derive(Args, Debug)]
struct IssueCommentArgs {
    issue: IssueRef,
    #[arg(long, conflicts_with_all = ["body_file", "body_stdin"])]
    body: Option<String>,
    #[arg(long = "body-file", conflicts_with_all = ["body", "body_stdin"])]
    body_file: Option<PathBuf>,
    #[arg(long = "body-stdin", conflicts_with_all = ["body", "body_file"])]
    body_stdin: bool,
}

#[derive(Args, Debug)]
struct IssueTransitionArgs {
    issue: IssueRef,
    #[arg(long)]
    to: WorkflowState,
    #[arg(long)]
    force: bool,
}

#[derive(Args, Debug)]
struct IssueOrchdStateArgs {
    issue: IssueRef,
    #[arg(long)]
    to: OrchdRuntimeState,
    #[arg(long = "reason-code")]
    reason_code: Option<String>,
}

#[derive(Args, Debug)]
struct IssueAssignArgs {
    issue: IssueRef,
    #[arg(long, conflicts_with_all = ["self_assign", "clear"])]
    to: Option<String>,
    #[arg(long = "self", conflicts_with_all = ["to", "clear"])]
    self_assign: bool,
    #[arg(long, conflicts_with_all = ["to", "self_assign"])]
    clear: bool,
    #[arg(long)]
    json: bool,
}

#[derive(Args, Debug)]
struct IssueClaimArgs {
    issue: IssueRef,
    #[arg(long)]
    agent: Option<String>,
    #[arg(long = "ttl-min")]
    ttl_min: Option<i64>,
}

#[derive(Args, Debug)]
struct IssueReleaseArgs {
    issue: IssueRef,
    #[arg(long)]
    agent: Option<String>,
}

#[derive(Args, Debug)]
struct IssueBlockerArgs {
    issue: IssueRef,
    #[arg(long)]
    title: String,
    #[arg(long)]
    body: Option<String>,
}

#[derive(Args, Debug)]
struct IssueCloseArgs {
    issue: IssueRef,
    #[arg(long)]
    force: bool,
}

#[derive(Args, Debug)]
struct IssueReopenArgs {
    issue: IssueRef,
    #[arg(long, default_value = "triage")]
    workflow: WorkflowState,
}

#[derive(Subcommand, Debug)]
enum WorkerCommand {
    Run(WorkerRunArgs),
}

#[derive(Args, Debug)]
struct WorkerRunArgs {
    #[arg(long)]
    repo: Option<RepoRef>,
    #[arg(long)]
    workdir: Option<PathBuf>,
    #[arg(long)]
    agent: Option<String>,
    #[arg(long, default_value_t = 60)]
    interval_sec: u64,
    #[arg(long)]
    execute: bool,
    #[arg(long)]
    once: bool,
    #[arg(long)]
    close_on_success: bool,
}

fn main() {
    if let Err(err) = run() {
        eprintln!("error: {err:#}");
        std::process::exit(1);
    }
}

fn run() -> Result<()> {
    let cli = Cli::parse();
    let cfg = AgentConfig::load(cli.config, cli.token_file)?;
    let api = ForgejoClient::new(&cfg)?;

    match cli.command {
        TopCommand::Whoami { json } => {
            let who = api.whoami(&cfg)?;
            if json {
                println!("{}", serde_json::to_string_pretty(&who)?);
            } else {
                let login = who
                    .get("login")
                    .and_then(serde_json::Value::as_str)
                    .unwrap_or("<unknown>");
                let email = who
                    .get("email")
                    .and_then(serde_json::Value::as_str)
                    .unwrap_or("");
                println!("login: {login}");
                println!("email: {email}");
            }
        }
        TopCommand::Repo { command } => match command {
            RepoCommand::Ensure(args) => cmd_repo_ensure(&api, &cfg, args)?,
        },
        TopCommand::Issue { command } => match command {
            IssueCommand::List(args) => cmd_issue_list(&api, &cfg, args)?,
            IssueCommand::Show(args) => cmd_issue_show(&api, &cfg, args)?,
            IssueCommand::Create(args) => cmd_issue_create(&api, &cfg, args)?,
            IssueCommand::Edit(args) => cmd_issue_edit(&api, &cfg, args)?,
            IssueCommand::Comment(args) => cmd_issue_comment(&api, &cfg, args)?,
            IssueCommand::OrchdState(args) => cmd_issue_orchd_state(&api, &cfg, args)?,
            IssueCommand::Transition(args) => cmd_issue_transition(&api, &cfg, args)?,
            IssueCommand::Assign(args) => cmd_issue_assign(&api, &cfg, args)?,
            IssueCommand::Claim(args) => cmd_issue_claim(&api, &cfg, args)?,
            IssueCommand::Release(args) => cmd_issue_release(&api, &cfg, args)?,
            IssueCommand::Blocker(args) => cmd_issue_blocker(&api, &cfg, args)?,
            IssueCommand::Close(args) => cmd_issue_close(&api, &cfg, args)?,
            IssueCommand::Reopen(args) => cmd_issue_reopen(&api, &cfg, args)?,
        },
        TopCommand::Worker { command } => match command {
            WorkerCommand::Run(args) => cmd_worker_run(&api, &cfg, args)?,
        },
    }

    Ok(())
}

fn default_repo(repo: Option<RepoRef>, cfg: &AgentConfig) -> RepoRef {
    repo.unwrap_or_else(|| cfg.default_repo.clone())
}

fn ensure_policy_labels(api: &ForgejoClient, cfg: &AgentConfig, repo: &RepoRef) -> Result<()> {
    for (name, color, description, exclusive) in STATE_LABEL_COLOR {
        api.ensure_label(cfg, repo, name, color, description, exclusive)?;
    }
    for (name, color, description, exclusive) in OTHER_LABELS {
        api.ensure_label(cfg, repo, name, color, description, exclusive)?;
    }
    for (name, color, description, exclusive) in ORCHD_STATE_LABELS {
        api.ensure_label(cfg, repo, name, color, description, exclusive)?;
    }
    for (name, color, description, exclusive) in ORCHD_CONTROL_LABELS {
        api.ensure_label(cfg, repo, name, color, description, exclusive)?;
    }
    Ok(())
}

fn cmd_repo_ensure(api: &ForgejoClient, cfg: &AgentConfig, args: RepoEnsureArgs) -> Result<()> {
    let repo = default_repo(args.repo, cfg);
    api.ensure_repo(cfg, &repo, "Agent control queue")?;
    ensure_policy_labels(api, cfg, &repo)?;
    println!("repo ensured: {repo}");
    Ok(())
}

fn is_workflow_label(name: &str) -> bool {
    WorkflowState::from_label(name).is_some()
}

fn is_orchd_state_label(name: &str) -> bool {
    OrchdRuntimeState::from_label(name).is_some()
}

fn ensure_issue_state(
    api: &ForgejoClient,
    cfg: &AgentConfig,
    issue_ref: &IssueRef,
    target: WorkflowState,
) -> Result<()> {
    let issue = api.get_issue(cfg, issue_ref)?;
    let labels = api.list_labels(cfg, &issue_ref.repo)?;

    let target_id = if let Some(label) = labels.iter().find(|label| label.name == target.label()) {
        label.id
    } else {
        api.ensure_label(
            cfg,
            &issue_ref.repo,
            target.label(),
            "5319e7",
            "workflow state",
            true,
        )?
        .id
    };

    let mut has_target = false;
    for label in issue.labels {
        if is_workflow_label(&label.name) {
            if label.name == target.label() {
                has_target = true;
            } else {
                api.remove_issue_label(cfg, issue_ref, label.id)?;
            }
        }
    }

    if !has_target {
        api.add_issue_label_ids(cfg, issue_ref, vec![target_id])?;
    }
    Ok(())
}

fn orchd_state_label_meta(state: OrchdRuntimeState) -> (&'static str, &'static str, bool) {
    ORCHD_STATE_LABELS
        .iter()
        .find_map(|(name, color, description, exclusive)| {
            if *name == state.label() {
                Some((*color, *description, *exclusive))
            } else {
                None
            }
        })
        .unwrap_or(("5319e7", "orchd runtime state", true))
}

fn set_issue_orchd_state(
    api: &ForgejoClient,
    cfg: &AgentConfig,
    issue_ref: &IssueRef,
    target: OrchdRuntimeState,
    reason_code: Option<&str>,
) -> Result<Option<OrchdRuntimeState>> {
    ensure_policy_labels(api, cfg, &issue_ref.repo)?;
    let issue = api.get_issue(cfg, issue_ref)?;

    let previous = issue
        .labels
        .iter()
        .find_map(|label| OrchdRuntimeState::from_label(&label.name));

    let (color, description, exclusive) = orchd_state_label_meta(target);
    let target_id = api
        .ensure_label(
            cfg,
            &issue_ref.repo,
            target.label(),
            color,
            description,
            exclusive,
        )?
        .id;

    let failure_reason_label_id = if target == OrchdRuntimeState::Failed {
        if let Some(reason_label_name) = reason_code.and_then(orchd_failure_label) {
            Some(
                api.ensure_label(
                    cfg,
                    &issue_ref.repo,
                    &reason_label_name,
                    "8a1c2d",
                    "dispatch failed for this reason",
                    false,
                )?
                .id,
            )
        } else {
            None
        }
    } else {
        None
    };

    let mut replacement_ids = issue
        .labels
        .iter()
        .filter(|label| !is_orchd_state_label(&label.name) && !is_orchd_failure_label(&label.name))
        .map(|label| label.id)
        .collect::<Vec<_>>();
    replacement_ids.push(target_id);
    if let Some(reason_label_id) = failure_reason_label_id {
        replacement_ids.push(reason_label_id);
    }
    replacement_ids.sort_unstable();
    replacement_ids.dedup();

    api.replace_issue_label_ids(cfg, issue_ref, replacement_ids)?;
    Ok(previous)
}

fn ensure_labels_exist(
    api: &ForgejoClient,
    cfg: &AgentConfig,
    repo: &RepoRef,
    names: &[String],
) -> Result<Vec<u64>> {
    let mut labels = api.list_labels(cfg, repo)?;
    let mut ids = Vec::new();

    for name in names {
        let id = if let Some(label) = labels.iter().find(|label| label.name == *name) {
            label.id
        } else {
            let created = api.ensure_label(cfg, repo, name, "5319e7", "custom label", false)?;
            labels.push(created.clone());
            created.id
        };
        ids.push(id);
    }

    Ok(ids)
}

fn join_label_names(issue: &ApiIssue) -> String {
    let mut names: Vec<&str> = issue
        .labels
        .iter()
        .map(|label| label.name.as_str())
        .collect();
    names.sort_unstable();
    names.join(",")
}

fn cmd_issue_list(api: &ForgejoClient, cfg: &AgentConfig, args: IssueListArgs) -> Result<()> {
    let repo = default_repo(args.repo, cfg);
    let mut issues = api.list_issues(cfg, &repo, &args.state.to_string(), args.limit)?;
    issues.retain(|issue| issue.pull_request.is_none());

    if !args.labels.is_empty() {
        let wanted: HashSet<&str> = args.labels.iter().map(String::as_str).collect();
        issues.retain(|issue| {
            let labels: HashSet<&str> = issue
                .labels
                .iter()
                .map(|label| label.name.as_str())
                .collect();
            wanted.iter().all(|name| labels.contains(name))
        });
    }

    if args.json {
        println!("{}", serde_json::to_string_pretty(&issues)?);
        return Ok(());
    }

    println!("repo: {repo}");
    println!(
        "{:<8} {:<8} {:<12} {:<24} {}",
        "issue", "state", "workflow", "claims", "title"
    );
    println!(
        "{:<8} {:<8} {:<12} {:<24} {}",
        "-----", "-----", "--------", "------", "-----"
    );

    issues.sort_by_key(|issue| issue.number);
    for issue in issues {
        let workflow = issue
            .workflow_state()?
            .map(|state| state.to_string())
            .unwrap_or_else(|| "-".to_string());

        let mut claims = issue
            .claimed_labels()
            .map(|label| label.name.as_str())
            .collect::<Vec<_>>();
        claims.sort_unstable();
        let claims = if claims.is_empty() {
            "-".to_string()
        } else {
            claims.join(",")
        };

        println!(
            "{:<8} {:<8} {:<12} {:<24} {}",
            format!("#{}", issue.number),
            issue.state,
            workflow,
            claims,
            issue.title
        );
    }

    Ok(())
}

fn cmd_issue_show(api: &ForgejoClient, cfg: &AgentConfig, args: IssueShowArgs) -> Result<()> {
    let issue = api.get_issue(cfg, &args.issue)?;
    if args.json {
        println!("{}", serde_json::to_string_pretty(&issue)?);
        return Ok(());
    }

    let assignees = issue
        .assignees
        .iter()
        .map(|user| user.login.as_str())
        .collect::<Vec<_>>();

    println!("ref: {}", args.issue);
    println!("state: {}", issue.state);
    println!(
        "workflow: {}",
        issue
            .workflow_state()?
            .map(|state| state.to_string())
            .unwrap_or_else(|| "-".to_string())
    );
    println!(
        "assignees: {}",
        if assignees.is_empty() {
            "-".to_string()
        } else {
            assignees.join(",")
        }
    );
    println!("title: {}", issue.title);
    println!("url: {}", issue.html_url);
    println!("labels: {}", join_label_names(&issue));
    println!("---");
    println!("{}", issue.body.unwrap_or_default());

    Ok(())
}

fn cmd_issue_assign(api: &ForgejoClient, cfg: &AgentConfig, args: IssueAssignArgs) -> Result<()> {
    let assignees = if args.clear {
        Vec::new()
    } else if args.self_assign {
        let who = api.whoami(cfg)?;
        let login = who
            .get("login")
            .and_then(serde_json::Value::as_str)
            .ok_or_else(|| anyhow!("Forgejo whoami response missing login"))?;
        vec![login.to_ascii_lowercase()]
    } else if let Some(to) = args.to.as_ref() {
        vec![to.to_ascii_lowercase()]
    } else {
        bail!("provide exactly one of: --to <login>, --self, or --clear");
    };

    let updated = api.set_issue_assignees(cfg, &args.issue, assignees)?;
    if args.json {
        println!("{}", serde_json::to_string_pretty(&updated)?);
        return Ok(());
    }

    let logins = updated
        .assignees
        .iter()
        .map(|user| user.login.as_str())
        .collect::<Vec<_>>();
    println!(
        "assignees: {}",
        if logins.is_empty() {
            "-".to_string()
        } else {
            logins.join(",")
        }
    );
    Ok(())
}

fn read_issue_body_stdin() -> Result<String> {
    let mut body = String::new();
    io::stdin()
        .read_to_string(&mut body)
        .context("failed to read issue body from stdin (--body-stdin)")?;
    Ok(body)
}

fn issue_body_from_args(
    body: Option<String>,
    body_file: Option<PathBuf>,
    body_stdin: bool,
    required: bool,
) -> Result<String> {
    let mut source_count = 0_u8;
    if body.is_some() {
        source_count += 1;
    }
    if body_file.is_some() {
        source_count += 1;
    }
    if body_stdin {
        source_count += 1;
    }

    if source_count > 1 {
        bail!("provide exactly one body source: --body, --body-file, or --body-stdin");
    }
    if source_count == 0 {
        if required {
            bail!("missing body: provide one of --body, --body-file, or --body-stdin");
        }
        return Ok(String::new());
    }

    if let Some(text) = body {
        return Ok(text);
    }
    if let Some(path) = body_file {
        return fs::read_to_string(&path)
            .with_context(|| format!("failed to read body file: {}", path.display()));
    }

    read_issue_body_stdin()
}

fn issue_body_option_from_args(
    body: Option<String>,
    body_file: Option<PathBuf>,
    body_stdin: bool,
) -> Result<Option<String>> {
    let mut source_count = 0_u8;
    if body.is_some() {
        source_count += 1;
    }
    if body_file.is_some() {
        source_count += 1;
    }
    if body_stdin {
        source_count += 1;
    }

    if source_count > 1 {
        bail!("provide exactly one body source: --body, --body-file, or --body-stdin");
    }
    if source_count == 0 {
        return Ok(None);
    }

    if let Some(text) = body {
        return Ok(Some(text));
    }
    if let Some(path) = body_file {
        return fs::read_to_string(&path)
            .map(Some)
            .with_context(|| format!("failed to read body file: {}", path.display()));
    }

    read_issue_body_stdin().map(Some)
}

fn cmd_issue_create(api: &ForgejoClient, cfg: &AgentConfig, args: IssueCreateArgs) -> Result<()> {
    let repo = default_repo(args.repo, cfg);
    ensure_policy_labels(api, cfg, &repo)?;

    let body = issue_body_from_args(args.body, args.body_file, args.body_stdin, false)?;
    let created = api.create_issue(cfg, &repo, &args.title, &body)?;
    let issue_ref = IssueRef {
        repo: repo.clone(),
        number: created.number,
    };

    let mut labels = args.labels;
    labels.push(args.workflow.label().to_string());
    let label_ids = ensure_labels_exist(api, cfg, &repo, &labels)?;
    api.add_issue_label_ids(cfg, &issue_ref, label_ids)?;

    let issue = api.get_issue(cfg, &issue_ref)?;
    if args.json {
        println!("{}", serde_json::to_string_pretty(&issue)?);
    } else {
        println!("created: {} {}", issue_ref, issue.html_url);
    }
    Ok(())
}

fn cmd_issue_edit(api: &ForgejoClient, cfg: &AgentConfig, args: IssueEditArgs) -> Result<()> {
    let body = issue_body_option_from_args(args.body, args.body_file, args.body_stdin)?;
    let title = args.title.as_deref();
    let body = body.as_deref();

    if title.is_none() && body.is_none() {
        bail!("no updates requested: provide --title and/or one body source");
    }

    let issue = api.update_issue(cfg, &args.issue, title, body)?;
    if args.json {
        println!("{}", serde_json::to_string_pretty(&issue)?);
    } else {
        println!("edited: {} {}", args.issue, issue.html_url);
    }
    Ok(())
}

fn cmd_issue_comment(api: &ForgejoClient, cfg: &AgentConfig, args: IssueCommentArgs) -> Result<()> {
    let body = issue_body_from_args(args.body, args.body_file, args.body_stdin, true)?;
    api.comment_issue(cfg, &args.issue, &body)?;
    println!("commented: {}", args.issue);
    Ok(())
}

fn cmd_issue_orchd_state(
    api: &ForgejoClient,
    cfg: &AgentConfig,
    args: IssueOrchdStateArgs,
) -> Result<()> {
    let from = set_issue_orchd_state(api, cfg, &args.issue, args.to, args.reason_code.as_deref())?;
    let from = from
        .map(|state| state.to_string())
        .unwrap_or_else(|| "none".to_string());
    println!("orchd-state: {} {} -> {}", args.issue, from, args.to);
    Ok(())
}

fn cmd_issue_transition(
    api: &ForgejoClient,
    cfg: &AgentConfig,
    args: IssueTransitionArgs,
) -> Result<()> {
    let issue = api.get_issue(cfg, &args.issue)?;
    let from = policy::assert_transition(&issue, args.to, args.force)?;
    ensure_issue_state(api, cfg, &args.issue, args.to)?;
    let from_str = from
        .map(|state| state.to_string())
        .unwrap_or_else(|| "none".to_string());
    println!("transitioned: {} {} -> {}", args.issue, from_str, args.to);
    Ok(())
}

fn claim_label(agent: &str) -> String {
    format!("claimed/{agent}")
}

fn collect_conflicting_claims(issue: &ApiIssue, own_claim: &str) -> Vec<String> {
    let mut claims = issue
        .claimed_labels()
        .map(|label| label.name.clone())
        .filter(|name| name != own_claim)
        .collect::<Vec<_>>();
    claims.sort_unstable();
    claims.dedup();
    claims
}

fn find_claim_label_id(issue: &ApiIssue, claim_name: &str) -> Option<u64> {
    issue
        .labels
        .iter()
        .find(|label| label.name == claim_name)
        .map(|label| label.id)
}

fn cmd_issue_claim(api: &ForgejoClient, cfg: &AgentConfig, args: IssueClaimArgs) -> Result<()> {
    let agent = args.agent.unwrap_or_else(|| cfg.agent_name.clone());
    let ttl_min = args.ttl_min.unwrap_or(cfg.lease_minutes);
    let issue = api.get_issue(cfg, &args.issue)?;

    policy::assert_claimable(&issue)?;

    let claim = claim_label(&agent);
    let existing_conflicts = collect_conflicting_claims(&issue, &claim);
    if !existing_conflicts.is_empty() {
        bail!(
            "cannot claim {}; already claimed by {}",
            args.issue,
            existing_conflicts.join(",")
        );
    }

    let had_own_claim = issue.labels.iter().any(|label| label.name == claim);
    let claim_id = api
        .ensure_label(
            cfg,
            &args.issue.repo,
            &claim,
            "fbca04",
            "lease label for active agent claim",
            false,
        )?
        .id;

    if !had_own_claim {
        api.add_issue_label_ids(cfg, &args.issue, vec![claim_id])?;
    }

    let claimed_issue = api.get_issue(cfg, &args.issue)?;
    let post_claim_conflicts = collect_conflicting_claims(&claimed_issue, &claim);
    if !post_claim_conflicts.is_empty() {
        if !had_own_claim && let Some(own_claim_id) = find_claim_label_id(&claimed_issue, &claim) {
            let _ = api.remove_issue_label(cfg, &args.issue, own_claim_id);
        }
        bail!(
            "cannot claim {}; concurrent claim detected with {}",
            args.issue,
            post_claim_conflicts.join(",")
        );
    }

    ensure_issue_state(api, cfg, &args.issue, WorkflowState::InProgress)?;

    let now = Utc::now();
    let until = now + ChronoDuration::minutes(ttl_min);
    println!(
        "claimed: {} as {} until {}",
        args.issue,
        agent,
        until.format("%Y-%m-%dT%H:%M:%SZ")
    );
    Ok(())
}

fn cmd_issue_release(api: &ForgejoClient, cfg: &AgentConfig, args: IssueReleaseArgs) -> Result<()> {
    let issue = api.get_issue(cfg, &args.issue)?;

    let claimed = issue.claimed_labels().cloned().collect::<Vec<_>>();
    let to_remove = if let Some(agent) = &args.agent {
        let target = claim_label(agent);
        claimed
            .into_iter()
            .filter(|label| label.name == target)
            .collect::<Vec<_>>()
    } else {
        claimed
    };

    for label in to_remove {
        api.remove_issue_label(cfg, &args.issue, label.id)?;
    }

    if issue.workflow_state()? == Some(WorkflowState::InProgress) {
        ensure_issue_state(api, cfg, &args.issue, WorkflowState::Ready)?;
    }

    println!("released: {}", args.issue);
    Ok(())
}

fn cmd_issue_blocker(api: &ForgejoClient, cfg: &AgentConfig, args: IssueBlockerArgs) -> Result<()> {
    let parent_issue = api.get_issue(cfg, &args.issue)?;
    if parent_issue.state == OpenState::Closed {
        bail!("cannot spawn blocker from closed issue {}", args.issue);
    }

    let blocker_title = format!("[BLOCKER] {}", args.title);
    let blocker_body = args.body.unwrap_or_default();
    let created = api.create_issue(cfg, &args.issue.repo, &blocker_title, &blocker_body)?;
    let blocker_ref = IssueRef {
        repo: args.issue.repo.clone(),
        number: created.number,
    };

    ensure_policy_labels(api, cfg, &args.issue.repo)?;
    let blocker_labels = vec![
        "type/blocker".to_string(),
        WorkflowState::Triage.label().to_string(),
    ];
    let blocker_ids = ensure_labels_exist(api, cfg, &args.issue.repo, &blocker_labels)?;
    api.add_issue_label_ids(cfg, &blocker_ref, blocker_ids)?;

    let parent = api.get_issue(cfg, &args.issue)?;
    policy::assert_transition(&parent, WorkflowState::Blocked, false)?;
    ensure_issue_state(api, cfg, &args.issue, WorkflowState::Blocked)?;

    let mut parent_msg = String::new();
    let _ = write!(
        parent_msg,
        "blocked by #{} ({})",
        blocker_ref.number, created.html_url
    );
    api.comment_issue(cfg, &args.issue, &parent_msg)?;

    let child_msg = format!("blocks {}", args.issue);
    api.comment_issue(cfg, &blocker_ref, &child_msg)?;

    println!("blocker: {} -> {}", args.issue, blocker_ref);
    Ok(())
}

fn cmd_issue_close(api: &ForgejoClient, cfg: &AgentConfig, args: IssueCloseArgs) -> Result<()> {
    let issue = api.get_issue(cfg, &args.issue)?;
    policy::assert_closable(&issue, args.force)?;
    api.set_issue_open_state(cfg, &args.issue, OpenState::Closed)?;
    println!("closed: {}", args.issue);
    Ok(())
}

fn cmd_issue_reopen(api: &ForgejoClient, cfg: &AgentConfig, args: IssueReopenArgs) -> Result<()> {
    api.set_issue_open_state(cfg, &args.issue, OpenState::Open)?;
    ensure_issue_state(api, cfg, &args.issue, args.workflow)?;
    println!("reopened: {} -> {}", args.issue, args.workflow);
    Ok(())
}

fn pick_ready_issue(
    api: &ForgejoClient,
    cfg: &AgentConfig,
    repo: &RepoRef,
) -> Result<Option<IssueRef>> {
    let mut issues = api.list_issues(cfg, repo, "open", 200)?;
    issues.retain(|issue| issue.pull_request.is_none());
    issues.retain(|issue| {
        issue
            .labels
            .iter()
            .any(|label| label.name == WorkflowState::Ready.label())
    });
    issues.retain(|issue| {
        !issue
            .labels
            .iter()
            .any(|label| label.name.starts_with("claimed/"))
    });
    issues.sort_by_key(|issue| issue.number);

    Ok(issues.first().map(|issue| IssueRef {
        repo: repo.clone(),
        number: issue.number,
    }))
}

fn worker_prompt(issue_ref: &IssueRef, issue: &ApiIssue) -> String {
    let body = issue.body.clone().unwrap_or_default();
    format!(
        "Work issue {issue_ref}.\n\nIssue URL: {}\nTitle: {}\nBody:\n{}\n\nExecution contract:\n1. Implement in the declared local repo/workdir only.\n2. If blocked, run: forgejoctl issue blocker {} --title \"...\" --body \"...\"\n3. Post progress/final notes with: forgejoctl issue comment {} --body \"...\"\n4. Keep the issue workflow state correct via forgejoctl issue transition.\n",
        issue.html_url, issue.title, body, issue_ref, issue_ref
    )
}

fn cmd_worker_run(api: &ForgejoClient, cfg: &AgentConfig, args: WorkerRunArgs) -> Result<()> {
    let repo = default_repo(args.repo, cfg);
    let agent = args.agent.unwrap_or_else(|| cfg.agent_name.clone());

    if args.execute && args.workdir.is_none() {
        bail!("--workdir is required when --execute is set");
    }

    loop {
        let maybe_issue = pick_ready_issue(api, cfg, &repo)?;
        let Some(issue_ref) = maybe_issue else {
            println!("[worker] queue empty for {repo}");
            if args.once {
                return Ok(());
            }
            thread::sleep(Duration::from_secs(args.interval_sec));
            continue;
        };

        println!("[worker] picked {issue_ref}");

        cmd_issue_claim(
            api,
            cfg,
            IssueClaimArgs {
                issue: issue_ref.clone(),
                agent: Some(agent.clone()),
                ttl_min: None,
            },
        )?;

        let issue = api.get_issue(cfg, &issue_ref)?;
        let prompt = worker_prompt(&issue_ref, &issue);

        if !args.execute {
            println!("[worker] dry-run prompt for {}:\n{}", issue_ref, prompt);
            cmd_issue_release(
                api,
                cfg,
                IssueReleaseArgs {
                    issue: issue_ref,
                    agent: Some(agent.clone()),
                },
            )?;
            if args.once {
                return Ok(());
            }
            thread::sleep(Duration::from_secs(args.interval_sec));
            continue;
        }

        let workdir = args
            .workdir
            .as_ref()
            .ok_or_else(|| anyhow!("--workdir required with --execute"))?;

        let status = Command::new("codex")
            .arg("exec")
            .arg("--cd")
            .arg(workdir)
            .arg("--full-auto")
            .arg(&prompt)
            .status()
            .context("failed to start codex executable")?;

        if status.success() {
            api.comment_issue(
                cfg,
                &issue_ref,
                "[worker] codex run completed successfully.",
            )?;
            if args.close_on_success {
                let issue_now = api.get_issue(cfg, &issue_ref)?;
                let force = issue_now.workflow_state()? != Some(WorkflowState::Review);
                cmd_issue_close(
                    api,
                    cfg,
                    IssueCloseArgs {
                        issue: issue_ref.clone(),
                        force,
                    },
                )?;
            } else {
                cmd_issue_release(
                    api,
                    cfg,
                    IssueReleaseArgs {
                        issue: issue_ref.clone(),
                        agent: Some(agent.clone()),
                    },
                )?;
            }
            println!("[worker] completed {issue_ref}");
        } else {
            api.comment_issue(
                cfg,
                &issue_ref,
                &format!("[worker] codex run exited non-zero ({status}). Review logs and retry."),
            )?;
            cmd_issue_release(
                api,
                cfg,
                IssueReleaseArgs {
                    issue: issue_ref.clone(),
                    agent: Some(agent.clone()),
                },
            )?;
            println!("[worker] failed {issue_ref} with status {status}");
        }

        if args.once {
            return Ok(());
        }

        thread::sleep(Duration::from_secs(args.interval_sec));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use forgejo_agent::types::{ApiLabel, OpenState};

    fn issue_with_labels(labels: &[(&str, u64)]) -> ApiIssue {
        ApiIssue {
            number: 7,
            state: OpenState::Open,
            title: "claim test".to_string(),
            body: None,
            html_url: "http://localhost/issue/7".to_string(),
            labels: labels
                .iter()
                .map(|(name, id)| ApiLabel {
                    id: *id,
                    name: (*name).to_string(),
                })
                .collect(),
            assignees: Vec::new(),
            pull_request: None,
            repository: None,
        }
    }

    #[test]
    fn conflicting_claims_exclude_own_claim() {
        let issue = issue_with_labels(&[
            ("claimed/codex-a", 1),
            ("claimed/codex-b", 2),
            ("state/ready", 3),
        ]);
        let conflicts = collect_conflicting_claims(&issue, "claimed/codex-a");
        assert_eq!(conflicts, vec!["claimed/codex-b".to_string()]);
    }

    #[test]
    fn find_claim_label_id_returns_matching_label() {
        let issue = issue_with_labels(&[("claimed/codex-a", 42), ("state/ready", 3)]);
        assert_eq!(find_claim_label_id(&issue, "claimed/codex-a"), Some(42));
        assert_eq!(find_claim_label_id(&issue, "claimed/codex-b"), None);
    }
}
