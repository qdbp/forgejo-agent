use std::env;
use std::fs;
use std::io::{self, Read, Write as _};
use std::path::PathBuf;
use std::process::Command;
use std::thread;
use std::time::Duration;
use std::time::SystemTime;
use std::time::UNIX_EPOCH;

use anyhow::{Context, Result};

fn parse_output_path(args: &[String]) -> Option<PathBuf> {
    args.windows(2).find_map(|window| {
        if window[0] == "-o" {
            Some(PathBuf::from(window[1].as_str()))
        } else {
            None
        }
    })
}

fn parse_cd_path(args: &[String]) -> Option<PathBuf> {
    args.windows(2)
        .find_map(|window| {
            if window[0] == "--cd" {
                Some(PathBuf::from(window[1].as_str()))
            } else {
                None
            }
        })
        .or_else(|| {
            args.iter().find_map(|arg| {
                let prefix = "--cd=";
                arg.strip_prefix(prefix).map(PathBuf::from)
            })
        })
}

fn mode() -> String {
    env::var("FAKE_CODEX_MODE")
        .unwrap_or_else(|_| "success".to_string())
        .to_ascii_lowercase()
}

fn now_unix_millis() -> Result<u128> {
    Ok(SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .context("system clock before UNIX_EPOCH")?
        .as_millis())
}

fn optional_sleep() -> Result<()> {
    let raw = env::var("FAKE_CODEX_SLEEP_MS").ok();
    let Some(raw) = raw else {
        return Ok(());
    };
    let millis: u64 = raw.parse().context("invalid FAKE_CODEX_SLEEP_MS")?;
    if millis == 0 {
        return Ok(());
    }
    thread::sleep(Duration::from_millis(millis));
    Ok(())
}

fn git_append_and_commit() -> Result<()> {
    optional_sleep()?;

    let file = env::var("FAKE_CODEX_GIT_FILE").unwrap_or_else(|_| "fake-codex.txt".to_string());
    let message = env::var("FAKE_CODEX_GIT_COMMIT_MESSAGE").unwrap_or_else(|_| {
        format!(
            "fake-codex: commit {}",
            now_unix_millis().unwrap_or_default()
        )
    });

    let stamp = now_unix_millis()?;
    let line = format!("fake-codex stamp={stamp}\n");
    fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&file)
        .with_context(|| format!("failed opening {file} for append"))?
        .write_all(line.as_bytes())
        .with_context(|| format!("failed appending to {file}"))?;

    let add_status = Command::new("git")
        .args(["add", "--"])
        .arg(&file)
        .status()
        .context("failed spawning git add")?;
    if !add_status.success() {
        anyhow::bail!("git add failed (status={add_status})");
    }

    let commit_status = Command::new("git")
        .args([
            "-c",
            "user.name=fake-codex",
            "-c",
            "user.email=fake-codex@localhost",
            "commit",
            "-m",
            &message,
        ])
        .status()
        .context("failed spawning git commit")?;
    if !commit_status.success() {
        anyhow::bail!("git commit failed (status={commit_status})");
    }

    Ok(())
}

fn run() -> Result<i32> {
    let args: Vec<String> = env::args().collect();
    let output_path = parse_output_path(&args);
    if let Some(cd) = parse_cd_path(&args) {
        env::set_current_dir(&cd)
            .with_context(|| format!("failed to chdir to {}", cd.display()))?;
    }

    let mut stdin_buf = String::new();
    let _ = io::stdin()
        .read_to_string(&mut stdin_buf)
        .context("failed reading stdin")?;

    println!("OpenAI Codex v0.fake");
    println!("session id: 00000000-0000-0000-0000-000000000001");
    println!("tokens used");
    println!("42");

    match mode().as_str() {
        "git_append_commit" => {
            git_append_and_commit()?;
            if let Some(path) = output_path {
                fs::write(
                    path,
                    "Status: fake-codex committed changes.\nNext action: orchd should land them.\n",
                )
                .context("failed writing fake codex output file")?;
            }
            Ok(0)
        }
        "timeout" => {
            thread::sleep(Duration::from_secs(120));
            Ok(0)
        }
        "nonzero" => Ok(7),
        "no_output" => Ok(0),
        _ => {
            if let Some(path) = output_path {
                fs::write(
                    path,
                    "Status: fake-codex completed successfully.\nNext action: continue.\n",
                )
                .context("failed writing fake codex output file")?;
            }
            Ok(0)
        }
    }
}

fn main() {
    match run() {
        Ok(code) => std::process::exit(code),
        Err(err) => {
            eprintln!("fake-codex error: {err:#}");
            std::process::exit(1);
        }
    }
}
