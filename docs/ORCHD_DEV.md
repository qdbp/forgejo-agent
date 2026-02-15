# orchd Dev Mode

`orchd` is the reactive orchestrator prototype binary.

Current implementation supports two dispatch modes:

- `dry-run`: parse directives, persist decisions, comment intent only.
- `tmux-tui` (default): create a dispatch record and spawn Codex TUI inside tmux; dispatch auto-stops after first `final_answer` so issue completion comments are not delayed behind long-lived interactive sessions.
- `tmux-exec`: create a dispatch record and run Codex non-interactively.

Core behavior:

- accepts webhook events at `POST /webhook`
- parses directives (`@codex-orch design`, `@codex-orch impl`, `@codex poke`)
- persists `events`, `decisions`, and `dispatches` in sqlite
- emits heartbeat + reconcile logs periodically
- ignores self-generated `orchd:` comments to avoid echo loops

Dispatch behavior is configured in `config/orchd-dispatch.toml`:

- actor allowlist (`allowed_actors`)
- directive -> role mapping
- role -> codex binary / token file / workdir mapping
- prompt envelopes (`prompt_envelopes.fresh_envelope_file`, `prompt_envelopes.followup_envelope_file`)
- tmux session naming and remain-on-exit policy

## Run

`tmux-tui` mode (default):

```bash
cargo run --bin orchd -- \
  --listen 127.0.0.1:7878 \
  --db-path ~/.local/state/orchd-dev/orchd.sqlite \
  --reconcile-repo main/orchd-debug \
  --heartbeat-sec 15 \
  --reconcile-sec 45 \
  --dispatch-config /home/main/forgejo-agent/config/orchd-dispatch.toml
```

`tmux-exec` mode:

```bash
cargo run --bin orchd -- \
  --listen 127.0.0.1:7878 \
  --db-path ~/.local/state/orchd-dev/orchd.sqlite \
  --reconcile-repo main/orchd-debug \
  --heartbeat-sec 15 \
  --reconcile-sec 45 \
  --dispatch-mode tmux-exec \
  --dispatch-config /home/main/forgejo-agent/config/orchd-dispatch.toml
```

`dry-run` mode:

```bash
cargo run --bin orchd -- \
  --listen 127.0.0.1:7878 \
  --db-path ~/.local/state/orchd-dev/orchd.sqlite \
  --reconcile-repo main/orchd-debug \
  --heartbeat-sec 15 \
  --reconcile-sec 45 \
  --dispatch-mode dry-run
```

## Health

```bash
curl -fsS http://127.0.0.1:7878/healthz | jq .
```

## Debug/Tail

Tail orchestrator logs:

```bash
tail -f ~/.local/state/orchd-dev/orchd.log
```

Inspect recent decisions:

```bash
sqlite3 ~/.local/state/orchd-dev/orchd.sqlite \
  "SELECT id,event_id,repo_full_name,issue_number,actor_login,directive,decision,reason_code,created_at FROM decisions ORDER BY id DESC LIMIT 20;"
```

Inspect recent dispatches:

```bash
sqlite3 ~/.local/state/orchd-dev/orchd.sqlite \
  "SELECT id,repo_full_name,issue_number,actor_login,directive,target_role,status,reason_code,tmux_session,tmux_window,codex_session_id,exit_code,started_at,ended_at FROM dispatches ORDER BY id DESC LIMIT 20;"
```

Inspect dispatch transition events:

```bash
sqlite3 ~/.local/state/orchd-dev/orchd.sqlite \
  "SELECT id,dispatch_id,event_kind,from_state,to_state,reason_code,created_at FROM dispatch_events ORDER BY id DESC LIMIT 50;"
```

Inspect role cursors used for follow-up deltas:

```bash
sqlite3 ~/.local/state/orchd-dev/orchd.sqlite \
  "SELECT repo_full_name,issue_number,role_name,last_event_id,updated_at FROM issue_role_cursors ORDER BY updated_at DESC LIMIT 20;"
```

Inspect issue-scoped tmux windows:

```bash
tmux list-sessions
tmux list-windows -t codex-orch
tmux attach -t codex-orch
```

For a human-operator guide (attach/rejoin/inspect lifecycle), see:

- `docs/TMUX_OBSERVABILITY.md`

## Webhook Secret (Optional)

If `--webhook-secret-file` is set, `orchd` verifies HMAC SHA-256 signatures from:

- `X-Forgejo-Signature`
- `X-Gitea-Signature`

Header values may be raw hex or `sha256=<hex>`.

## Local Smoke via curl

Synthetic event (useful when you want to test dispatch without creating a real
Forgejo comment):

```bash
body='{
  "action":"created",
  "repository":{"full_name":"main/orchd-debug"},
  "issue":{"number":1},
  "comment":{"body":"@codex-orch poke","user":{"login":"main"}},
  "sender":{"login":"main"}
}'

curl -fsS -X POST http://127.0.0.1:7878/webhook \
  -H 'Content-Type: application/json' \
  -H 'X-Forgejo-Event: issue_comment' \
  -H 'X-Forgejo-Delivery: dev-smoke-1' \
  --data "$body" | jq .
```

Expected in `tmux-tui` mode:

- response: `decision=accepted`, `reason_code=explicit_directive`
- issue runtime labels: `orchd/state/queued` then `orchd/state/running`
- sqlite `dispatches` row with `status=running` then terminal status
- tmux window `r<repo_slug>-i<issue_number>` created (or respawned) under the configured session
- tmux-tui run watcher monitors session JSONL and sends `Ctrl-C` after first `final_answer` to close the turn promptly
- auto-reap is skipped while the window is held (`@orchd_hold=1`) or actively focused in an attached tmux client
- completion status is projected to `orchd/state/completed` on success or `orchd/state/failed` otherwise
- for `impl` directive runs, orchd applies work-plane transitions (`state/review` on success, `state/blocked` on non-success) via `forgejoctl issue transition --force`
- completion in sqlite/comment is keyed off a `final_answer` message found in session JSONL when present
- generated run artifacts include `prompt.md` and `prompt_mode.txt` (`fresh` or `followup`)
- follow-up prompts include an issue delta block derived from events newer than the role cursor
- stale in-flight rows are lazily healed on the next launch attempt (status set to `failed_runtime`, reason `stale_dispatch_autohealed`) when tmux no longer has a live pane for that issue
- repo lockfiles under `locks/` are metadata only; dispatch gating is driven by sqlite `dispatches` state

Manual hold controls:

```bash
# pin window (do not auto-reap after final_answer)
tmux set-option -w -t codex-orch:rmain-orchd-debug-i2 @orchd_hold 1

# unpin window (allow auto-reap)
tmux set-option -u -w -t codex-orch:rmain-orchd-debug-i2 @orchd_hold
```
