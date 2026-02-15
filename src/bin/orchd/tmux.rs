use std::borrow::Cow;
use std::path::Path;
use std::process::{Command, Stdio};

use anyhow::Result;

use super::errors::DispatchError;

pub(super) struct TmuxRunScriptInputs<'a> {
    pub(super) dispatch_id: i64,
    pub(super) db_path: &'a Path,
    pub(super) lock_path: &'a Path,
    pub(super) run_dir: &'a Path,
    pub(super) prompt_path: &'a Path,
    pub(super) summary_path: &'a Path,
    pub(super) completion_path: &'a Path,
    pub(super) last_message_path: &'a Path,
    pub(super) codex_log_path: &'a Path,
    pub(super) marker_path: &'a Path,
    pub(super) issue_ref_text: &'a str,
    pub(super) orchd_bin: &'a Path,
    pub(super) forgejoctl_bin: &'a Path,
    pub(super) forgejo_config_file: Option<&'a Path>,
    pub(super) token_file: &'a Path,
    pub(super) workdir: &'a Path,
    pub(super) codex_sandbox: &'a str,
    pub(super) git_remote: &'a str,
    pub(super) git_base: &'a str,
    pub(super) git_branch: &'a str,
    pub(super) issue_title: &'a str,
    pub(super) issue_url: &'a str,
    pub(super) codex_bin: &'a Path,
    pub(super) codex_role_arg: &'a str,
    pub(super) issue_session_id: Option<&'a str>,
    pub(super) directive_name: &'a str,
    pub(super) role_name: &'a str,
    pub(super) tmux_locator: &'a str,
    pub(super) timeout_sec: u64,
}

fn shell_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\"'\"'"))
}

fn tmux_repo_slug(repo_full_name: &str) -> String {
    let mut slug = String::with_capacity(repo_full_name.len());
    let mut last_dash = false;
    for ch in repo_full_name.chars() {
        if ch.is_ascii_alphanumeric() {
            slug.push(ch.to_ascii_lowercase());
            last_dash = false;
        } else if !last_dash {
            slug.push('-');
            last_dash = true;
        }
    }
    let trimmed = slug.trim_matches('-');
    let normalized = if trimmed.is_empty() { "repo" } else { trimmed };
    normalized.chars().take(24).collect()
}

pub(super) fn issue_tmux_window_name(repo_full_name: &str, issue_number: u64) -> String {
    let repo_slug = tmux_repo_slug(repo_full_name);
    format!("r{repo_slug}-i{issue_number}")
}

fn tmux_has_session(session: &str) -> Result<bool, DispatchError> {
    let status = Command::new("tmux")
        .args(["has-session", "-t", session])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map_err(|err| DispatchError::Tmux(format!("failed checking tmux session: {err}")))?;
    Ok(status.success())
}

fn tmux_set_remain_on_exit(session: &str, enabled: bool) -> Result<(), DispatchError> {
    let flag = if enabled { "on" } else { "off" };
    let status = Command::new("tmux")
        .args(["set-option", "-t", session, "remain-on-exit", flag])
        .status()
        .map_err(|err| DispatchError::Tmux(format!("failed setting remain-on-exit: {err}")))?;
    if !status.success() {
        return Err(DispatchError::Tmux(format!(
            "tmux set-option failed for session {session}"
        )));
    }
    Ok(())
}

fn tmux_has_window(session: &str, window: &str) -> Result<bool, DispatchError> {
    let output = Command::new("tmux")
        .args(["list-windows", "-t", session, "-F", "#{window_name}"])
        .output()
        .map_err(|err| DispatchError::Tmux(format!("failed listing tmux windows: {err}")))?;
    if !output.status.success() {
        return Err(DispatchError::Tmux(format!(
            "tmux list-windows failed for session {session}"
        )));
    }
    let target = window.trim();
    let windows = String::from_utf8_lossy(&output.stdout);
    Ok(windows.lines().any(|name| name.trim() == target))
}

