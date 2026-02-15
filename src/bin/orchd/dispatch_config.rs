use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, anyhow};
use serde::Deserialize;

use super::paths::expand_tilde_path;

#[derive(Clone, Debug)]
pub(super) struct DispatchConfig {
    pub(super) allowed_actors: Vec<String>,
    pub(super) tmux: DispatchTmuxConfig,
    pub(super) prompt_envelopes: DispatchPromptEnvelopeConfig,
    pub(super) roles: HashMap<String, DispatchRoleConfig>,
    pub(super) directives: HashMap<String, DispatchDirectiveConfig>,
    pub(super) forgejoctl_bin: PathBuf,
}

#[derive(Clone, Debug)]
pub(super) struct DispatchTmuxConfig {
    pub(super) session: String,
    pub(super) remain_on_exit: bool,
}

#[derive(Clone, Debug)]
pub(super) struct DispatchPromptEnvelopeConfig {
    pub(super) fresh_envelope: PathBuf,
    pub(super) followup_envelope: PathBuf,
    pub(super) tmux_tui_bootstrap: PathBuf,
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

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct DispatchConfigFile {
    version: u32,
    #[serde(default)]
    allowed_actors: Vec<String>,
    tmux: DispatchTmuxConfigFile,
    #[serde(default)]
    prompt_envelopes: DispatchPromptEnvelopeConfigFile,
    roles: HashMap<String, DispatchRoleConfigFile>,
    directives: HashMap<String, DispatchDirectiveConfigFile>,
    #[serde(default = "default_forgejoctl_bin")]
    forgejoctl_bin: String,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct DispatchTmuxConfigFile {
    session: String,
    #[serde(default = "default_tmux_remain_on_exit")]
    remain_on_exit: bool,
}

#[derive(Debug, Deserialize, Default)]
#[serde(deny_unknown_fields)]
struct DispatchPromptEnvelopeConfigFile {
    #[serde(default = "default_fresh_envelope")]
    fresh_envelope: String,
    #[serde(default = "default_followup_envelope")]
    followup_envelope: String,
    #[serde(default = "default_tmux_tui_bootstrap_file")]
    tmux_tui_bootstrap: String,
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

const fn default_tmux_remain_on_exit() -> bool {
    true
}

fn default_codex_bin() -> String {
    "/home/main/forgejo-agent/bin/codex-role".to_string()
}

fn default_forgejoctl_bin() -> String {
    "/home/main/.local/bin/forgejoctl".to_string()
}

fn default_fresh_envelope() -> String {
    "../prompts/orchd-envelope-fresh.md".to_string()
}

fn default_followup_envelope() -> String {
    "../prompts/orchd-envelope-followup.md".to_string()
}

fn default_tmux_tui_bootstrap_file() -> String {
    "../prompts/orchd-tui-bootstrap.md".to_string()
}

const fn default_timeout_sec() -> u64 {
    3600
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

    Ok(DispatchConfig {
        allowed_actors: raw
            .allowed_actors
            .into_iter()
            .map(|actor| actor.to_ascii_lowercase())
            .collect(),
        tmux: DispatchTmuxConfig {
            session: raw.tmux.session,
            remain_on_exit: raw.tmux.remain_on_exit,
        },
        prompt_envelopes: DispatchPromptEnvelopeConfig {
            fresh_envelope: resolve_config_path(&base_dir, &raw.prompt_envelopes.fresh_envelope)?,
            followup_envelope: resolve_config_path(
                &base_dir,
                &raw.prompt_envelopes.followup_envelope,
            )?,
            tmux_tui_bootstrap: resolve_config_path(
                &base_dir,
                &raw.prompt_envelopes.tmux_tui_bootstrap,
            )?,
        },
        roles,
        directives,
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
