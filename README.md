# Forgejo Agent Control Plane

Rust-first, policy-enforcing issue gateway for local multi-agent workflows.

## Principle

Agents should use one executable for Forgejo operations:

- `$HOME/.local/bin/forgejoctl`

Direct raw API poking is discouraged for agents.

## Swarm Home

Swarm doctrine and configuration are owned by the private `main/swarm` repo,
checked out at `SWARM_HOME` (default: `$HOME/swarm`).

Example:

- `SWARM_HOME=$HOME/swarm`

This is the canonical home for:

- `docs/` (swarm-wide runbooks and workflow contract)
- `prompts/` (role cards, orders, envelopes)
- `config/orchd-dispatch.toml` (orchd dispatch + reading material routing)

`forgejoctl` token discovery order:

1. `--token-file`
2. `FORGEJO_TOKEN_FILE` from process environment
3. systemd credential `CREDENTIALS_DIRECTORY/forgejo_token`
4. `FORGEJO_TOKEN_FILE` in config file
5. owner fallback token path (`~/.config/forgejo-agent/token`) only when `FORGEJO_ALLOW_OWNER_TOKEN=1` (break-glass only)

Automation policy:
- Do not run automation as `main`.
- Use dedicated principals (`orchd`, `codex-orch`, `codex-lead`, `codex-dev`) with dedicated tokens under `~/.config/forgejo-agent/creds/`.

## Install

```bash
./scripts/install.sh
```

This builds release and installs `forgejoctl` to `~/.local/bin/forgejoctl`.

## Push To Both Remotes

This repo now treats `origin` (GitHub) as primary and `forgejo` (local Forgejo)
as execution mirror.

Use:

```bash
./scripts/push-both.sh
```

That pushes the current ref to both remotes using `--force-with-lease`
(origin first, then forgejo with token-backed non-interactive auth).

## Interactive Role Isolation (No Wrappers)

Use transient systemd units with injected credentials:

```bash
sudo systemd-run \
  --unit=codex-dev-$(date +%s) \
  --collect \
  --wait \
  --pty \
  -p User=codex-dev \
  -p WorkingDirectory=/home/main/programming/projects/your-repo \
  -p LoadCredential=forgejo_token:/etc/forgejo-agent/creds/codex-dev.token \
  /usr/bin/bash -lc 'exec codex'
```

Inside that session, plain `forgejoctl ...` will automatically use the injected
`forgejo_token` credential only.

## Core Commands

```bash
# Ensure queue repo + canonical labels
/home/main/.local/bin/forgejoctl repo ensure main/backlog

# List/show issues
/home/main/.local/bin/forgejoctl issue list main/backlog --state open
/home/main/.local/bin/forgejoctl issue show main/backlog#1

# Create issue in explicit workflow state
/home/main/.local/bin/forgejoctl issue create main/backlog \
  --title "Implement X" \
  --body-file /home/main/forgejo-agent/templates/issue-template.md \
  --workflow ready \
  --label pri/med

# Edit existing issue title/body
/home/main/.local/bin/forgejoctl issue edit main/backlog#1 \
  --title "Reframe: Implement X safely" \
  --body-stdin <<'EOF'
## Context
...
EOF

# State transitions
/home/main/.local/bin/forgejoctl issue transition main/backlog#1 --to in-progress
/home/main/.local/bin/forgejoctl issue transition main/backlog#1 --to review

# orchd runtime state projection (exclusive orchd/state/* label)
/home/main/.local/bin/forgejoctl issue orchd-state main/backlog#1 --to running

# Claim/release and blocker flow
/home/main/.local/bin/forgejoctl issue claim main/backlog#1 --agent codex-dev
/home/main/.local/bin/forgejoctl issue release main/backlog#1 --agent codex-dev
/home/main/.local/bin/forgejoctl issue blocker main/backlog#1 --title "Need Y" --body "Details"

# Safe multiline body via stdin (no \n escaping footguns)
cat <<'EOF' | /home/main/.local/bin/forgejoctl issue comment main/backlog#1 --body-stdin
## Status
- implemented X
- validated Y
EOF
```

## Worker Mode