pub(super) fn tmux_window_has_live_pane(
    session: &str,
    window: &str,
) -> Result<bool, DispatchError> {
    let target = format!("{session}:{window}");
    let output = Command::new("tmux")
        .args(["list-panes", "-t", &target, "-F", "#{pane_dead}"])
        .output()
        .map_err(|err| DispatchError::Tmux(format!("failed listing tmux panes: {err}")))?;
    if !output.status.success() {
        return Ok(false);
    }
    let pane_states = String::from_utf8_lossy(&output.stdout);
    Ok(pane_states.lines().any(|line| line.trim() == "0"))
}

pub(super) fn tmux_spawn_or_respawn_window(
    session: &str,
    window: &str,
    script_path: &Path,
    remain_on_exit: bool,
) -> Result<(), DispatchError> {
    let cmd = format!("bash {}", shell_quote(&script_path.to_string_lossy()));
    if tmux_has_session(session)? {
        if tmux_has_window(session, window)? {
            let status = Command::new("tmux")
                .args([
                    "respawn-window",
                    "-k",
                    "-t",
                    &format!("{session}:{window}"),
                    &cmd,
                ])
                .status()
                .map_err(|err| {
                    DispatchError::Tmux(format!("failed respawning tmux window: {err}"))
                })?;
            if !status.success() {
                return Err(DispatchError::Tmux(format!(
                    "tmux respawn-window failed for {session}:{window}"
                )));
            }
        } else {
            let status = Command::new("tmux")
                .args([
                    "new-window",
                    "-d",
                    "-t",
                    &format!("{session}:"),
                    "-n",
                    window,
                    &cmd,
                ])
                .status()
                .map_err(|err| {
                    DispatchError::Tmux(format!("failed creating tmux window: {err}"))
                })?;
            if !status.success() {
                return Err(DispatchError::Tmux(format!(
                    "tmux new-window failed for {session}:{window}"
                )));
            }
        }
    } else {
        let status = Command::new("tmux")
            .args(["new-session", "-d", "-s", session, "-n", window, &cmd])
            .status()
            .map_err(|err| DispatchError::Tmux(format!("failed creating tmux session: {err}")))?;
        if !status.success() {
            return Err(DispatchError::Tmux(format!(
                "tmux new-session failed for {session}:{window}"
            )));
        }
    }
    tmux_set_remain_on_exit(session, remain_on_exit)?;
    Ok(())
}

