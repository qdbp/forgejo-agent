use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::Result;
use arc_swap::ArcSwap;
use serde_json::json;
use sha2::Digest as _;

use super::dispatch_config::{
    DispatchConfig, load_dispatch_config_from_str, read_dispatch_config_text,
};
use super::telemetry::log_line;

#[derive(Clone)]
pub(super) enum DispatchConfigHandle {
    Disabled,
    Live(Arc<DispatchConfigLive>),
}

impl DispatchConfigHandle {
    pub(super) fn load(path: PathBuf) -> Result<Self> {
        let raw_text = read_dispatch_config_text(&path)?;
        let sha256 = sha256_hex(raw_text.as_bytes());
        let config = load_dispatch_config_from_str(&path, &raw_text)?;
        Ok(Self::Live(Arc::new(DispatchConfigLive {
            path,
            current: ArcSwap::from_pointee(config),
            reload: Mutex::new(DispatchConfigReloadState {
                last_good_sha256: sha256,
                last_error: None,
            }),
        })))
    }

    pub(super) fn snapshot(&self) -> Option<Arc<DispatchConfig>> {
        match self {
            Self::Disabled => None,
            Self::Live(live) => Some(live.current.load_full()),
        }
    }

    pub(super) fn reload_once(&self) {
        let Self::Live(live) = self else {
            return;
        };
        live.reload_once();
    }
}

pub(super) async fn run_dispatch_config_reload_loop(
    handle: DispatchConfigHandle,
    interval_sec: u64,
) {
    if interval_sec == 0 {
        return;
    }
    if matches!(&handle, DispatchConfigHandle::Disabled) {
        return;
    }
    let interval = Duration::from_secs(interval_sec.max(1));
    loop {
        handle.reload_once();
        tokio::time::sleep(interval).await;
    }
}

pub(super) struct DispatchConfigLive {
    path: PathBuf,
    current: ArcSwap<DispatchConfig>,
    reload: Mutex<DispatchConfigReloadState>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum DispatchConfigReloadError {
    Io(String),
    InvalidToml { sha256: String, error: String },
}

#[derive(Debug)]
struct DispatchConfigReloadState {
    last_good_sha256: String,
    last_error: Option<DispatchConfigReloadError>,
}

impl DispatchConfigLive {
    fn reload_once(&self) {
        let mut state = self
            .reload
            .lock()
            .expect("dispatch config reload mutex poisoned");

        let raw_text = match read_dispatch_config_text(&self.path) {
            Ok(text) => text,
            Err(err) => {
                let key = DispatchConfigReloadError::Io(err.to_string());
                if state.last_error.as_ref() != Some(&key) {
                    log_line(
                        "dispatch_config_reload_failed",
                        json!({
                            "path": self.path.display().to_string(),
                            "kind": "io",
                            "error": err.to_string(),
                        }),
                    );
                    state.last_error = Some(key);
                }
                return;
            }
        };

        let sha256 = sha256_hex(raw_text.as_bytes());
        if sha256 == state.last_good_sha256 {
            state.last_error = None;
            return;
        }
        if state
            .last_error
            .as_ref()
            .is_some_and(|err| matches!(err, DispatchConfigReloadError::InvalidToml { sha256: last, .. } if *last == sha256))
        {
            return;
        }

        match load_dispatch_config_from_str(&self.path, &raw_text) {
            Ok(new_cfg) => {
                self.current.store(Arc::new(new_cfg));
                log_line(
                    "dispatch_config_reloaded",
                    json!({
                        "path": self.path.display().to_string(),
                        "sha256": sha256,
                    }),
                );
                state.last_good_sha256 = sha256;
                state.last_error = None;
            }
            Err(err) => {
                let key = DispatchConfigReloadError::InvalidToml {
                    sha256: sha256.clone(),
                    error: err.to_string(),
                };
                if state.last_error.as_ref() != Some(&key) {
                    log_line(
                        "dispatch_config_reload_failed",
                        json!({
                            "path": self.path.display().to_string(),
                            "kind": "parse",
                            "sha256": sha256,
                            "error": err.to_string(),
                        }),
                    );
                    state.last_error = Some(key);
                }
            }
        }
    }
}

fn sha256_hex(bytes: &[u8]) -> String {
    let mut hasher = sha2::Sha256::new();
    hasher.update(bytes);
    hex::encode(hasher.finalize())
}

#[cfg(test)]
mod tests {
    use std::fs;

    use anyhow::Result;
    use tempfile::tempdir;

    use super::{DispatchConfigHandle, sha256_hex};

    #[test]
    fn reload_once_swaps_snapshot_on_valid_change() -> Result<()> {
        let temp = tempdir()?;
        let root = temp.path();
        let config_path = root.join("dispatch.toml");
        let prompts_dir = root.join("prompts");
        let roles_dir = prompts_dir.join("roles");
        fs::create_dir_all(&roles_dir)?;
        fs::write(roles_dir.join("codex-orch.md"), "# role\n\n- OF-8\n")?;
        fs::write(roles_dir.join("main.md"), "# main\n\n- OF-10\n")?;

        let config_v1 = format!(
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

[[repo_bindings]]
repo = "main/forgejo-agent"
local_path = "/tmp/forgejo-agent"
git_remote = "origin"
git_base = "main"
"#,
            preamble = prompts_dir.join("orchd-preamble.md").display(),
            fresh = prompts_dir.join("orchd-envelope-fresh.md").display(),
            followup = prompts_dir.join("orchd-envelope-followup.md").display(),
            token = root.join("token.txt").display(),
            reply_prompt = prompts_dir.join("orders").join("orchd-reply.md").display(),
        );
        fs::write(&config_path, &config_v1)?;

        let handle = DispatchConfigHandle::load(config_path.clone())?;
        let snap1 = handle.snapshot().expect("snapshot");
        assert!(!snap1.repo_bindings.contains_key("main/empty-status"));

        let config_v2 = format!(
            r#"{base}

[[repo_bindings]]
repo = "main/empty-status"
local_path = "/tmp/empty-status"
git_remote = "origin"
git_base = "main"
"#,
            base = config_v1.trim_end(),
        );
        fs::write(&config_path, &config_v2)?;
        assert_ne!(
            sha256_hex(config_v1.as_bytes()),
            sha256_hex(config_v2.as_bytes())
        );

        handle.reload_once();
        let snap2 = handle.snapshot().expect("snapshot");
        assert!(snap2.repo_bindings.contains_key("main/empty-status"));
        Ok(())
    }
}
