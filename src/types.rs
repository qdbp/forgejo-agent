use std::fmt::{self, Display};
use std::str::FromStr;

use anyhow::{Context, Result, anyhow, bail};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct RepoRef {
    pub owner: String,
    pub repo: String,
}

impl RepoRef {
    pub fn new(owner: impl Into<String>, repo: impl Into<String>) -> Self {
        Self {
            owner: owner.into(),
            repo: repo.into(),
        }
    }

    pub fn parse(input: &str) -> Result<Self> {
        let (owner, repo) = input
            .split_once('/')
            .ok_or_else(|| anyhow!("expected repo like owner/repo, got: {input}"))?;
        if owner.is_empty() || repo.is_empty() || repo.contains('/') {
            bail!("expected repo like owner/repo, got: {input}");
        }
        Ok(Self::new(owner, repo))
    }
}

impl Display for RepoRef {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}/{}", self.owner, self.repo)
    }
}

impl FromStr for RepoRef {
    type Err = anyhow::Error;

    fn from_str(s: &str) -> Result<Self> {
        Self::parse(s)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct IssueRef {
    pub repo: RepoRef,
    pub number: u64,
}

impl IssueRef {
    pub fn parse(input: &str) -> Result<Self> {
        let (repo_part, issue_part) = input
            .split_once('#')
            .ok_or_else(|| anyhow!("expected ref like owner/repo#123, got: {input}"))?;
        let repo = RepoRef::parse(repo_part)?;
        let number: u64 = issue_part
            .parse()
            .with_context(|| format!("invalid issue number in ref: {input}"))?;
        Ok(Self { repo, number })
    }
}

impl Display for IssueRef {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}#{}", self.repo, self.number)
    }
}

impl FromStr for IssueRef {
    type Err = anyhow::Error;

