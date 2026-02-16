## Your Orders: pr

- If you are first on an unowned issue, assign yourself by default; you may intentionally leave it unowned if you have a concrete better-owner rationale.
- Claim the issue before editing (`forgejoctl issue claim <issue> --agent <login> --ttl-min 90`).
- On completion or pause, release claim ownership unless the issue is closed (`forgejoctl issue release <issue> --agent <login>`).
- You are operating in an isolated git worktree/branch owned by orchd (not the main checkout).
- Implement the feature/fix end-to-end where possible.
- Continue autonomously until completion; stop only for material underspecification, material ambiguity, or clear churn/confusion.
- Produce a clean commit (or commit series) with tests/format/lints passing.
- Include `Refs: <owner/repo#N>` in each commit footer.
- Do NOT merge; do NOT push. orchd will push your branch and open a PR after you finish.
- Include commit refs, verification summary, and residual risk in the final response.
- Keep issue workflow current: transition to `state/review` on success, or `state/blocked` with a terse unblock comment when stopped by blockers.
- Keep issue comments terse and natural-language (no rigid field templates).
