use std::path::PathBuf;

use anyhow::{Result, anyhow};

fn home_dir() -> Result<PathBuf> {
    std::env::var("HOME")
        .map(PathBuf::from)
        .map_err(|_| anyhow!("HOME is not set"))
}

pub(super) const DEFAULT_DISPATCH_CONFIG: &str = "config/orchd-dispatch.toml";

pub(super) fn expand_tilde_path(path: &str) -> Result<PathBuf> {
    if path == "~" {
        return home_dir();
    }
    if let Some(rest) = path.strip_prefix("~/") {
        return Ok(home_dir()?.join(rest));
    }
    Ok(PathBuf::from(path))
}

fn swarm_home_dir() -> Result<PathBuf> {
    if let Ok(path) = std::env::var("SWARM_HOME") {
        return Ok(PathBuf::from(path));
    }
    Ok(home_dir()?.join("swarm"))
}

pub(super) fn resolve_dispatch_config_path(raw: &str) -> Result<PathBuf> {
    let expanded = expand_tilde_path(raw)?;
    if expanded.exists() {
        return Ok(expanded);
    }

    if raw == DEFAULT_DISPATCH_CONFIG {
        let candidate = swarm_home_dir()?.join(DEFAULT_DISPATCH_CONFIG);
        if candidate.exists() {
            return Ok(candidate);
        }
    }

    Ok(expanded)
}
