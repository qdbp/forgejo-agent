# orchd Core Refactor Plan (SQLite-First Telemetry)

> Historical note: this plan predates tmux removal. Current runtime path is `exec` with `systemd`/`local` backends.

## 1. Goals

- Refactor orchd into a typed, backend-agnostic orchestration core.
- Keep SQLite as the sole dispatch/lock source of truth.
- Keep telemetry SQLite-first (queryable by agents); avoid OTel for now.
- Preserve tmux/TUI operator ergonomics while enabling deterministic local tests.
- Keep issue comments natural-language; no model-output parsing contract.

## 2. Non-goals (This Phase)

- No multi-agent framework adoption.
- No OPA/Rego policy engine.
- No external broker/event bus.
- No compatibility shims that preserve internal legacy mode wiring.

## 3. Target Shape

### 3.1 Typed core model

- `DispatchIntentV1`: typed envelope for a single dispatch attempt.
- `DispatchState`: explicit lifecycle states.
- `DispatchEvent`: append-only transition events.
- `PolicyDecision`: allow/deny/hold with human reasons.
- `RunHandle`: backend-neutral launch handle (`backend_kind` + `backend_ref`).
- `OutputProjection`: explicit control-plane projection operations.

### 3.2 Phase decomposition

- `plan_dispatch(intent) -> DispatchPlan` (pure).
- `materialize_run_artifacts(plan) -> RunArtifacts`.
- `backend.launch(plan, artifacts) -> RunHandle`.
- `backend.probe(handle) -> Liveness`.
- `finalize_dispatch(dispatch_id, terminal_reason, projections)`.

### 3.3 Invariants

- Dispatch state mutation must go through a typed reducer.
- DB row update and event append happen in one transaction.
- Labels are control-plane state projection; comments are narrative.
- Backend-specific operations are isolated behind backend adapters.
- No lock/state authority outside SQLite.

## 4. Telemetry Contract (SQLite-First)

Telemetry is derived from typed dispatch events and persisted in SQLite.

### 4.1 Canonical event ledger

- Every dispatch transition appends one row to `dispatch_events` in the same transaction as the `dispatches` row update.
- The ledger is the canonical truth for:
  - lifecycle state transitions
  - stale auto-heal transitions
  - failure reasons (via `reason_code` + `error_text`)

### 4.2 Optional phase timings

Phase timings can be logged (via tracing) and later persisted for slicing/reporting. We intentionally avoid
requiring a metrics backend in early dogfooding.

### 4.3 Cardinality policy

- SQLite can store high-cardinality IDs (dispatch_id, delivery_id, etc.).
- Any derived aggregates or future exports should avoid high-cardinality labels by default.

## 5. Backend Strategy

- `TmuxBackend`: operator-facing default backend.
- `LocalBackend`: detached process backend for deterministic integration tests.
- Optional `MockBackend`: only if LocalBackend + fake-codex leaves gaps.

Backend contract:

- `launch` -> `RunHandle`
- `probe` -> `alive|dead|error`
- `terminate` (socket included; may be partial initially)

## 6. Checkpoint Plan

## CP0: Typed spine + reducer + schema freeze

- Add typed dispatch domain module.
- Add transition reducer with invalid-transition rejection.
- Add telemetry event schema contract (code + doc-level freeze).
- Use typed state constants in current SQL transition helpers.

Exit criteria:

- No direct raw string state transitions in primary transition helpers.
- Reducer tests cover valid and invalid edges.

## CP1: DB event ledger normalization

- Add `dispatch_events` table (append-only).
- Ensure each transition writes:
  - dispatch row update
  - event append
  in one transaction.
- Keep current lock semantics unchanged, but routed through typed state.

Exit criteria:

- In-flight lock and status semantics are DB-authored and event-backed.

## CP2: Orchestration phase extraction

- Split monolith dispatch path into plan/materialize/launch/probe/finalize units.
- Keep external behavior intact while reducing hidden side effects.

Exit criteria:

- Old monolith reduced to thin coordinator.

## CP3: Backend abstraction + adapters

- Introduce backend trait and plug in `TmuxBackend`.
- Add `LocalBackend` for integration harness.

Exit criteria:

- Coordinator no longer issues raw tmux/process commands directly.

## CP4: Remove shell-owned control-plane side effects

- Move completion/failure/status writes from shell scripts into Rust.
- Keep scripts only for process bootstrap where still needed.

Exit criteria:

- Control-plane mutation is Rust-owned and deterministic.

## CP5: Telemetry slicing (SQLite) (future)

- Add an `orchd obs ...` CLI that slices SQLite into concise, agent-friendly views:
  - issue timeline
  - dispatch timeline
  - recent failures + reasons
  - latency reports (if persisted)

Exit criteria:

- Operators and agents can debug most failures from SQLite + run artifacts without a metrics backend.

## CP6: Live orchd integration harness

- Real Forgejo fixture + real orchd process.
- LocalBackend + fake-codex scenario matrix:
  - success
  - nonzero
  - timeout
  - missing final answer
- Timing capture JSONL + summary script.

Exit criteria:

- Repeatable live tests verify end-to-end dispatch state and projection behavior.

## 7. Testing posture for this refactor

- Tier 0: `python3 scripts/check.py` on every commit.
- Tier 1: live Forgejo + orchd LocalBackend integration tests for core orchd changes.
- Tier 2: tmux adapter smoke tests (env-gated/manual or nightly).
- Keep timing budgets visible to prevent test-latency drift.

## 8. Open decisions to resolve before/within CP1

- Exact `dispatch_events` schema fields and index strategy.
- Canonical label vocabulary for control-plane projections.
- Timeout policy and lazy-heal edge rules.
- Minimum fake-codex scenario matrix for stable CI.
