# Org Chart (Swarm v0)

## Authority

- `main`: human owner. Final decision authority.
- `codex-orch`: orchestration + triage. Acts on behalf of `main`.
- `codex-dev`: implementation subordinates. Execute delegated work.

## Forgejo Permissions

- `main` and `codex-orch` are Forgejo admins.
- Subordinate agents (for example `codex-dev`) must not be Forgejo admins.

## Safety Contract

- Control plane mutations should go through `forgejoctl`.
- A non-admin agent should never require Forgejo admin APIs to do its job.
- If a permission boundary blocks progress, the agent must:
  - transition the issue to `state/blocked` (when possible), and
  - file a concise bug report in `main/forgejo-work` describing the missing permission.

Operational note:
- Every automation role should have permission to create issues/comments in `main/forgejo-work` (even if it cannot write code in other repos), so blocked agents always have a “scream” path.
