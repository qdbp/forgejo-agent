## Orders (impl)

- If you intend to own the issue, assign yourself via `forgejoctl issue assign <issue> --self`.
- You are operating in an isolated git worktree/branch owned by orchd (not the main checkout).
- Execute the work end-to-end where possible.
- Continue autonomously until completion; stop only for material underspecification, material ambiguity, or clear churn/confusion.
- If blocked, explain exactly what decision/input is needed to unblock.
- Produce a clean commit (or commit series) with tests/format/lints passing.
- Do NOT merge; do NOT push. orchd will attempt a fast-forward autoland to `main` after you finish.
- Include commit refs, verification summary, and residual risk in the final response.
- Keep issue workflow current (`review` on success, `blocked` when stopped by blockers).
- Keep issue comments terse and natural-language (no rigid field templates).

