use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, anyhow};
use serde::Deserialize;

use forgejo_agent::orchd_dispatch_core::DispatchNotificationPhase;
use forgejo_agent::types::RepoRef;

use super::paths::expand_tilde_path;

#[derive(Clone, Debug)]
pub(super) struct DispatchConfig {
    pub(super) allowed_actors: Vec<String>,
    pub(super) prompt_envelopes: DispatchPromptEnvelopeConfig,
    pub(super) notifications: DispatchNotificationsConfig,
    pub(super) roles: HashMap<String, DispatchRoleConfig>,
    pub(super) directives: HashMap<String, DispatchDirectiveConfig>,
    pub(super) repo_bindings: HashMap<String, DispatchRepoBindingConfig>,
    pub(super) forgejoctl_bin: PathBuf,
}

#[derive(Clone, Debug)]
pub(super) struct DispatchPromptEnvelopeConfig {
    pub(super) preamble_file: PathBuf,
    pub(super) fresh_envelope: PathBuf,
    pub(super) followup_envelope: PathBuf,
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
    directives: HashMap<String, DispatchDirectiveConfigFile>,
    #[serde(default)]
    repo_bindings: Vec<DispatchRepoBindingConfigFile>,
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
}

impl Default for DispatchPromptEnvelopeConfigFile {
    fn default() -> Self {
        Self {
            preamble_file: default_preamble_file(),
            fresh_envelope: default_fresh_envelope(),
            followup_envelope: default_followup_envelope(),
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

fn default_git_remote() -> String {
    "origin".to_string()
}

fn default_git_base() -> String {
    "main".to_string()
}

const fn default_timeout_sec() -> u64 {
    3600
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
            directive_name,
            DispatchDirectiveConfig {
                role: directive.role,
                prompt_file: resolve_config_path(&base_dir, &directive.prompt_file)?,
                timeout_sec: directive.timeout_sec.max(30),
            },
        );
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

    Ok(DispatchConfig {
        allowed_actors: raw
            .allowed_actors
            .into_iter()
            .map(|actor| actor.to_ascii_lowercase())
            .collect(),
        prompt_envelopes: DispatchPromptEnvelopeConfig {
            preamble_file: resolve_config_path(&base_dir, &raw.prompt_envelopes.preamble_file)?,
            fresh_envelope: resolve_config_path(&base_dir, &raw.prompt_envelopes.fresh_envelope)?,
            followup_envelope: resolve_config_path(
                &base_dir,
                &raw.prompt_envelopes.followup_envelope,
            )?,
        },
        notifications: DispatchNotificationsConfig {
            enabled: raw.notifications.enabled,
            poll_sec: raw.notifications.poll_sec.max(1),
            phases: notification_phases,
            app_name: raw.notifications.app_name,
            watch_login: raw.notifications.watch_login.to_ascii_lowercase(),
            notify_send_bin: resolve_config_path(&base_dir, &raw.notifications.notify_send_bin)?,
        },
        roles,
        directives,
        repo_bindings,
        forgejoctl_bin: resolve_config_path(&base_dir, &raw.forgejoctl_bin)?,
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
    use std::path::PathBuf;

    fn temp_config_path(label: &str) -> PathBuf {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("clock drift")
            .as_nanos();
        std::env::temp_dir().join(format!(
            "forgejo-agent-dispatch-config-{label}-{}-{nanos}.toml",
            std::process::id()
        ))
    }

    fn write_config(path: &PathBuf, repo_bindings_block: &str) {
        let body = format!(
            r#"
version = 1
allowed_actors = ["main"]

[prompt_envelopes]
preamble_file = "../prompts/orchd-preamble.md"
fresh_envelope = "../prompts/orchd-envelope-fresh.md"
followup_envelope = "../prompts/orchd-envelope-followup.md"

[roles.codex-orch]
codex_bin = "/tmp/fake-codex"
codex_role_arg = "orch"
token_file = "/tmp/fake-token"

[directives.impl]
role = "codex-orch"
prompt_file = "../prompts/orchd-impl.md"
timeout_sec = 60
{repo_bindings_block}
"#
        );
        fs::write(path, body).expect("write config");
    }

    #[test]
    fn load_dispatch_config_parses_repo_bindings() {
        let path = temp_config_path("repo-bindings");
        write_config(
            &path,
            r#"
[[repo_bindings]]
repo = "main/forgejo-agent"
local_path = "/home/main/forgejo-agent"
git_remote = "origin"
git_base = "main"
"#,
        );
        let cfg = super::load_dispatch_config(&path).expect("load config");
        let binding = cfg
            .repo_bindings
            .get("main/forgejo-agent")
            .expect("repo binding present");
        assert_eq!(
            binding.local_path,
            PathBuf::from("/home/main/forgejo-agent")
        );
        assert_eq!(binding.git_remote, "origin");
        assert_eq!(binding.git_base, "main");
        let _ = fs::remove_file(path);
    }

    #[test]
    fn load_dispatch_config_rejects_duplicate_repo_binding() {
        let path = temp_config_path("duplicate-repo-bindings");
        write_config(
            &path,
            r#"
[[repo_bindings]]
repo = "main/forgejo-agent"
local_path = "/home/main/forgejo-agent"

[[repo_bindings]]
repo = "main/forgejo-agent"
local_path = "/home/main/forgejo-agent"
"#,
        );
        let err = super::load_dispatch_config(&path).expect_err("duplicate should fail");
        assert!(
            err.to_string()
                .contains("duplicate repo binding for main/forgejo-agent"),
            "unexpected error: {err:#}"
        );
        let _ = fs::remove_file(path);
    }
}
