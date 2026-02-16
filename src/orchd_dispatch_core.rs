use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DispatchBackendKind {
    Systemd,
    Local,
}

impl DispatchBackendKind {
    #[must_use]
    pub const fn as_db_str(self) -> &'static str {
        match self {
            Self::Systemd => "systemd",
            Self::Local => "local",
        }
    }

    #[must_use]
    pub fn parse_db(value: &str) -> Option<Self> {
        match value {
            "systemd" => Some(Self::Systemd),
            "local" => Some(Self::Local),
            _ => None,
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct RunHandle {
    pub backend_kind: DispatchBackendKind,
    pub backend_ref: String,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DispatchPolicyOutcome {
    Allow,
    Deny,
    Hold,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct PolicyDecision {
    pub outcome: DispatchPolicyOutcome,
    pub reasons: Vec<String>,
}

impl PolicyDecision {
    #[must_use]
    pub const fn allow() -> Self {
        Self {
            outcome: DispatchPolicyOutcome::Allow,
            reasons: Vec::new(),
        }
    }

    #[must_use]
    pub fn deny(reason: impl Into<String>) -> Self {
        Self {
            outcome: DispatchPolicyOutcome::Deny,
            reasons: vec![reason.into()],
        }
    }

    #[must_use]
    pub fn hold(reason: impl Into<String>) -> Self {
        Self {
            outcome: DispatchPolicyOutcome::Hold,
            reasons: vec![reason.into()],
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct DispatchIntentV1 {
    pub intent_id: String,
    pub repo_full_name: String,
    pub issue_number: u64,
    pub role: String,
    pub directive: String,
    pub actor_login: String,
    pub delivery_id: String,
    pub parent_dispatch_id: Option<i64>,
    pub created_at: DateTime<Utc>,
    pub policy_snapshot: Option<String>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DispatchState {
    Queued,
    Launching,
    Starting,
    Running,
    Completed,
    FailedStart,
    FailedRuntime,
    Blocked,
    TimedOut,
    Canceled,
}

impl DispatchState {
    #[must_use]
    pub const fn as_db_str(self) -> &'static str {
        match self {
            Self::Queued => "queued",
            Self::Launching => "launching",
            Self::Starting => "starting",
            Self::Running => "running",
            Self::Completed => "completed",
            Self::FailedStart => "failed_start",
            Self::FailedRuntime => "failed_runtime",
            Self::Blocked => "blocked",
            Self::TimedOut => "timed_out",
            Self::Canceled => "canceled",
        }
    }

    #[must_use]
    pub fn parse_db(value: &str) -> Option<Self> {
        match value {
            "queued" => Some(Self::Queued),
            "launching" => Some(Self::Launching),
            "starting" => Some(Self::Starting),
            "running" => Some(Self::Running),
            "completed" => Some(Self::Completed),
            "failed_start" => Some(Self::FailedStart),
            "failed_runtime" => Some(Self::FailedRuntime),
            "blocked" => Some(Self::Blocked),
            "timed_out" => Some(Self::TimedOut),
            "canceled" => Some(Self::Canceled),
            _ => None,
        }
    }

    #[must_use]
    pub const fn is_terminal(self) -> bool {
        matches!(
            self,
            Self::Completed
                | Self::FailedStart
                | Self::FailedRuntime
                | Self::Blocked
                | Self::TimedOut
                | Self::Canceled
        )
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DispatchEventKind {
    Reserve,
    BeginLaunch,
    MarkStarting,
    MarkRunning,
    Complete,
    FailStart,
    FailRuntime,
    Block,
    Timeout,
    Cancel,
    HealStale,
}

impl DispatchEventKind {
    #[must_use]
    pub const fn as_db_str(self) -> &'static str {
        match self {
            Self::Reserve => "reserve",
            Self::BeginLaunch => "begin_launch",
            Self::MarkStarting => "mark_starting",
            Self::MarkRunning => "mark_running",
            Self::Complete => "complete",
            Self::FailStart => "fail_start",
            Self::FailRuntime => "fail_runtime",
            Self::Block => "block",
            Self::Timeout => "timeout",
            Self::Cancel => "cancel",
            Self::HealStale => "heal_stale",
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash, Ord, PartialOrd, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DispatchNotificationPhase {
    Started,
    Completed,
    Failed,
    Blocked,
}

impl DispatchNotificationPhase {
    #[must_use]
    pub const fn as_db_str(self) -> &'static str {
        match self {
            Self::Started => "started",
            Self::Completed => "completed",
            Self::Failed => "failed",
            Self::Blocked => "blocked",
        }
    }

    #[must_use]
    pub fn parse_db(value: &str) -> Option<Self> {
        match value {
            "started" => Some(Self::Started),
            "completed" => Some(Self::Completed),
            "failed" => Some(Self::Failed),
            "blocked" => Some(Self::Blocked),
            _ => None,
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct DispatchEvent {
    pub dispatch_id: i64,
    pub kind: DispatchEventKind,
    pub reason_code: Option<String>,
    pub error_text: Option<String>,
    pub happened_at: DateTime<Utc>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub enum OutputProjection {
    OrchdStateLabel { state: DispatchState },
    Comment { body: String },
}

#[derive(Debug, thiserror::Error)]
#[error("invalid dispatch transition: {state:?} + {event:?}")]
pub struct TransitionError {
    pub state: DispatchState,
    pub event: DispatchEventKind,
}

pub fn reduce_dispatch_state(
    state: DispatchState,
    event: DispatchEventKind,
) -> Result<DispatchState, TransitionError> {
    use DispatchEventKind::{
        BeginLaunch, Block, Cancel, Complete, FailRuntime, FailStart, HealStale, MarkRunning,
        MarkStarting, Reserve, Timeout,
    };
    use DispatchState::{
        Blocked, Canceled, Completed, FailedRuntime, FailedStart, Launching, Queued, Running,
        Starting, TimedOut,
    };

    let next = match (state, event) {
        (Queued, Reserve) => Queued,
        (Queued, BeginLaunch) => Launching,
        (Launching, MarkStarting) => Starting,
        (Launching, FailStart | Timeout | Cancel) => match event {
            FailStart => FailedStart,
            Timeout => TimedOut,
            Cancel => Canceled,
            _ => unreachable!("match arm only accepts fail/timeout/cancel"),
        },
        (Starting, MarkRunning) => Running,
        (Starting, FailStart) => FailedStart,
        (Starting | Running, FailRuntime | HealStale) => FailedRuntime,
        (Starting | Running, Timeout) => TimedOut,
        (Starting | Running, Cancel) => Canceled,
        (Running, Complete) => Completed,
        (Running, Block) => Blocked,
        _ => return Err(TransitionError { state, event }),
    };
    Ok(next)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reducer_accepts_happy_path() {
        let state = reduce_dispatch_state(DispatchState::Queued, DispatchEventKind::BeginLaunch)
            .expect("queued -> launching");
        let state = reduce_dispatch_state(state, DispatchEventKind::MarkStarting)
            .expect("launching -> starting");
        let state = reduce_dispatch_state(state, DispatchEventKind::MarkRunning)
            .expect("starting -> running");
        let state =
            reduce_dispatch_state(state, DispatchEventKind::Complete).expect("running -> complete");
        assert_eq!(state, DispatchState::Completed);
        assert!(state.is_terminal());
    }

    #[test]
    fn reducer_rejects_invalid_edge() {
        let err = reduce_dispatch_state(DispatchState::Queued, DispatchEventKind::Complete)
            .expect_err("queued cannot complete directly");
        assert_eq!(err.state, DispatchState::Queued);
        assert_eq!(err.event, DispatchEventKind::Complete);
    }

    #[test]
    fn db_round_trip_for_state_names() {
        for state in [
            DispatchState::Queued,
            DispatchState::Launching,
            DispatchState::Starting,
            DispatchState::Running,
            DispatchState::Completed,
            DispatchState::FailedStart,
            DispatchState::FailedRuntime,
            DispatchState::Blocked,
            DispatchState::TimedOut,
            DispatchState::Canceled,
        ] {
            let parsed = DispatchState::parse_db(state.as_db_str())
                .expect("state should parse from db literal");
            assert_eq!(parsed, state);
        }
    }
}
