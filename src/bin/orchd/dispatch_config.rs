use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, anyhow};
use serde::Deserialize;

use forgejo_agent::orchd_dispatch_core::DispatchNotificationPhase;
use forgejo_agent::types::RepoRef;

use super::lexicon::{DIRECTIVE_REPLY, EVENT_ISSUE_COMMENT, EVENT_ISSUES, directive_is_known};
use super::paths::expand_tilde_path;

#[derive(Clone, Debug)]
pub(super) struct DispatchConfig {
    pub(super) allowed_actors: Vec<String>,
    pub(super) prompt_envelopes: DispatchPromptEnvelopeConfig,
    pub(super) notifications: DispatchNotificationsConfig,
    pub(super) roles: HashMap<String, DispatchRoleConfig>,
    pub(super) control_plane: Option<DispatchControlPlaneConfig>,
    pub(super) directives: HashMap<String, DispatchDirectiveConfig>,
    pub(super) repo_bindings: HashMap<String, DispatchRepoBindingConfig>,
    pub(super) triggers: Vec<DispatchTriggerConfig>,
    pub(super) trigger_guardrails: DispatchTriggerGuardrailsConfig,
    pub(super) forgejoctl_bin: PathBuf,
}

#[derive(Clone, Debug)]
pub(super) struct DispatchPromptEnvelopeConfig {
    pub(super) preamble_file: PathBuf,
    pub(super) fresh_envelope: PathBuf,
    pub(super) followup_envelope: PathBuf,
    pub(super) turn_context_file: PathBuf,
    pub(super) issue_fresh_file: PathBuf,
    pub(super) issue_followup_file: PathBuf,
}

impl DispatchPromptEnvelopeConfig {
    pub(super) fn role_card_file_for(&self, role_name: &str) -> PathBuf {
        self.preamble_file
            .parent()
            .map_or_else(|| PathBuf::from("roles"), |parent| parent.join("roles"))
            .join(format!("{role_name}.md"))
    }
}

#[derive(Clone, Debug)]
pub(super) struct DispatchNotificationsConfig {
    pub(super) enabled: bool,
    pub(super) poll_sec: u64,
    pub(super) phases: Vec<DispatchNotificationPhase>,
    pub(super) app_name: String,
    pub(super) watch_login: String,
    pub(super) notify_send_bin: PathBuf,
}

#[derive(Clone, Debug)]
pub(super) struct DispatchRoleConfig {
    pub(super) codex_bin: PathBuf,
    pub(super) codex_role_arg: String,
    pub(super) forgejo_login: String,
    pub(super) token_file: PathBuf,
}

#[derive(Clone, Debug)]
pub(super) struct DispatchControlPlaneConfig {
    pub(super) token_file: PathBuf,
}

#[derive(Clone, Debug)]
pub(super) struct DispatchDirectiveConfig {
    pub(super) role: String,
    pub(super) prompt_file: PathBuf,
    pub(super) timeout_sec: u64,
}

#[derive(Clone, Debug)]
pub(super) struct DispatchRepoBindingConfig {
    pub(super) local_path: PathBuf,
    pub(super) git_remote: String,
    pub(super) git_base: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) enum DispatchTriggerClass {
    ExplicitDirective,
    AssigneeReply,
    Registered,
}

impl DispatchTriggerClass {
    pub(super) const fn precedence_rank(&self) -> u8 {
        match self {
            Self::ExplicitDirective => 3,
            Self::AssigneeReply => 2,
            Self::Registered => 1,
        }
    }
}

#[derive(Clone, Debug)]
pub(super) struct DispatchTriggerConfig {
    pub(super) id: String,
    pub(super) class: DispatchTriggerClass,
    pub(super) priority: i32,
    pub(super) matcher: DispatchTriggerMatcher,
    pub(super) guards: DispatchTriggerGuards,
    pub(super) action: DispatchTriggerAction,
    pub(super) apply_guardrails: bool,
}

#[derive(Clone, Debug)]
pub(super) struct DispatchTriggerMatcher {
    pub(super) event_type: String,
    pub(super) actions: Vec<String>,
}

#[derive(Clone, Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct DispatchTriggerGuards {
    pub(super) directive: DispatchTriggerDirectiveGuard,
    pub(super) assignee: DispatchTriggerAssigneeGuard,
    pub(super) actor: DispatchTriggerActorGuard,
}

