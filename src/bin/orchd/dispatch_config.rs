use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::fmt;
use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, anyhow};
use serde::Deserialize;

use forgejo_agent::orchd_dispatch_core::DispatchNotificationPhase;
use forgejo_agent::types::RepoRef;

use super::lexicon::{
    DIRECTIVE_AUDIT, DIRECTIVE_DESIGN, DIRECTIVE_IMPL, DIRECTIVE_INVESTIGATE, DIRECTIVE_REPLY,
    EVENT_ISSUE_COMMENT, EVENT_ISSUES, directive_is_known,
};
use super::paths::expand_tilde_path;

#[derive(Clone, Debug)]
pub(super) struct DispatchConfig {
    pub(super) allowed_actors: Vec<String>,
    pub(super) prompt_envelopes: DispatchPromptEnvelopeConfig,
    pub(super) notifications: DispatchNotificationsConfig,
    pub(super) rank_acl: DispatchRankAclConfig,
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

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub(super) struct DispatchRank(u8);

impl DispatchRank {
    fn parse(raw: &str) -> Result<Self> {
        let normalized = raw
            .trim()
            .trim_matches(|ch: char| [',', ';', ':', '.'].contains(&ch))
            .to_ascii_uppercase();
        let Some(digits) = normalized.strip_prefix("OF-") else {
            return Err(anyhow!(
                "expected officer rank in format OF-<n>, got '{raw}'"
            ));
        };
        let value: u8 = digits
            .parse()
            .with_context(|| format!("invalid officer rank digits in '{raw}'"))?;
        Ok(Self(value))
    }
}

impl fmt::Display for DispatchRank {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "OF-{}", self.0)
    }
}

#[derive(Clone, Debug)]
pub(super) struct DispatchRankAclRolePolicy {
    pub(super) rank: DispatchRank,
    pub(super) own_directives: BTreeSet<String>,
    pub(super) delegation_directives: BTreeSet<String>,
}

#[derive(Clone, Debug)]
struct DispatchRankAclRankPolicy {
    own_directives: BTreeSet<String>,
    delegation_directives: BTreeSet<String>,
}

#[derive(Clone, Debug)]
pub(super) struct DispatchRankAclConfig {
    pub(super) enabled: bool,
    rank_policies: BTreeMap<DispatchRank, DispatchRankAclRankPolicy>,
    role_policies: BTreeMap<String, DispatchRankAclRolePolicy>,
}

impl DispatchRankAclConfig {
    pub(super) fn has_role_policy(&self, role_name: &str) -> bool {
        let role_name = role_name.trim().to_ascii_lowercase();
        self.role_policies.contains_key(role_name.as_str())
    }

    pub(super) fn assert_target_can_execute(
        &self,
        target_role: &str,
        directive: &str,
    ) -> Result<()> {
        if !self.enabled {
            return Ok(());
        }

        let target_role = target_role.trim().to_ascii_lowercase();
        let directive = directive.trim().to_ascii_lowercase();
        let target_policy = self
            .role_policies
            .get(target_role.as_str())
            .ok_or_else(|| anyhow!("target role '{target_role}' has no rank ACL policy"))?;
        if !target_policy.own_directives.contains(directive.as_str()) {
            return Err(anyhow!(
                "target role '{target_role}' rank {} is not permitted to execute directive '{directive}'",
                target_policy.rank
            ));
        }
        Ok(())
    }

    pub(super) fn assert_actor_can_dispatch(
        &self,
        actor_login: &str,
        target_role: &str,
        directive: &str,
    ) -> Result<()> {
        if !self.enabled {
            return Ok(());
        }

        let actor_login = actor_login.trim().to_ascii_lowercase();
        let target_role = target_role.trim().to_ascii_lowercase();
        let directive = directive.trim().to_ascii_lowercase();

        let actor_policy = self
            .role_policies
            .get(actor_login.as_str())
            .ok_or_else(|| anyhow!("actor '{actor_login}' has no rank ACL policy"))?;
        let target_policy = self
            .role_policies
            .get(target_role.as_str())
            .ok_or_else(|| anyhow!("target role '{target_role}' has no rank ACL policy"))?;

        let strict_downrank = actor_policy.rank > target_policy.rank;
        let is_reply = directive == DIRECTIVE_REPLY;
        let uprank_reply_exception = is_reply && actor_policy.rank < target_policy.rank;
        let self_reply_exception = is_reply && actor_login == target_role;
        if !strict_downrank && !uprank_reply_exception && !self_reply_exception {
            return Err(anyhow!(
                "actor '{actor_login}' rank {} cannot delegate directive '{directive}' to role '{target_role}' rank {} (requires strict downrank; reply exceptions: uprank and self-edge)",
                actor_policy.rank,
                target_policy.rank
            ));
        }
        if !actor_policy
            .delegation_directives
            .contains(directive.as_str())
        {
            return Err(anyhow!(
                "actor '{actor_login}' rank {} is not permitted to delegate directive '{directive}'",
                actor_policy.rank
            ));
        }
        if !target_policy.own_directives.contains(directive.as_str()) {
            return Err(anyhow!(
                "target role '{target_role}' rank {} is not permitted to execute directive '{directive}'",
                target_policy.rank
            ));
        }
        Ok(())
    }

