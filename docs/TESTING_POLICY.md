# Testing Policy

This repo uses a tiered testing policy. The goal is to keep fast feedback for all
changes while preserving real end-to-end confidence on high-risk control-plane
paths.

## Test tiers

1. Tier 0 (`always`)
- Command: `python3 scripts/check.py`
- Includes: `fmt`, strict `clippy`, unit tests, skill-sync checks.
- Required for every Rust change.

2. Tier 1 (`live forgejoctl e2e`)
- Command: `FORGEJO_LIVE_TESTS=1 cargo test --test live_forgejo -- --nocapture`
- Includes: live Forgejo process + real `forgejoctl` command round-trips.
- Required when changing issue/repo lifecycle behavior, API wiring, label/state semantics, or integration tests.

3. Tier 2 (`orchd live manual`, optional confidence pass)
- Follow runbook in `docs/ORCHD_DEV.md` for webhook -> dispatch -> completion checks.
- Recommended when changing `src/bin/orchd/*`, dispatch scripts, or webhook/reconcile logic.

## Required tier matrix

Run Tier 0 + Tier 1 if a change touches any of:
- `src/main.rs`
- `src/api.rs`
- `src/policy.rs`
- `src/types.rs`
- `tests/live_forgejo.rs`

Run Tier 0 + Tier 2 if a change touches any of:
- `src/bin/orchd/*.rs`
- `config/orchd-dispatch.toml`
- `prompts/orchd-*.md`
- `prompts/roles/*.md`
- `templates/role-card-template.md`

Run all three tiers if a change touches both `forgejoctl` and `orchd` surfaces.

## Timing telemetry

Live integration tests write timing JSONL to:
- default: `target/live-test-timings.jsonl`
- override: `FORGEJO_LIVE_TIMINGS_PATH=/path/file.jsonl`

Summary report:
- `python3 scripts/live_timing_report.py`
- recent-window summary: `python3 scripts/live_timing_report.py --last 200`

Current soft budgets (p95):
- `fixture.spawn_and_ready`: <= 2000 ms
- `repo.ensure`: <= 900 ms
- `issue.create`: <= 400 ms
- `issue.verify_read_back`: <= 50 ms

If a step regresses >2x versus recent baseline, treat it as a bug and open an issue.

## Ordering and isolation

- Live tests are serialized with `serial_test` to avoid port/process cross-talk.
- Each test uses an isolated temp Forgejo instance and repo.
- Do not make live tests depend on shared external Forgejo state.