pub(super) fn build_tmux_exec_run_script(inputs: &TmuxRunScriptInputs<'_>) -> String {
    let forgejo_config_file = inputs
        .forgejo_config_file
        .map_or(Cow::Borrowed(""), |path| path.to_string_lossy());
    format!(
        r#"#!/usr/bin/env bash
set -euo pipefail

DISPATCH_ID={dispatch_id}
DB_PATH={db_path}
LOCK_PATH={lock_path}
RUN_DIR={run_dir}
PROMPT_FILE={prompt_file}
SUMMARY_FILE={summary_file}
COMPLETION_FILE={completion_file}
LAST_MESSAGE_FILE={last_message_file}
CODEX_LOG_FILE={codex_log_file}
MARKER_FILE={marker_file}
ISSUE_REF={issue_ref}
ISSUE_TITLE={issue_title}
ISSUE_URL={issue_url}
ORCHD_BIN={orchd_bin}
FORGEJOCTL_BIN={forgejoctl_bin}
FORGEJO_CONFIG_FILE={forgejo_config_file}
TOKEN_FILE={token_file}
WORKDIR={workdir}
CODEX_SANDBOX={codex_sandbox}
GIT_WORKDIR={git_workdir}
GIT_REMOTE={git_remote}
GIT_BASE={git_base}
GIT_BRANCH={git_branch}
CODEX_BIN={codex_bin}
CODEX_ROLE_ARG={codex_role_arg}
ISSUE_SESSION_ID={issue_session_id}
DIRECTIVE={directive}
ROLE_NAME={role_name}
TMUX_LOCATOR={tmux_locator}
TIMEOUT_SEC={timeout_sec}

cleanup() {{
  rm -f "$LOCK_PATH"
}}
trap cleanup EXIT

touch "$MARKER_FILE"
cd "$WORKDIR"
: > "$CODEX_LOG_FILE"

run_codex_fresh() {{
  cat "$PROMPT_FILE" \
    | timeout --preserve-status "$TIMEOUT_SEC" "$CODEX_BIN" "$CODEX_ROLE_ARG" --sandbox "$CODEX_SANDBOX" --cd "$WORKDIR" --no-alt-screen exec --skip-git-repo-check -o "$LAST_MESSAGE_FILE" - \
      2>&1 | tee -a "$CODEX_LOG_FILE"
}}

set +e
if [[ -n "$ISSUE_SESSION_ID" ]]; then
  cat "$PROMPT_FILE" \
    | timeout --preserve-status "$TIMEOUT_SEC" "$CODEX_BIN" "$CODEX_ROLE_ARG" --sandbox "$CODEX_SANDBOX" --cd "$WORKDIR" --no-alt-screen exec -o "$LAST_MESSAGE_FILE" resume --skip-git-repo-check "$ISSUE_SESSION_ID" - \
      2>&1 | tee -a "$CODEX_LOG_FILE"
  exit_code=$?
  if [[ "$exit_code" -ne 0 && "$exit_code" -ne 124 ]]; then
    echo "orchd: resume failed for issue session $ISSUE_SESSION_ID, falling back to fresh exec" | tee -a "$CODEX_LOG_FILE"
    run_codex_fresh
    exit_code=$?
  fi
else
  run_codex_fresh
  exit_code=$?
fi
set -e

session_id="$(sed -n 's/^session id: //p' "$CODEX_LOG_FILE" | tail -n 1)"
if [[ -z "$session_id" ]]; then
  session_id="$(find "$HOME/.codex/sessions" -type f -name '*.jsonl' -newer "$MARKER_FILE" 2>/dev/null | sort | tail -n 1 | sed -n 's#.*-\\([0-9a-fA-F-]\\{{36\\}}\\)\\.jsonl#\\1#p')"
fi

if [[ -s "$LAST_MESSAGE_FILE" ]]; then
  head -n 120 "$LAST_MESSAGE_FILE" > "$SUMMARY_FILE"
else
  echo "(no final assistant message)" > "$SUMMARY_FILE"
fi

if [[ "$exit_code" -eq 0 ]]; then
  status="completed"
  reason_code="completed"
elif [[ "$exit_code" -eq 124 ]]; then
  status="timed_out"
  reason_code="timeout"
else
  status="failed_runtime"
  reason_code="codex_exit_nonzero"
fi

if [[ "$status" == "completed" ]]; then
  runtime_state="completed"
else
  runtime_state="failed"
fi

if [[ -n "$session_id" ]]; then
  session_for_finalize="$session_id"
else
  session_for_finalize=""
fi

{{
  echo "orchd: dispatch completed id=$DISPATCH_ID status=$status reason=$reason_code"
  echo "directive=$DIRECTIVE role=$ROLE_NAME"
  echo "tmux=$TMUX_LOCATOR"
  echo "codex_session_id=${{session_id:-unknown}}"
  echo "run_dir=$RUN_DIR"
  echo "log=$CODEX_LOG_FILE"
  echo
  echo '```markdown'
  cat "$SUMMARY_FILE"
  echo '```'
}} > "$COMPLETION_FILE"

config_args=()
if [[ -n "$FORGEJO_CONFIG_FILE" ]]; then
  config_args+=(--forgejo-config "$FORGEJO_CONFIG_FILE")
fi

"$ORCHD_BIN" finalize-dispatch "${{config_args[@]}}" \
  --db-path "$DB_PATH" \
  --dispatch-id "$DISPATCH_ID" \
  --status "$status" \
  --reason-code "$reason_code" \
  --exit-code "$exit_code" \
  --session-id "$session_for_finalize" \
  --issue-ref "$ISSUE_REF" \
  --issue-title "$ISSUE_TITLE" \
  --issue-url "$ISSUE_URL" \
  --directive "$DIRECTIVE" \
  --role-name "$ROLE_NAME" \
  --tmux-locator "$TMUX_LOCATOR" \
  --run-dir "$RUN_DIR" \
  --log-file "$CODEX_LOG_FILE" \
  --completion-file "$COMPLETION_FILE" \
  --git-workdir "$GIT_WORKDIR" \
  --git-remote "$GIT_REMOTE" \
  --git-base "$GIT_BASE" \
  --git-branch "$GIT_BRANCH" \
  --forgejoctl-bin "$FORGEJOCTL_BIN" \
  --token-file "$TOKEN_FILE" || true
"#,
        dispatch_id = inputs.dispatch_id,
        db_path = shell_quote(&inputs.db_path.to_string_lossy()),
        lock_path = shell_quote(&inputs.lock_path.to_string_lossy()),
        run_dir = shell_quote(&inputs.run_dir.to_string_lossy()),
        prompt_file = shell_quote(&inputs.prompt_path.to_string_lossy()),
        summary_file = shell_quote(&inputs.summary_path.to_string_lossy()),
        completion_file = shell_quote(&inputs.completion_path.to_string_lossy()),
        last_message_file = shell_quote(&inputs.last_message_path.to_string_lossy()),
        codex_log_file = shell_quote(&inputs.codex_log_path.to_string_lossy()),
        marker_file = shell_quote(&inputs.marker_path.to_string_lossy()),
        issue_ref = shell_quote(inputs.issue_ref_text),
        issue_title = shell_quote(inputs.issue_title),
        issue_url = shell_quote(inputs.issue_url),
        orchd_bin = shell_quote(&inputs.orchd_bin.to_string_lossy()),
        forgejoctl_bin = shell_quote(&inputs.forgejoctl_bin.to_string_lossy()),
        forgejo_config_file = shell_quote(forgejo_config_file.as_ref()),
        token_file = shell_quote(&inputs.token_file.to_string_lossy()),
        workdir = shell_quote(&inputs.workdir.to_string_lossy()),
        codex_sandbox = shell_quote(inputs.codex_sandbox),
        git_workdir = shell_quote(&inputs.workdir.to_string_lossy()),
        git_remote = shell_quote(inputs.git_remote),
        git_base = shell_quote(inputs.git_base),
        git_branch = shell_quote(inputs.git_branch),
        codex_bin = shell_quote(&inputs.codex_bin.to_string_lossy()),
        codex_role_arg = shell_quote(inputs.codex_role_arg),
        issue_session_id = shell_quote(inputs.issue_session_id.unwrap_or("")),
        directive = shell_quote(inputs.directive_name),
        role_name = shell_quote(inputs.role_name),
        tmux_locator = shell_quote(inputs.tmux_locator),
        timeout_sec = inputs.timeout_sec,
    )
}