    pub(super) fn acl_summary_markdown(&self, role_name: &str) -> String {
        if !self.enabled {
            return "## Rank ACL Envelope\n- disabled".to_string();
        }

        let mut lines = vec![
            "## Rank ACL Envelope (Delegation + Execution Scope v2)".to_string(),
            "- Delegation edge policy: strict downrank only.".to_string(),
            "- Exception: `reply` may travel uprank (subordinate -> superior).".to_string(),
            "- `delegation_directives`: what this role may dispatch onto another role".to_string(),
            "- `own_directives`: what directives this role may execute when targeted".to_string(),
        ];
        for (
            rank,
            DispatchRankAclRankPolicy {
                own_directives,
                delegation_directives,
            },
        ) in &self.rank_policies
        {
            let own_list = own_directives
                .iter()
                .cloned()
                .collect::<Vec<_>>()
                .join(", ");
            let delegation_list = delegation_directives
                .iter()
                .cloned()
                .collect::<Vec<_>>()
                .join(", ");
            lines.push(format!(
                "- rank {rank}: own={own_list}; delegation={delegation_list}"
            ));
        }

        let role_name = role_name.trim().to_ascii_lowercase();
        if let Some(policy) = self.role_policies.get(role_name.as_str()) {
            let own = policy
                .own_directives
                .iter()
                .cloned()
                .collect::<Vec<_>>()
                .join(", ");
            let delegation = policy
                .delegation_directives
                .iter()
                .cloned()
                .collect::<Vec<_>>()
                .join(", ");
            lines.push(format!(
                "- effective for {role_name} ({rank}): own={own}; delegation={delegation}",
                rank = policy.rank
            ));
        } else {
            lines.push(format!("- effective for {role_name}: unavailable"));
        }

        lines.join("\n")
    }
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
    #[serde(default)]
    rank_acl: DispatchRankAclConfigFile,
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

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct DispatchRankAclConfigFile {
    #[serde(default = "default_rank_acl_enabled")]
    enabled: bool,
    #[serde(default)]
    ranks: HashMap<String, DispatchRankAclRankConfigFile>,
    #[serde(default)]
    role_overrides: HashMap<String, DispatchRankAclRoleOverrideConfigFile>,
}

impl Default for DispatchRankAclConfigFile {
    fn default() -> Self {
        Self {
            enabled: default_rank_acl_enabled(),
            ranks: HashMap::new(),
            role_overrides: HashMap::new(),
        }
    }
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct DispatchRankAclRankConfigFile {
    own_directives: Option<Vec<String>>,
    delegation_directives: Option<Vec<String>>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct DispatchRankAclRoleOverrideConfigFile {
    own_directives: Option<Vec<String>>,
    delegation_directives: Option<Vec<String>>,
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

const fn default_rank_acl_enabled() -> bool {
    true
}

fn acl_policy_with_shared_directives(directives: BTreeSet<String>) -> DispatchRankAclRankPolicy {
    DispatchRankAclRankPolicy {
        own_directives: directives.clone(),
        delegation_directives: directives,
    }
}

fn default_rank_directives() -> BTreeMap<DispatchRank, DispatchRankAclRankPolicy> {
    let mut policies = BTreeMap::new();
    let all_directives = [
        DIRECTIVE_DESIGN,
        DIRECTIVE_INVESTIGATE,
        DIRECTIVE_IMPL,
        DIRECTIVE_REPLY,
        DIRECTIVE_AUDIT,
    ];
    let all_directives = all_directives
        .into_iter()
        .map(ToOwned::to_owned)
        .collect::<BTreeSet<_>>();
    policies.insert(
        DispatchRank(10),
        acl_policy_with_shared_directives(all_directives.clone()),
    );
    policies.insert(
        DispatchRank(8),
        acl_policy_with_shared_directives(all_directives.clone()),
    );
    policies.insert(
        DispatchRank(6),
        acl_policy_with_shared_directives(all_directives),
    );
    policies.insert(
        DispatchRank(2),
        acl_policy_with_shared_directives(
            [DIRECTIVE_IMPL, DIRECTIVE_REPLY]
                .into_iter()
                .map(ToOwned::to_owned)
                .collect(),
        ),
    );
    policies
}

fn parse_acl_directive_set(
    config_path: &Path,
    source: &str,
    directives: Vec<String>,
) -> Result<BTreeSet<String>> {
    directives
        .into_iter()
        .map(|directive| {
            let directive = directive.trim().to_ascii_lowercase();
            if directive.is_empty() {
                return Err(anyhow!(
                    "dispatch config {} {} includes empty directive",
                    config_path.display(),
                    source
                ));
            }
            if !directive_is_known(directive.as_str()) {
                return Err(anyhow!(
                    "dispatch config {} {} references unknown directive '{}'",
                    config_path.display(),
                    source,
                    directive
                ));
            }
            Ok(directive)
        })
        .collect()
}

fn compile_acl_directive_surfaces(
    config_path: &Path,
    source_prefix: &str,
    own_directives: Option<Vec<String>>,
    delegation_directives: Option<Vec<String>>,
) -> Result<DispatchRankAclRankPolicy> {
    let own_directives = own_directives.ok_or_else(|| {
        anyhow!(
            "dispatch config {} {} is missing required key 'own_directives'",
            config_path.display(),
            source_prefix
        )
    })?;
    let delegation_directives = delegation_directives.ok_or_else(|| {
        anyhow!(
            "dispatch config {} {} is missing required key 'delegation_directives'",
            config_path.display(),
            source_prefix
        )
    })?;
    let own = parse_acl_directive_set(
        config_path,
        &format!("{source_prefix}.own_directives"),
        own_directives,
    )?;
    let delegation = parse_acl_directive_set(
        config_path,
        &format!("{source_prefix}.delegation_directives"),
        delegation_directives,
    )?;
    Ok(DispatchRankAclRankPolicy {
        own_directives: own,
        delegation_directives: delegation,
    })
}

fn parse_role_card_rank(role_card_md: &str) -> Option<DispatchRank> {
    role_card_md.lines().find_map(|line| {
        let line = line.trim();
        let line = line
            .strip_prefix('-')
            .or_else(|| line.strip_prefix('*'))
            .map(str::trim_start)?;
        let token = line.split_whitespace().next()?;
        DispatchRank::parse(token).ok()
    })
}

fn read_role_card_rank(
    config_path: &Path,
    prompt_envelopes: &DispatchPromptEnvelopeConfig,
    role_name: &str,
) -> Result<DispatchRank> {
    let role_card_file = prompt_envelopes.role_card_file_for(role_name);
    if !role_card_file.is_file() {
        return Err(anyhow!(
            "dispatch config {} missing role card for role {} at {}",
            config_path.display(),
            role_name,
            role_card_file.display()
        ));
    }
    let role_card_md = fs::read_to_string(&role_card_file).with_context(|| {
        format!(
            "failed reading role card {} for role {}",
            role_card_file.display(),
            role_name
        )
    })?;
    parse_role_card_rank(&role_card_md).ok_or_else(|| {
        anyhow!(
            "dispatch config {} role card for role {} is missing rank bullet '- OF-<n>' at {}",
            config_path.display(),
            role_name,
            role_card_file.display()
        )
    })
}

fn compile_rank_acl_config(
    config_path: &Path,
    raw: DispatchRankAclConfigFile,
    prompt_envelopes: &DispatchPromptEnvelopeConfig,
    roles: &HashMap<String, DispatchRoleConfig>,
    allowed_actors: &[String],
) -> Result<DispatchRankAclConfig> {
    if !raw.enabled {
        return Ok(DispatchRankAclConfig {
            enabled: false,
            rank_policies: BTreeMap::new(),
            role_policies: BTreeMap::new(),
        });
    }

    let mut rank_policies = default_rank_directives();
    for (raw_rank, rank_policy) in raw.ranks {
        let rank = DispatchRank::parse(raw_rank.as_str()).with_context(|| {
            format!(
                "dispatch config {} rank_acl has invalid rank key '{}'",
                config_path.display(),
                raw_rank
            )
        })?;
        let source = format!("rank_acl.ranks.{rank}");
        let policy = compile_acl_directive_surfaces(
            config_path,
            &source,
            rank_policy.own_directives,
            rank_policy.delegation_directives,
        )?;
        rank_policies.insert(rank, policy);
    }

    let mut role_overrides: BTreeMap<String, DispatchRankAclRankPolicy> = BTreeMap::new();
    for (raw_role, role_policy) in raw.role_overrides {
        let role_name = raw_role.trim().to_ascii_lowercase();
        if role_name.is_empty() {
            return Err(anyhow!(
                "dispatch config {} rank_acl has empty role override key",
                config_path.display()
            ));
        }
        if role_overrides.contains_key(role_name.as_str()) {
            return Err(anyhow!(
                "dispatch config {} rank_acl has duplicate role override '{}'",
                config_path.display(),
                role_name
            ));
        }
        let source = format!("rank_acl.role_overrides.{role_name}");
        let policy = compile_acl_directive_surfaces(
            config_path,
            &source,
            role_policy.own_directives,
            role_policy.delegation_directives,
        )?;
        role_overrides.insert(role_name, policy);
    }

    let mut policy_roles: BTreeSet<String> = roles.keys().cloned().collect();
    policy_roles.extend(
        allowed_actors
            .iter()
            .map(|actor| actor.trim().to_ascii_lowercase())
            .filter(|actor| !actor.is_empty()),
    );
    policy_roles.extend(role_overrides.keys().cloned());

    let mut role_policies = BTreeMap::new();
    for role_name in policy_roles {
        let rank = read_role_card_rank(config_path, prompt_envelopes, role_name.as_str())?;
        let rank_policy_for_role = rank_policies.get(&rank).ok_or_else(|| {
            anyhow!(
                "dispatch config {} rank_acl is missing directives for rank {} (role '{}')",
                config_path.display(),
                rank,
                role_name
            )
        })?;

        let (own_directives, delegation_directives) = if let Some(override_policy) =
            role_overrides.get(role_name.as_str())
        {
            let widened_own = override_policy
                .own_directives
                .difference(&rank_policy_for_role.own_directives)
                .cloned()
                .collect::<Vec<_>>();
            if !widened_own.is_empty() {
                return Err(anyhow!(
                    "dispatch config {} rank_acl.role_overrides.{} widens rank {} own_directives envelope with directives: {}",
                    config_path.display(),
                    role_name,
                    rank,
                    widened_own.join(", ")
                ));
            }
            let widened_delegation = override_policy
                .delegation_directives
                .difference(&rank_policy_for_role.delegation_directives)
                .cloned()
                .collect::<Vec<_>>();
            if !widened_delegation.is_empty() {
                return Err(anyhow!(
                    "dispatch config {} rank_acl.role_overrides.{} widens rank {} delegation_directives envelope with directives: {}",
                    config_path.display(),
                    role_name,
                    rank,
                    widened_delegation.join(", ")
                ));
            }
            (
                override_policy.own_directives.clone(),
                override_policy.delegation_directives.clone(),
            )
        } else {
            (
                rank_policy_for_role.own_directives.clone(),
                rank_policy_for_role.delegation_directives.clone(),
            )
        };

        role_policies.insert(
            role_name,
            DispatchRankAclRolePolicy {
                rank,
                own_directives,
                delegation_directives,
            },
        );
    }

    for role_name in roles.keys() {
        if !role_policies.contains_key(role_name.as_str()) {
            return Err(anyhow!(
                "dispatch config {} rank_acl is missing policy for configured role '{}'",
                config_path.display(),
                role_name
            ));
        }
    }

    Ok(DispatchRankAclConfig {
        enabled: true,
        rank_policies,
        role_policies,
    })
}

fn validate_role_mappings(
    config_path: &Path,
    roles: &HashMap<String, DispatchRoleConfig>,
    control_plane: Option<&DispatchControlPlaneConfig>,
) -> Result<()> {
    let owner_token_path = expand_tilde_path("~/.config/forgejo-agent/token").ok();

    let mut token_owners: HashMap<PathBuf, String> = HashMap::new();
    let mut login_owners: HashMap<String, String> = HashMap::new();

    for (role_name, role) in roles {
        if let Some(existing) = token_owners.insert(role.token_file.clone(), role_name.clone()) {
            return Err(anyhow!(
                "dispatch config {} roles '{}' and '{}' share token_file {}",
                config_path.display(),
                existing,
                role_name,
                role.token_file.display()
            ));
        }

        let forgejo_login = role.forgejo_login.trim().to_ascii_lowercase();
        if forgejo_login.is_empty() {
            return Err(anyhow!(
                "dispatch config {} role '{}' has empty forgejo_login",
                config_path.display(),
                role_name
            ));
        }
        if forgejo_login == "main" {
            return Err(anyhow!(
                "dispatch config {} role '{}' uses forbidden forgejo_login 'main'; use a dedicated role principal",
                config_path.display(),
                role_name
            ));
        }
        if let Some(existing) = login_owners.insert(forgejo_login.clone(), role_name.clone()) {
            return Err(anyhow!(
                "dispatch config {} roles '{}' and '{}' share forgejo_login '{}'",
                config_path.display(),
                existing,
                role_name,
                forgejo_login
            ));
        }

        if let Some(owner_token_path) = owner_token_path.as_ref()
            && role.token_file == *owner_token_path
        {
            return Err(anyhow!(
                "dispatch config {} role '{}' token_file {} points to owner fallback token; use dedicated role token under ~/.config/forgejo-agent/creds/",
                config_path.display(),
                role_name,
                role.token_file.display()
            ));
        }
    }

    if let Some(control_plane) = control_plane {
        if let Some(owner_token_path) = owner_token_path.as_ref()
            && control_plane.token_file == *owner_token_path
        {
            return Err(anyhow!(
                "dispatch config {} control_plane token_file {} points to owner fallback token; use dedicated machine token under ~/.config/forgejo-agent/creds/",
                config_path.display(),
                control_plane.token_file.display()
            ));
        }

        if let Some(role_name) = roles.iter().find_map(|(role_name, role)| {
            if role.token_file == control_plane.token_file {
                Some(role_name)
            } else {
                None
            }
        }) {
            return Err(anyhow!(
                "dispatch config {} control_plane token_file {} is shared with role '{}'",
                config_path.display(),
                control_plane.token_file.display(),
                role_name
            ));
        }
    }

    Ok(())
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

    let allowed_actors = raw
        .allowed_actors
        .into_iter()
        .map(|actor| actor.trim().to_ascii_lowercase())
        .filter(|actor| !actor.is_empty())
        .collect::<Vec<_>>();
    if allowed_actors.is_empty() {
        return Err(anyhow!(
            "dispatch config {} has empty allowed_actors",
            path.display()
        ));
    }

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

    validate_role_mappings(path, &roles, control_plane.as_ref())?;

    let mut role_names: Vec<_> = roles.keys().cloned().collect();
    role_names.sort();
    for role_name in role_names {
        let _ = read_role_card_rank(path, &prompt_envelopes, role_name.as_str())?;
    }

    let rank_acl = compile_rank_acl_config(
        path,
        raw.rank_acl,
        &prompt_envelopes,
        &roles,
        &allowed_actors,
    )?;

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
        allowed_actors,
        prompt_envelopes,
        notifications: DispatchNotificationsConfig {
            enabled: raw.notifications.enabled,
            poll_sec: raw.notifications.poll_sec.max(1),
            phases: notification_phases,
            app_name: raw.notifications.app_name,
            watch_login: raw.notifications.watch_login.to_ascii_lowercase(),
            notify_send_bin: resolve_config_path(&base_dir, &raw.notifications.notify_send_bin)?,
        },
        rank_acl,
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
            design_prompt = prompts_dir.join("orders").join("orchd-design.md").display(),
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
        fs::write(
            roles_dir.join("codex-orch.md"),
            "# codex-orch role card\n\n- OF-8\n",
        )?;
        fs::write(roles_dir.join("main.md"), "# main role card\n\n- OF-10\n")?;

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
            design_prompt = prompts_dir.join("orders").join("orchd-design.md").display(),
            reply_prompt = prompts_dir.join("orders").join("orchd-reply.md").display(),
        );
        fs::write(&config_path, config_toml)?;

        let config = load_dispatch_config(&config_path)?;
        assert!(config.roles.contains_key("codex-orch"));
        assert!(
            config
                .rank_acl
                .assert_actor_can_dispatch("main", "codex-orch", "design")
                .is_ok()
        );
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
        fs::write(roles_dir.join("codex-orch.md"), "# role\n\n- OF-8\n")?;
        fs::write(roles_dir.join("main.md"), "# main role\n\n- OF-10\n")?;

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

[directives.reply]
role = "codex-orch"
prompt_file = "{reply_prompt}"

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
            reply_prompt = prompts_dir.join("orders").join("orchd-reply.md").display(),
        );
        fs::write(&config_path, config_toml)?;

        let err = load_dispatch_config(&config_path).expect_err("config should fail");
        assert!(
            err.to_string()
                .contains("trigger 'bad' references unknown directive 'debrief'")
        );
        Ok(())
    }

