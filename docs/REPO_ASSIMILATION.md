# Repo Assimilation Package

This document is the canonical package to hand `codex-orch` when onboarding a
new repository into the swarm.

## Copy/Paste Handoff For `codex-orch`

Use this in an issue/comment:

```text
Assimilate owner/repo using /home/main/forgejo-agent/docs/REPO_ASSIMILATION.md.
Run /home/main/forgejo-agent/scripts/assimilate-repo.sh with the correct local path.
If any step fails, stop and report the exact blocker.
When done, post:
- which files changed
- smoke-test result
- any follow-up action needed from main
```

## Purpose

Given:
- a Forgejo repo ref (`owner/repo`)
- an on-disk local checkout path

produce:
- Forgejo repo + policy labels
- repo-scoped ACLs for swarm roles
- local git forgejo remote + initial bootstrap push
- orchd repo binding (`repo_bindings`) for worktree-backed `impl`
- repo-local `AGENTS.md` integration snippet

with minimal manual work and no non-repo-specific admin steps.

## Identity Model (Important)

- `codex-lead`, `codex-dev`, and `codex-orch` are role templates, not unique humans.
- Any role may be materialized in parallel across repos/issues.
- Capability comes from role card + token + repo ACL.
- Repository ownership is therefore policy/ACL scoped, not "which singleton model instance exists".

## One-Time Global Prerequisites

Do this once per installation, not per repo:

1. Ensure role users/tokens exist:
- `codex-orch` (admin)
- `codex-lead` (non-admin user)
- `codex-dev` (non-admin user)
2. Store token files under `~/.config/forgejo-agent/creds/`.
3. Ensure orchd dispatch config has role blocks and role cards:
- `config/orchd-dispatch.toml`
- `prompts/roles/*.md`

## Per-Repo Assimilation (Single Command)

Run:

```bash
/home/main/forgejo-agent/scripts/assimilate-repo.sh \
  --repo owner/repo \
  --local-path /home/main/programming/projects/repo
```

Default behavior:
- ensures repo + policy labels via `forgejoctl repo ensure`
- applies collaborator ACLs:
  - `codex-orch`: admin
  - `codex-lead`: admin (repo-scoped)
  - `codex-dev`: write
- ensures local git remote `forgejo` points to local Forgejo URL
- bootstraps `main` to Forgejo when remote branch is absent
- appends a `[[repo_bindings]]` block to `config/orchd-dispatch.toml` if missing
- injects a marker-delimited assimilation snippet into `<repo>/AGENTS.md`

## Optional Flags

```text
--dispatch-config PATH
--dispatch-git-remote NAME
--dispatch-git-base BRANCH
--forgejo-remote NAME
--forgejo-login LOGIN
--orch-login LOGIN
--lead-login LOGIN
--dev-login LOGIN
--skip-acl
--skip-bootstrap-push
--skip-agents-patch
--dry-run
```

## After Assimilation

1. Review generated changes:
- `config/orchd-dispatch.toml`
- `<repo>/AGENTS.md` (if patched)
2. Restart orchd user service:

```bash
XDG_RUNTIME_DIR=/run/user/$(id -u) \
DBUS_SESSION_BUS_ADDRESS=unix:path=/run/user/$(id -u)/bus \
systemctl --user restart orchd.service
```

3. Smoke test:
- open issue in `owner/repo` with `@codex-dev poke`
- verify:
  - runtime labels progress (`orchd/state/*`)
  - codex reply is posted

## Safety Notes

- The script is idempotent for existing remotes/bindings/snippet markers.
- It does not force-push; bootstrap push only runs when target branch is absent.
- If collaborator ACL API calls fail, treat that as a hard blocker and fix before dispatching implementation work.

## Related Docs

- `docs/ORG_CHART.md`
- `docs/AGENT_WORKFLOW.md`
- `docs/ORCHD_DEV.md`
