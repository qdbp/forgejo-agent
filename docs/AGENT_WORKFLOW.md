# Agent Workflow Contract

## Core invariants

- Every work issue must include target local repo path and acceptance checks.
- If blocked, agent must open blocker issue and link it in parent comments.
- Agents must claim before editing, then release or close when done.
- Branches and commits must reference issue ID.
- Agents should mutate Forgejo state through `forgejoctl`.

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

```bash
/home/main/.local/bin/forgejoctl issue blocker main/backlog#123 \
  --title "Need upstream API key" \
  --body "Missing credential X; cannot continue implementation."
```

This creates a blocker, transitions parent to `state/blocked`, and comments cross-link.

## Suggested issue body schema

Start new issues from `templates/issue-template.md`.

## Dispatch shorthands

Use one of these on its own line in an issue body/comment:

- `@codex-orch design` (or `codex-orch design`)
- `@codex-orch impl` (or `codex-orch impl`)

Current semantics:

- `design`: produce high-level design/spec response and drive state toward `spec`.
- `impl`: execute implementation loop; claim, implement, verify, and transition toward `review` (or open blocker).

## Traceability

- Branch: `i/<id>-short-slug` (example: `i/123-cache-index`)
- Commit footer: `Refs: main/backlog#123`
- PR body (if any): include same `Refs:` line