    #[test]
    fn load_dispatch_config_rejects_roles_sharing_token_file() -> Result<()> {
        let temp = tempdir()?;
        let root = temp.path();
        let config_path = root.join("dispatch.toml");
        let prompts_dir = root.join("prompts");
        let roles_dir = prompts_dir.join("roles");
        fs::create_dir_all(&roles_dir)?;
        fs::write(
            roles_dir.join("codex-a.md"),
            "# codex-a role card\n\n- OF-8\n",
        )?;
        fs::write(
            roles_dir.join("codex-b.md"),
            "# codex-b role card\n\n- OF-8\n",
        )?;
        fs::write(roles_dir.join("main.md"), "# main role card\n\n- OF-10\n")?;

        let shared_token = root.join("shared.token");
        let config_toml = format!(
            r#"version = 1
allowed_actors = ["main"]
legacy_triggers = false

[prompt_envelopes]
preamble_file = "{preamble}"
fresh_envelope = "{fresh}"
followup_envelope = "{followup}"

[roles.codex-a]
token_file = "{shared_token}"

[roles.codex-b]
token_file = "{shared_token}"

[directives.reply]
role = "codex-a"
prompt_file = "{reply_prompt}"
"#,
            preamble = prompts_dir.join("orchd-preamble.md").display(),
            fresh = prompts_dir.join("orchd-envelope-fresh.md").display(),
            followup = prompts_dir.join("orchd-envelope-followup.md").display(),
            shared_token = shared_token.display(),
            reply_prompt = prompts_dir.join("orders").join("orchd-reply.md").display(),
        );
        fs::write(&config_path, config_toml)?;

        let err = load_dispatch_config(&config_path).expect_err("config should fail");
        assert!(err.to_string().contains("share token_file"));
        Ok(())
    }

