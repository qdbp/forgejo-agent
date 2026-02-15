# orchd Prompt + State Plan (Draft)

Status: draft v0.1

## Intent

Define a practical implementation path for:

1. moving orchestration runtime status from noisy comments into Forgejo labels
2. splitting dispatch prompts into fresh-context vs follow-up-context forms
3. enforcing directive-specific interaction contracts (`design`, `impl`, `poke`) without strict parsing of model freeform output

This plan is optimized for rapid MVP iteration, not final architecture.

## Operating Model (Policy Baseline)

- Hierarchy: `main` -> `codex-orch` -> implementation agents (future).
- `main` is the current human decision authority.
- `codex-orch` is the orchestration/triage layer.
- `poke` is conversational and read-only; intent is inferred from issue context.

## Comment Dogma (Applies To All Directives)

- Issue comments are natural language, not rigid machine forms.
- Be terse by default. Prefer high-signal, low-wording replies.
- Include concrete facts and requests, but avoid rote boilerplate.
- Use judgment and context; do not force pro-forma templates when they add no value.

## Two State Planes (Required)

State is intentionally split:

- Runtime orchestration plane (`orchd/*` labels): machine-owned dispatch lifecycle.
- Work/product plane (`state/*` labels): human/agent workflow status for the issue itself.

`orchd` must never overwrite `state/*` as part of runtime bookkeeping.

## Directive Contracts

### `design`

- Read-only.
- Must not edit repo files or produce implementation commits.
- Output must include: framing, options, recommendation, risks, and unblock decisions.
- Reply style: terse natural language.

### `impl`

- Execute until completion or explicit stop condition.
- Completion requires commit(s) and validation evidence (hooks/tests/formatting as applicable), but these are validated via control-plane actions and repo state checks where possible, not strict NLP parsing.
- If blocked, transition work plane to blocked state and post structured unblock request.
- Reply style: terse natural language.

Stop conditions:

- material underspecification
- material spec ambiguity
- self-detected churn/confusion/tarpit behavior
- temptation to change locked semantics instead of solving root cause

### `poke`

- Read-only conversational mode.
- Infer likely intent from issue context and recent thread activity.
- Provide high-signal status, next action, or targeted question.
- Reply style: terse natural language.

## Checkpoints

### C1: Labelized Runtime State

Scope:

- Introduce and document `orchd/*` label vocabulary.
- Add `forgejoctl` helper(s) to set scoped/exclusive runtime labels atomically (remove old `orchd/state/*`, set exactly one new state).
- Replace status comments with label updates; `orchd` does not post metadata into issue comments.

Proposed runtime labels:

- `orchd/state/queued`
- `orchd/state/running`
- `orchd/state/blocked`
- `orchd/state/failed`
- `orchd/state/completed`
- `orchd/control/hold`
- `orchd/control/retry`

Acceptance:

- dispatch status transitions are visible via labels without reading comments
- at most one runtime state label is present at a time
- no regression in sqlite canonical dispatch lifecycle

### C2: Prompt Envelope Split

Scope:

- Introduce prompt composition with two envelopes:
  - fresh session envelope
  - follow-up envelope
- Keep directive-specific body prompts.

Fresh envelope must include:

1. org/authority snapshot
2. Forgejo workflow sketch + doc pointers
3. bug-reporting guidance (`forgejo-work` feedback encouraged)
4. directive contract (`design`/`impl`/`poke`)
5. task payload

Follow-up envelope must include:

1. minimal immutable control header (role, authority, scope)
2. issue delta summary since last handled event (placeholder if not yet implemented)
3. task payload

Acceptance:

- config-driven template files exist for fresh and follow-up envelopes
- dispatch uses fresh envelope for new session, follow-up for session reuse
- generated prompt artifacts in run dir reflect selected envelope

### C3: Issue Delta Memory

Scope:

- Persist per `(repo, issue, role)` cursor for last processed issue comment/event.
- Persist stable webhook-derived identity fields for delta slicing (comment id and comment timestamp when present).
- Build follow-up delta block from newly added comments/events only.
- Include actor and timestamp in rendered delta summary.

Acceptance:

- follow-up prompts include only new thread content since prior dispatch
- no full-issue replay on each poke
- recovery behavior defined for missing/stale cursor

### C4: Completion + Block Protocol (Control-Plane First)

Scope:

- Avoid hard dependence on parsing structured model prose.
- Standardize required control-plane actions for blocked/completed runs.

Blocked behavior:

- transition issue to `state/blocked` (work plane)
- add a terse natural-language comment that includes:
  - why work is blocked
  - the concrete decisions needed to unblock
  - who is expected to decide (currently `@main`)
  - the recommended immediate next step

Completion behavior (`impl`):

- transition issue to target work state (typically `state/review` or `state/done`)
- add a terse natural-language completion comment with commit refs, verification summary, and residual risk

Acceptance:

- blocked runs set work plane blocked label and include decision list
- completion/block correctness is determined from tool-visible state transitions and side effects
- no runtime failure purely because a prose comment does not match a rigid schema

### C5: Runtime Label Projection

Scope:

- Project dispatch lifecycle to `orchd/state/*` labels.
- Keep sqlite as canonical lock/dispatch state source.
- Reserve comments for completion summaries and actionable human-facing context.

Acceptance:

- runtime state is readable from labels without scanning comment history
- projection survives retries/restarts and remains consistent with sqlite state

## Non-Goals (This Draft)

- Epic object modeling and hierarchy automation.
- Timebox-based forced blocking.
- Multi-role dispatch graph execution.
- Replacing sqlite as dispatch lock source of truth.
- Auto-poke from assignment/reply heuristics.

## Risks + Mitigations

- Label drift vs sqlite truth:
  - mitigation: sqlite remains canonical, labels are projection.
- Overly verbose comments:
  - mitigation: keep comments for completion/block details only.
- Prompt bloat:
  - mitigation: envelope templates are versioned and editable in repo; follow-up path stays minimal.

## Open Design Follow-Ups

- Exact mapping from current `state/*` workflow to blocked/unblocked transitions during `impl`.
- Whether `orchd/control/retry` is label-triggered or comment-triggered.
- Long-term authority model for delegated reviewer/owner agents.
- Auto-poke trigger policy and required webhook metadata (assignee context, role mapping).
