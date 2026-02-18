use anyhow::{Result, bail};

use crate::types::{ApiIssue, OpenState, WorkflowState};

pub const STATE_LABEL_COLOR: [(&str, &str, &str, bool); 7] = [
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
        "explicit human review/verification requested",
        true,
    ),
    (
        "state/done",
        "1f883d",
        "implementation complete; ready to close",
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

pub const DONE_REASON_LABEL_PREFIX: &str = "done/";
pub const DONE_REASON_LABELS: [(&str, &str, &str, bool); 3] = [
    ("done/fixed", "1f883d", "closed as fixed", false),
    ("done/wontfix", "d93f0b", "closed as wontfix", false),
    ("done/dupe", "5319e7", "closed as duplicate", false),
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
pub const ORCHD_REASON_LABEL_PREFIX: &str = "orchd/reason/";

#[must_use]
pub fn is_orchd_failure_label(name: &str) -> bool {
    name.starts_with(ORCHD_FAILURE_LABEL_PREFIX)
}

#[must_use]
pub fn is_orchd_reason_label(name: &str) -> bool {
    name.starts_with(ORCHD_REASON_LABEL_PREFIX)
}

#[must_use]
pub fn is_done_resolution_label(name: &str) -> bool {
    name.starts_with(DONE_REASON_LABEL_PREFIX)
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
pub fn orchd_reason_label(reason_code: &str) -> Option<String> {
    let normalized = normalize_reason_code_label_segment(reason_code);
    if normalized.is_empty() {
        None
    } else {
        Some(format!("{ORCHD_REASON_LABEL_PREFIX}{normalized}"))
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
        Some(S::Triage) => matches!(to, S::Spec | S::Ready | S::Done | S::Blocked),
        Some(S::Spec) => matches!(to, S::Triage | S::Ready | S::Done | S::Blocked),
        Some(S::Ready) => matches!(to, S::InProgress | S::Triage | S::Done | S::Blocked),
        Some(S::InProgress) => matches!(to, S::Review | S::Ready | S::Done | S::Blocked),
        Some(S::Review) => matches!(to, S::InProgress | S::Ready | S::Done | S::Blocked),
        Some(S::Done) => matches!(
            to,
            S::Triage | S::Spec | S::Ready | S::InProgress | S::Blocked
        ),
        Some(S::Blocked) => matches!(to, S::Triage | S::Spec | S::Ready | S::Done),
    }
}

#[must_use]
pub const fn is_closable_workflow_state(state: WorkflowState) -> bool {
    matches!(state, WorkflowState::Done | WorkflowState::Review)
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
                "issue #{} is not ready (current state: {other}); run: forgejoctl issue transition --to ready <issue>",
                issue.number,
            ),
            None => bail!(
                "issue #{} has no workflow state label; run: forgejoctl issue transition --to ready <issue>",
                issue.number
            ),
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
    match state {
        Some(current) if is_closable_workflow_state(current) => {}
        Some(other) => {
            bail!(
                "refusing to close issue #{} from state {}; expected done (legacy review accepted) (use --force)",
                issue.number,
                other
            );
        }
        None => {
            bail!(
                "refusing to close issue #{} without workflow state label (use --force)",
                issue.number
            );
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::ApiLabel;
    use pretty_assertions::assert_eq;

    fn issue_with_workflow(workflow: Option<WorkflowState>) -> ApiIssue {
        let labels = workflow
            .map(|state| {
                vec![ApiLabel {
                    id: 1,
                    name: state.label().to_string(),
                }]
            })
            .unwrap_or_default();
        ApiIssue {
            number: 7,
            state: OpenState::Open,
            title: "closable".to_string(),
            body: None,
            html_url: "http://localhost/issue/7".to_string(),
            labels,
            assignees: Vec::new(),
            pull_request: None,
            repository: None,
        }
    }

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
            Some(WorkflowState::InProgress),
            WorkflowState::Done
        ));
        assert!(can_transition(
            Some(WorkflowState::Review),
            WorkflowState::Ready
        ));
        assert!(can_transition(
            Some(WorkflowState::Review),
            WorkflowState::Done
        ));
        assert!(can_transition(
            Some(WorkflowState::Triage),
            WorkflowState::Done
        ));
        assert!(can_transition(
            Some(WorkflowState::Done),
            WorkflowState::InProgress
        ));
        assert!(!can_transition(
            Some(WorkflowState::Done),
            WorkflowState::Review
        ));
        assert!(!can_transition(
            Some(WorkflowState::Blocked),
            WorkflowState::Review
        ));
        assert_eq!(can_transition(None, WorkflowState::Triage), true);
        assert_eq!(can_transition(None, WorkflowState::Blocked), false);
    }

    #[test]
    fn assert_closable_accepts_done_and_legacy_review() {
        assert!(assert_closable(&issue_with_workflow(Some(WorkflowState::Done)), false).is_ok());
        assert!(assert_closable(&issue_with_workflow(Some(WorkflowState::Review)), false).is_ok());
        assert!(assert_closable(&issue_with_workflow(Some(WorkflowState::Ready)), false).is_err());
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

    #[test]
    fn orchd_reason_label_normalizes_reason_code() {
        assert_eq!(
            orchd_reason_label("issue_dispatch_in_flight"),
            Some("orchd/reason/issue_dispatch_in_flight".to_string())
        );
        assert_eq!(
            orchd_reason_label("actor not allowed"),
            Some("orchd/reason/actor-not-allowed".to_string())
        );
        assert_eq!(orchd_reason_label("   "), None);
    }
}
