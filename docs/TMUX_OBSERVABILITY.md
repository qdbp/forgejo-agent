# tmux Observability for orchd

This is the operator view of dispatched Codex runs.

## Mental model

- `orchd` uses one named tmux session (from `config/orchd-dispatch.toml`).
- Each dispatch becomes a tmux window in that session.
- Dispatch metadata lives in sqlite (`dispatches` table), while the terminal view
  lives in tmux.
- Run artifacts are written under:
  `~/.local/state/orchd-dev/dispatch-runs/dispatch-<id>/`.

## Fast path: "what is running right now?"

```bash
tmux list-sessions
tmux list-windows -t codex-orch
sqlite3 ~/.local/state/orchd-dev/orchd.sqlite \
  "SELECT id,directive,status,tmux_session,tmux_window,codex_session_id,exit_code FROM dispatches ORDER BY id DESC LIMIT 20;"
```

## Attach and watch

Attach to session:

```bash
tmux attach -t codex-orch
```

Useful keys once attached:

- `Ctrl-b n`: next window
- `Ctrl-b p`: previous window
- `Ctrl-b w`: interactive window list
- `Ctrl-b d`: detach without killing anything

Attach read-only (safe observer mode):

```bash
tmux attach -r -t codex-orch
```

Expected behavior: dispatched Codex output streams live in the pane and is also
written to `dispatch-<id>/codex.log`.

## Inspect finished runs

If `remain_on_exit` is enabled, exited windows stay visible. For any dispatch:

```bash
id=1
run_dir="$HOME/.local/state/orchd-dev/dispatch-runs/dispatch-$id"
ls -la "$run_dir"
tail -n 80 "$run_dir/codex.log"
cat "$run_dir/last_message.md" 2>/dev/null || true
```

Cross-check sqlite status:

```bash
sqlite3 ~/.local/state/orchd-dev/orchd.sqlite \
  "SELECT id,status,reason_code,codex_session_id,exit_code,started_at,ended_at FROM dispatches WHERE id=$id;"
```

## Capture tmux output without attaching

```bash
tmux list-panes -t codex-orch -F '#{session_name}:#{window_name} #{pane_id}'
tmux capture-pane -pt codex-orch:0 | tail -n 120
```

## Troubleshooting

Black pane until exit:

- Cause: older `orchd` generated run scripts that redirected all output to file.
- Fix: restart `orchd` on the latest build, then dispatch again.
- Verify: new `dispatch-<id>/run.sh` contains `| tee "$CODEX_LOG_FILE"`.

## Session lifecycle patterns

tmux supports all of the following, and orchd can own any/all of them:

- Dedicated long-lived session:
  - Keep one session name per orchestrator (`codex-orch`) and only manage windows.
- Ephemeral session:
  - Create session on first dispatch, kill it when empty.
- Hybrid:
  - Keep session alive in dev mode, reap automatically in prod mode.

Operations available to orchd:

- create session/window: `new-session`, `new-window`
- check existence: `has-session`
- keep exited windows: `set-option remain-on-exit on`
- kill finished windows: `kill-window`
- kill empty session: `kill-session`
- rename windows for stable references: `rename-window`

## Persistence expectations

- tmux state persists while tmux server is alive.
- tmux state does not survive reboot unless you add extra tooling.
- Durable run history should come from sqlite + run artifacts, not tmux alone.

## Recommended operator policy

- Treat sqlite as source of truth for status and IDs.
- Treat tmux as live observability + interactive debugging.
- Keep `remain_on_exit=true` during rapid development.
- Later, add orchd-driven reap policy (for example: close completed windows older
  than N hours).