```bash
/home/main/.local/bin/forgejoctl worker run \
  --repo main/backlog \
  --workdir /home/main/programming/projects/your-repo \
  --execute \
  --interval-sec 45 \
  --agent codex-dev
```

## `orchd` Dev Mode

`orchd` is a separate binary for reactive orchestration prototyping.

Current mode supports both:

- `dry-run`: parse directives and persist decisions only (no dispatch launch)
- `exec` (default): explicit non-interactive dispatch (`codex exec`)

Core behavior:

- webhook ingest
- directive parse (`@codex-orch design|investigate|impl|reply`; `poke` is an alias for `reply`, and `@codex` aliases to `@codex-orch`)
- sqlite event/decision/dispatch persistence
- runtime dispatch status projected to `orchd/state/*` labels (`queued|running|blocked|failed|completed`)
- one active dispatch per issue (duplicates blocked while running)
- issue+role-scoped codex session reuse (`codex exec resume`) from latest `codex_session_id`
- periodic heartbeat + reconcile scan logs
- postmortem issue session inspection/resume via `orchd issue sessions` and `orchd issue resume`

Role launch wrapper:

- `orchd` dispatches Codex through the repo-managed wrapper `bin/codex-role` (configured in `/home/main/swarm/config/orchd-dispatch.toml`), not by clobbering `/usr/bin/codex`.
- `orchd` service auth uses `~/.config/forgejo-agent/creds/orchd.token` so orchestrator actions never impersonate `main`.

Role hygiene commands:

```bash
cargo run --bin orchd -- role list
cargo run --bin orchd -- role check
cargo run --bin orchd -- role check --role codex-dev --json
```

`exec` startup enforces role integrity by default (`orchd role check` equivalent).
Use `--skip-startup-role-check` only as an emergency recovery override.

Run (`exec`, default + restart-resilient backend):

```bash
cargo run --bin orchd -- \
  --token-file ~/.config/forgejo-agent/creds/orchd.token \
  --listen 127.0.0.1:7878 \
  --db-path ~/.local/state/orchd-dev/orchd.sqlite \
  --reconcile-repo main/orchd-debug \
  --heartbeat-sec 15 \
  --reconcile-sec 45 \
  --dispatch-mode exec \
  --dispatch-backend systemd \
  --dispatch-config /home/main/swarm/config/orchd-dispatch.toml
```

Run (`exec` + local backend, convenient for tests):

```bash
cargo run --bin orchd -- \
  --token-file ~/.config/forgejo-agent/creds/orchd.token \
  --listen 127.0.0.1:7878 \
  --db-path ~/.local/state/orchd-dev/orchd.sqlite \
  --reconcile-repo main/orchd-debug \
  --heartbeat-sec 15 \
  --reconcile-sec 45 \
  --dispatch-mode exec \
  --dispatch-backend local \
  --dispatch-config /home/main/swarm/config/orchd-dispatch.toml
```

Run as user service (recommended for easy restart/visibility):

```bash
/home/main/forgejo-agent/scripts/install.sh
/home/main/forgejo-agent/scripts/install-orchd-user-service.sh
```

Then:

```bash
XDG_RUNTIME_DIR=/run/user/$(id -u) DBUS_SESSION_BUS_ADDRESS=unix:path=/run/user/$(id -u)/bus systemctl --user status orchd.service --no-pager
XDG_RUNTIME_DIR=/run/user/$(id -u) DBUS_SESSION_BUS_ADDRESS=unix:path=/run/user/$(id -u)/bus systemctl --user restart orchd.service
XDG_RUNTIME_DIR=/run/user/$(id -u) DBUS_SESSION_BUS_ADDRESS=unix:path=/run/user/$(id -u)/bus journalctl --user -u orchd.service -f
```

Run (`dry-run`):

```bash
cargo run --bin orchd -- \
  --token-file ~/.config/forgejo-agent/creds/orchd.token \
  --listen 127.0.0.1:7878 \
  --db-path ~/.local/state/orchd-dev/orchd.sqlite \
  --reconcile-repo main/orchd-debug \
  --heartbeat-sec 15 \
  --reconcile-sec 45 \
  --dispatch-mode dry-run
```

