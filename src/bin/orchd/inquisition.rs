use std::fmt::Write as _;
use std::path::Path;
use std::process::Command;

use anyhow::{Context, Result, anyhow};

use forgejo_agent::types::{IssueRef, RepoRef};

use super::db;
use super::projection::CommentIdentity;

const INQUISITION_REPO: &str = "forgejo-work";
const INQUISITION_CATEGORY: &str = "inquisition";

#[derive(Debug, Clone)]
pub(super) struct InquisitionSpec {
    pub(super) source_issue: IssueRef,
    pub(super) source_issue_title: Option<String>,
    pub(super) source_issue_url: Option<String>,
    pub(super) dispatch_id: Option<i64>,
    pub(super) directive: Option<String>,
    pub(super) role_name: Option<String>,
    pub(super) reason_code: String,
    pub(super) exit_code: Option<i64>,
    pub(super) run_dir: Option<String>,
    pub(super) log_file: Option<String>,
    pub(super) completion_file: Option<String>,
    pub(super) error_text: Option<String>,
}

impl InquisitionSpec {
    fn dedupe_key(&self) -> String {
        if let Some(dispatch_id) = self.dispatch_id {
            return format!("dispatch:{dispatch_id}:inquisition");
        }
        format!(
            "issue:{}:inquisition:{}",
            self.source_issue, self.reason_code
        )
    }

    fn title(&self) -> String {
        let mut title = format!("audit-failure: {}", self.source_issue);
        if let Some(dispatch_id) = self.dispatch_id {
            let _ = write!(&mut title, " dispatch {dispatch_id}");
        }
        if !self.reason_code.trim().is_empty() {
            let _ = write!(&mut title, " ({})", self.reason_code);
        }
        title
    }

    fn body(&self) -> Result<String> {
        let mut out = String::new();
        writeln!(&mut out, "@codex-audit audit-failure")?;
        writeln!(&mut out)?;
        writeln!(
            &mut out,
            "Automatically spawned harness failure inquisition. Findings and follow-up issues belong in this repo."
        )?;
        writeln!(&mut out)?;
        writeln!(&mut out, "- source_issue: {}", self.source_issue)?;
        if let Some(url) = self.source_issue_url.as_deref() {
            writeln!(&mut out, "- source_url: {url}")?;
        }
        if let Some(title) = self.source_issue_title.as_deref() {
            writeln!(&mut out, "- source_title: {title}")?;
        }
        if let Some(dispatch_id) = self.dispatch_id {
            writeln!(&mut out, "- dispatch_id: {dispatch_id}")?;
        }
        if let Some(directive) = self.directive.as_deref() {
            writeln!(&mut out, "- directive: {directive}")?;
        }
        if let Some(role_name) = self.role_name.as_deref() {
            writeln!(&mut out, "- role: {role_name}")?;
        }
        if !self.reason_code.trim().is_empty() {
            writeln!(&mut out, "- reason_code: {}", self.reason_code.trim())?;
        }
        if let Some(exit_code) = self.exit_code {
            writeln!(&mut out, "- exit_code: {exit_code}")?;
        }
        if let Some(run_dir) = self.run_dir.as_deref() {
            writeln!(&mut out, "- run_dir: {run_dir}")?;
        }
        if let Some(log_file) = self.log_file.as_deref() {
            writeln!(&mut out, "- codex_log: {log_file}")?;
        }
        if let Some(completion_file) = self.completion_file.as_deref() {
            writeln!(&mut out, "- completion_file: {completion_file}")?;
        }
        if let Some(error_text) = self.error_text.as_deref() {
            writeln!(&mut out)?;
            writeln!(&mut out, "error_text:")?;
            writeln!(&mut out, "```")?;
            writeln!(&mut out, "{error_text}")?;
            writeln!(&mut out, "```")?;
        }
        Ok(out)
    }
}

pub(super) fn maybe_spawn_inquisition(
    db_path: &Path,
    default_owner: &str,
    identity: &CommentIdentity,
    spec: InquisitionSpec,
) -> Result<()> {
    let dedupe_key = spec.dedupe_key();
    let should_spawn =
        db::record_notification_delivery(db_path, &dedupe_key, INQUISITION_CATEGORY)?;
    if !should_spawn {
        return Ok(());
    }

    let sink_repo = RepoRef::new(default_owner, INQUISITION_REPO);
    let sink_repo_ref = sink_repo.to_string();
    run_forgejoctl_with_output(
        &identity.forgejoctl_bin,
        identity.config_file.as_deref(),
        &identity.token_file,
        &["repo", "ensure", sink_repo_ref.as_str()],
    )
    .with_context(|| format!("failed ensuring inquisition repo {sink_repo}"))?;

    let title = spec.title();
    let body = spec.body()?;
    run_forgejoctl_with_output(
        &identity.forgejoctl_bin,
        identity.config_file.as_deref(),
        &identity.token_file,
        &[
            "issue",
            "create",
            sink_repo_ref.as_str(),
            "--title",
            &title,
            "--body",
            &body,
            "--workflow",
            "triage",
        ],
    )
    .with_context(|| format!("failed creating inquisition ticket in {sink_repo}"))?;

    Ok(())
}

fn run_forgejoctl_with_output(
    forgejoctl_bin: &Path,
    config_file: Option<&Path>,
    token_file: &Path,
    args: &[&str],
) -> Result<()> {
    let mut cmd = Command::new(forgejoctl_bin);
    if let Some(config_file) = config_file {
        cmd.arg("--config").arg(config_file);
    }
    let output = cmd
        .arg("--token-file")
        .arg(token_file)
        .args(args)
        .output()
        .with_context(|| format!("failed invoking forgejoctl {}", forgejoctl_bin.display()))?;
    if output.status.success() {
        return Ok(());
    }
    let stderr = String::from_utf8_lossy(&output.stderr);
    Err(anyhow!(
        "forgejoctl command failed (exit={:?}) args={args:?} stderr={}",
        output.status.code(),
        stderr.trim()
    ))
}
