use anyhow::{Result, bail};

use crate::types::{ApiIssue, OpenState, WorkflowState};

pub const STATE_LABEL_COLOR: [(&str, &str, &str, bool); 6] = [
    ("state/triage", "8a8a8a", "needs triage", true),
    ("state/spec", "1d76db", "spec/design in progress", true),
    ("state/ready", "0e8a16", "ready for pickup", true),
    (
        "state/in-progress",
        "fbca04",
        "actively worked by an agent",
        true,
    ),
    (
        "state/review",
        "5319e7",
        "awaiting review/verification",
        true,
    ),
    ("state/blocked", "d73a4a", "blocked by dependency", true),
];

pub const OTHER_LABELS: [(&str, &str, &str, bool); 4] = [
    ("type/blocker", "6f42c1", "blocking dependency issue", false),
    ("pri/high", "b60205", "high priority", false),
    ("pri/med", "fbca04", "medium priority", false),
    ("pri/low", "c2e0c6", "low priority", false),
];

pub const ORCHD_STATE_LABELS: [(&str, &str, &str, bool); 5] = [
    (
        "orchd/state/queued",
        "d4c5f9",
        "dispatch accepted and queued",
        true,
    ),
    (
        "orchd/state/running",
        "1d76db",
        "dispatch currently running",
        true,
    ),
    (
        "orchd/state/blocked",
        "d73a4a",
        "dispatch blocked on a dependency or operator decision",
        true,
    ),
    ("orchd/state/failed", "b60205", "dispatch failed", true),
    (
        "orchd/state/completed",
        "0e8a16",
        "dispatch completed successfully",
        true,
    ),
];

pub const ORCHD_CONTROL_LABELS: [(&str, &str, &str, bool); 2] = [
    (
        "orchd/control/hold",
        "5319e7",
        "hold dispatch lifecycle progression",
        false,
    ),
    (
        "orchd/control/retry",
        "fbca04",
        "request dispatch retry",
        false,
    ),
];

pub const ORCHD_FAILURE_LABEL_PREFIX: &str = "orchd/failure/";

#[must_use]
pub fn is_orchd_failure_label(name: &str) -> bool {
    name.starts_with(ORCHD_FAILURE_LABEL_PREFIX)
}

#[must_use]
pub fn orchd_failure_label(reason_code: &str) -> Option<String> {
    let normalized = normalize_reason_code_label_segment(reason_code);
    if normalized.is_empty() {
        None
    } else {
        Some(format!("{ORCHD_FAILURE_LABEL_PREFIX}{normalized}"))
    }
}

#[must_use]
fn normalize_reason_code_label_segment(reason_code: &str) -> String {
    let mut out = String::new();
    let mut last_was_dash = false;
    for ch in reason_code.trim().chars() {
        let lower = ch.to_ascii_lowercase();
        if lower.is_ascii_lowercase() || lower.is_ascii_digit() || lower == '_' {
            out.push(lower);
            last_was_dash = false;
            continue;
        }
        if !last_was_dash {
            out.push('-');
            last_was_dash = true;
        }
    }
    out.trim_matches('-').to_string()
}

#[must_use]
pub const fn can_transition(from: Option<WorkflowState>, to: WorkflowState) -> bool {
    use WorkflowState as S;
    match from {
        None => matches!(to, S::Triage | S::Spec | S::Ready),
        Some(S::Triage) => matches!(to, S::Spec | S::Ready | S::Blocked),
        Some(S::Spec) => matches!(to, S::Triage | S::Ready | S::Blocked),
        Some(S::Ready) => matches!(to, S::InProgress | S::Blocked | S::Triage),
        Some(S::InProgress) => matches!(to, S::Review | S::Blocked | S::Ready),
        Some(S::Review) => matches!(to, S::InProgress | S::Blocked | S::Ready),
        Some(S::Blocked) => matches!(to, S::Triage | S::Spec | S::Ready),
    }
}

pub fn assert_transition(
    issue: &ApiIssue,
    to: WorkflowState,
    force: bool,
) -> Result<Option<WorkflowState>> {
    if issue.state != OpenState::Open {
        bail!(
            "issue #{} is closed; reopen before transitioning",
            issue.number
        );
    }
    let from = issue.workflow_state()?;
    if from == Some(to) {
        return Ok(from);
    }
    if !force && !can_transition(from, to) {
        match from {
            Some(current) => {
                bail!("illegal workflow transition: {current} -> {to} (use --force to override)")
            }
            None => bail!(
                "issue #{} has no workflow label and cannot transition directly to {} without --force",
                issue.number,
                to
            ),
        }
    }
    Ok(from)
}

pub fn assert_claimable(issue: &ApiIssue) -> Result<()> {
    if issue.state != OpenState::Open {
        bail!("issue #{} is closed", issue.number);
    }
    let state = issue.workflow_state()?;
    if state != Some(WorkflowState::Ready) {
        match state {
            Some(other) => bail!(
                "issue #{} is not ready (current state: {})",
                issue.number,
                other
            ),
            None => bail!("issue #{} has no workflow state label", issue.number),
        }
    }
    Ok(())
}

pub fn assert_closable(issue: &ApiIssue, force: bool) -> Result<()> {
    if issue.state == OpenState::Closed {
        bail!("issue #{} is already closed", issue.number);
    }
    let state = issue.workflow_state()?;
    if force {
        return Ok(());
    }
    if state != Some(WorkflowState::Review) {
        match state {
            Some(other) => bail!(
                "refusing to close issue #{} from state {}; expected review (use --force)",
                issue.number,
                other
            ),
            None => bail!(
                "refusing to close issue #{} without workflow state label (use --force)",
                issue.number
            ),
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;

    #[test]
    fn transition_matrix_smoke() {
        assert!(can_transition(
            Some(WorkflowState::Triage),
            WorkflowState::Ready
        ));
        assert!(can_transition(
            Some(WorkflowState::Ready),
            WorkflowState::InProgress
        ));
        assert!(!can_transition(
            Some(WorkflowState::Ready),
            WorkflowState::Review
        ));
        assert!(can_transition(
            Some(WorkflowState::Review),
            WorkflowState::Ready
        ));
        assert!(!can_transition(
            Some(WorkflowState::Blocked),
            WorkflowState::Review
        ));
        assert_eq!(can_transition(None, WorkflowState::Triage), true);
        assert_eq!(can_transition(None, WorkflowState::Blocked), false);
    }

    #[test]
    fn orchd_failure_label_normalizes_reason_code() {
        assert_eq!(
            orchd_failure_label("prompt_template_error"),
            Some("orchd/failure/prompt_template_error".to_string())
        );
        assert_eq!(
            orchd_failure_label("registered_trigger:custom.closed"),
            Some("orchd/failure/registered_trigger-custom-closed".to_string())
        );
        assert_eq!(orchd_failure_label("   "), None);
    }
}
