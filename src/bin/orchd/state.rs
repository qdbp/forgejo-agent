use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use forgejo_agent::config::AgentConfig;
use forgejo_agent::types::RepoRef;

use super::cli::{DispatchBackend, DispatchMode};
use super::dispatch_config::DispatchConfig;
use super::lexicon::DECISION_IGNORED;

#[derive(Clone)]
pub(super) struct AppState {
    pub(super) db_path: PathBuf,
    pub(super) webhook_secret: Option<Vec<u8>>,
    pub(super) webhook_url: String,
    pub(super) cfg: AgentConfig,
    pub(super) forgejo_config_file: Option<PathBuf>,
    pub(super) reconcile_repo: RepoRef,
    pub(super) dispatch_mode: DispatchMode,
    pub(super) dispatch_backend: DispatchBackend,
    pub(super) dispatch_config: Option<DispatchConfig>,
}

#[derive(Debug, Deserialize)]
pub(super) struct WebhookPayload {
    pub(super) action: Option<String>,
    pub(super) repository: Option<WebhookRepository>,
    pub(super) issue: Option<WebhookIssue>,
    pub(super) comment: Option<WebhookComment>,
    pub(super) sender: Option<WebhookUser>,
}

#[derive(Debug, Deserialize)]
pub(super) struct WebhookRepository {
    pub(super) full_name: String,
}

#[derive(Debug, Deserialize)]
pub(super) struct WebhookIssue {
    #[serde(default)]
    pub(super) id: Option<u64>,
    pub(super) number: u64,
    #[serde(default)]
    pub(super) body: Option<String>,
    #[serde(default)]
    pub(super) created_at: Option<String>,
    #[serde(default)]
    pub(super) updated_at: Option<String>,
    #[serde(default)]
    pub(super) closed_at: Option<String>,
    #[serde(default)]
    pub(super) assignee: Option<WebhookUser>,
    #[serde(default)]
    pub(super) assignees: Option<Vec<WebhookUser>>,
}

#[derive(Debug, Deserialize)]
pub(super) struct WebhookComment {
    #[serde(default)]
    pub(super) id: Option<u64>,
    pub(super) body: String,
    #[serde(default)]
    pub(super) created_at: Option<String>,
    #[serde(default)]
    pub(super) user: Option<WebhookUser>,
}

#[derive(Debug, Deserialize)]
pub(super) struct WebhookUser {
    pub(super) login: String,
}

#[derive(Debug, Clone)]
pub(super) struct EventRecord {
    pub(super) delivery_id: String,
    pub(super) event_type: String,
    pub(super) repo_full_name: String,
    pub(super) issue_number: Option<u64>,
    pub(super) source_issue_id: Option<u64>,
    pub(super) source_issue_anchor_at: Option<String>,
    pub(super) action: Option<String>,
    pub(super) actor_login: Option<String>,
    pub(super) event_text: Option<String>,
    pub(super) source_comment_id: Option<u64>,
    pub(super) source_created_at: Option<String>,
    pub(super) raw_json: String,
}

#[derive(Debug, Clone)]
pub(super) struct EventContext {
    pub(super) repo_full_name: String,
    pub(super) issue_number: Option<u64>,
    pub(super) source_issue_id: Option<u64>,
    pub(super) source_issue_anchor_at: Option<String>,
    pub(super) actor_login: Option<String>,
    pub(super) text: Option<String>,
    pub(super) source_comment_id: Option<u64>,
    pub(super) source_created_at: Option<String>,
    pub(super) assignees: Vec<String>,
}

#[derive(Debug, Clone)]
pub(super) struct ParsedDirective {
    pub(super) role: String,
    pub(super) directive: String,
    pub(super) profile: Option<String>,
}

#[derive(Debug, Clone)]
pub(super) struct DecisionRecord {
    pub(super) decision: String,
    pub(super) reason_code: String,
    pub(super) directive: Option<String>,
    pub(super) target_role: Option<String>,
    pub(super) would_dispatch: bool,
    pub(super) decision_source: String,
    pub(super) trigger_id: Option<String>,
    pub(super) trigger_dedupe_key: Option<String>,
    pub(super) trigger_apply_guardrails: bool,
}

impl DecisionRecord {
    pub(super) fn ignored(reason_code: impl Into<String>) -> Self {
        Self {
            decision: DECISION_IGNORED.to_string(),
            reason_code: reason_code.into(),
            directive: None,
            target_role: None,
            would_dispatch: false,
            decision_source: "none".to_string(),
            trigger_id: None,
            trigger_dedupe_key: None,
            trigger_apply_guardrails: false,
        }
    }

    pub(super) fn suppress_dispatch(&mut self, reason_code: impl Into<String>) {
        self.decision = DECISION_IGNORED.to_string();
        self.reason_code = reason_code.into();
        self.directive = None;
        self.target_role = None;
        self.would_dispatch = false;
        self.decision_source = "none".to_string();
        self.trigger_apply_guardrails = false;
    }
}

#[derive(Debug, Serialize)]
pub(super) struct WebhookOutcome {
    pub(super) status: String,
    pub(super) delivery_id: String,
    pub(super) event_type: String,
    pub(super) decision: String,
    pub(super) reason_code: String,
    pub(super) duplicate: bool,
}

#[derive(Debug)]
pub(super) struct IssueEventDeltaRow {
    pub(super) event_type: String,
    pub(super) actor_login: Option<String>,
    pub(super) event_text: Option<String>,
    pub(super) received_at: String,
    pub(super) source_created_at: Option<String>,
}

#[derive(Debug, Serialize)]
pub(super) struct ErrorEnvelope {
    pub(super) error: String,
}

#[derive(Debug, Serialize)]
pub(super) struct HealthEnvelope {
    pub(super) status: &'static str,
    pub(super) build: &'static str,
    pub(super) git_sha: Option<&'static str>,
}