    #[test]
    fn load_dispatch_config_rejects_role_using_owner_fallback_token_path() -> Result<()> {
        let home = std::env::var("HOME").unwrap_or_else(|_| "/tmp".to_string());
        let owner_token = Path::new(&home).join(".config/forgejo-agent/token");

        let temp = tempdir()?;
        let root = temp.path();
        let config_path = root.join("dispatch.toml");
        let prompts_dir = root.join("prompts");
        let roles_dir = prompts_dir.join("roles");
        fs::create_dir_all(&roles_dir)?;
        fs::write(
            roles_dir.join("codex-orch.md"),
            "# codex-orch role card\n\n- OF-8\n",
        )?;
        fs::write(roles_dir.join("main.md"), "# main role card\n\n- OF-10\n")?;

        let config_toml = format!(
            r#"version = 1
allowed_actors = ["main"]
legacy_triggers = false

[prompt_envelopes]
preamble_file = "{preamble}"
fresh_envelope = "{fresh}"
followup_envelope = "{followup}"

[roles.codex-orch]
token_file = "{owner_token}"

[directives.reply]
role = "codex-orch"
prompt_file = "{reply_prompt}"
"#,
            preamble = prompts_dir.join("orchd-preamble.md").display(),
            fresh = prompts_dir.join("orchd-envelope-fresh.md").display(),
            followup = prompts_dir.join("orchd-envelope-followup.md").display(),
            owner_token = owner_token.display(),
            reply_prompt = prompts_dir.join("orders").join("orchd-reply.md").display(),
        );
        fs::write(&config_path, config_toml)?;

        let err = load_dispatch_config(&config_path).expect_err("config should fail");
        assert!(err.to_string().contains("owner fallback token"));
        Ok(())
    }

