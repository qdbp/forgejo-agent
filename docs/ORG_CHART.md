# Org Chart (Swarm v0)

## Identity Model

- Agent identities are role templates, not unique long-lived individuals.
- Each dispatch materializes a fresh context for that role.
- A single role (for example `codex-lead`) may be materialized in parallel across many repos.
- Capability is defined by role card + token permissions + repo ACLs, not by a "personality" instance.

## Authority

- `main`: human owner. Final decision authority.
- `codex-orch`: orchestration + platform administration. Acts on behalf of `main`.
- `codex-lead`: repo-scoped design ownership and delegation layer.
- `codex-dev`: implementation execution subordinates.

## Forgejo Permissions

- `main` and `codex-orch` are Forgejo admins.
- `codex-lead` is not a Forgejo admin; grant repo-local rights only in assigned repos.
- Subordinate agents (for example `codex-dev`) must not be Forgejo admins.

## Safety Contract

- Control plane mutations should go through `forgejoctl`.
- A non-admin agent should never require Forgejo admin APIs to do its job.
- If a permission boundary blocks progress, the agent must:
  - transition the issue to `state/blocked` (when possible), and
  - file a concise bug report in `main/forgejo-agent` describing the missing permission.

Operational note:
- Every automation role should have permission to create issues/comments in `main/forgejo-agent` (even if it cannot write code in other repos), so blocked agents always have a "scream" path.
