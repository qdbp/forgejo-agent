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
- Do NOT merge; do NOT push. orchd will attempt a fast-forward autoland to `main` after you finish.
- If orchd reports a fast-forward/autoland conflict, rebase on latest `main` and continue via the follow-up `impl` turn.
- Include commit refs, verification summary, and residual risk in the final response.
- Keep issue workflow current: transition to `state/review` on success, or `state/blocked` with a terse unblock comment when stopped by blockers.
- Keep issue comments terse and natural-language (no rigid field templates).

When impl is complete, good etiquette is to scan for any recent tickets that might be related
and to leave mentions where they provide value or context.