    #[test]
    fn load_dispatch_config_rejects_roles_sharing_forgejo_login() -> Result<()> {
        let temp = tempdir()?;
        let root = temp.path();
        let config_path = root.join("dispatch.toml");
        let prompts_dir = root.join("prompts");
        let roles_dir = prompts_dir.join("roles");
        fs::create_dir_all(&roles_dir)?;
        fs::write(
            roles_dir.join("codex-alpha.md"),
            "# codex-alpha role card\n\n- OF-8\n",
        )?;
        fs::write(
            roles_dir.join("codex-beta.md"),
            "# codex-beta role card\n\n- OF-8\n",
        )?;
        fs::write(roles_dir.join("main.md"), "# main role card\n\n- OF-10\n")?;

        let config_toml = format!(
            r#"version = 1
allowed_actors = ["main"]
legacy_triggers = false

[prompt_envelopes]
preamble_file = "{preamble}"
fresh_envelope = "{fresh}"
followup_envelope = "{followup}"

[roles.codex-alpha]
forgejo_login = "shared-login"
token_file = "{token_alpha}"

[roles.codex-beta]
forgejo_login = "shared-login"
token_file = "{token_beta}"

[directives.reply]
role = "codex-alpha"
prompt_file = "{reply_prompt}"
"#,
            preamble = prompts_dir.join("orchd-preamble.md").display(),
            fresh = prompts_dir.join("orchd-envelope-fresh.md").display(),
            followup = prompts_dir.join("orchd-envelope-followup.md").display(),
            token_alpha = root.join("alpha.token").display(),
            token_beta = root.join("beta.token").display(),
            reply_prompt = prompts_dir.join("orders").join("orchd-reply.md").display(),
        );
        fs::write(&config_path, config_toml)?;

        let err = load_dispatch_config(&config_path).expect_err("config should fail");
        assert!(err.to_string().contains("share forgejo_login"));
        Ok(())
    }

