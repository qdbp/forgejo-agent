use std::env;
use std::fs;
use std::io::{self, Read};
use std::path::PathBuf;
use std::thread;
use std::time::Duration;

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

fn mode() -> String {
    env::var("FAKE_CODEX_MODE")
        .unwrap_or_else(|_| "success".to_string())
        .to_ascii_lowercase()
}

fn run() -> Result<i32> {
    let args: Vec<String> = env::args().collect();
    let output_path = parse_output_path(&args);

    let mut stdin_buf = String::new();
    let _ = io::stdin()
        .read_to_string(&mut stdin_buf)
        .context("failed reading stdin")?;

    println!("OpenAI Codex v0.fake");
    println!("session id: 00000000-0000-0000-0000-000000000001");
    println!("tokens used");
    println!("42");

    match mode().as_str() {
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
