use std::path::Path;
use std::process::Command;

use anyhow::{Context, Result, anyhow};

pub(super) fn run_forgejoctl(
    forgejoctl_bin: &Path,
    config_file: Option<&Path>,
    token_file: &Path,
    args: &[&str],
) -> Result<()> {
    let mut cmd = Command::new(forgejoctl_bin);
    if let Some(config_file) = config_file {
        cmd.arg("--config").arg(config_file);
    }
    let status = cmd
        .arg("--token-file")
        .arg(token_file)
        .args(args)
        .status()
        .with_context(|| format!("failed invoking forgejoctl {}", forgejoctl_bin.display()))?;
    if status.success() {
        Ok(())
    } else {
        Err(anyhow!(
            "forgejoctl command failed (exit={:?}) args={args:?}",
            status.code(),
        ))
    }
}
