## Your Orders: impl

- If you are first on an unowned issue, assign yourself by default; you may intentionally leave it unowned if you have a concrete better-owner rationale.
- Claim the issue before editing (`forgejoctl issue claim <issue> --agent <login> --ttl-min 90`).
- On completion or pause, release claim ownership unless the issue is closed (`forgejoctl issue release <issue> --agent <login>`).
- You are operating in an isolated git worktree/branch owned by orchd (not the main checkout).
- Execute the work end-to-end where possible.
- Continue autonomously until completion; stop only for material underspecification, material ambiguity, or clear churn/confusion.
- If blocked, explain exactly what decision/input is needed to unblock.
- Produce a clean commit (or commit series) with tests/format/lints passing.
- Include `Refs: <owner/repo#N>` in each commit footer.
- Do NOT merge. Do NOT push by default. orchd will push your branch to local Forgejo, ensure a PR exists, and attempt a fast-forward-only merge into `main`.
- If orchd reports PR landing blocked, follow the instructions it posts (rebase, resolve conflicts, push with `--force-with-lease`), then retry via a follow-up `impl` turn.
- Note: in your orchd-managed checkout, `origin` points at local Forgejo (not GitHub). Do not run `scripts/push-both.sh` from inside that checkout.
- Include commit refs, verification summary, and residual risk in the final response.
- Keep issue workflow current: transition to `state/review` on success, or `state/blocked` with a terse unblock comment when stopped by blockers.
- Keep issue comments terse and natural-language (no rigid field templates).

When impl is complete, good etiquette is to scan for any recent tickets that might be related
and to leave mentions where they provide value or context.
