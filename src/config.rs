use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, anyhow};
use url::Url;

use crate::types::RepoRef;

#[derive(Debug, Clone)]
pub struct AgentConfig {
    pub base_url: Url,
    pub default_repo: RepoRef,
    pub agent_name: String,
    pub lease_minutes: i64,
    pub token: String,
}

fn home_dir() -> Result<PathBuf> {
    std::env::var("HOME")
        .map(PathBuf::from)
        .map_err(|_| anyhow!("HOME is not set"))
}

fn expand_tilde(path: &str) -> Result<PathBuf> {
    if path == "~" {
        return home_dir();
    }
    if let Some(rest) = path.strip_prefix("~/") {
        return Ok(home_dir()?.join(rest));
    }
    Ok(PathBuf::from(path))
}

fn default_config_path() -> Result<PathBuf> {
    let xdg = std::env::var("XDG_CONFIG_HOME").ok();
    if let Some(xdg) = xdg {
        return Ok(PathBuf::from(xdg).join("forgejo-agent/config.env"));
    }
    Ok(home_dir()?.join(".config/forgejo-agent/config.env"))
}

fn load_env_file(path: &Path) -> Result<HashMap<String, String>> {
    let iter = dotenvy::from_path_iter(path)
        .with_context(|| format!("failed to read config file: {}", path.display()))?;
    let mut map = HashMap::new();
    for item in iter {
        let (k, v) = item.with_context(|| format!("failed to parse {}", path.display()))?;
        map.insert(k, v);
    }
    Ok(map)
}

fn systemd_token_file() -> Option<PathBuf> {
    let cred_dir = std::env::var_os("CREDENTIALS_DIRECTORY")?;
    let token_path = PathBuf::from(cred_dir).join("forgejo_token");
    if token_path.is_file() {
        return Some(token_path);
    }
    None
}

impl AgentConfig {
    pub fn load(config_override: Option<PathBuf>, token_override: Option<PathBuf>) -> Result<Self> {
        let config_path = if let Some(path) = config_override {
            path
        } else {
            default_config_path()?
        };

        let vars = load_env_file(&config_path)?;

        let base_url = vars
            .get("FORGEJO_BASE_URL")
            .map_or("http://127.0.0.1:3000", String::as_str);
        let base_url = Url::parse(base_url)
            .with_context(|| format!("invalid FORGEJO_BASE_URL: {base_url}"))?;

        let owner = vars
            .get("FORGEJO_DEFAULT_OWNER")
            .cloned()
            .unwrap_or_else(|| "main".to_string());
        let repo = vars
            .get("FORGEJO_DEFAULT_REPO")
            .cloned()
            .unwrap_or_else(|| "backlog".to_string());

        let default_repo = RepoRef::new(owner, repo);

        let agent_name = vars
            .get("FORGEJO_AGENT_NAME")
            .cloned()
            .unwrap_or_else(|| "codex".to_string());

        let lease_minutes = vars
            .get("FORGEJO_LEASE_MINUTES")
            .map_or("90", String::as_str)
            .parse()
            .context("invalid FORGEJO_LEASE_MINUTES")?;

        let token_file = if let Some(path) = token_override {
            path
        } else if let Ok(path) = std::env::var("FORGEJO_TOKEN_FILE") {
            expand_tilde(&path)?
        } else if let Some(path) = systemd_token_file() {
            path
        } else if let Some(path) = vars.get("FORGEJO_TOKEN_FILE") {
            expand_tilde(path)?
        } else {
            let cfg_dir = config_path
                .parent()
                .map(PathBuf::from)
                .ok_or_else(|| anyhow!("config path has no parent: {}", config_path.display()))?;
            cfg_dir.join("token")
        };

        let token = fs::read_to_string(&token_file)
            .with_context(|| format!("failed to read token file: {}", token_file.display()))?;
        let token = token.trim().to_string();
        if token.is_empty() {
            return Err(anyhow!("token file is empty: {}", token_file.display()));
        }

        Ok(Self {
            base_url,
            default_repo,
            agent_name,
            lease_minutes,
            token,
        })
    }
}