    fn from_str(s: &str) -> Result<Self> {
        Self::parse(s)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum OpenState {
    Open,
    Closed,
}

impl Display for OpenState {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let v = match self {
            Self::Open => "open",
            Self::Closed => "closed",
        };
        f.write_str(v)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ListState {
    Open,
    Closed,
    All,
}

impl Display for ListState {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let v = match self {
            Self::Open => "open",
            Self::Closed => "closed",
            Self::All => "all",
        };
        f.write_str(v)
    }
}

impl FromStr for ListState {
    type Err = anyhow::Error;

    fn from_str(s: &str) -> Result<Self> {
        match s {
            "open" => Ok(Self::Open),
            "closed" => Ok(Self::Closed),
            "all" => Ok(Self::All),
            _ => bail!("invalid list state: {s} (expected open|closed|all)"),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum WorkflowState {
    Triage,
    Spec,
    Ready,
    InProgress,
    Review,
    Blocked,
}

impl WorkflowState {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Triage => "triage",
            Self::Spec => "spec",
            Self::Ready => "ready",
            Self::InProgress => "in-progress",
            Self::Review => "review",
            Self::Blocked => "blocked",
        }
    }

    pub const fn label(self) -> &'static str {
        match self {
            Self::Triage => "state/triage",
            Self::Spec => "state/spec",
            Self::Ready => "state/ready",
            Self::InProgress => "state/in-progress",
            Self::Review => "state/review",
            Self::Blocked => "state/blocked",
        }
    }

    pub fn from_label(label: &str) -> Option<Self> {
        match label {
            "state/triage" => Some(Self::Triage),
            "state/spec" => Some(Self::Spec),
            "state/ready" => Some(Self::Ready),
            "state/in-progress" => Some(Self::InProgress),
            "state/review" => Some(Self::Review),
            "state/blocked" => Some(Self::Blocked),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum OrchdRuntimeState {
    Queued,
    Running,
    Blocked,
    Failed,
    Completed,
}

impl OrchdRuntimeState {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Queued => "queued",
            Self::Running => "running",
            Self::Blocked => "blocked",
            Self::Failed => "failed",
            Self::Completed => "completed",
        }
    }

    pub const fn label(self) -> &'static str {
        match self {
            Self::Queued => "orchd/state/queued",
            Self::Running => "orchd/state/running",
            Self::Blocked => "orchd/state/blocked",
            Self::Failed => "orchd/state/failed",
            Self::Completed => "orchd/state/completed",
        }
    }

    pub fn from_label(label: &str) -> Option<Self> {
        match label {
            "orchd/state/queued" => Some(Self::Queued),
            "orchd/state/running" => Some(Self::Running),
            "orchd/state/blocked" => Some(Self::Blocked),
            "orchd/state/failed" => Some(Self::Failed),
            "orchd/state/completed" => Some(Self::Completed),
            _ => None,
        }
    }
}

impl Display for OrchdRuntimeState {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl FromStr for OrchdRuntimeState {
    type Err = anyhow::Error;

    fn from_str(s: &str) -> Result<Self> {
        match s {
            "queued" => Ok(Self::Queued),
            "running" => Ok(Self::Running),
            "blocked" => Ok(Self::Blocked),
            "failed" => Ok(Self::Failed),
            "completed" => Ok(Self::Completed),
            _ => bail!(
                "invalid orchd runtime state: {s} (expected queued|running|blocked|failed|completed)"
            ),
        }
    }
}

impl Display for WorkflowState {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl FromStr for WorkflowState {
    type Err = anyhow::Error;

    fn from_str(s: &str) -> Result<Self> {
        match s {
            "triage" => Ok(Self::Triage),
            "spec" => Ok(Self::Spec),
            "ready" => Ok(Self::Ready),
            "in-progress" | "in_progress" => Ok(Self::InProgress),
            "review" => Ok(Self::Review),
            "blocked" => Ok(Self::Blocked),
            _ => bail!("invalid workflow state: {s}"),
        }
    }
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct ApiRepo {
    pub full_name: String,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct ApiLabel {
    pub id: u64,
    pub name: String,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct ApiIssue {
    pub number: u64,
    pub state: OpenState,
    pub title: String,
    #[serde(default)]
    pub body: Option<String>,
    pub html_url: String,
    #[serde(default)]
    pub labels: Vec<ApiLabel>,
    #[serde(default)]
    pub pull_request: Option<serde_json::Value>,
    pub repository: Option<ApiRepo>,
}

impl ApiIssue {
    pub fn workflow_state(&self) -> Result<Option<WorkflowState>> {
        let mut seen: Option<WorkflowState> = None;
        for label in &self.labels {
            if let Some(state) = WorkflowState::from_label(&label.name) {
                if let Some(prev) = seen {
                    bail!(
                        "issue #{} has multiple workflow labels: {} and {}",
                        self.number,
                        prev,
                        state
                    );
                }
                seen = Some(state);
            }
        }
        Ok(seen)
    }

    pub fn claimed_labels(&self) -> impl Iterator<Item = &ApiLabel> {
        self.labels
            .iter()
            .filter(|label| label.name.starts_with("claimed/"))
    }
}

#[cfg(test)]
mod tests {
    use super::OrchdRuntimeState;
    use anyhow::Result;

    #[test]
    fn orchd_runtime_state_round_trip_label() {
        for state in [
            OrchdRuntimeState::Queued,
            OrchdRuntimeState::Running,
            OrchdRuntimeState::Blocked,
            OrchdRuntimeState::Failed,
            OrchdRuntimeState::Completed,
        ] {
            assert_eq!(OrchdRuntimeState::from_label(state.label()), Some(state));
        }
    }

    #[test]
    fn orchd_runtime_state_parses_known_values() -> Result<()> {
        let parsed: OrchdRuntimeState = "running".parse()?;
        assert_eq!(parsed, OrchdRuntimeState::Running);
        Ok(())
    }
}
