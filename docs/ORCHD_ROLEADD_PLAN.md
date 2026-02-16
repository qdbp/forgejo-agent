# orchd `roleadd` Design (v0)

## Why this exists

Role drift caused a control-plane integrity failure:

- dispatch targeted `codex-dev`
- runtime identity resolved to `main`
- issue ownership/claim/comments were authored under the wrong principal

This happened because role provisioning is currently split across docs/manual edits
and wrappers with fallback behavior. We need one checked entrypoint.

## Goals

- Single authoritative workflow to add a new swarm role.
- Fail fast on partial/unsafe role state.
- Prevent silent principal fallback/impersonation.
- Provide operator-visible diagnostics and machine-readable output.
- Keep repo assimilation (`scripts/assimilate-repo.sh`) repo-scoped; keep roleadd global.

## Non-goals (v0)

- Full org-chart policy engine in orchd.
- Dynamic runtime role creation via issue comments.
- Replacing repo assimilation.

## Current role surfaces (audit)

Role state is currently spread across these artifacts:

1. `config/orchd-dispatch.toml`
- `[roles.<name>]` mapping (`codex_bin`, `codex_role_arg`, `token_file`, optional `forgejo_login`)
- directive -> default role mapping
- trigger actions with explicit `target_role`

2. `prompts/roles/<role>.md`
- rank source (`- OF-<n>` bullet parsed by dispatch config loader)
- role contract text

3. Forgejo users/tokens (external)
- user existence/active/admin flags
- personal access token bound to that login

4. Local credential files
- `~/.config/forgejo-agent/creds/codex-<suffix>.token`

5. Repo-scoped ACLs (per onboarded repo)
- collaborator permission assignment done by assimilation tooling

## Gaps today

- No single command verifies all role surfaces together.
- Role token/login mismatch is possible.
- Missing role token can degrade into wrong identity if wrappers allow fallback.
- Role onboarding is procedural text, not executable validation.
- Errors are discovered late (during dispatch), not at provisioning time.

## Proposed CLI

Add `orchd role` command group:

- `orchd role add ...`
- `orchd role check [--role <name>] [--json]`
- `orchd role list [--json]`

### `orchd role add` (proposed flags)

Required:

- `--role <codex-*>`
- `--rank <OF-n>`
- `--forgejo-login <login>`

Optional:

- `--codex-role-arg <arg>` (default strips `codex-`)
- `--token-file <path>` (default `~/.config/forgejo-agent/creds/<role>.token`)
- `--codex-bin <path>` (default from dispatch config)
- `--can-dispatch` (if role should be in `allowed_actors`)
- `--scream-repo <owner/repo>` (default `main/forgejo-work`)
- `--scream-permission <read|write>` (default `write`)
- `--admin-token-file <path>` (required for Forgejo provisioning)
- `--create-user` (create Forgejo user if missing)
- `--rotate-token` (revoke old token and mint new)
- `--dry-run`
- `--json`

## Transaction model (atomic where practical)

`role add` runs in phases with explicit compensation.

### Phase 1: preflight (no mutation)

- Parse and validate dispatch config.
- Validate role name shape and non-existence in config.
- Validate role card path non-conflict.
- Validate rank exists in ACL map (or fail with exact patch guidance).
- Validate token target path policy (no shared token path, no duplicate role token paths).
- Validate Forgejo connectivity and admin token.
- Validate `forgejo_login` user status (exists/create required/admin=false).

Failure: exit nonzero with step-tagged error and no writes.

### Phase 2: remote provisioning (Forgejo)

- Create user when requested and missing.
- Mint token for `forgejo_login`.
- Verify minted token (`/api/v1/user`) resolves to exact login.
- Ensure scream-path collaborator ACL on `scream_repo`.

Compensation on later failure:

- Revoke minted token (best effort).
- If user was created in this run and `--create-user` was used, optionally deactivate user (best effort).

### Phase 3: local writes (staged + atomic rename)

- Write token to temp file (`0600`) then rename.
- Patch `config/orchd-dispatch.toml` using structured edit (`toml_edit`):
  - add `[roles.<role>]`
  - set `forgejo_login`
  - set `token_file`
  - set `codex_role_arg`
  - optionally append to `allowed_actors`
- Create `prompts/roles/<role>.md` from template with rank skeleton.

### Phase 4: post-write verification

- Re-load dispatch config with strict validation.
- Run `orchd role check --role <name>` logic in-process.
- Verify token-file -> login mapping again from disk.

Failure: rollback local files to backups and attempt remote compensation.

## `orchd role check` contract

Checks all configured roles or one role:

- role entry exists in dispatch config
- role card exists and rank parses
- token file exists, readable, non-empty, mode-safe
- token maps to configured `forgejo_login`
- `forgejo_login` exists and is active
- `forgejo_login` admin bit is policy-compliant for role class
- role is compatible with trigger/directive references

Output:

- human table by default
- machine JSON (`ok`, `errors[]`, `warnings[]`, `suggested_fixes[]`)

Exit codes:

- `0`: all checks pass
- `2`: warnings only
- `3`: hard failures

## Required hardening in support of roleadd

1. No implicit principal fallback in role launcher.
2. Dispatch config validation rejects:
- duplicate role token paths
- role token path that equals global owner token path
- duplicate `forgejo_login` across roles unless explicitly allowed
3. Startup optionally runs `role check` and emits loud failures.

## Migration plan

CP1:

- implement `orchd role check`
- enforce strict role-token path invariants
- wire into tests

CP2:

- implement `orchd role add --dry-run` (full preflight)

CP3:

- implement mutating `orchd role add` with rollback/compensation

CP4:

- integrate role checks into `scripts/assimilate-repo.sh` preflight
- add operator runbook updates

## Success criteria

- Adding role is one command, with deterministic output.
- Partial role additions are impossible or loudly auto-reverted.
- Dispatch cannot run under a different principal than target role.
- Role drift is detectable via one command and CI-gateable.
