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
}
