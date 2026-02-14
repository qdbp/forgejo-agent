# orchd Dev Mode

`orchd` is the reactive orchestrator prototype binary.

Current implementation supports two dispatch modes:

- `dry-run`: parse directives, persist decisions, comment intent only.
- `tmux-tui` (default): create a dispatch record and spawn interactive Codex TUI inside tmux.
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
- issue comment: `orchd: dispatch started ...`
- sqlite `dispatches` row with `status=running` then terminal status
- tmux window `r<repo_slug>-i<issue_number>` created (or respawned) under the configured session
- completion in sqlite/comment is keyed off a `final_answer` message found in session JSONL when present
