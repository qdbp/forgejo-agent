use std::env;
use std::path::Path;
use std::process::Command;

fn git_stdout_trim(workdir: &Path, args: &[&str]) -> Option<String> {
    let out = Command::new("git")
        .args(args)
        .current_dir(workdir)
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let s = String::from_utf8(out.stdout).ok()?;
    Some(s.trim().to_string())
}

fn main() {
    println!("cargo:rerun-if-env-changed=CARGO_PKG_VERSION");
    // Keep the build identifier in sync with git state when available.
    println!("cargo:rerun-if-changed=.git/HEAD");
    println!("cargo:rerun-if-changed=.git/index");

    let manifest_dir = env::var("CARGO_MANIFEST_DIR").unwrap_or_else(|_| ".".to_string());
    let manifest_dir = Path::new(&manifest_dir);

    let pkg_version = env::var("CARGO_PKG_VERSION").unwrap_or_else(|_| "0.0.0".to_string());

    let build = git_stdout_trim(manifest_dir, &["describe", "--tags", "--always", "--dirty"])
        .unwrap_or(pkg_version);
    println!("cargo:rustc-env=FORGEJO_AGENT_BUILD={build}");

    if let Some(sha) = git_stdout_trim(manifest_dir, &["rev-parse", "HEAD"]) {
        println!("cargo:rustc-env=FORGEJO_AGENT_GIT_SHA={sha}");
    }
}
