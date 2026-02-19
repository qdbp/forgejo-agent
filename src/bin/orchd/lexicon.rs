// Canonical string tokens for directives, decisions, and webhook event types.
//
// Keep these centralized: they gate DB literals, directive parsing, and control-plane semantics.

pub(super) const EVENT_ISSUES: &str = "issues";
pub(super) const EVENT_ISSUE_COMMENT: &str = "issue_comment";
pub(super) const EVENT_PUSH: &str = "push";
pub(super) const EVENT_SCHEDULE: &str = "schedule";

pub(super) const DECISION_ACCEPTED: &str = "accepted";
pub(super) const DECISION_IGNORED: &str = "ignored";

pub(super) const DIRECTIVE_DESIGN: &str = "design";
pub(super) const DIRECTIVE_INVESTIGATE: &str = "investigate";
pub(super) const DIRECTIVE_TRIAGE: &str = "triage";
pub(super) const DIRECTIVE_IMPL: &str = "impl";
pub(super) const DIRECTIVE_REPLY: &str = "reply";
pub(super) const DIRECTIVE_AUDIT: &str = "audit";
pub(super) const DIRECTIVE_AUDIT_FAILURE: &str = "audit-failure";

pub(super) fn directive_is_known(directive: &str) -> bool {
    matches!(
        directive,
        DIRECTIVE_DESIGN
            | DIRECTIVE_INVESTIGATE
            | DIRECTIVE_TRIAGE
            | DIRECTIVE_IMPL
            | DIRECTIVE_REPLY
            | DIRECTIVE_AUDIT
            | DIRECTIVE_AUDIT_FAILURE
    )
}

pub(super) fn directive_uses_worktree(directive: &str) -> bool {
    directive == DIRECTIVE_IMPL
}

pub(super) fn directive_serializes_repo(directive: &str) -> bool {
    directive_uses_worktree(directive)
}

#[cfg(test)]
mod tests {
    use super::{
        DIRECTIVE_AUDIT, DIRECTIVE_AUDIT_FAILURE, DIRECTIVE_DESIGN, DIRECTIVE_IMPL,
        DIRECTIVE_INVESTIGATE, DIRECTIVE_REPLY, DIRECTIVE_TRIAGE, directive_is_known,
        directive_uses_worktree,
    };

    #[test]
    fn investigate_is_a_known_directive() {
        assert!(directive_is_known(DIRECTIVE_INVESTIGATE));
    }

    #[test]
    fn audit_is_a_known_directive() {
        assert!(directive_is_known(DIRECTIVE_AUDIT));
    }

    #[test]
    fn triage_is_a_known_directive() {
        assert!(directive_is_known(DIRECTIVE_TRIAGE));
    }

    #[test]
    fn audit_failure_is_a_known_directive() {
        assert!(directive_is_known(DIRECTIVE_AUDIT_FAILURE));
    }

    #[test]
    fn only_impl_uses_a_worktree() {
        assert!(directive_uses_worktree(DIRECTIVE_IMPL));
        assert!(!directive_uses_worktree(DIRECTIVE_DESIGN));
        assert!(!directive_uses_worktree(DIRECTIVE_INVESTIGATE));
        assert!(!directive_uses_worktree(DIRECTIVE_TRIAGE));
        assert!(!directive_uses_worktree(DIRECTIVE_REPLY));
        assert!(!directive_uses_worktree(DIRECTIVE_AUDIT));
        assert!(!directive_uses_worktree(DIRECTIVE_AUDIT_FAILURE));
    }
}
