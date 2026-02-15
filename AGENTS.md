# AGENTS Notes

## Start Here

- Read `README.md` first for the canonical operator/developer workflow and command patterns.
- Treat `docs/skill-sync/checklist.md` as mandatory when `forgejoctl` CLI surface changes.

## Rust Quality Gate

- For any Rust code change in this repo, run `python3 scripts/check.py` before finishing.
- Treat any failure from `scripts/check.py` as blocking; do not ship until green.
- Prefer fixing code over weakening lint settings; only add an allow when there is a clear policy reason.
- Follow `docs/TESTING_POLICY.md` for tiered test requirements.
- If you touch `src/main.rs`, `src/api.rs`, `src/policy.rs`, `src/types.rs`, or `tests/live_forgejo.rs`, also run:
  - `FORGEJO_LIVE_TESTS=1 cargo test --test live_forgejo -- --nocapture`

## Module Indexing Policy

- For any `mod.rs` that declares submodules, add a 1-2 line comment above each `mod foo;` answering:
  - what the submodule does
  - when you should read it (vs safely skip it)

## Forgejo Access Policy

- Agent workflows should use `/home/main/.local/bin/forgejoctl` for Forgejo mutations.
- Do not add new agent-facing flows that call raw REST endpoints directly.
- Dogfooding rule: when working on Forgejo process/tooling itself, use `forgejoctl` only for issue lifecycle operations (create/read/update/transition/claim/release/blocker/close).
- Do not use direct Forgejo API clients or ad-hoc `curl` paths for normal agent workflow actions.

## Required Sync Procedure

When CLI surface changes (commands/flags/state names):

1. run `python3 /home/main/forgejo-agent/scripts/verify_skill_sync.py --update`
2. review `docs/skill-sync/cli-surface.txt`
3. update this skill’s command/reference guidance
4. update `docs/skill-sync/checklist.md`
5. run `python3 /home/main/forgejo-agent/scripts/check.py`

## Delivery Policy

- For major features/refactors, finish by committing and pushing to `origin` in the same execution loop unless explicitly told not to.
