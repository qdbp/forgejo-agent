# orchd Dev Mode

`orchd` is the reactive orchestrator prototype binary.

Current implementation supports two dispatch modes:

- `dry-run`: parse directives, persist decisions. No metadata comments are posted to issues.
- `exec` (default): create a dispatch record and run Codex non-interactively (`codex exec`).

## Design Dogma

- Issue comments are meat-only. `orchd` must never post metadata into issue comments.
- `orchd` projects orchestration state out-of-band via labels (`orchd/state/*`) and (for worktree directives) via work-plane transitions (`state/*`).
- Agents should use `forgejoctl` as the control plane API surface.

Core behavior:

- accepts webhook events at `POST /webhook`
- parses directives (`@codex-orch design`, `@codex-orch impl`, `@codex-orch reply`, `@codex poke`)
- persists `events`, `decisions`, and `dispatches` in sqlite
- ensures per-repo Forgejo webhooks exist (best-effort) for repos owned by `FORGEJO_DEFAULT_OWNER`
- ensures per-repo policy labels exist (best-effort) and maintains per-role local checkouts under the orchd state dir
- emits heartbeat + reconcile logs periodically
- dispatches `reply` implicitly on new issue comments when the issue has exactly one assignee and it is a `codex-*` user (unless an explicit directive is present)

Dispatch behavior is configured in `config/orchd-dispatch.toml`:

- actor allowlist (`allowed_actors`)
- directive -> role mapping
- role -> codex binary / token file / Forgejo login mapping
- prompt envelopes (`prompt_envelopes.fresh_envelope`, `prompt_envelopes.followup_envelope`)
- control-plane command path (`forgejoctl_bin`)

## Run

`exec` mode (default, restart-resilient via transient user units):

```bash
cargo run --bin orchd -- \
  --listen 127.0.0.1:7878 \
  --db-path ~/.local/state/orchd-dev/orchd.sqlite \
  --reconcile-repo main/orchd-debug \
  --heartbeat-sec 15 \
  --reconcile-sec 45 \
  --dispatch-mode exec \
  --dispatch-backend systemd \
  --dispatch-config /home/main/forgejo-agent/config/orchd-dispatch.toml
```

`exec` mode with local backend (test/dev):

```bash
cargo run --bin orchd -- \
  --listen 127.0.0.1:7878 \
  --db-path ~/.local/state/orchd-dev/orchd.sqlite \
  --reconcile-repo main/orchd-debug \
  --heartbeat-sec 15 \
  --reconcile-sec 45 \
  --dispatch-mode exec \
  --dispatch-backend local \
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
  "SELECT id,repo_full_name,issue_number,actor_login,directive,target_role,status,backend_kind,backend_ref,codex_session_id,exit_code,started_at,ended_at FROM dispatches ORDER BY id DESC LIMIT 20;"
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
  "comment":{"body":"@codex-orch reply","user":{"login":"main"}},
  "sender":{"login":"main"}
}'

curl -fsS -X POST http://127.0.0.1:7878/webhook \
  -H 'Content-Type: application/json' \
  -H 'X-Forgejo-Event: issue_comment' \
  -H 'X-Forgejo-Delivery: dev-smoke-1' \
  --data "$body" | jq .
```

Expected in `exec` mode:

- response: `decision=accepted`, `reason_code=explicit_directive`
- issue runtime labels: `orchd/state/queued` then `orchd/state/running`
- sqlite `dispatches` row with `status=running` then terminal status
- completion status is projected to `orchd/state/completed` on success or `orchd/state/failed` otherwise
- for `impl` directive runs, orchd applies work-plane transitions (`state/review` on success, `state/blocked` on non-success) via `forgejoctl issue transition --force`
- generated run artifacts include `prompt.md` and `prompt_mode.txt` (`fresh` or `followup`)
- follow-up prompts include an issue delta block derived from events newer than the role cursor
- stale in-flight rows are healed on startup and before launch attempts (status set to `failed_runtime`, reason `stale_dispatch_autohealed`) when the backend handle is no longer live
- repo lockfiles under `locks/` are metadata only; dispatch gating is driven by sqlite `dispatches` state