    #[test]
    fn load_dispatch_config_rejects_role_using_main_login() -> Result<()> {
        let temp = tempdir()?;
        let root = temp.path();
        let config_path = root.join("dispatch.toml");
        let prompts_dir = root.join("prompts");
        let roles_dir = prompts_dir.join("roles");
        fs::create_dir_all(&roles_dir)?;
        fs::write(
            roles_dir.join("codex-orch.md"),
            "# codex-orch role card\n\n- OF-8\n",
        )?;
        fs::write(roles_dir.join("main.md"), "# main role card\n\n- OF-10\n")?;

        let config_toml = format!(
            r#"version = 1
allowed_actors = ["main"]
legacy_triggers = false

[prompt_envelopes]
preamble_file = "{preamble}"
fresh_envelope = "{fresh}"
followup_envelope = "{followup}"

[roles.codex-orch]
forgejo_login = "main"
token_file = "{token}"

[directives.reply]
role = "codex-orch"
prompt_file = "{reply_prompt}"
"#,
            preamble = prompts_dir.join("orchd-preamble.md").display(),
            fresh = prompts_dir.join("orchd-envelope-fresh.md").display(),
            followup = prompts_dir.join("orchd-envelope-followup.md").display(),
            token = root.join("orch.token").display(),
            reply_prompt = prompts_dir.join("orders").join("orchd-reply.md").display(),
        );
        fs::write(&config_path, config_toml)?;

        let err = load_dispatch_config(&config_path).expect_err("config should fail");
        assert!(err.to_string().contains("forbidden forgejo_login 'main'"));
        Ok(())
    }