pub(super) fn build_tmux_tui_run_script(
    inputs: &TmuxRunScriptInputs<'_>,
    bootstrap_prompt_path: &Path,
    session_jsonl_path: &Path,
) -> String {
    let forgejo_config_file = inputs
        .forgejo_config_file
        .map_or(Cow::Borrowed(""), |path| path.to_string_lossy());
    format!(
        r#"#!/usr/bin/env bash
set -euo pipefail

DISPATCH_ID={dispatch_id}
DB_PATH={db_path}
LOCK_PATH={lock_path}
RUN_DIR={run_dir}
PROMPT_FILE={prompt_file}
BOOTSTRAP_PROMPT_FILE={bootstrap_prompt_file}
SESSION_JSONL_FILE={session_jsonl_file}
SUMMARY_FILE={summary_file}
COMPLETION_FILE={completion_file}
LAST_MESSAGE_FILE={last_message_file}
CODEX_LOG_FILE={codex_log_file}
MARKER_FILE={marker_file}
ISSUE_REF={issue_ref}
ISSUE_TITLE={issue_title}
ISSUE_URL={issue_url}
ORCHD_BIN={orchd_bin}
FORGEJOCTL_BIN={forgejoctl_bin}
FORGEJO_CONFIG_FILE={forgejo_config_file}
TOKEN_FILE={token_file}
WORKDIR={workdir}
CODEX_SANDBOX={codex_sandbox}
GIT_WORKDIR={git_workdir}
GIT_REMOTE={git_remote}
GIT_BASE={git_base}
GIT_BRANCH={git_branch}
CODEX_BIN={codex_bin}
CODEX_ROLE_ARG={codex_role_arg}
ISSUE_SESSION_ID={issue_session_id}
DIRECTIVE={directive}
ROLE_NAME={role_name}
TMUX_LOCATOR={tmux_locator}
TIMEOUT_SEC={timeout_sec}

cleanup() {{
  rm -f "$LOCK_PATH"
}}
trap cleanup EXIT

touch "$MARKER_FILE"
cd "$WORKDIR"
: > "$CODEX_LOG_FILE"

run_codex_fresh() {{
  echo "user"
  cat "$BOOTSTRAP_PROMPT_FILE"
  echo
  echo "user"
  cat "$PROMPT_FILE" \
    | timeout --preserve-status "$TIMEOUT_SEC" "$CODEX_BIN" "$CODEX_ROLE_ARG" --sandbox "$CODEX_SANDBOX" --cd "$WORKDIR" tui --skip-git-repo-check --bootstrap-file "$BOOTSTRAP_PROMPT_FILE" --prompt-file "$PROMPT_FILE" --session-jsonl "$SESSION_JSONL_FILE" -o "$LAST_MESSAGE_FILE" \
      2>&1 | tee -a "$CODEX_LOG_FILE"
}}

set +e
if [[ -n "$ISSUE_SESSION_ID" ]]; then
  echo "user"
  cat "$BOOTSTRAP_PROMPT_FILE"
  echo
  echo "user"
  cat "$PROMPT_FILE" \
    | timeout --preserve-status "$TIMEOUT_SEC" "$CODEX_BIN" "$CODEX_ROLE_ARG" --sandbox "$CODEX_SANDBOX" --cd "$WORKDIR" tui resume --skip-git-repo-check "$ISSUE_SESSION_ID" --bootstrap-file "$BOOTSTRAP_PROMPT_FILE" --prompt-file "$PROMPT_FILE" --session-jsonl "$SESSION_JSONL_FILE" -o "$LAST_MESSAGE_FILE" \
      2>&1 | tee -a "$CODEX_LOG_FILE"
  exit_code=$?
  if [[ "$exit_code" -ne 0 && "$exit_code" -ne 124 ]]; then
    echo "orchd: resume failed for issue session $ISSUE_SESSION_ID, falling back to fresh tui" | tee -a "$CODEX_LOG_FILE"
    run_codex_fresh
    exit_code=$?
  fi
else
  run_codex_fresh
  exit_code=$?
fi
set -e

session_id="$(sed -n 's/^session id: //p' "$CODEX_LOG_FILE" | tail -n 1)"
if [[ -z "$session_id" ]]; then
  session_id="$(find "$HOME/.codex/sessions" -type f -name '*.jsonl' -newer "$MARKER_FILE" 2>/dev/null | sort | tail -n 1 | sed -n 's#.*-\\([0-9a-fA-F-]\\{{36\\}}\\)\\.jsonl#\\1#p')"
fi

if [[ -s "$LAST_MESSAGE_FILE" ]]; then
  head -n 120 "$LAST_MESSAGE_FILE" > "$SUMMARY_FILE"
else
  echo "(no final assistant message)" > "$SUMMARY_FILE"
fi

if [[ "$exit_code" -eq 0 ]]; then
  status="completed"
  reason_code="completed"
elif [[ "$exit_code" -eq 124 ]]; then
  status="timed_out"
  reason_code="timeout"
else
  status="failed_runtime"
  reason_code="codex_exit_nonzero"
fi

if [[ "$status" == "completed" ]]; then
  runtime_state="completed"
else
  runtime_state="failed"
fi

if [[ -n "$session_id" ]]; then
  session_for_finalize="$session_id"
else
  session_for_finalize=""
fi

{{
  echo "orchd: dispatch completed id=$DISPATCH_ID status=$status reason=$reason_code"
  echo "directive=$DIRECTIVE role=$ROLE_NAME"
  echo "tmux=$TMUX_LOCATOR"
  echo "codex_session_id=${{session_id:-unknown}}"
  echo "run_dir=$RUN_DIR"
  echo "log=$CODEX_LOG_FILE"
  echo
  echo '```markdown'
  cat "$SUMMARY_FILE"
  echo '```'
}} > "$COMPLETION_FILE"

config_args=()
if [[ -n "$FORGEJO_CONFIG_FILE" ]]; then
  config_args+=(--forgejo-config "$FORGEJO_CONFIG_FILE")
fi

"$ORCHD_BIN" finalize-dispatch "${{config_args[@]}}" \
  --db-path "$DB_PATH" \
  --dispatch-id "$DISPATCH_ID" \
  --status "$status" \
  --reason-code "$reason_code" \
  --exit-code "$exit_code" \
  --session-id "$session_for_finalize" \
  --issue-ref "$ISSUE_REF" \
  --issue-title "$ISSUE_TITLE" \
  --issue-url "$ISSUE_URL" \
  --directive "$DIRECTIVE" \
  --role-name "$ROLE_NAME" \
  --tmux-locator "$TMUX_LOCATOR" \
  --run-dir "$RUN_DIR" \
  --log-file "$CODEX_LOG_FILE" \
  --completion-file "$COMPLETION_FILE" \
  --git-workdir "$GIT_WORKDIR" \
  --git-remote "$GIT_REMOTE" \
  --git-base "$GIT_BASE" \
  --git-branch "$GIT_BRANCH" \
  --forgejoctl-bin "$FORGEJOCTL_BIN" \
  --token-file "$TOKEN_FILE" || true
"#,
        dispatch_id = inputs.dispatch_id,
        db_path = shell_quote(&inputs.db_path.to_string_lossy()),
        lock_path = shell_quote(&inputs.lock_path.to_string_lossy()),
        run_dir = shell_quote(&inputs.run_dir.to_string_lossy()),
        prompt_file = shell_quote(&inputs.prompt_path.to_string_lossy()),
        bootstrap_prompt_file = shell_quote(&bootstrap_prompt_path.to_string_lossy()),
        session_jsonl_file = shell_quote(&session_jsonl_path.to_string_lossy()),
        summary_file = shell_quote(&inputs.summary_path.to_string_lossy()),
        completion_file = shell_quote(&inputs.completion_path.to_string_lossy()),
        last_message_file = shell_quote(&inputs.last_message_path.to_string_lossy()),
        codex_log_file = shell_quote(&inputs.codex_log_path.to_string_lossy()),
        marker_file = shell_quote(&inputs.marker_path.to_string_lossy()),
        issue_ref = shell_quote(inputs.issue_ref_text),
        issue_title = shell_quote(inputs.issue_title),
        issue_url = shell_quote(inputs.issue_url),
        orchd_bin = shell_quote(&inputs.orchd_bin.to_string_lossy()),
        forgejoctl_bin = shell_quote(&inputs.forgejoctl_bin.to_string_lossy()),
        forgejo_config_file = shell_quote(forgejo_config_file.as_ref()),
        token_file = shell_quote(&inputs.token_file.to_string_lossy()),
        workdir = shell_quote(&inputs.workdir.to_string_lossy()),
        codex_sandbox = shell_quote(inputs.codex_sandbox),
        git_workdir = shell_quote(&inputs.workdir.to_string_lossy()),
        git_remote = shell_quote(inputs.git_remote),
        git_base = shell_quote(inputs.git_base),
        git_branch = shell_quote(inputs.git_branch),
        codex_bin = shell_quote(&inputs.codex_bin.to_string_lossy()),
        codex_role_arg = shell_quote(inputs.codex_role_arg),
        issue_session_id = shell_quote(inputs.issue_session_id.unwrap_or("")),
        directive = shell_quote(inputs.directive_name),
        role_name = shell_quote(inputs.role_name),
        tmux_locator = shell_quote(inputs.tmux_locator),
        timeout_sec = inputs.timeout_sec,
    )
}
