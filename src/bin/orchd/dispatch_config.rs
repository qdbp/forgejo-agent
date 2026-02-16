use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, anyhow};
use serde::Deserialize;

use super::paths::expand_tilde_path;

#[derive(Clone, Debug)]
pub(super) struct DispatchConfig {
    pub(super) allowed_actors: Vec<String>,
    pub(super) prompt_envelopes: DispatchPromptEnvelopeConfig,
    pub(super) roles: HashMap<String, DispatchRoleConfig>,
    pub(super) directives: HashMap<String, DispatchDirectiveConfig>,
    pub(super) forgejoctl_bin: PathBuf,
}

#[derive(Clone, Debug)]
pub(super) struct DispatchPromptEnvelopeConfig {
    pub(super) preamble_file: PathBuf,
    pub(super) fresh_envelope: PathBuf,
    pub(super) followup_envelope: PathBuf,
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
    #[serde(default)]
    prompt_envelopes: DispatchPromptEnvelopeConfigFile,
    roles: HashMap<String, DispatchRoleConfigFile>,
    directives: HashMap<String, DispatchDirectiveConfigFile>,
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

    let prompt_envelopes = DispatchPromptEnvelopeConfig {
        preamble_file: resolve_config_path(&base_dir, &raw.prompt_envelopes.preamble_file)?,
        fresh_envelope: resolve_config_path(&base_dir, &raw.prompt_envelopes.fresh_envelope)?,
        followup_envelope: resolve_config_path(&base_dir, &raw.prompt_envelopes.followup_envelope)?,
    };

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

    Ok(DispatchConfig {
        allowed_actors: raw
            .allowed_actors
            .into_iter()
            .map(|actor| actor.to_ascii_lowercase())
            .collect(),
        prompt_envelopes,
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
"#,
            preamble = prompts_dir.join("orchd-preamble.md").display(),
            fresh = prompts_dir.join("orchd-envelope-fresh.md").display(),
            followup = prompts_dir.join("orchd-envelope-followup.md").display(),
            token = root.join("token.txt").display(),
            design_prompt = prompts_dir.join("orchd-design.md").display(),
        );
        fs::write(&config_path, config_toml)?;

        let config = load_dispatch_config(&config_path)?;
        assert!(config.roles.contains_key("codex-orch"));
        Ok(())
    }
}
