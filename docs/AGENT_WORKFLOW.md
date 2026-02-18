# Agent Workflow Contract

## Core invariants

- Every work issue must include acceptance checks.
- Prefer filing issues in the repo that will be modified (orchd maps repo -> local checkout automatically).
- For cross-repo/system issues (for example `main/forgejo-agent`), explicitly name the target repo(s).
- If blocked, agent must transition the issue to `state/blocked` and post a terse natural-language unblock comment.
- Agents must claim before editing, then release or close when done.
- Branches and commits must reference issue ID.
- Agents should mutate Forgejo state through `forgejoctl`.
- `orchd`-dispatched checkouts are cloned from local Forgejo; in those checkouts, `origin` points at local Forgejo (not GitHub).

## Reply etiquette

- All issue comments should be natural language, not rigid field dumps.
- Be terse by default.
- Include concrete facts: what changed, what is blocked, and what decision/action is needed.
- Avoid filler and repetitive boilerplate.
- When referencing issues/PRs, write plain text refs (#123, owner/repo#123), not inline code/backticks. Create follow-up issues before mentioning them so backlinks are guaranteed.

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

Start a line with one of these shorthands in an issue body/comment:

- `@codex-orch design` (or `codex-orch design`)
- `@codex-orch investigate` (or `codex-orch investigate`)
- `@codex-orch impl` (or `codex-orch impl`)
- `@codex-orch reply` (or `codex-orch reply`; `poke` is accepted as an alias)
- `@codex-lead design` (or `codex-lead design`)
- `@codex-dev impl` (or `codex-dev impl`)

You can add suffix text on the same line only when the directive is immediately
followed by `,`, `.`, `:`, or `;` (for example `@codex-orch design, open ended`).

Current semantics:

- `design`: produce high-level design/spec response and drive state toward `spec`.
- `investigate`: run bounded feasibility/current-state/options discovery in read-only mode; include evidence and concrete next-step guidance without auto-advancing workflow state.
- `impl`: execute implementation loop; on success, orchd pushes your branch, opens/ensures a PR, and attempts a fast-forward-only merge into `main` (with a rebase+retry path; remaining conflicts are punted back to a follow-up `impl` turn).
- `reply`: conversational “status/next action” response (read-only); `poke` is an alias.

Role note:
- Roles are templates, not singular "people"; one role can be materialized in many parallel issue contexts.

## Traceability

- Branch: `i/<id>-short-slug` (example: `i/123-cache-index`)
- Commit footer: `Refs: main/backlog#123`
- PR body (if you edit/create it manually): include same `Refs:` line (orchd may auto-create a minimal PR body).
