use std::env;
use std::fs::{self, File, OpenOptions};
use std::io::{BufRead, BufReader, Write as _};
use std::path::{Path, PathBuf};
use std::process::{Command, ExitStatus, Stdio};
use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail};
use serde::{Deserialize, Serialize};

use forgejo_agent::types::IssueRef;

use super::cli::{FinalizeDispatchArgs, RunDispatchArgs};
use super::finalize;

const SPEC_VERSION_V1: u8 = 1;
const SUMMARY_LINE_LIMIT: usize = 120;

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub(super) enum CodexSandbox {
    ReadOnly,
    WorkspaceWrite,
}

impl CodexSandbox {
    pub(super) const fn as_str(self) -> &'static str {
        match self {
            Self::ReadOnly => "read-only",
            Self::WorkspaceWrite => "workspace-write",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(transparent)]
pub(super) struct CodexSessionId(String);

impl CodexSessionId {
    pub(super) fn parse(raw: &str) -> Result<Self> {
        let value = raw.trim();
        if value.is_empty() {
            bail!("codex session id must be non-empty");
        }
        // Codex currently uses UUIDs; enforce the canonical 36-byte hex+hyphen format.
        if value.len() != 36 {
            bail!("invalid codex session id length: {value}");
        }
        for (idx, ch) in value.bytes().enumerate() {
            let is_hyphen = matches!(idx, 8 | 13 | 18 | 23);
            if is_hyphen {
                if ch != b'-' {
                    bail!("invalid codex session id (expected hyphen): {value}");
                }
                continue;
            }
            if !ch.is_ascii_hexdigit() {
                bail!("invalid codex session id (expected hex): {value}");
            }
        }
        Ok(Self(value.to_string()))
    }

