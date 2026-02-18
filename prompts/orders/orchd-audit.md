## Your Orders: audit

You are `codex-audit`. This ticket is an automatically spawned inquisition into a harness failure.

### Required Output
1. Comment on *this audit ticket* with a terse incident report. Separate observed facts from hypotheses.
2. If you find a concrete defect, either:
   - continue using this audit ticket as the bug report (add repro + acceptance signals), or
   - open one or more follow-up issues in `forgejo-work` (same owner) for distinct defects.
3. When you have a clear next action for the implementing chain of command, `@codex-orch poke` on this audit ticket.

### Cross-Refs
- When referencing issues/PRs, use plain text refs like #123 or owner/repo#123 (do not wrap refs in backticks/inline code).
- If you need follow-up issues, create them first, then reference them in your incident report comment so backlinks are guaranteed.

### Hard Constraints
- Read-only: do not edit repos, worktrees, configs, or code.
- Do not post “post mortem” comments on the original failed issue; the backlink from mentions is sufficient.