    #[test]
    fn load_dispatch_config_rejects_control_plane_owner_fallback_token_path() -> Result<()> {
        let home = std::env::var("HOME").unwrap_or_else(|_| "/tmp".to_string());
        let owner_token = Path::new(&home).join(".config/forgejo-agent/token");

        let temp = tempdir()?;
        let root = temp.path();
        let config_path = root.join("dispatch.toml");
        let prompts_dir = root.join("prompts");
        let roles_dir = prompts_dir.join("roles");
        fs::create_dir_all(&roles_dir)?;
        fs::write(
            roles_dir.join("codex-orch.md"),
            "# codex-orch role card\n\n- OF-8\n",
        )?;
        fs::write(roles_dir.join("main.md"), "# main role card\n\n- OF-10\n")?;

        let config_toml = format!(
            r#"version = 1
allowed_actors = ["main"]
legacy_triggers = false

[prompt_envelopes]
preamble_file = "{preamble}"
fresh_envelope = "{fresh}"
followup_envelope = "{followup}"

[roles.codex-orch]
token_file = "{role_token}"

[control_plane]
token_file = "{owner_token}"

[directives.reply]
role = "codex-orch"
prompt_file = "{reply_prompt}"
"#,
            preamble = prompts_dir.join("orchd-preamble.md").display(),
            fresh = prompts_dir.join("orchd-envelope-fresh.md").display(),
            followup = prompts_dir.join("orchd-envelope-followup.md").display(),
            role_token = root.join("orch.token").display(),
            owner_token = owner_token.display(),
            reply_prompt = prompts_dir.join("orders").join("orchd-reply.md").display(),
        );
        fs::write(&config_path, config_toml)?;

        let err = load_dispatch_config(&config_path).expect_err("config should fail");
        assert!(err.to_string().contains("control_plane token_file"));
        assert!(err.to_string().contains("owner fallback token"));
        Ok(())
    }

    #[test]
    fn load_dispatch_config_rejects_rank_override_widening_rank_envelope() -> Result<()> {
        let temp = tempdir()?;
        let root = temp.path();
        let config_path = root.join("dispatch.toml");
        let prompts_dir = root.join("prompts");
        let roles_dir = prompts_dir.join("roles");
        fs::create_dir_all(&roles_dir)?;
        fs::write(
            roles_dir.join("codex-orch.md"),
            "# codex-orch role card\n\n- OF-8\n",
        )?;
        fs::write(roles_dir.join("main.md"), "# main role card\n\n- OF-10\n")?;

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

	[rank_acl.ranks."OF-8"]
	own_directives = ["reply"]
	delegation_directives = ["reply"]
	
	[rank_acl.role_overrides.codex-orch]
	own_directives = ["reply", "impl"]
	delegation_directives = ["reply", "impl"]
	
	[directives.reply]
	role = "codex-orch"
	prompt_file = "{reply_prompt}"
"#,
            preamble = prompts_dir.join("orchd-preamble.md").display(),
            fresh = prompts_dir.join("orchd-envelope-fresh.md").display(),
            followup = prompts_dir.join("orchd-envelope-followup.md").display(),
            token = root.join("token.txt").display(),
            reply_prompt = prompts_dir.join("orders").join("orchd-reply.md").display(),
        );
        fs::write(&config_path, config_toml)?;

        let err = load_dispatch_config(&config_path).expect_err("config should fail");
        let msg = err.to_string();
        assert!(
            msg.contains(
                "rank_acl.role_overrides.codex-orch widens rank OF-8 own_directives envelope"
            ) || msg.contains(
                "rank_acl.role_overrides.codex-orch widens rank OF-8 delegation_directives envelope"
            )
        );
        Ok(())
    }

