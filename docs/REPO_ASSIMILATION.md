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
3. Ensure every configured role has a dedicated token file; do not point role
   entries at `~/.config/forgejo-agent/token` (owner fallback token).
4. Ensure orchd dispatch config has role blocks and role cards:
- `config/orchd-dispatch.toml`
- `prompts/roles/*.md`

### Role Credential Bootstrap (No Root Path)

If a role exists in Forgejo but `~/.config/forgejo-agent/creds/<role>.token` is
missing, bootstrap it in-band:

```bash
BASE_URL="${FORGEJO_BASE_URL:-http://127.0.0.1:3000}"
ADMIN_TOKEN="$(tr -d '\r\n' < ~/.config/forgejo-agent/token)"
ROLE_LOGIN="codex-lead"   # or codex-dev / future role
TOKEN_NAME="${ROLE_LOGIN}-$(date +%Y%m%d-%H%M%S)"

TMP_PASS="$(python3 - <<'PY'
import secrets, string
alphabet = string.ascii_letters + string.digits
print(''.join(secrets.choice(alphabet) for _ in range(32)))
PY
)"

# 1) Ensure role user is active and set a temporary password for token minting.
curl -fsS -X PATCH \
  -H "Authorization: token ${ADMIN_TOKEN}" \
  -H 'Content-Type: application/json' \
  -d "{\"active\":true,\"must_change_password\":false,\"password\":\"${TMP_PASS}\"}" \
  "${BASE_URL}/api/v1/admin/users/${ROLE_LOGIN}" >/dev/null

# 2) Mint role token (Forgejo requires explicit scopes for this endpoint).
ROLE_TOKEN="$(
  curl -fsS -X POST \
    -u "${ROLE_LOGIN}:${TMP_PASS}" \
    -H 'Content-Type: application/json' \
    -d "{\"name\":\"${TOKEN_NAME}\",\"scopes\":[\"all\"]}" \
    "${BASE_URL}/api/v1/users/${ROLE_LOGIN}/tokens" | jq -r '.sha1'
)"

# 3) Persist role credential for orchd dispatch.
install -d -m 700 ~/.config/forgejo-agent/creds
printf '%s\n' "${ROLE_TOKEN}" > ~/.config/forgejo-agent/creds/"${ROLE_LOGIN}".token
chmod 600 ~/.config/forgejo-agent/creds/"${ROLE_LOGIN}".token
```

Sanity check:

```bash
curl -fsS \
  -H "Authorization: token $(tr -d '\r\n' < ~/.config/forgejo-agent/creds/codex-lead.token)" \
  "${BASE_URL}/api/v1/user" | jq -r '.login'
```

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