    pub(super) fn as_str(&self) -> &str {
        &self.0
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct DispatchExecSpecV1 {
    pub(super) version: u8,
    pub(super) dispatch_id: i64,
    pub(super) db_path: PathBuf,
    pub(super) lock_path: PathBuf,
    pub(super) run_dir: PathBuf,
    pub(super) prompt_path: PathBuf,
    pub(super) summary_path: PathBuf,
    pub(super) completion_path: PathBuf,
    pub(super) last_message_path: PathBuf,
    pub(super) codex_log_path: PathBuf,
    pub(super) marker_path: PathBuf,
    pub(super) issue_ref: String,
    pub(super) issue_title: String,
    pub(super) issue_url: String,
    pub(super) forgejoctl_bin: PathBuf,
    pub(super) forgejo_config_file: Option<PathBuf>,
    pub(super) token_file: PathBuf,
    pub(super) control_token_file: Option<PathBuf>,
    pub(super) workdir: PathBuf,
    pub(super) principal_workdir: Option<PathBuf>,
    pub(super) codex_sandbox: CodexSandbox,
    pub(super) git_remote: String,
    pub(super) git_base: String,
    pub(super) git_branch: String,
    pub(super) codex_bin: PathBuf,
    pub(super) codex_role_arg: String,
    pub(super) issue_session_id: Option<CodexSessionId>,
    pub(super) directive: String,
    pub(super) role_name: String,
    pub(super) timeout_sec: u64,
}

impl DispatchExecSpecV1 {
    pub(super) fn validate(&self) -> Result<()> {
        if self.version != SPEC_VERSION_V1 {
            bail!("unsupported dispatch exec spec version {}", self.version);
        }
        if self.dispatch_id <= 0 {
            bail!("dispatch id must be positive (got {})", self.dispatch_id);
        }
        if self.directive.trim().is_empty() {
            bail!("directive must be non-empty");
        }
        if self.role_name.trim().is_empty() {
            bail!("role name must be non-empty");
        }
        let _ = IssueRef::parse(&self.issue_ref)?;
        Ok(())
    }

    pub(super) fn write_json(&self, path: &Path) -> Result<()> {
        let body = serde_json::to_vec_pretty(self)?;
        fs::write(path, body)
            .with_context(|| format!("failed writing dispatch spec {}", path.display()))
    }

    fn load_json(path: &Path) -> Result<Self> {
        let raw = fs::read(path)
            .with_context(|| format!("failed reading dispatch spec {}", path.display()))?;
        let spec: Self = serde_json::from_slice(&raw)
            .with_context(|| format!("failed parsing dispatch spec {}", path.display()))?;
        spec.validate()?;
        Ok(spec)
    }
}

#[derive(Debug, Clone, Copy)]
enum ReportedStatus {
    Completed,
    TimedOut,
    FailedRuntime,
}

impl ReportedStatus {
    const fn status_str(self) -> &'static str {
        match self {
            Self::Completed => "completed",
            Self::TimedOut => "timed_out",
            Self::FailedRuntime => "failed_runtime",
        }
    }

    const fn reason_code(self) -> &'static str {
        match self {
            Self::Completed => "completed",
            Self::TimedOut => "timeout",
            Self::FailedRuntime => "codex_exit_nonzero",
        }
    }
}

#[derive(Debug, Clone, Copy)]
enum CodexRunMode<'a> {
    Fresh,
    Resume(&'a CodexSessionId),
}

fn append_log_line(path: &Path, line: &str) -> Result<()> {
    let mut file = OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .with_context(|| format!("failed opening codex log {}", path.display()))?;
    writeln!(file, "{line}")?;
    Ok(())
}

fn truncate_file(path: &Path) -> Result<()> {
    let _ = OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(true)
        .open(path)
        .with_context(|| format!("failed truncating {}", path.display()))?;
    Ok(())
}

fn write_marker_file(path: &Path) -> Result<()> {
    let _ = OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(true)
        .open(path)
        .with_context(|| format!("failed touching marker {}", path.display()))?;
    Ok(())
}

fn exit_code(status: ExitStatus) -> i64 {
    match status.code() {
        Some(code) => i64::from(code),
        None => {
            #[cfg(unix)]
            {
                use std::os::unix::process::ExitStatusExt as _;
                status.signal().map(|sig| i64::from(128 + sig)).unwrap_or(1)
            }
            #[cfg(not(unix))]
            {
                1
            }
        }
    }
}

const fn codex_status_for_exit_code(code: i64) -> ReportedStatus {
    match code {
        0 => ReportedStatus::Completed,
        124 => ReportedStatus::TimedOut,
        _ => ReportedStatus::FailedRuntime,
    }
}

fn run_codex_once(spec: &DispatchExecSpecV1, mode: CodexRunMode<'_>) -> Result<i64> {
    let log_out = OpenOptions::new()
        .create(true)
        .append(true)
        .open(&spec.codex_log_path)
        .with_context(|| format!("failed opening codex log {}", spec.codex_log_path.display()))?;
    let log_err = log_out
        .try_clone()
        .context("failed duplicating codex log handle")?;

    let mut cmd = Command::new("timeout");
    cmd.arg(spec.timeout_sec.to_string())
        .arg(&spec.codex_bin)
        .arg(&spec.codex_role_arg)
        .arg("--sandbox")
        .arg(spec.codex_sandbox.as_str())
        .arg("--cd")
        .arg(&spec.workdir)
        .arg("--no-alt-screen")
        .arg("exec")
        .arg("-o")
        .arg(&spec.last_message_path);

    match mode {
        CodexRunMode::Fresh => {
            cmd.arg("-");
        }
        CodexRunMode::Resume(session_id) => {
            cmd.args(["resume", session_id.as_str(), "-"]);
        }
    }

    cmd.current_dir(&spec.workdir)
        .stdin(Stdio::piped())
        .stdout(Stdio::from(log_out))
        .stderr(Stdio::from(log_err));

    let mut child = cmd.spawn().context("failed spawning codex via timeout")?;

    let prompt_path = spec.prompt_path.clone();
    let mut stdin = child
        .stdin
        .take()
        .ok_or_else(|| anyhow!("failed opening stdin for codex process"))?;
    let feed_thread = std::thread::spawn(move || -> Result<()> {
        let mut file = File::open(&prompt_path)
            .with_context(|| format!("failed opening prompt {}", prompt_path.display()))?;
        match std::io::copy(&mut file, &mut stdin) {
            Ok(_) => Ok(()),
            Err(err) if err.kind() == std::io::ErrorKind::BrokenPipe => Ok(()),
            Err(err) => Err(err).context("failed streaming prompt into codex stdin"),
        }
    });

    let status = child.wait().context("failed waiting for codex process")?;
    feed_thread
        .join()
        .map_err(|_| anyhow!("prompt feed thread panicked"))??;

    Ok(exit_code(status))
}

fn extract_session_id_from_codex_log(path: &Path) -> Result<Option<CodexSessionId>> {
    let file =
        File::open(path).with_context(|| format!("failed opening codex log {}", path.display()))?;
    let reader = BufReader::new(file);
    let mut last = None;
    for line in reader.lines() {
        let line = line.context("failed reading codex log line")?;
        if let Some(value) = line.strip_prefix("session id: ")
            && let Ok(parsed) = CodexSessionId::parse(value)
        {
            last = Some(parsed);
        }
    }
    Ok(last)
}

fn sessions_dir() -> Option<PathBuf> {
    let home = env::var("HOME").ok()?;
    Some(PathBuf::from(home).join(".codex").join("sessions"))
}

fn marker_mtime(path: &Path) -> Result<std::time::SystemTime> {
    fs::metadata(path)
        .with_context(|| format!("failed stating marker {}", path.display()))?
        .modified()
        .with_context(|| format!("failed reading marker mtime {}", path.display()))
}

fn find_session_id_from_newer_sessions(marker: &Path) -> Result<Option<CodexSessionId>> {
    let Some(root) = sessions_dir() else {
        return Ok(None);
    };
    if !root.is_dir() {
        return Ok(None);
    }
    let marker_time = marker_mtime(marker)?;

    let mut stack = vec![root];
    let mut candidates = Vec::<PathBuf>::new();
    while let Some(dir) = stack.pop() {
        for entry in
            fs::read_dir(&dir).with_context(|| format!("failed reading {}", dir.display()))?
        {
            let entry = entry?;
            let path = entry.path();
            if path.is_dir() {
                stack.push(path);
                continue;
            }
            if path.extension().and_then(|ext| ext.to_str()) != Some("jsonl") {
                continue;
            }
            let Ok(modified) = entry.metadata().and_then(|m| m.modified()) else {
                continue;
            };
            if modified <= marker_time {
                continue;
            }
            candidates.push(path);
        }
    }

    candidates.sort_by(|a, b| a.as_os_str().cmp(b.as_os_str()));
    let Some(latest) = candidates.pop() else {
        return Ok(None);
    };
    let Some(file_name) = latest.file_name().and_then(|v| v.to_str()) else {
        return Ok(None);
    };
    let Some(base) = file_name.strip_suffix(".jsonl") else {
        return Ok(None);
    };
    if base.len() < 36 {
        return Ok(None);
    }
    CodexSessionId::parse(&base[base.len() - 36..]).map(Some)
}

fn write_summary(last_message_path: &Path, summary_path: &Path) -> Result<()> {
    let Ok(file) = File::open(last_message_path) else {
        fs::write(summary_path, "(no final assistant message)\n")?;
        return Ok(());
    };
    let reader = BufReader::new(file);
    let mut out = String::new();
    for (idx, line) in reader.lines().enumerate() {
        if idx >= SUMMARY_LINE_LIMIT {
            break;
        }
        let line = line?;
        out.push_str(&line);
        out.push('\n');
    }
    if out.is_empty() {
        out = "(no final assistant message)\n".to_string();
    }
    fs::write(summary_path, out)
        .with_context(|| format!("failed writing {}", summary_path.display()))
}

fn write_completion(
    spec: &DispatchExecSpecV1,
    status: ReportedStatus,
    session_id: Option<&CodexSessionId>,
) -> Result<()> {
    let summary = fs::read_to_string(&spec.summary_path)
        .with_context(|| format!("failed reading {}", spec.summary_path.display()))?;
    let session_id = session_id.map_or_else(|| "unknown".to_string(), |id| id.as_str().to_string());
    let text = format!(
        "orchd: dispatch completed id={} status={} reason={}\n\
directive={} role={}\n\
codex_session_id={}\n\
run_dir={}\n\
log={}\n\
\n\
```markdown\n\
{summary}```\n",
        spec.dispatch_id,
        status.status_str(),
        status.reason_code(),
        spec.directive,
        spec.role_name,
        session_id,
        spec.run_dir.display(),
        spec.codex_log_path.display(),
    );
    fs::write(&spec.completion_path, text)
        .with_context(|| format!("failed writing {}", spec.completion_path.display()))
}

fn finalize_with_retries(
    spec: &DispatchExecSpecV1,
    status: ReportedStatus,
    exit_code: i64,
    session_id: Option<&CodexSessionId>,
) -> Result<()> {
    let issue_ref = IssueRef::parse(&spec.issue_ref)?;
    let session_for_finalize = session_id.map_or(String::new(), |id| id.as_str().to_string());

    let finalize_token = spec
        .control_token_file
        .as_ref()
        .unwrap_or(&spec.token_file)
        .clone();

    let max_attempts = 8u32;
    for attempt in 1..=max_attempts {
        let args = FinalizeDispatchArgs {
            db_path: spec.db_path.clone(),
            dispatch_id: spec.dispatch_id,
            status: status.status_str().to_string(),
            reason_code: status.reason_code().to_string(),
            exit_code,
            session_id: session_for_finalize.clone(),
            issue_ref: issue_ref.clone(),
            issue_title: spec.issue_title.clone(),
            issue_url: spec.issue_url.clone(),
            directive: spec.directive.clone(),
            role_name: spec.role_name.clone(),
            run_dir: spec.run_dir.clone(),
            log_file: spec.codex_log_path.clone(),
            completion_file: spec.completion_path.clone(),
            git_workdir: spec.workdir.clone(),
            git_remote: spec.git_remote.clone(),
            git_base: spec.git_base.clone(),
            git_branch: spec.git_branch.clone(),
            forgejoctl_bin: spec.forgejoctl_bin.clone(),
            forgejo_config: spec.forgejo_config_file.clone(),
            token_file: finalize_token.clone(),
            principal_workdir: spec.principal_workdir.clone(),
        };
        match finalize::finalize_dispatch_command(args) {
            Ok(()) => return Ok(()),
            Err(err) => {
                if attempt >= max_attempts {
                    append_log_line(
                        &spec.codex_log_path,
                        &format!(
                            "orchd: finalize-dispatch failed after {attempt} attempts: {err:#}"
                        ),
                    )?;
                    return Err(err);
                }
                let sleep_sec = u64::from(attempt) * 2;
                append_log_line(
                    &spec.codex_log_path,
                    &format!(
                        "orchd: finalize-dispatch attempt {attempt} failed; retrying in {sleep_sec}s: {err:#}"
                    ),
                )?;
                std::thread::sleep(Duration::from_secs(sleep_sec));
            }
        }
    }
    Ok(())
}

pub(super) fn run_dispatch_command(args: RunDispatchArgs) -> Result<()> {
    let spec = DispatchExecSpecV1::load_json(&args.spec)?;
    truncate_file(&spec.codex_log_path)?;
    write_marker_file(&spec.marker_path)?;

    let exit_code = if let Some(session_id) = spec.issue_session_id.as_ref() {
        let mut exit_code = run_codex_once(&spec, CodexRunMode::Resume(session_id))?;
        if exit_code != 0 && exit_code != 124 {
            append_log_line(
                &spec.codex_log_path,
                &format!(
                    "orchd: resume failed for issue session {}, falling back to fresh exec",
                    session_id.as_str()
                ),
            )?;
            exit_code = run_codex_once(&spec, CodexRunMode::Fresh)?;
        }
        exit_code
    } else {
        run_codex_once(&spec, CodexRunMode::Fresh)?
    };

    let session_id = extract_session_id_from_codex_log(&spec.codex_log_path)?
        .or(find_session_id_from_newer_sessions(&spec.marker_path)?);

    write_summary(&spec.last_message_path, &spec.summary_path)?;
    let status = codex_status_for_exit_code(exit_code);
    write_completion(&spec, status, session_id.as_ref())?;

    let finalize_outcome = finalize_with_retries(&spec, status, exit_code, session_id.as_ref());
    if let Err(err) = fs::remove_file(&spec.lock_path)
        && err.kind() != std::io::ErrorKind::NotFound
    {
        append_log_line(
            &spec.codex_log_path,
            &format!(
                "orchd: lock cleanup failed for {}: {err}",
                spec.lock_path.display()
            ),
        )?;
    }
    finalize_outcome
}

#[cfg(test)]
mod tests {
    use super::CodexSessionId;

    #[test]
    fn codex_session_id_rejects_non_uuid() {
        assert!(CodexSessionId::parse("").is_err());
        assert!(CodexSessionId::parse("not-a-uuid").is_err());
    }

    #[test]
    fn codex_session_id_accepts_uuid() {
        let value = "00000000-0000-0000-0000-000000000000";
        assert!(CodexSessionId::parse(value).is_ok());
    }
}