#[derive(Clone, Debug, Default, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub(super) enum DispatchTriggerDirectiveGuard {
    #[default]
    Any,
    RequireParsed,
    RequireAbsent,
}

#[derive(Clone, Debug, Default, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub(super) enum DispatchTriggerAssigneeGuard {
    #[default]
    Any,
    RequireSingleCodex,
}

#[derive(Clone, Debug, Default, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub(super) enum DispatchTriggerActorGuard {
    #[default]
    Any,
    RequireNotAssignee,
}

#[derive(Clone, Debug)]
pub(super) struct DispatchTriggerAction {
    pub(super) directive: DispatchTriggerDirectiveSource,
    pub(super) target_role: DispatchTriggerRoleSource,
    pub(super) reason_code: String,
}

#[derive(Clone, Debug)]
pub(super) enum DispatchTriggerDirectiveSource {
    Literal(String),
    ParsedDirective,
}

#[derive(Clone, Debug)]
pub(super) enum DispatchTriggerRoleSource {
    Literal(String),
    ParsedDirectiveRole,
    SingleAssignee,
}

#[derive(Clone, Debug)]
pub(super) struct DispatchTriggerGuardrailsConfig {
    pub(super) max_depth_per_issue: u32,
    pub(super) max_dispatches_per_window: u32,
    pub(super) window_sec: u64,
    pub(super) cooldown_sec: u64,
    pub(super) deny_immediate_self_loop: bool,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct DispatchConfigFile {
    version: u32,
    #[serde(default)]
    allowed_actors: Vec<String>,
    #[serde(default)]
    prompt_envelopes: DispatchPromptEnvelopeConfigFile,
    #[serde(default)]
    notifications: DispatchNotificationsConfigFile,
    roles: HashMap<String, DispatchRoleConfigFile>,
    #[serde(default)]
    control_plane: Option<DispatchControlPlaneConfigFile>,
    directives: HashMap<String, DispatchDirectiveConfigFile>,
    #[serde(default)]
    repo_bindings: Vec<DispatchRepoBindingConfigFile>,
    #[serde(default = "default_legacy_triggers")]
    legacy_triggers: bool,
    #[serde(default)]
    trigger_guardrails: DispatchTriggerGuardrailsConfigFile,
    #[serde(default)]
    triggers: Vec<DispatchTriggerConfigFile>,
    #[serde(default = "default_forgejoctl_bin")]
    forgejoctl_bin: String,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct DispatchPromptEnvelopeConfigFile {
    #[serde(default = "default_preamble_file")]
    preamble_file: String,
    #[serde(default = "default_fresh_envelope")]
    fresh_envelope: String,
    #[serde(default = "default_followup_envelope")]
    followup_envelope: String,
    #[serde(default = "default_turn_context_file")]
    turn_context_file: String,
    #[serde(default = "default_issue_fresh_file")]
    issue_fresh_file: String,
    #[serde(default = "default_issue_followup_file")]
    issue_followup_file: String,
}

impl Default for DispatchPromptEnvelopeConfigFile {
    fn default() -> Self {
        Self {
            preamble_file: default_preamble_file(),
            fresh_envelope: default_fresh_envelope(),
            followup_envelope: default_followup_envelope(),
            turn_context_file: default_turn_context_file(),
            issue_fresh_file: default_issue_fresh_file(),
            issue_followup_file: default_issue_followup_file(),
        }
    }
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct DispatchNotificationsConfigFile {
    #[serde(default)]
    enabled: bool,
    #[serde(default = "default_notify_poll_sec")]
    poll_sec: u64,
    #[serde(default = "default_notify_phases")]
    phases: Vec<DispatchNotificationPhase>,
    #[serde(default = "default_notify_app_name")]
    app_name: String,
    #[serde(default = "default_watch_login")]
    watch_login: String,
    #[serde(default = "default_notify_send_bin")]
    notify_send_bin: String,
}

impl Default for DispatchNotificationsConfigFile {
    fn default() -> Self {
        Self {
            enabled: false,
            poll_sec: default_notify_poll_sec(),
            phases: default_notify_phases(),
            app_name: default_notify_app_name(),
            watch_login: default_watch_login(),
            notify_send_bin: default_notify_send_bin(),
        }
    }
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct DispatchRoleConfigFile {
    #[serde(default = "default_codex_bin")]
    codex_bin: String,
    codex_role_arg: Option<String>,
    forgejo_login: Option<String>,
    token_file: String,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct DispatchControlPlaneConfigFile {
    token_file: String,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct DispatchDirectiveConfigFile {
    role: String,
    prompt_file: String,
    #[serde(default = "default_timeout_sec")]
    timeout_sec: u64,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct DispatchRepoBindingConfigFile {
    repo: String,
    local_path: String,
    #[serde(default = "default_git_remote")]
    git_remote: String,
    #[serde(default = "default_git_base")]
    git_base: String,
}

#[derive(Debug, Deserialize, Default)]
#[serde(deny_unknown_fields)]
struct DispatchTriggerGuardrailsConfigFile {
    #[serde(default = "default_trigger_depth")]
    max_depth_per_issue: u32,
    #[serde(default = "default_trigger_window_limit")]
    max_dispatches_per_window: u32,
    #[serde(default = "default_trigger_window_sec")]
    window_sec: u64,
    #[serde(default = "default_trigger_cooldown_sec")]
    cooldown_sec: u64,
    #[serde(default = "default_trigger_deny_self_loop")]
    deny_immediate_self_loop: bool,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct DispatchTriggerConfigFile {
    id: String,
    event: String,
    actions: Vec<String>,
    #[serde(default)]
    priority: i32,
    #[serde(default)]
    guards: DispatchTriggerGuards,
    action: DispatchTriggerActionConfigFile,
    #[serde(default = "default_true")]
    apply_guardrails: bool,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct DispatchTriggerActionConfigFile {
    directive: Option<String>,
    directive_from: Option<String>,
    target_role: Option<String>,
    target_role_from: Option<String>,
    reason_code: Option<String>,
}

fn default_codex_bin() -> String {
    "/home/main/forgejo-agent/bin/codex-role".to_string()
}

fn default_forgejoctl_bin() -> String {
    "/home/main/.local/bin/forgejoctl".to_string()
}

fn default_preamble_file() -> String {
    "../prompts/orchd-preamble.md".to_string()
}

fn default_fresh_envelope() -> String {
    "../prompts/orchd-envelope-fresh.md".to_string()
}

fn default_followup_envelope() -> String {
    "../prompts/orchd-envelope-followup.md".to_string()
}

fn default_turn_context_file() -> String {
    "../prompts/orchd-turn-context.md".to_string()
}

fn default_issue_fresh_file() -> String {
    "../prompts/orchd-issue-fresh.md".to_string()
}

fn default_issue_followup_file() -> String {
    "../prompts/orchd-issue-followup.md".to_string()
}

fn default_git_remote() -> String {
    "origin".to_string()
}

fn default_git_base() -> String {
    "main".to_string()
}

const fn default_notify_poll_sec() -> u64 {
    10
}

fn default_notify_phases() -> Vec<DispatchNotificationPhase> {
    vec![
        DispatchNotificationPhase::Completed,
        DispatchNotificationPhase::Failed,
        DispatchNotificationPhase::Blocked,
    ]
}

fn default_notify_app_name() -> String {
    "orchd".to_string()
}

fn default_watch_login() -> String {
    "main".to_string()
}

fn default_notify_send_bin() -> String {
    "/usr/bin/notify-send".to_string()
}

const fn default_timeout_sec() -> u64 {
    3600
}

const fn default_true() -> bool {
    true
}

const fn default_legacy_triggers() -> bool {
    true
}

const fn default_trigger_depth() -> u32 {
    6
}

const fn default_trigger_window_limit() -> u32 {
    12
}

const fn default_trigger_window_sec() -> u64 {
    3600
}

const fn default_trigger_cooldown_sec() -> u64 {
    60
}

const fn default_trigger_deny_self_loop() -> bool {
    true
}

pub(super) fn legacy_trigger_pack() -> Vec<DispatchTriggerConfig> {
    vec![
        DispatchTriggerConfig {
            id: "legacy.explicit.issue_comment".to_string(),
            class: DispatchTriggerClass::ExplicitDirective,
            priority: 0,
            matcher: DispatchTriggerMatcher {
                event_type: EVENT_ISSUE_COMMENT.to_string(),
                actions: vec!["created".to_string(), "edited".to_string()],
            },
            guards: DispatchTriggerGuards {
                directive: DispatchTriggerDirectiveGuard::RequireParsed,
                assignee: DispatchTriggerAssigneeGuard::Any,
                actor: DispatchTriggerActorGuard::Any,
            },
            action: DispatchTriggerAction {
                directive: DispatchTriggerDirectiveSource::ParsedDirective,
                target_role: DispatchTriggerRoleSource::ParsedDirectiveRole,
                reason_code: "explicit_directive".to_string(),
            },
            apply_guardrails: false,
        },
        DispatchTriggerConfig {
            id: "legacy.explicit.issues".to_string(),
            class: DispatchTriggerClass::ExplicitDirective,
            priority: 0,
            matcher: DispatchTriggerMatcher {
                event_type: EVENT_ISSUES.to_string(),
                actions: vec!["opened".to_string(), "edited".to_string()],
            },
            guards: DispatchTriggerGuards {
                directive: DispatchTriggerDirectiveGuard::RequireParsed,
                assignee: DispatchTriggerAssigneeGuard::Any,
                actor: DispatchTriggerActorGuard::Any,
            },
            action: DispatchTriggerAction {
                directive: DispatchTriggerDirectiveSource::ParsedDirective,
                target_role: DispatchTriggerRoleSource::ParsedDirectiveRole,
                reason_code: "explicit_directive".to_string(),
            },
            apply_guardrails: false,
        },
        DispatchTriggerConfig {
            id: "legacy.assignee.reply".to_string(),
            class: DispatchTriggerClass::AssigneeReply,
            priority: 0,
            matcher: DispatchTriggerMatcher {
                event_type: EVENT_ISSUE_COMMENT.to_string(),
                actions: vec!["created".to_string(), "edited".to_string()],
            },
            guards: DispatchTriggerGuards {
                directive: DispatchTriggerDirectiveGuard::RequireAbsent,
                assignee: DispatchTriggerAssigneeGuard::RequireSingleCodex,
                actor: DispatchTriggerActorGuard::RequireNotAssignee,
            },
            action: DispatchTriggerAction {
                directive: DispatchTriggerDirectiveSource::Literal(DIRECTIVE_REPLY.to_string()),
                target_role: DispatchTriggerRoleSource::SingleAssignee,
                reason_code: "assignee_reply".to_string(),
            },
            apply_guardrails: true,
        },
    ]
}

pub(super) fn load_dispatch_config(path: &Path) -> Result<DispatchConfig> {
    let raw_text = fs::read_to_string(path)
        .with_context(|| format!("failed to read dispatch config: {}", path.display()))?;
    let raw: DispatchConfigFile =
        toml::from_str(&raw_text).with_context(|| format!("invalid TOML: {}", path.display()))?;

    if raw.version != 1 {
        return Err(anyhow!(
            "unsupported dispatch config version {} in {}",
            raw.version,
            path.display()
        ));
    }
    if raw.allowed_actors.is_empty() {
        return Err(anyhow!(
            "dispatch config {} has empty allowed_actors",
            path.display()
        ));
    }

    let base_dir = path
        .parent()
        .map(PathBuf::from)
        .ok_or_else(|| anyhow!("dispatch config has no parent: {}", path.display()))?;

    let mut roles = HashMap::new();
    for (role_name, role) in raw.roles {
        let role_name = role_name.to_ascii_lowercase();
        let forgejo_login = role.forgejo_login.unwrap_or_else(|| role_name.clone());
        let codex_role_arg = role.codex_role_arg.unwrap_or_else(|| {
            role_name
                .strip_prefix("codex-")
                .unwrap_or(role_name.as_str())
                .to_string()
        });
        roles.insert(
            role_name,
            DispatchRoleConfig {
                codex_bin: resolve_config_path(&base_dir, &role.codex_bin)?,
                codex_role_arg,
                forgejo_login,
                token_file: resolve_config_path(&base_dir, &role.token_file)?,
            },
        );
    }

    let mut directives = HashMap::new();
    for (directive_name, directive) in raw.directives {
        directives.insert(
            directive_name.to_ascii_lowercase(),
            DispatchDirectiveConfig {
                role: directive.role.to_ascii_lowercase(),
                prompt_file: resolve_config_path(&base_dir, &directive.prompt_file)?,
                timeout_sec: directive.timeout_sec.max(30),
            },
        );
    }

    let prompt_envelopes = DispatchPromptEnvelopeConfig {
        preamble_file: resolve_config_path(&base_dir, &raw.prompt_envelopes.preamble_file)?,
        fresh_envelope: resolve_config_path(&base_dir, &raw.prompt_envelopes.fresh_envelope)?,
        followup_envelope: resolve_config_path(&base_dir, &raw.prompt_envelopes.followup_envelope)?,
        turn_context_file: resolve_config_path(&base_dir, &raw.prompt_envelopes.turn_context_file)?,
        issue_fresh_file: resolve_config_path(&base_dir, &raw.prompt_envelopes.issue_fresh_file)?,
        issue_followup_file: resolve_config_path(
            &base_dir,
            &raw.prompt_envelopes.issue_followup_file,
        )?,
    };

    let control_plane = raw
        .control_plane
        .map(|control| -> Result<DispatchControlPlaneConfig> {
            Ok(DispatchControlPlaneConfig {
                token_file: resolve_config_path(&base_dir, &control.token_file)?,
            })
        })
        .transpose()?;

    let mut role_names: Vec<_> = roles.keys().cloned().collect();
    role_names.sort();
    for role_name in role_names {
        let role_card_file = prompt_envelopes.role_card_file_for(&role_name);
        if !role_card_file.is_file() {
            return Err(anyhow!(
                "dispatch config {} missing role card for role {} at {}",
                path.display(),
                role_name,
                role_card_file.display()
            ));
        }
    }
    let mut repo_bindings = HashMap::new();
    for binding in raw.repo_bindings {
        let repo = RepoRef::parse(&binding.repo)
            .with_context(|| format!("invalid repo binding repo '{}'", binding.repo))?;
        let repo_full_name = repo.to_string();
        if repo_bindings.contains_key(&repo_full_name) {
            return Err(anyhow!(
                "duplicate repo binding for {repo_full_name} in {}",
                path.display()
            ));
        }
        let git_remote = binding.git_remote.trim().to_string();
        if git_remote.is_empty() {
            return Err(anyhow!(
                "repo binding for {repo_full_name} has empty git_remote in {}",
                path.display()
            ));
        }
        let git_base = binding.git_base.trim().to_string();
        if git_base.is_empty() {
            return Err(anyhow!(
                "repo binding for {repo_full_name} has empty git_base in {}",
                path.display()
            ));
        }
        repo_bindings.insert(
            repo_full_name,
            DispatchRepoBindingConfig {
                local_path: resolve_config_path(&base_dir, &binding.local_path)?,
                git_remote,
                git_base,
            },
        );
    }

    let mut notification_phases = raw.notifications.phases;
    notification_phases.sort_unstable();
    notification_phases.dedup();

    let mut triggers = Vec::new();
    if raw.legacy_triggers {
        triggers.extend(legacy_trigger_pack());
    }

    for trigger in raw.triggers {
        triggers.push(compile_registered_trigger(
            path,
            trigger,
            &directives,
            &roles,
        )?);
    }

    let mut seen_ids = HashSet::new();
    for trigger in &triggers {
        if !seen_ids.insert(trigger.id.clone()) {
            return Err(anyhow!(
                "dispatch config {} has duplicate trigger id '{}'",
                path.display(),
                trigger.id
            ));
        }
    }

    Ok(DispatchConfig {
        allowed_actors: raw
            .allowed_actors
            .into_iter()
            .map(|actor| actor.to_ascii_lowercase())
            .collect(),
        prompt_envelopes,
        notifications: DispatchNotificationsConfig {
            enabled: raw.notifications.enabled,
            poll_sec: raw.notifications.poll_sec.max(1),
            phases: notification_phases,
            app_name: raw.notifications.app_name,
            watch_login: raw.notifications.watch_login.to_ascii_lowercase(),
            notify_send_bin: resolve_config_path(&base_dir, &raw.notifications.notify_send_bin)?,
        },
        roles,
        control_plane,
        directives,
        repo_bindings,
        triggers,
        trigger_guardrails: DispatchTriggerGuardrailsConfig {
            max_depth_per_issue: raw.trigger_guardrails.max_depth_per_issue.max(1),
            max_dispatches_per_window: raw.trigger_guardrails.max_dispatches_per_window.max(1),
            window_sec: raw.trigger_guardrails.window_sec.max(1),
            cooldown_sec: raw.trigger_guardrails.cooldown_sec,
            deny_immediate_self_loop: raw.trigger_guardrails.deny_immediate_self_loop,
        },
        forgejoctl_bin: resolve_config_path(&base_dir, &raw.forgejoctl_bin)?,
    })
}

fn compile_registered_trigger(
    config_path: &Path,
    trigger: DispatchTriggerConfigFile,
    directives: &HashMap<String, DispatchDirectiveConfig>,
    roles: &HashMap<String, DispatchRoleConfig>,
) -> Result<DispatchTriggerConfig> {
    let trigger_id = trigger.id.trim().to_string();
    if trigger_id.is_empty() {
        return Err(anyhow!(
            "dispatch config {} contains trigger with empty id",
            config_path.display()
        ));
    }

    let event_type = trigger.event.trim().to_ascii_lowercase();
    if !matches!(event_type.as_str(), EVENT_ISSUES | EVENT_ISSUE_COMMENT) {
        return Err(anyhow!(
            "dispatch config {} trigger '{}' has unsupported event '{}'",
            config_path.display(),
            trigger_id,
            trigger.event
        ));
    }

    if trigger.actions.is_empty() {
        return Err(anyhow!(
            "dispatch config {} trigger '{}' has empty actions",
            config_path.display(),
            trigger_id
        ));
    }
    let actions = trigger
        .actions
        .into_iter()
        .map(|action| action.trim().to_ascii_lowercase())
        .map(|action| {
            if action.is_empty() {
                Err(anyhow!(
                    "dispatch config {} trigger '{}' includes blank action",
                    config_path.display(),
                    trigger_id
                ))
            } else {
                Ok(action)
            }
        })
        .collect::<Result<Vec<_>>>()?;

    let guards = trigger.guards;
    if guards.actor == DispatchTriggerActorGuard::RequireNotAssignee
        && guards.assignee == DispatchTriggerAssigneeGuard::Any
    {
        return Err(anyhow!(
            "dispatch config {} trigger '{}' sets actor=require_not_assignee without assignee=require_single_codex",
            config_path.display(),
            trigger_id
        ));
    }

    let action =
        compile_trigger_action(config_path, &trigger_id, trigger.action, directives, roles)?;

    Ok(DispatchTriggerConfig {
        id: trigger_id,
        class: DispatchTriggerClass::Registered,
        priority: trigger.priority,
        matcher: DispatchTriggerMatcher {
            event_type,
            actions,
        },
        guards,
        action,
        apply_guardrails: trigger.apply_guardrails,
    })
}

fn compile_trigger_action(
    config_path: &Path,
    trigger_id: &str,
    action: DispatchTriggerActionConfigFile,
    directives: &HashMap<String, DispatchDirectiveConfig>,
    roles: &HashMap<String, DispatchRoleConfig>,
) -> Result<DispatchTriggerAction> {
    let directive = match (action.directive, action.directive_from) {
        (Some(directive), None) => {
            let directive = directive.trim().to_ascii_lowercase();
            if directive.is_empty() {
                return Err(anyhow!(
                    "dispatch config {} trigger '{}' has empty action.directive",
                    config_path.display(),
                    trigger_id
                ));
            }
            if !directive_is_known(&directive) {
                return Err(anyhow!(
                    "dispatch config {} trigger '{}' references unknown directive '{}'",
                    config_path.display(),
                    trigger_id,
                    directive
                ));
            }
            if !directives.contains_key(&directive) {
                return Err(anyhow!(
                    "dispatch config {} trigger '{}' references unconfigured directive '{}'",
                    config_path.display(),
                    trigger_id,
                    directive
                ));
            }
            DispatchTriggerDirectiveSource::Literal(directive)
        }
        (None, Some(source)) => {
            if source.trim().eq_ignore_ascii_case("parsed") {
                DispatchTriggerDirectiveSource::ParsedDirective
            } else {
                return Err(anyhow!(
                    "dispatch config {} trigger '{}' has unsupported action.directive_from '{}'; expected 'parsed'",
                    config_path.display(),
                    trigger_id,
                    source
                ));
            }
        }
        (Some(_), Some(_)) => {
            return Err(anyhow!(
                "dispatch config {} trigger '{}' action must set exactly one of directive or directive_from",
                config_path.display(),
                trigger_id
            ));
        }
        (None, None) => {
            return Err(anyhow!(
                "dispatch config {} trigger '{}' action is missing directive source",
                config_path.display(),
                trigger_id
            ));
        }
    };

    let target_role = match (action.target_role, action.target_role_from) {
        (Some(role), None) => {
            let role = role.trim().to_ascii_lowercase();
            if role.is_empty() {
                return Err(anyhow!(
                    "dispatch config {} trigger '{}' has empty action.target_role",
                    config_path.display(),
                    trigger_id
                ));
            }
            if !roles.contains_key(&role) {
                return Err(anyhow!(
                    "dispatch config {} trigger '{}' references unknown role '{}'",
                    config_path.display(),
                    trigger_id,
                    role
                ));
            }
            DispatchTriggerRoleSource::Literal(role)
        }
        (None, Some(source)) if source.trim().eq_ignore_ascii_case("parsed") => {
            DispatchTriggerRoleSource::ParsedDirectiveRole
        }
        (None, Some(source)) if source.trim().eq_ignore_ascii_case("assignee") => {
            DispatchTriggerRoleSource::SingleAssignee
        }
        (None, Some(source)) => {
            return Err(anyhow!(
                "dispatch config {} trigger '{}' has unsupported action.target_role_from '{}'; expected 'parsed' or 'assignee'",
                config_path.display(),
                trigger_id,
                source
            ));
        }
        (Some(_), Some(_)) => {
            return Err(anyhow!(
                "dispatch config {} trigger '{}' action must set exactly one of target_role or target_role_from",
                config_path.display(),
                trigger_id
            ));
        }
        (None, None) => {
            return Err(anyhow!(
                "dispatch config {} trigger '{}' action is missing target role source",
                config_path.display(),
                trigger_id
            ));
        }
    };

    let reason_code = action
        .reason_code
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
        .unwrap_or_else(|| format!("registered_trigger:{trigger_id}"));
    if !reason_code.starts_with("registered_trigger:") {
        return Err(anyhow!(
            "dispatch config {} trigger '{}' reason_code '{}' must start with 'registered_trigger:'",
            config_path.display(),
            trigger_id,
            reason_code
        ));
    }

    Ok(DispatchTriggerAction {
        directive,
        target_role,
        reason_code,
    })
}

fn resolve_config_path(base_dir: &Path, raw: &str) -> Result<PathBuf> {
    let expanded = expand_tilde_path(raw)?;
    if expanded.is_absolute() {
        Ok(expanded)
    } else {
        Ok(base_dir.join(expanded))
    }
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::path::Path;

    use anyhow::Result;
    use tempfile::tempdir;

    use super::{DispatchPromptEnvelopeConfig, load_dispatch_config};

    #[test]
    fn role_card_file_for_uses_roles_subdir_next_to_preamble() {
        let envelopes = DispatchPromptEnvelopeConfig {
            preamble_file: Path::new("/tmp/prompts/orchd-preamble.md").to_path_buf(),
            fresh_envelope: Path::new("/tmp/prompts/orchd-envelope-fresh.md").to_path_buf(),
            followup_envelope: Path::new("/tmp/prompts/orchd-envelope-followup.md").to_path_buf(),
            turn_context_file: Path::new("/tmp/prompts/orchd-turn-context.md").to_path_buf(),
            issue_fresh_file: Path::new("/tmp/prompts/orchd-issue-fresh.md").to_path_buf(),
            issue_followup_file: Path::new("/tmp/prompts/orchd-issue-followup.md").to_path_buf(),
        };

        let card = envelopes.role_card_file_for("codex-orch");
        assert_eq!(card, Path::new("/tmp/prompts/roles/codex-orch.md"));
    }

    #[test]
    fn load_dispatch_config_rejects_missing_role_card() -> Result<()> {
        let temp = tempdir()?;
        let root = temp.path();
        let config_path = root.join("dispatch.toml");
        let prompts_dir = root.join("prompts");
        fs::create_dir_all(&prompts_dir)?;

        let config_toml = format!(
            r#"version = 1
allowed_actors = ["main"]
forgejoctl_bin = "/home/main/.local/bin/forgejoctl"

[prompt_envelopes]
preamble_file = "{preamble}"
fresh_envelope = "{fresh}"
followup_envelope = "{followup}"

[roles.codex-orch]
token_file = "{token}"

[directives.design]
role = "codex-orch"
prompt_file = "{design_prompt}"
"#,
            preamble = prompts_dir.join("orchd-preamble.md").display(),
            fresh = prompts_dir.join("orchd-envelope-fresh.md").display(),
            followup = prompts_dir.join("orchd-envelope-followup.md").display(),
            token = root.join("token.txt").display(),
            design_prompt = prompts_dir.join("orchd-design.md").display(),
        );
        fs::write(&config_path, config_toml)?;

        let err = load_dispatch_config(&config_path).expect_err("config should fail");
        let err_text = err.to_string();
        assert!(err_text.contains("missing role card for role codex-orch"));
        Ok(())
    }

    #[test]
    fn load_dispatch_config_accepts_present_role_card() -> Result<()> {
        let temp = tempdir()?;
        let root = temp.path();
        let config_path = root.join("dispatch.toml");
        let prompts_dir = root.join("prompts");
        let roles_dir = prompts_dir.join("roles");
        fs::create_dir_all(&roles_dir)?;
        fs::write(roles_dir.join("codex-orch.md"), "# codex-orch role card\n")?;

        let config_toml = format!(
            r#"version = 1
allowed_actors = ["main"]
forgejoctl_bin = "/home/main/.local/bin/forgejoctl"

[prompt_envelopes]
preamble_file = "{preamble}"
fresh_envelope = "{fresh}"
followup_envelope = "{followup}"

[roles.codex-orch]
token_file = "{token}"

[directives.design]
role = "codex-orch"
prompt_file = "{design_prompt}"

[directives.reply]
role = "codex-orch"
prompt_file = "{reply_prompt}"
"#,
            preamble = prompts_dir.join("orchd-preamble.md").display(),
            fresh = prompts_dir.join("orchd-envelope-fresh.md").display(),
            followup = prompts_dir.join("orchd-envelope-followup.md").display(),
            token = root.join("token.txt").display(),
            design_prompt = prompts_dir.join("orchd-design.md").display(),
            reply_prompt = prompts_dir.join("orchd-poke.md").display(),
        );
        fs::write(&config_path, config_toml)?;

        let config = load_dispatch_config(&config_path)?;
        assert!(config.roles.contains_key("codex-orch"));
        assert!(!config.triggers.is_empty());
        Ok(())
    }

    #[test]
    fn load_dispatch_config_rejects_trigger_with_unknown_directive() -> Result<()> {
        let temp = tempdir()?;
        let root = temp.path();
        let config_path = root.join("dispatch.toml");
        let prompts_dir = root.join("prompts");
        let roles_dir = prompts_dir.join("roles");
        fs::create_dir_all(&roles_dir)?;
        fs::write(roles_dir.join("codex-orch.md"), "# role\n")?;

        let config_toml = format!(
            r#"version = 1
allowed_actors = ["main"]
legacy_triggers = false

[prompt_envelopes]
preamble_file = "{preamble}"
fresh_envelope = "{fresh}"
followup_envelope = "{followup}"

[roles.codex-orch]
token_file = "{token}"

[directives.poke]
role = "codex-orch"
prompt_file = "{poke_prompt}"

[[triggers]]
id = "bad"
event = "issues"
actions = ["closed"]

[triggers.action]
directive = "debrief"
target_role = "codex-orch"
"#,
            preamble = prompts_dir.join("orchd-preamble.md").display(),
            fresh = prompts_dir.join("orchd-envelope-fresh.md").display(),
            followup = prompts_dir.join("orchd-envelope-followup.md").display(),
            token = root.join("token.txt").display(),
            poke_prompt = prompts_dir.join("orchd-poke.md").display(),
        );
        fs::write(&config_path, config_toml)?;

        let err = load_dispatch_config(&config_path).expect_err("config should fail");
        assert!(
            err.to_string()
                .contains("trigger 'bad' references unknown directive 'debrief'")
        );
        Ok(())
    }
}
