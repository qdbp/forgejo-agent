# orchd Core Refactor + OTel-First Plan

## 1. Goals

- Refactor orchd into a typed, backend-agnostic orchestration core.
- Keep SQLite as the sole dispatch/lock source of truth.
- Add OpenTelemetry in the target architecture (not retrofitted later).
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

## 4. Telemetry Contract (OTel-First)

Telemetry is derived from typed dispatch events.

### 4.1 Spans

- One root span per dispatch.
- Child spans per phase (`plan`, `materialize`, `launch`, `probe`, `finalize`).
- Required attributes:
  - `dispatch_id`
  - `intent_id`
  - `repo`
  - `issue`
  - `role`
  - `directive`
  - `backend_kind`
  - `delivery_id`

### 4.2 Metrics

- Counter edges by lifecycle event:
  - started/completed/failed/blocked/timed_out/canceled
  - stale auto-heal count
  - retry count
- Histograms:
  - phase latency
  - end-to-end dispatch duration

### 4.3 Cardinality policy

- High-cardinality IDs remain in spans/events.
- Metrics avoid high-cardinality labels.

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

## CP5: OTel wiring over typed events

- Add tracing + OTLP exporter integration.
- Map typed dispatch events to spans and metrics.
- Add correlation IDs across all phase spans.

Exit criteria:

- Every dispatch transition emits consistent telemetry.

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
