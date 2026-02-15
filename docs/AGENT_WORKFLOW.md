# Agent Workflow Contract

## Core invariants

- Every work issue must include acceptance checks.
- Prefer filing issues in the repo that will be modified (orchd maps repo -> local checkout automatically).
- For cross-repo/system issues (for example `main/forgejo-work`), explicitly name the target repo(s).
- If blocked, agent must transition the issue to `state/blocked` and post a terse natural-language unblock comment.
- Agents must claim before editing, then release or close when done.
- Branches and commits must reference issue ID.
- Agents should mutate Forgejo state through `forgejoctl`.

## Reply etiquette

- All issue comments should be natural language, not rigid field dumps.
- Be terse by default.
- Include concrete facts: what changed, what is blocked, and what decision/action is needed.
- Avoid filler and repetitive boilerplate.

## Canonical labels

- `state/triage`: needs triage.
- `state/spec`: design/spec work.
- `state/ready`: eligible for pickup.
- `state/in-progress`: actively worked.
- `state/review`: awaiting review/validation.
- `state/blocked`: waiting on external dependency.
- `type/blocker`: issue exists to unblock another issue.
- `claimed/<agent>`: active lease owner.

## Claim lifecycle

Claim:

```bash
/home/main/.local/bin/forgejoctl issue claim main/backlog#123 --agent codex-a --ttl-min 90
```

Release:

```bash
/home/main/.local/bin/forgejoctl issue release main/backlog#123 --agent codex-a
```

## Blocker spawning

Blocker issues are optional and should be used for long-lived or parallelizable dependencies.
For short unblock questions to `main`/owner, prefer a single terse blocked comment on the same issue.

Optional blocker creation:

```bash
/home/main/.local/bin/forgejoctl issue blocker main/backlog#123 \
  --title "Need upstream API key" \
  --body "Missing credential X; cannot continue implementation."
```

When used, this creates a blocker, transitions parent to `state/blocked`, and comments cross-link.

## Suggested issue body schema

Start new issues from `templates/issue-template.md`.

## Dispatch shorthands

Use one of these on its own line in an issue body/comment:

- `@codex-orch design` (or `codex-orch design`)
- `@codex-orch impl` (or `codex-orch impl`)
- `@codex-orch pr` (or `codex-orch pr`)
- `@codex-orch poke` (or `codex-orch poke`)

Current semantics:

- `design`: produce high-level design/spec response and drive state toward `spec`.
- `impl`: execute implementation loop and autoland to `main` on success.
- `pr`: execute implementation loop and open a PR on success (no autoland).
- `poke`: conversational “status/next action” response (read-only).

## Traceability

- Branch: `i/<id>-short-slug` (example: `i/123-cache-index`)
- Commit footer: `Refs: main/backlog#123`
- PR body (if any): include same `Refs:` line
