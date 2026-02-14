# Forgejo Agent Control Plane

Rust-first, policy-enforcing issue gateway for local multi-agent workflows.

## Principle

Agents should use one executable for Forgejo operations:

- `/home/main/.local/bin/forgejoctl`

Direct raw API poking is discouraged for agents.

`forgejoctl` token discovery order:

1. `--token-file`
2. `FORGEJO_TOKEN_FILE` from process environment
3. systemd credential `CREDENTIALS_DIRECTORY/forgejo_token`
4. `FORGEJO_TOKEN_FILE` in config file
5. default `~/.config/forgejo-agent/token`

## Install

```bash
/home/main/forgejo-agent/scripts/install.sh
```

This builds release and installs `forgejoctl` to `~/.local/bin/forgejoctl`.

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

# Claim/release and blocker flow
/home/main/.local/bin/forgejoctl issue claim main/backlog#1 --agent codex-main
/home/main/.local/bin/forgejoctl issue release main/backlog#1 --agent codex-main
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
  --agent codex-main
```

## `orchd` Dev Mode

`orchd` is a separate binary for reactive orchestration prototyping.

Current mode supports both:

- `dry-run`: parse directives and post intent comments only
- `tmux-exec`: create a tracked dispatch run and spawn Codex in a tmux window

Core behavior:

- webhook ingest
- directive parse (`@codex-orch design|impl|poke`, `@codex poke`)
- sqlite event/decision/dispatch persistence
- periodic heartbeat + reconcile scan logs

Run (`tmux-exec`):

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

Run (`dry-run`):

```bash
cargo run --bin orchd -- \
  --listen 127.0.0.1:7878 \
  --db-path ~/.local/state/orchd-dev/orchd.sqlite \
  --reconcile-repo main/orchd-debug \
  --heartbeat-sec 15 \
  --reconcile-sec 45 \
  --dispatch-mode dry-run
```

Health check:

```bash
curl -fsS http://127.0.0.1:7878/healthz | jq .
```

## Quality Gate

For Rust changes in this repo, run:

```bash
python3 /home/main/forgejo-agent/scripts/check.py
```

## Git Hooks

Install repo-managed hooks:

```bash
/home/main/forgejo-agent/scripts/install-git-hooks.sh
```

Installed hooks:

- `pre-commit`: runs `scripts/check.py`
- `pre-push`: runs `scripts/check.py`

`check.py` includes a skill/API sync enforcement hook:

- verifies `forgejoctl` CLI surface snapshot/hash
- requires matching hash + command block in `~/.codex/skills/forgejoctl/SKILL.md`
- requires matching hash/checklist acknowledgement in `docs/skill-sync/checklist.md`

If CLI shape changes intentionally:

```bash
python3 /home/main/forgejo-agent/scripts/verify_skill_sync.py --update
python3 /home/main/forgejo-agent/scripts/check.py
```

## Docs

- `docs/ROOT_SETUP.md`
- `docs/AGENT_WORKFLOW.md`
- `docs/WORKER_LOOP.md`
- `docs/ORCHD_DEV.md`
- `docs/TMUX_OBSERVABILITY.md`
- `docs/MCP_SETUP.md`
- `docs/SECURITY.md`
- `docs/skill-sync/checklist.md`