Postmortem resume by issue (owner is fixed to `main`):

```bash
cargo run --bin orchd -- issue sessions forgejo-agent 20
cargo run --bin orchd -- issue resume forgejo-agent 20 --role codex-lead -- --no-alt-screen
cargo run --bin orchd -- issue resume forgejo-agent 20 --dispatch-id 123 -- --no-alt-screen
```

`issue resume` fails fast when:

- the issue has any non-terminal dispatch (`queued|launching|starting|running`)
- no `codex_session_id` has been recorded for the issue
- multiple role sessions exist and neither `--role` nor `--dispatch-id` is supplied

Health check:

```bash
curl -fsS http://127.0.0.1:7878/healthz | jq .
```

The `/healthz` response includes the running build identifier (`git describe`
when available), so you can quickly tell whether the user service is running
the latest installed binary.

## Quality Gate

For Rust changes in this repo, run:

```bash
python3 /home/main/forgejo-agent/scripts/check.py
```

## Repo Assimilation

To onboard another repository into orchd/Forgejo "it just works" mode, use the
single-package runbook:

- `/home/main/swarm/docs/REPO_ASSIMILATION.md`

Primary command:

```bash
/home/main/forgejo-agent/scripts/assimilate-repo.sh \
  --repo owner/repo \
  --local-path /home/main/programming/projects/repo
```

The assimilation script runs `orchd role check` preflight by default. Bypass only
for break-glass recovery with `--skip-role-check-preflight`.

## Live Forgejo Integration Test

`tests/live_forgejo.rs` provides the first full round-trip test:

- starts a self-contained Forgejo instance in a temp directory
- migrates DB + creates admin user/token
- runs `forgejoctl repo ensure`
- runs `forgejoctl issue create`
- verifies issue fields via direct Forgejo API read-back

Default `cargo test` keeps this test inert unless explicitly enabled.

Run it manually:

```bash
cd /home/main/forgejo-agent
FORGEJO_LIVE_TESTS=1 cargo test --test live_forgejo -- --nocapture
```

Timing collection:

- each step appends JSONL timing entries to `target/live-test-timings.jsonl`
- override sink path with `FORGEJO_LIVE_TIMINGS_PATH=/path/to/file.jsonl`
- keep fixture dirs for debugging with `FORGEJO_LIVE_KEEP_FIXTURE=1`
- summarize timings with `python3 scripts/live_timing_report.py`

## Git Hooks

Install repo-managed hooks:

```bash
/home/main/forgejo-agent/scripts/install-git-hooks.sh
```

Installed hooks:

- `pre-commit`: runs `scripts/check.py`
- `pre-push`: runs `scripts/check.py`
- `post-commit`: runs `scripts/deploy-local.sh` (builds + installs `forgejoctl` + `orchd`)
- `post-merge`: runs `scripts/deploy-local.sh` (keeps artifacts deployed after pulls/merges)

`check.py` includes a skill/API sync enforcement hook:

- verifies `forgejoctl` CLI surface snapshot/hash
- requires matching hash + command block in `~/.codex/skills/forgejoctl/SKILL.md`
- requires matching hash/checklist acknowledgement in `/home/main/swarm/docs/skill-sync/checklist.md`

If CLI shape changes intentionally:

```bash
python3 /home/main/forgejo-agent/scripts/verify_skill_sync.py --update
python3 /home/main/forgejo-agent/scripts/check.py
```

## Docs

- `/home/main/swarm/docs/ROOT_SETUP.md`
- `/home/main/swarm/docs/TESTING_POLICY.md`
- `/home/main/swarm/docs/AGENT_WORKFLOW.md`
- `/home/main/swarm/docs/WORKER_LOOP.md`
- `/home/main/swarm/docs/ORCHD_DEV.md`
- `/home/main/swarm/docs/ORCHD_PROMPT_STATE_PLAN.md` (historical draft)
- `/home/main/swarm/docs/MCP_SETUP.md`
- `/home/main/swarm/docs/SECURITY.md`
- `/home/main/swarm/docs/skill-sync/checklist.md`
