// Canonical string tokens for directives, decisions, and webhook event types.
//
// Keep these centralized: they gate DB literals, directive parsing, and control-plane semantics.

pub(super) const EVENT_ISSUES: &str = "issues";
pub(super) const EVENT_ISSUE_COMMENT: &str = "issue_comment";

pub(super) const DECISION_ACCEPTED: &str = "accepted";
pub(super) const DECISION_IGNORED: &str = "ignored";

pub(super) const DIRECTIVE_DESIGN: &str = "design";
pub(super) const DIRECTIVE_IMPL: &str = "impl";
pub(super) const DIRECTIVE_REPLY: &str = "reply";
pub(super) const DIRECTIVE_POKE: &str = "poke";

pub(super) fn directive_is_known(directive: &str) -> bool {
    matches!(
        directive,
        DIRECTIVE_DESIGN | DIRECTIVE_IMPL | DIRECTIVE_REPLY | DIRECTIVE_POKE
    )
}

pub(super) fn directive_uses_worktree(directive: &str) -> bool {
    directive == DIRECTIVE_IMPL
}