    #[test]
    fn rank_acl_enforces_strict_downrank_with_reply_exception() -> Result<()> {
        let temp = tempdir()?;
        let root = temp.path();
        let config_path = root.join("dispatch.toml");
        let prompts_dir = root.join("prompts");
        let roles_dir = prompts_dir.join("roles");
        let orders_dir = prompts_dir.join("orders");
        fs::create_dir_all(&roles_dir)?;
        fs::create_dir_all(&orders_dir)?;
        fs::write(roles_dir.join("main.md"), "# main role card\n\n- OF-10\n")?;
        fs::write(
            roles_dir.join("codex-orch.md"),
            "# orch role card\n\n- OF-8\n",
        )?;
        fs::write(
            roles_dir.join("codex-lead.md"),
            "# lead role card\n\n- OF-6\n",
        )?;
        fs::write(
            roles_dir.join("codex-dev.md"),
            "# dev role card\n\n- OF-2\n",
        )?;
        fs::write(
            roles_dir.join("codex-auditor.md"),
            "# auditor role card\n\n- OF-6\n",
        )?;
        fs::write(prompts_dir.join("orchd-preamble.md"), "preamble\n")?;
        fs::write(
            prompts_dir.join("orchd-envelope-fresh.md"),
            "{{preamble_md}}\n",
        )?;
        fs::write(
            prompts_dir.join("orchd-envelope-followup.md"),
            "{{dispatch_md}}\n",
        )?;
        fs::write(orders_dir.join("orchd-reply.md"), "reply\n")?;
        fs::write(root.join("orch.token"), "orch\n")?;
        fs::write(root.join("lead.token"), "lead\n")?;
        fs::write(root.join("dev.token"), "dev\n")?;
        fs::write(root.join("auditor.token"), "auditor\n")?;

        let config_toml = format!(
            r#"version = 1
allowed_actors = ["main"]
forgejoctl_bin = "/home/main/.local/bin/forgejoctl"
legacy_triggers = false

[prompt_envelopes]
preamble_file = "{preamble}"
fresh_envelope = "{fresh}"
followup_envelope = "{followup}"

[roles.codex-orch]
token_file = "{orch_token}"

[roles.codex-lead]
token_file = "{lead_token}"

[roles.codex-dev]
token_file = "{dev_token}"

[roles.codex-auditor]
token_file = "{auditor_token}"

	[directives.reply]
	role = "codex-orch"
	prompt_file = "{reply_prompt}"

[rank_acl.ranks."OF-10"]
delegation_directives = ["reply", "design"]
own_directives = ["reply"]

[rank_acl.ranks."OF-8"]
delegation_directives = ["reply", "design"]
own_directives = ["reply", "design"]

[rank_acl.ranks."OF-6"]
delegation_directives = ["reply", "design"]
own_directives = ["reply", "design"]

	[rank_acl.ranks."OF-2"]
	delegation_directives = ["reply", "impl"]
	own_directives = ["reply", "impl"]
	"#,
            preamble = prompts_dir.join("orchd-preamble.md").display(),
            fresh = prompts_dir.join("orchd-envelope-fresh.md").display(),
            followup = prompts_dir.join("orchd-envelope-followup.md").display(),
            orch_token = root.join("orch.token").display(),
            lead_token = root.join("lead.token").display(),
            dev_token = root.join("dev.token").display(),
            auditor_token = root.join("auditor.token").display(),
            reply_prompt = orders_dir.join("orchd-reply.md").display(),
        );
        fs::write(&config_path, config_toml)?;

        let config = load_dispatch_config(&config_path)?;
        let acl = &config.rank_acl;

        // reply exception: lead (OF-6) can trigger a reply from orch (OF-8).
        assert!(
            acl.assert_actor_can_dispatch("codex-lead", "codex-orch", "reply")
                .is_ok()
        );

        // strict downrank: lead (OF-6) cannot delegate non-reply directives uprank.
        assert!(
            acl.assert_actor_can_dispatch("codex-lead", "codex-orch", "design")
                .is_err()
        );

        // strict downrank: equal-rank delegation is rejected.
        assert!(
            acl.assert_actor_can_dispatch("codex-lead", "codex-auditor", "reply")
                .is_err()
        );

        // exception: dev (OF-2) can delegate reply uprank to lead (OF-6).
        assert!(
            acl.assert_actor_can_dispatch("codex-dev", "codex-lead", "reply")
                .is_ok()
        );

        // self-edge: reply to self is always allowed.
        assert!(
            acl.assert_actor_can_dispatch("codex-orch", "codex-orch", "reply")
                .is_ok()
        );

        // strict downrank: non-reply self delegation is rejected.
        assert!(
            acl.assert_actor_can_dispatch("codex-dev", "codex-dev", "impl")
                .is_err()
        );

        // downrank delegation allowed: main (OF-10) to orch (OF-8).
        assert!(
            acl.assert_actor_can_dispatch("main", "codex-orch", "reply")
                .is_ok()
        );

        Ok(())
    }
}
