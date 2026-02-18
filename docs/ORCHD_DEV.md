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
- compiles trigger rules from `orchd-dispatch.toml` into typed matchers/guards/actions
- persists `events`, `decisions`, and `dispatches` in sqlite
- ensures per-repo Forgejo webhooks exist (best-effort) for repos owned by `FORGEJO_DEFAULT_OWNER`
- ensures per-repo policy labels exist (best-effort) and maintains per-role local checkouts under the orchd state dir
- emits heartbeat + reconcile logs periodically
- evaluates trigger precedence as:
  - explicit directive triggers
  - implicit assignee-reply trigger
  - registered config triggers
- enforces logical trigger dedupe keys so replayed deliveries do not re-dispatch the same logical event
- enforces anti-spiral guardrails on trigger-fired dispatches (`depth`, `rate`, `cooldown`, `self-loop`)
- supports postmortem session inspection/re-entry via `orchd issue sessions|resume`
- provides role hygiene subcommands:
  - `orchd role list`
  - `orchd role check [--role <name>]`
  - `orchd role add ...`
- enforces role integrity on `exec` startup (same checks as `orchd role check`; override with `--skip-startup-role-check` only for break-glass recovery)

Dispatch behavior is configured in `config/orchd-dispatch.toml`:

- actor allowlist (`allowed_actors`)
- directive -> role mapping
- role -> codex binary / token file / Forgejo login mapping
- trigger guardrails (`trigger_guardrails.*`)
- registered trigger list (`[[triggers]]`) with matcher + guards + action
- optional legacy trigger pack toggle (`legacy_triggers`, defaults true)
- prompt envelopes (`prompt_envelopes.fresh_envelope`, `prompt_envelopes.followup_envelope`)
- control-plane command path (`forgejoctl_bin`)

## Reading Material (DocPlan)

`orchd` can inject repo/global “reading material” into the *fresh* prompt envelope (never on follow-ups).

### Global Rules (orchd-owned)

In `config/orchd-dispatch.toml`:

```toml
[reading_material]
max_doc_bytes = 262144
max_total_bytes = 1048576

[[reading_material.rule]]
kind = "point"                       # include|point
ref = "repo:main/forgejo-agent:docs/AGENT_WORKFLOW.md"
roles = ["*"]
directives = ["*"]
order = 10
importance = "recommended"           # required|recommended
```

### Repo Rules (repo-owned)

In the *dispatched repo root*:

```toml
# .orchd/config.toml
[docs]

[[docs.rule]]
kind = "include"
ref = "workdir:docs/SECURITY.md"
roles = ["codex-dev"]
directives = ["impl"]
order = 10
importance = "required"
```

Notes:
- `workdir:` resolves relative to the repo being dispatched.
- `repo:` resolves only if the target repo exists in `repo_bindings` (so refs are grounded in known local checkouts).
- Missing/unreadable docs are warned (logs + `doc-plan.json`) and omitted from the prompt.

### Prompt Preview

Render the prompt (or inspect the DocPlan) without running a dispatch:

```bash
cargo run --bin orchd -- prompt preview main/foo#123 --role codex-dev --directive impl
cargo run --bin orchd -- prompt preview main/foo#123 --role codex-dev --directive impl --json
```

### Postmortem Artifacts

Each dispatch run dir includes `doc-plan.json` next to `prompt.md`.

## Trigger Spec

`orchd` has a single trigger engine. Legacy behavior (explicit directives + assignee reply)
is represented as built-in trigger rules by default (`legacy_triggers = true`), and custom
automation is added via `[[triggers]]`.

Custom trigger shape:

- `id`: unique identifier used for reason codes and dedupe
- `event`: webhook event family (`issues` or `issue_comment`)
- `actions`: accepted actions for that event family
- `priority`: tie-break within the same precedence tier
- `apply_guardrails`: enable anti-spiral guardrails for this rule
- `guards`: optional predicate block:
  - `directive = any|require_parsed|require_absent`
  - `assignee = any|require_single_codex`
  - `actor = any|require_not_assignee`
- `action`: dispatch action (`directive`/`directive_from`, `target_role`/`target_role_from`)

Precedence is deterministic and fixed:

1. explicit directive triggers
2. implicit assignee-reply trigger
3. registered config triggers

Within a tier, higher `priority` wins; ties break by file order.

### End-to-End Example (`issues.closed -> dispatch`)

```toml
[trigger_guardrails]
max_depth_per_issue = 6
max_dispatches_per_window = 12
window_sec = 3600
cooldown_sec = 60
deny_immediate_self_loop = true

[[triggers]]
id = "closed_issue_poke"
event = "issues"
actions = ["closed"]

[triggers.action]
directive = "reply"
target_role = "codex-orch"
```

With the above rule, closing an issue causes `orchd` to emit a normal dispatch intent
through the existing directive/role pipeline. Retried/replayed deliveries for the same
logical close event are suppressed by logical trigger dedupe.

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

## Postmortem Resume

List resumable sessions for an issue:

```bash
cargo run --bin orchd -- issue sessions forgejo-agent 20
```

Resume a specific session for an issue:

```bash
cargo run --bin orchd -- issue resume forgejo-agent 20 --role codex-lead -- --no-alt-screen
cargo run --bin orchd -- issue resume forgejo-agent 20 --dispatch-id 123 -- --no-alt-screen
```

Behavior is intentionally strict:

- owner is hardcoded to `main` (`forgejo-agent 20` maps to `main/forgejo-agent#20`)
- errors if any non-terminal dispatch exists for that issue
- errors if no `codex_session_id` exists in dispatch history
- errors if multiple role sessions exist and no `--role` or `--dispatch-id` is supplied
- never falls back to spawning a fresh Codex session

## Role Hygiene

List configured roles:

```bash
cargo run --bin orchd -- role list
```

Check role integrity (role cards, token/login mapping, Forgejo user posture):

```bash
cargo run --bin orchd -- role check
cargo run --bin orchd -- role check --role codex-dev --json
```

Add a role atomically (with rollback on local failure):

```bash
cargo run --bin orchd -- role add \
  --role codex-dev \
  --rank OF-2 \
  --forgejo-login codex-dev \
  --admin-token-file ~/.config/forgejo-agent/creds/codex-orch.token \
  --create-user
```

Role add template source:
- `templates/role-card-template.md`

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

Inspect trigger dedupe claims:

```bash
sqlite3 ~/.local/state/orchd-dev/orchd.sqlite \
  "SELECT id,trigger_id,dedupe_key,repo_full_name,issue_number,event_id,created_at FROM trigger_dispatch_dedupes ORDER BY id DESC LIMIT 20;"
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
