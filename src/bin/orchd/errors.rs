use forgejo_agent::types::OrchdRuntimeState;

#[derive(Debug, thiserror::Error)]
pub(super) enum DispatchError {
    #[error("dispatch config not loaded")]
    ConfigNotLoaded,
    #[error("actor not allowed: {0}")]
    ActorNotAllowed(String),
    #[error("directive not configured: {0}")]
    DirectiveNotConfigured(String),
    #[error("role not configured: {0}")]
    RoleNotConfigured(String),
    #[error("repo workspace binding is required for impl dispatch: {0}")]
    RepoBindingMissing(String),
    #[error(
        "issue dispatch already in flight for {repo_full_name}#{issue_number} (dispatch {dispatch_id})"
    )]
    IssueDispatchInFlight {
        repo_full_name: String,
        issue_number: u64,
        dispatch_id: i64,
    },
    #[error("repo impl dispatch already in flight for {repo_full_name} (dispatch {dispatch_id})")]
    RepoImplDispatchInFlight {
        repo_full_name: String,
        dispatch_id: i64,
    },
    #[error("invalid issue ref: {0}")]
    InvalidIssueRef(String),
    #[error("prompt template failure: {0}")]
    PromptTemplate(String),
    #[error("io failure: {0}")]
    Io(String),
    #[error("launch failure: {0}")]
    Launch(String),
    #[error("issue fetch failure: {0}")]
    IssueFetch(String),
    #[error("db failure: {0}")]
    Db(String),
}

impl DispatchError {
    pub(super) const fn reason_code(&self) -> &'static str {
        match self {
            Self::ConfigNotLoaded => "dispatch_config_missing",
            Self::ActorNotAllowed(_) => "actor_not_allowed",
            Self::DirectiveNotConfigured(_) => "directive_not_configured",
            Self::RoleNotConfigured(_) => "role_not_configured",
            Self::RepoBindingMissing(_) => "repo_binding_missing",
            Self::IssueDispatchInFlight { .. } => "issue_dispatch_in_flight",
            Self::RepoImplDispatchInFlight { .. } => "repo_impl_dispatch_in_flight",
            Self::InvalidIssueRef(_) => "invalid_issue_ref",
            Self::PromptTemplate(_) => "prompt_template_error",
            Self::Io(_) => "io_failure",
            Self::Launch(_) => "launch_failure",
            Self::IssueFetch(_) => "issue_fetch_failure",
            Self::Db(_) => "db_failure",
        }
    }
}

pub(super) const fn runtime_state_for_dispatch_error(error: &DispatchError) -> OrchdRuntimeState {
    match error {
        DispatchError::IssueDispatchInFlight { .. } => OrchdRuntimeState::Running,
        DispatchError::RepoImplDispatchInFlight { .. } => OrchdRuntimeState::Queued,
        DispatchError::ActorNotAllowed(_)
        | DispatchError::DirectiveNotConfigured(_)
        | DispatchError::RoleNotConfigured(_)
        | DispatchError::RepoBindingMissing(_)
        | DispatchError::InvalidIssueRef(_) => OrchdRuntimeState::Blocked,
        DispatchError::PromptTemplate(_)
        | DispatchError::ConfigNotLoaded
        | DispatchError::Io(_)
        | DispatchError::Launch(_)
        | DispatchError::IssueFetch(_)
        | DispatchError::Db(_) => OrchdRuntimeState::Failed,
    }
}
