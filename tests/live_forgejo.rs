use std::fmt::Write as _;
use std::fs::{self, File, OpenOptions};
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Output, Stdio};
use std::sync::{LazyLock, Mutex};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, anyhow, bail};
use portpicker::pick_unused_port;
use reqwest::StatusCode;
use reqwest::blocking::Client;
use rusqlite::{Connection, OptionalExtension, params};
use serde_json::Value;
use serial_test::serial;
use tempfile::TempDir;

const LIVE_TESTS_ENV: &str = "FORGEJO_LIVE_TESTS";
const TIMINGS_PATH_ENV: &str = "FORGEJO_LIVE_TIMINGS_PATH";
const KEEP_FIXTURE_ENV: &str = "FORGEJO_LIVE_KEEP_FIXTURE";
const FORGEJO_BIN_ENV: &str = "FORGEJO_BIN";

static TIMINGS_LOCK: LazyLock<Mutex<()>> = LazyLock::new(|| Mutex::new(()));

#[derive(Debug)]
struct ChildGuard(Child);

impl Drop for ChildGuard {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

fn live_tests_enabled() -> bool {
    match std::env::var(LIVE_TESTS_ENV) {
        Ok(value) => matches!(
            value.trim().to_ascii_lowercase().as_str(),
            "1" | "true" | "yes" | "on"
        ),
        Err(_) => false,
    }
}

fn fixture_keep_enabled() -> bool {
    match std::env::var(KEEP_FIXTURE_ENV) {
        Ok(value) => matches!(
            value.trim().to_ascii_lowercase().as_str(),
            "1" | "true" | "yes" | "on"
        ),
        Err(_) => false,
    }
}

fn timings_path() -> PathBuf {
    std::env::var(TIMINGS_PATH_ENV)
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from("target/live-test-timings.jsonl"))
}

fn forgejo_bin() -> String {
    std::env::var(FORGEJO_BIN_ENV).unwrap_or_else(|_| "forgejo".to_string())
}

fn now_unix_millis() -> Result<u128> {
    let duration = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .context("system clock before UNIX_EPOCH")?;
    Ok(duration.as_millis())
}

#[derive(Debug)]
struct StepTimer {
    test_name: &'static str,
    sink_path: PathBuf,
}

impl StepTimer {
    fn new(test_name: &'static str) -> Self {
        Self {
            test_name,
            sink_path: timings_path(),
        }
    }

    fn record(&self, step: &str, started_at: Instant) -> Result<()> {
        let elapsed_ms = started_at.elapsed().as_millis();
        eprintln!(
            "[live-timing] test={} step={} elapsed_ms={elapsed_ms}",
            self.test_name, step
        );

        if let Some(parent) = self.sink_path.parent() {
            fs::create_dir_all(parent).with_context(|| {
                format!("failed to create timing parent dir: {}", parent.display())
            })?;
        }

        let _hold_lock = TIMINGS_LOCK
            .lock()
            .map_err(|_| anyhow!("timings file lock poisoned"))?;

        let mut file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.sink_path)
            .with_context(|| {
                format!(
                    "failed opening timings sink for append: {}",
                    self.sink_path.display()
                )
            })?;

        let entry = serde_json::json!({
            "ts_unix_ms": now_unix_millis()?,
            "test": self.test_name,
            "step": step,
            "elapsed_ms": elapsed_ms,
        });

        writeln!(file, "{entry}")
            .with_context(|| format!("failed writing timings to {}", self.sink_path.display()))?;
        Ok(())
    }
}

fn unique_suffix() -> Result<String> {
    let millis = now_unix_millis()?;
    Ok(format!("{}-{millis}", std::process::id()))
}

fn path_to_string(path: &Path) -> String {
    path.to_string_lossy().into_owned()
}

fn run_command_checked(cmd: &mut Command, context: &str) -> Result<Output> {
    let output = cmd
        .output()
        .with_context(|| format!("failed spawning command for {context}"))?;
    if output.status.success() {
        return Ok(output);
    }

    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    bail!(
        "command failed for {context} (status={:?})\nstdout:\n{stdout}\nstderr:\n{stderr}",
        output.status.code()
    )
}

#[derive(Debug)]
struct ForgejoFixture {
    base_url: String,
    work_path: PathBuf,
    repos_root: PathBuf,
    owner: String,
    token: String,
    server_stdout: PathBuf,
    server_stderr: PathBuf,
    keep_fixture: bool,
    process: Option<Child>,
    temp_dir: Option<TempDir>,
}

impl ForgejoFixture {
    fn spawn(timer: &StepTimer) -> Result<Self> {
        let bootstrap_started = Instant::now();
        let temp_dir = TempDir::new().context("failed to create temporary fixture directory")?;
        let root = temp_dir.path().to_path_buf();

        let work_path = root.join("work");
        let custom_path = root.join("custom");
        let data_path = root.join("data");
        let repos_path = root.join("repos");
        let log_path = root.join("log");
        let package_chunk_path = data_path.join("tmp/package-upload");
        let app_ini = root.join("app.ini");
        let server_stdout = root.join("forgejo-stdout.log");
        let server_stderr = root.join("forgejo-stderr.log");

        for path in [
            &work_path,
            &custom_path,
            &data_path,
            &repos_path,
            &log_path,
            &package_chunk_path,
        ] {
            fs::create_dir_all(path)
                .with_context(|| format!("failed to create fixture path: {}", path.display()))?;
        }

        let port =
            pick_unused_port().ok_or_else(|| anyhow!("failed to pick an unused TCP port"))?;
        let base_url = format!("http://127.0.0.1:{port}");

        let app_ini_contents = format!(
            "[DEFAULT]\nAPP_NAME = forgejo-agent-integration-tests\nRUN_MODE = prod\nWORK_PATH = {}\nAPP_DATA_PATH = {}\n\n[server]\nPROTOCOL = http\nHTTP_ADDR = 127.0.0.1\nHTTP_PORT = {port}\nDOMAIN = 127.0.0.1\nROOT_URL = {base_url}/\nDISABLE_SSH = true\nOFFLINE_MODE = true\n\n[database]\nDB_TYPE = sqlite3\nPATH = {}\n\n[repository]\nROOT = {}\n\n[packages]\nENABLED = false\nCHUNKED_UPLOAD_PATH = {}\n\n[actions]\nENABLED = false\n\n[service]\nDISABLE_REGISTRATION = true\n\n[security]\nINSTALL_LOCK = true\nSECRET_KEY = secret-{}\nINTERNAL_TOKEN = internal-{}\n\n[log]\nMODE = file\nROOT_PATH = {}\n",
            path_to_string(&work_path),
            path_to_string(&data_path),
            path_to_string(&root.join("forgejo.sqlite")),
            path_to_string(&repos_path),
            path_to_string(&package_chunk_path),
            unique_suffix()?,
            unique_suffix()?,
            path_to_string(&log_path),
        );
        fs::write(&app_ini, app_ini_contents)
            .with_context(|| format!("failed to write {}", app_ini.display()))?;

        let owner = "itest-admin".to_string();
        let password = "itest-password";
        Self::migrate_database(&app_ini, &work_path)?;
        Self::create_admin_user(&app_ini, &work_path, &owner, password)?;
        let token = Self::create_access_token(&app_ini, &work_path, &owner)?;

        let stdout_file = File::create(&server_stdout)
            .with_context(|| format!("failed to create {}", server_stdout.display()))?;
        let stderr_file = File::create(&server_stderr)
            .with_context(|| format!("failed to create {}", server_stderr.display()))?;

        let mut web_cmd = Command::new(forgejo_bin());
        web_cmd
            .arg("web")
            .arg("--config")
            .arg(&app_ini)
            .arg("--work-path")
            .arg(&work_path)
            .arg("--custom-path")
            .arg(&custom_path)
            .stdout(Stdio::from(stdout_file))
            .stderr(Stdio::from(stderr_file));

        let process = web_cmd.spawn().with_context(|| {
            format!(
                "failed to spawn forgejo web with config {}",
                app_ini.display()
            )
        })?;

        let mut fixture = Self {
            base_url,
            work_path,
            repos_root: repos_path,
            owner,
            token,
            server_stdout,
            server_stderr,
            keep_fixture: fixture_keep_enabled(),
            process: Some(process),
            temp_dir: Some(temp_dir),
        };

        fixture.wait_ready(Duration::from_secs(30))?;
        timer.record("fixture.spawn_and_ready", bootstrap_started)?;
        Ok(fixture)
    }

    fn migrate_database(app_ini: &Path, work_path: &Path) -> Result<()> {
        let mut cmd = Command::new(forgejo_bin());
        cmd.arg("migrate")
            .arg("--config")
            .arg(app_ini)
            .arg("--work-path")
            .arg(work_path);

        run_command_checked(&mut cmd, "forgejo migrate")?;
        Ok(())
    }

    fn create_admin_user(
        app_ini: &Path,
        work_path: &Path,
        username: &str,
        password: &str,
    ) -> Result<()> {
        let mut cmd = Command::new(forgejo_bin());
        cmd.arg("admin")
            .arg("user")
            .arg("create")
            .arg("--config")
            .arg(app_ini)
            .arg("--work-path")
            .arg(work_path)
            .arg("--username")
            .arg(username)
            .arg("--email")
            .arg(format!("{username}@localhost"))
            .arg("--password")
            .arg(password)
            .arg("--admin")
            .arg("--must-change-password=false");

        run_command_checked(&mut cmd, "forgejo admin user create")?;
        Ok(())
    }

    fn create_access_token(app_ini: &Path, work_path: &Path, username: &str) -> Result<String> {
        let token_name = format!("itest-{}", unique_suffix()?);
        let mut cmd = Command::new(forgejo_bin());
        cmd.arg("admin")
            .arg("user")
            .arg("generate-access-token")
            .arg("--config")
            .arg(app_ini)
            .arg("--work-path")
            .arg(work_path)
            .arg("--username")
            .arg(username)
            .arg("--token-name")
            .arg(token_name)
            .arg("--scopes")
            .arg("all")
            .arg("--raw");

        let output = run_command_checked(&mut cmd, "forgejo admin user generate-access-token")?;
        let token = String::from_utf8(output.stdout)
            .context("token output from forgejo admin was not utf-8")?
            .trim()
            .to_string();

        if token.is_empty() {
            bail!("forgejo admin returned an empty access token");
        }
        Ok(token)
    }

    fn wait_ready(&mut self, timeout: Duration) -> Result<()> {
        let client = Client::builder()
            .timeout(Duration::from_millis(750))
            .build()
            .context("failed to build readiness HTTP client")?;

        let deadline = Instant::now() + timeout;
        loop {
            if let Some(exit) = self
                .process
                .as_mut()
                .ok_or_else(|| anyhow!("forgejo process missing during readiness wait"))?
                .try_wait()
                .context("failed to poll forgejo process")?
            {
                let stdout = fs::read_to_string(&self.server_stdout).unwrap_or_default();
                let stderr = fs::read_to_string(&self.server_stderr).unwrap_or_default();
                bail!(
                    "forgejo exited before readiness (status={exit})\nstdout:\n{stdout}\nstderr:\n{stderr}"
                );
            }

            let version_url = format!("{}/api/v1/version", self.base_url);
            if let Ok(resp) = client.get(&version_url).send()
                && resp.status() == StatusCode::OK
            {
                return Ok(());
            }

            if Instant::now() >= deadline {
                let stdout = fs::read_to_string(&self.server_stdout).unwrap_or_default();
                let stderr = fs::read_to_string(&self.server_stderr).unwrap_or_default();
                bail!(
                    "forgejo did not become ready within {timeout:?}\nstdout:\n{stdout}\nstderr:\n{stderr}"
                );
            }

            thread::sleep(Duration::from_millis(125));
        }
    }

    fn write_agent_config(&self, repo: &str, timer: &StepTimer) -> Result<(PathBuf, PathBuf)> {
        let started = Instant::now();
        let cfg_dir = self.work_path.join("agent-config");
        fs::create_dir_all(&cfg_dir)
            .with_context(|| format!("failed to create {}", cfg_dir.display()))?;

        let token_path = cfg_dir.join("token");
        fs::write(&token_path, format!("{}\n", self.token))
            .with_context(|| format!("failed writing token file: {}", token_path.display()))?;

        let config_path = cfg_dir.join("config.env");
        let config_body = format!(
            "FORGEJO_BASE_URL={}\nFORGEJO_DEFAULT_OWNER={}\nFORGEJO_DEFAULT_REPO={}\nFORGEJO_AGENT_NAME=itest\nFORGEJO_LEASE_MINUTES=30\n",
            self.base_url, self.owner, repo
        );
        fs::write(&config_path, config_body)
            .with_context(|| format!("failed writing config: {}", config_path.display()))?;

        timer.record("fixture.write_agent_config", started)?;
        Ok((config_path, token_path))
    }

    fn repo_git_dir(&self, repo_name: &str) -> PathBuf {
        self.repos_root
            .join(self.owner.as_str())
            .join(format!("{repo_name}.git"))
    }

    fn create_principal_checkout(&self, repo_name: &str) -> Result<PathBuf> {
        let bare_repo = self.repo_git_dir(repo_name);
        if !bare_repo.is_dir() {
            bail!(
                "expected bare repo at {}, but it does not exist",
                bare_repo.display()
            );
        }

        let checkout = self.work_path.join("principal").join(repo_name);
        if checkout.exists() {
            fs::remove_dir_all(&checkout).with_context(|| {
                format!("failed removing stale checkout {}", checkout.display())
            })?;
        }
        if let Some(parent) = checkout.parent() {
            fs::create_dir_all(parent)
                .with_context(|| format!("failed creating {}", parent.display()))?;
        }

        let mut clone = Command::new("git");
        clone.args(["clone"]).arg(&bare_repo).arg(&checkout);
        run_command_checked(&mut clone, "git clone principal checkout")?;

        let head = stdout_trim(&git_output_checked(
            &checkout,
            &["rev-parse", "--abbrev-ref", "HEAD"],
            "git rev-parse --abbrev-ref HEAD",
        )?)?;
        if head != "main" {
            git_output_checked(
                &checkout,
                &["branch", "-f", "main", "HEAD"],
                "git branch -f main HEAD",
            )?;
            git_output_checked(&checkout, &["checkout", "main"], "git checkout main")?;
        }

        Ok(checkout)
    }

    fn authed_get(&self, path: &str) -> Result<Value> {
        let client = Client::builder()
            .timeout(Duration::from_secs(3))
            .build()
            .context("failed to build verification HTTP client")?;

        let url = format!("{}{}", self.base_url, path);
        let response = client
            .get(url)
            .header("Accept", "application/json")
            .header("Authorization", format!("token {}", self.token))
            .send()
            .context("verification GET failed")?;

        let status = response.status();
        let text = response
            .text()
            .context("failed to read verification response body")?;
        if !status.is_success() {
            bail!("verification GET failed with status={status} body={text}");
        }

        serde_json::from_str(&text)
            .with_context(|| format!("failed to parse verification JSON: {text}"))
    }
}

impl Drop for ForgejoFixture {
    fn drop(&mut self) {
        if let Some(mut child) = self.process.take() {
            if let Ok(Some(_)) = child.try_wait() {
            } else {
                let _ = child.kill();
                let _ = child.wait();
            }
        }

        if self.keep_fixture
            && let Some(temp_dir) = self.temp_dir.take()
        {
            let kept = temp_dir.keep();
            eprintln!(
                "[live-fixture] keeping fixture directory at {}",
                kept.display()
            );
        }
    }
}

fn forgejo_agent_bin() -> Result<PathBuf> {
    const CANDIDATE_ENV: [&str; 2] = ["CARGO_BIN_EXE_forgejo-agent", "CARGO_BIN_EXE_forgejo_agent"];
    for key in CANDIDATE_ENV {
        if let Ok(value) = std::env::var(key) {
            return Ok(PathBuf::from(value));
        }
    }

    let mut path = std::env::current_exe().context("failed to inspect current test executable")?;
    path.pop();
    if path.ends_with("deps") {
        path.pop();
    }
    path.push(format!("forgejo-agent{}", std::env::consts::EXE_SUFFIX));
    if path.is_file() {
        return Ok(path);
    }

    bail!(
        "failed to locate forgejo-agent binary; checked env vars {CANDIDATE_ENV:?} and {}",
        path.display()
    );
}

fn orchd_bin() -> Result<PathBuf> {
    const CANDIDATE_ENV: [&str; 1] = ["CARGO_BIN_EXE_orchd"];
    for key in CANDIDATE_ENV {
        if let Ok(value) = std::env::var(key) {
            return Ok(PathBuf::from(value));
        }
    }

    let mut path = std::env::current_exe().context("failed to inspect current test executable")?;
    path.pop();
    if path.ends_with("deps") {
        path.pop();
    }
    path.push(format!("orchd{}", std::env::consts::EXE_SUFFIX));
    if path.is_file() {
        return Ok(path);
    }

    bail!(
        "failed to locate orchd binary; checked env vars {CANDIDATE_ENV:?} and {}",
        path.display()
    );
}

fn fake_codex_bin() -> Result<PathBuf> {
    const CANDIDATE_ENV: [&str; 2] = ["CARGO_BIN_EXE_fake-codex", "CARGO_BIN_EXE_fake_codex"];
    for key in CANDIDATE_ENV {
        if let Ok(value) = std::env::var(key) {
            let path = PathBuf::from(value);
            if path.is_file() {
                return Ok(path);
            }
        }
    }

    let mut path = std::env::current_exe().context("failed to inspect current test executable")?;
    path.pop();
    if path.ends_with("deps") {
        path.pop();
    }
    path.push(format!("fake-codex{}", std::env::consts::EXE_SUFFIX));
    if path.is_file() {
        return Ok(path);
    }

    bail!(
        "failed to locate fake-codex binary; checked env vars {CANDIDATE_ENV:?} and {}",
        path.display()
    );
}

fn ensure_fake_codex_bin() -> Result<PathBuf> {
    fake_codex_bin().or_else(|_| {
        let status = Command::new("cargo")
            .current_dir(env!("CARGO_MANIFEST_DIR"))
            .args(["build", "--quiet", "--bin", "fake-codex"])
            .status()
            .context("failed to build fake-codex")?;
        if !status.success() {
            bail!("failed building fake-codex");
        }
        fake_codex_bin()
    })
}

fn git_output_checked(workdir: &Path, args: &[&str], context: &str) -> Result<Output> {
    let mut cmd = Command::new("git");
    cmd.arg("-C").arg(workdir).args(args);
    run_command_checked(&mut cmd, context)
}

fn git_bare_output_checked(git_dir: &Path, args: &[&str], context: &str) -> Result<Output> {
    let mut cmd = Command::new("git");
    cmd.arg("--git-dir").arg(git_dir).args(args);
    run_command_checked(&mut cmd, context)
}

fn stdout_trim(output: &Output) -> Result<String> {
    String::from_utf8(output.stdout.clone())
        .context("command stdout was not utf-8")
        .map(|s| s.trim().to_string())
}

#[derive(Debug)]
struct GitWorkspace {
    bare_repo: PathBuf,
}

impl GitWorkspace {
    fn from_fixture(fixture: &ForgejoFixture, repo_name: &str) -> Result<Self> {
        let bare_repo = fixture.repo_git_dir(repo_name);
        if !bare_repo.is_dir() {
            bail!(
                "expected bare repo at {}, but it does not exist",
                bare_repo.display()
            );
        }

        let temp = TempDir::new().context("failed to create temp dir for git workspace")?;
        let checkout = temp.path().join("checkout");

        let mut clone = Command::new("git");
        clone.args(["clone"]).arg(&bare_repo).arg(&checkout);
        run_command_checked(&mut clone, "git clone bare repo")?;

        let head = stdout_trim(&git_output_checked(
            &checkout,
            &["rev-parse", "--abbrev-ref", "HEAD"],
            "git rev-parse --abbrev-ref HEAD",
        )?)?;

        if head != "main" {
            git_output_checked(
                &checkout,
                &["branch", "-f", "main", "HEAD"],
                "git branch -f main HEAD",
            )?;
            git_output_checked(&checkout, &["checkout", "main"], "git checkout main")?;
        }

        // Ensure remote branch exists so orchd's `git fetch origin main` + `origin/main` worktree
        // base does not fail.
        git_output_checked(
            &checkout,
            &["push", "-u", "origin", "main"],
            "git push -u origin main",
        )?;

        Ok(Self { bare_repo })
    }

    fn bare_head_main(&self) -> Result<String> {
        stdout_trim(&git_bare_output_checked(
            &self.bare_repo,
            &["rev-parse", "refs/heads/main"],
            "git --git-dir rev-parse refs/heads/main",
        )?)
    }

    fn bare_commit_count_main(&self) -> Result<u64> {
        let out = stdout_trim(&git_bare_output_checked(
            &self.bare_repo,
            &["rev-list", "--count", "refs/heads/main"],
            "git --git-dir rev-list --count refs/heads/main",
        )?)?;
        out.parse::<u64>().context("rev-list count was not a u64")
    }
}

fn run_cli_output(config_path: &Path, token_path: &Path, args: &[&str]) -> Result<Output> {
    let mut cmd = Command::new(forgejo_agent_bin()?);

    cmd.arg("--config")
        .arg(config_path)
        .arg("--token-file")
        .arg(token_path)
        .args(args);

    cmd.output().context("failed to run forgejo-agent CLI")
}

fn decode_output_stdout(output: &Output) -> Result<String> {
    String::from_utf8(output.stdout.clone()).context("forgejo-agent stdout not utf-8")
}

fn decode_output_stderr(output: &Output) -> Result<String> {
    String::from_utf8(output.stderr.clone()).context("forgejo-agent stderr not utf-8")
}

fn run_cli_plain(config_path: &Path, token_path: &Path, args: &[&str]) -> Result<String> {
    let output = run_cli_output(config_path, token_path, args)?;
    let stdout = decode_output_stdout(&output)?;
    let stderr = decode_output_stderr(&output)?;
    if output.status.success() {
        return Ok(stdout);
    }

    bail!(
        "forgejo-agent command failed (status={:?})\nstdout:\n{stdout}\nstderr:\n{stderr}",
        output.status.code()
    )
}

fn run_cli_json(config_path: &Path, token_path: &Path, args: &[&str]) -> Result<Value> {
    let stdout = run_cli_plain(config_path, token_path, args)?;
    serde_json::from_str(&stdout).with_context(|| format!("stdout was not JSON: {stdout}"))
}

fn run_cli_expect_failure(config_path: &Path, token_path: &Path, args: &[&str]) -> Result<Output> {
    let output = run_cli_output(config_path, token_path, args)?;
    if !output.status.success() {
        return Ok(output);
    }

    let stdout = decode_output_stdout(&output)?;
    let stderr = decode_output_stderr(&output)?;
    bail!("expected failure but command succeeded\nstdout:\n{stdout}\nstderr:\n{stderr}")
}

fn json_u64_field(value: &Value, field: &str) -> Result<u64> {
    value
        .get(field)
        .and_then(Value::as_u64)
        .ok_or_else(|| anyhow!("JSON missing numeric field '{field}'"))
}

fn json_str_field<'a>(value: &'a Value, field: &str) -> Result<&'a str> {
    value
        .get(field)
        .and_then(Value::as_str)
        .ok_or_else(|| anyhow!("JSON missing string field '{field}'"))
}

fn issue_label_names(issue: &Value) -> Result<Vec<String>> {
    let labels = issue
        .get("labels")
        .and_then(Value::as_array)
        .ok_or_else(|| anyhow!("issue JSON missing labels array"))?;

    let mut names = Vec::with_capacity(labels.len());
    for label in labels {
        let name = label
            .get("name")
            .and_then(Value::as_str)
            .ok_or_else(|| anyhow!("issue label missing name"))?;
        names.push(name.to_string());
    }
    names.sort_unstable();
    Ok(names)
}

fn issue_has_label(issue: &Value, label_name: &str) -> Result<bool> {
    Ok(issue_label_names(issue)?
        .iter()
        .any(|name| name == label_name))
}

fn issue_label_prefix_count(issue: &Value, prefix: &str) -> Result<usize> {
    Ok(issue_label_names(issue)?
        .iter()
        .filter(|name| name.starts_with(prefix))
        .count())
}

fn ensure_contains(haystack: &str, needle: &str, context: &str) -> Result<()> {
    if haystack.contains(needle) {
        return Ok(());
    }
    bail!("expected '{needle}' in {context}: {haystack}");
}

fn orchd_dispatch_count(db_path: &Path) -> Result<i64> {
    let conn = Connection::open(db_path)
        .with_context(|| format!("failed opening orchd db: {}", db_path.display()))?;
    conn.query_row("SELECT COUNT(1) FROM dispatches", [], |row| row.get(0))
        .with_context(|| format!("failed counting dispatches in {}", db_path.display()))
}

fn orchd_latest_dispatch_directive_and_role(
    db_path: &Path,
    repo_full_name: &str,
    issue_number: u64,
) -> Result<Option<(String, String)>> {
    let conn = Connection::open(db_path)
        .with_context(|| format!("failed opening orchd db: {}", db_path.display()))?;
    let issue_number = i64::try_from(issue_number)
        .with_context(|| format!("issue_number overflowed i64: {issue_number}"))?;
    conn.query_row(
        "SELECT directive, target_role \
         FROM dispatches \
         WHERE repo_full_name = ?1 AND issue_number = ?2 \
         ORDER BY id DESC LIMIT 1",
        params![repo_full_name, issue_number],
        |row| Ok((row.get(0)?, row.get(1)?)),
    )
    .optional()
    .with_context(|| {
        format!(
            "failed selecting latest dispatch from {}",
            db_path.display()
        )
    })
}

fn orchd_latest_dispatch_status_reason(
    db_path: &Path,
    repo_full_name: &str,
    issue_number: u64,
) -> Result<Option<(String, Option<String>)>> {
    let conn = Connection::open(db_path)
        .with_context(|| format!("failed opening orchd db: {}", db_path.display()))?;
    let issue_number = i64::try_from(issue_number)
        .with_context(|| format!("issue_number overflowed i64: {issue_number}"))?;
    conn.query_row(
        "SELECT status, reason_code \
         FROM dispatches \
         WHERE repo_full_name = ?1 AND issue_number = ?2 \
         ORDER BY id DESC LIMIT 1",
        params![repo_full_name, issue_number],
        |row| Ok((row.get(0)?, row.get(1)?)),
    )
    .optional()
    .with_context(|| {
        format!(
            "failed selecting latest dispatch status from {}",
            db_path.display()
        )
    })
}

fn orchd_issue_dispatch_statuses(
    db_path: &Path,
    repo_full_name: &str,
    issue_number: u64,
) -> Result<Vec<String>> {
    let conn = Connection::open(db_path)
        .with_context(|| format!("failed opening orchd db: {}", db_path.display()))?;
    let issue_number = i64::try_from(issue_number)
        .with_context(|| format!("issue_number overflowed i64: {issue_number}"))?;
    let mut stmt = conn.prepare(
        "SELECT status \
         FROM dispatches \
         WHERE repo_full_name = ?1 AND issue_number = ?2 \
         ORDER BY id ASC",
    )?;
    let rows = stmt.query_map(params![repo_full_name, issue_number], |row| row.get(0))?;
    let statuses = rows.collect::<std::result::Result<Vec<String>, _>>()?;
    Ok(statuses)
}

fn orchd_starting_dispatch_count(
    db_path: &Path,
    repo_full_name: &str,
    issue_number: u64,
) -> Result<i64> {
    let conn = Connection::open(db_path)
        .with_context(|| format!("failed opening orchd db: {}", db_path.display()))?;
    let issue_number = i64::try_from(issue_number)
        .with_context(|| format!("issue_number overflowed i64: {issue_number}"))?;
    conn.query_row(
        "SELECT COUNT(1) \
         FROM dispatches \
         WHERE repo_full_name = ?1 AND issue_number = ?2 AND status = 'starting'",
        params![repo_full_name, issue_number],
        |row| row.get(0),
    )
    .with_context(|| {
        format!(
            "failed counting starting dispatch rows from {}",
            db_path.display()
        )
    })
}

#[derive(Debug)]
struct LiveHarness {
    timer: StepTimer,
    fixture: ForgejoFixture,
    repo_name: String,
    repo_ref: String,
    principal_workdir: PathBuf,
    config_path: PathBuf,
    token_path: PathBuf,
}

impl LiveHarness {
    fn bootstrap(test_name: &'static str) -> Result<Self> {
        let timer = StepTimer::new(test_name);
        let fixture = ForgejoFixture::spawn(&timer)?;
        let repo_name = format!("itest-{}", unique_suffix()?);
        let repo_ref = format!("{}/{}", fixture.owner, repo_name);
        let (config_path, token_path) = fixture.write_agent_config(&repo_name, &timer)?;

        let start = Instant::now();
        let ensure_output =
            run_cli_plain(&config_path, &token_path, &["repo", "ensure", &repo_ref])?;
        timer.record("repo.ensure", start)?;
        ensure_contains(&ensure_output, "repo ensured", "repo ensure output")?;
        let principal_workdir = fixture.create_principal_checkout(&repo_name)?;

        Ok(Self {
            timer,
            fixture,
            repo_name,
            repo_ref,
            principal_workdir,
            config_path,
            token_path,
        })
    }

    fn issue_ref(&self, issue_number: u64) -> String {
        format!("{}#{issue_number}", self.repo_ref)
    }

    fn issue_api_path(&self, issue_number: u64) -> String {
        format!(
            "/api/v1/repos/{}/{}/issues/{issue_number}",
            self.fixture.owner, self.repo_name
        )
    }

    fn run_plain_timed(&self, step: &str, args: &[&str]) -> Result<String> {
        let start = Instant::now();
        let output = run_cli_plain(&self.config_path, &self.token_path, args)?;
        self.timer.record(step, start)?;
        Ok(output)
    }

    fn run_json_timed(&self, step: &str, args: &[&str]) -> Result<Value> {
        let start = Instant::now();
        let output = run_cli_json(&self.config_path, &self.token_path, args)?;
        self.timer.record(step, start)?;
        Ok(output)
    }

    fn run_failure_timed(&self, step: &str, args: &[&str]) -> Result<Output> {
        let start = Instant::now();
        let output = run_cli_expect_failure(&self.config_path, &self.token_path, args)?;
        self.timer.record(step, start)?;
        Ok(output)
    }

    fn create_issue(&self, title: &str, body: &str, workflow: &str) -> Result<u64> {
        let issue = self.run_json_timed(
            "issue.create",
            &[
                "issue",
                "create",
                self.repo_ref.as_str(),
                "--title",
                title,
                "--body",
                body,
                "--workflow",
                workflow,
                "--json",
            ],
        )?;
        json_u64_field(&issue, "number")
    }

    fn get_issue(&self, issue_number: u64) -> Result<Value> {
        let start = Instant::now();
        let issue = self
            .fixture
            .authed_get(&self.issue_api_path(issue_number))?;
        self.timer.record("issue.verify_read_back", start)?;
        Ok(issue)
    }

    fn list_open_issues(&self) -> Result<Vec<Value>> {
        let start = Instant::now();
        let payload = self.fixture.authed_get(&format!(
            "/api/v1/repos/{}/{}/issues?state=open&limit=100",
            self.fixture.owner, self.repo_name
        ))?;
        self.timer.record("issues.list_open", start)?;
        let issues = payload
            .as_array()
            .ok_or_else(|| anyhow!("list open issues payload was not an array"))?;
        Ok(issues.clone())
    }
}

#[derive(Debug, Clone, Copy)]
enum OrchdTestDirective {
    Poke,
    Reply,
    Impl,
}

impl OrchdTestDirective {
    const fn as_str(self) -> &'static str {
        match self {
            Self::Poke => "poke",
            Self::Reply => "reply",
            Self::Impl => "impl",
        }
    }

    const fn prompt_file(self) -> &'static str {
        match self {
            Self::Poke | Self::Reply => "orchd-poke.md",
            Self::Impl => "orchd-impl.md",
        }
    }
}

struct OrchdDispatchTomlInputs<'a> {
    actor: &'a str,
    forgejo_login: &'a str,
    repo_ref: &'a str,
    principal_workdir: &'a Path,
    codex_bin: &'a Path,
    token_file: &'a Path,
    forgejoctl: &'a Path,
    directives: &'a [OrchdTestDirective],
    timeout_sec: u64,
}

fn write_orchd_dispatch_toml(path: &Path, inputs: OrchdDispatchTomlInputs<'_>) -> Result<()> {
    use std::fmt::Write as _;

    let prompts_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("prompts");
    let preamble = prompts_dir.join("orchd-preamble.md");
    let fresh_env = prompts_dir.join("orchd-envelope-fresh.md");
    let follow_env = prompts_dir.join("orchd-envelope-followup.md");
    let turn_context = prompts_dir.join("orchd-turn-context.md");
    let issue_fresh = prompts_dir.join("orchd-issue-fresh.md");
    let issue_followup = prompts_dir.join("orchd-issue-followup.md");

    let mut out = String::new();
    writeln!(&mut out, "version = 1")?;
    writeln!(&mut out, "allowed_actors = [\"{}\"]", inputs.actor)?;
    writeln!(
        &mut out,
        "forgejoctl_bin = \"{}\"\n",
        inputs.forgejoctl.display()
    )?;

    writeln!(&mut out, "[prompt_envelopes]")?;
    writeln!(&mut out, "preamble_file = \"{}\"", preamble.display())?;
    writeln!(&mut out, "fresh_envelope = \"{}\"", fresh_env.display())?;
    writeln!(&mut out, "followup_envelope = \"{}\"", follow_env.display())?;
    writeln!(
        &mut out,
        "turn_context_file = \"{}\"",
        turn_context.display()
    )?;
    writeln!(&mut out, "issue_fresh_file = \"{}\"", issue_fresh.display())?;
    writeln!(
        &mut out,
        "issue_followup_file = \"{}\"\n",
        issue_followup.display()
    )?;

    writeln!(&mut out, "[roles.codex-orch]")?;
    writeln!(&mut out, "codex_bin = \"{}\"", inputs.codex_bin.display())?;
    writeln!(&mut out, "codex_role_arg = \"orch\"")?;
    writeln!(&mut out, "forgejo_login = \"{}\"", inputs.forgejo_login)?;
    writeln!(&mut out, "token_file = \"{}\"", inputs.token_file.display())?;
    writeln!(&mut out)?;

    writeln!(&mut out, "[[repo_bindings]]")?;
    writeln!(&mut out, "repo = \"{}\"", inputs.repo_ref)?;
    writeln!(
        &mut out,
        "local_path = \"{}\"",
        inputs.principal_workdir.display()
    )?;
    writeln!(&mut out, "git_remote = \"origin\"")?;
    writeln!(&mut out, "git_base = \"main\"")?;
    writeln!(&mut out)?;

    for directive in inputs.directives {
        let prompt = prompts_dir.join(directive.prompt_file());
        writeln!(&mut out, "[directives.{}]", directive.as_str())?;
        writeln!(&mut out, "role = \"codex-orch\"")?;
        writeln!(&mut out, "prompt_file = \"{}\"", prompt.display())?;
        writeln!(&mut out, "timeout_sec = {}\n", inputs.timeout_sec)?;
    }

    fs::write(path, out).with_context(|| format!("failed writing {}", path.display()))?;
    Ok(())
}

fn post_orchd_issue_comment_webhook_with_issue(
    client: &Client,
    orchd_base_url: &str,
    repo_ref: &str,
    issue_payload: Value,
    actor: &str,
    comment_body: &str,
) -> Result<Value> {
    let delivery_id = format!("itest-webhook-{}", unique_suffix()?);
    let webhook_body = serde_json::json!({
        "action": "created",
        "repository": { "full_name": repo_ref },
        "issue": issue_payload,
        "comment": { "body": comment_body, "user": { "login": actor } },
        "sender": { "login": actor },
    });

    let webhook_resp = client
        .post(format!("{orchd_base_url}/webhook"))
        .header("Content-Type", "application/json")
        .header("X-Forgejo-Event", "issue_comment")
        .header("X-Forgejo-Delivery", delivery_id)
        .body(webhook_body.to_string())
        .send()
        .context("failed POSTing webhook to orchd")?;
    let webhook_status = webhook_resp.status();
    let body = webhook_resp.text().unwrap_or_default();
    if webhook_status != StatusCode::ACCEPTED && webhook_status != StatusCode::OK {
        bail!("orchd webhook returned {} body={body}", webhook_status);
    }
    serde_json::from_str(&body)
        .with_context(|| format!("orchd webhook response was not JSON: {body}"))
}

fn post_orchd_issue_comment_webhook(
    client: &Client,
    orchd_base_url: &str,
    repo_ref: &str,
    issue_number: u64,
    actor: &str,
    comment_body: &str,
) -> Result<()> {
    let _ = post_orchd_issue_comment_webhook_with_issue(
        client,
        orchd_base_url,
        repo_ref,
        serde_json::json!({ "number": issue_number }),
        actor,
        comment_body,
    )?;
    Ok(())
}

fn wait_for_issue_label(
    harness: &LiveHarness,
    issue_number: u64,
    label: &str,
    timeout: Duration,
) -> Result<Value> {
    let deadline = Instant::now() + timeout;
    loop {
        let issue = harness.get_issue(issue_number)?;
        if issue_has_label(&issue, label)? {
            return Ok(issue);
        }
        if Instant::now() >= deadline {
            bail!(
                "timed out waiting for label {label} on issue {} labels={:?}",
                harness.issue_ref(issue_number),
                issue_label_names(&issue)?
            );
        }
        thread::sleep(Duration::from_millis(200));
    }
}

#[derive(Debug)]
struct OrchdTestProcess {
    base_url: String,
    client: Client,
    _guard: ChildGuard,
}

struct OrchdSpawnInputs<'a> {
    listen_port: u16,
    db_path: &'a Path,
    repo_ref: &'a str,
    dispatch_cfg_path: &'a Path,
    config_path: &'a Path,
    token_path: &'a Path,
    stdout_path: &'a Path,
    stderr_path: &'a Path,
    env: &'a [(&'a str, &'a str)],
}

impl OrchdTestProcess {
    fn spawn(inputs: OrchdSpawnInputs<'_>) -> Result<Self> {
        let base_url = format!("http://127.0.0.1:{}", inputs.listen_port);
        let listen = format!("127.0.0.1:{}", inputs.listen_port);

        let stdout = fs::File::create(inputs.stdout_path)
            .with_context(|| format!("failed creating {}", inputs.stdout_path.display()))?;
        let stderr = fs::File::create(inputs.stderr_path)
            .with_context(|| format!("failed creating {}", inputs.stderr_path.display()))?;

        let mut cmd = Command::new(orchd_bin()?);
        cmd.arg("--listen")
            .arg(&listen)
            .arg("--db-path")
            .arg(inputs.db_path)
            .arg("--reconcile-repo")
            .arg(inputs.repo_ref)
            .arg("--heartbeat-sec")
            .arg("1")
            .arg("--reconcile-sec")
            .arg("1")
            .arg("--dispatch-mode")
            .arg("exec")
            .arg("--dispatch-backend")
            .arg("local")
            .arg("--dispatch-config")
            .arg(inputs.dispatch_cfg_path)
            .arg("--config")
            .arg(inputs.config_path)
            .arg("--token-file")
            .arg(inputs.token_path)
            .stdout(Stdio::from(stdout))
            .stderr(Stdio::from(stderr));

        for (k, v) in inputs.env {
            cmd.env(k, v);
        }

        let orchd = cmd.spawn().context("failed spawning orchd")?;
        let guard = ChildGuard(orchd);

        let client = Client::builder()
            .timeout(Duration::from_secs(3))
            .build()
            .context("failed to build orchd HTTP client")?;

        let deadline = Instant::now() + Duration::from_secs(30);
        loop {
            if Instant::now() > deadline {
                let stdout = fs::read_to_string(inputs.stdout_path).unwrap_or_default();
                let stderr = fs::read_to_string(inputs.stderr_path).unwrap_or_default();
                bail!("orchd did not become ready\nstdout:\n{stdout}\nstderr:\n{stderr}");
            }
            if let Ok(resp) = client.get(format!("{base_url}/healthz")).send()
                && resp.status() == StatusCode::OK
            {
                break;
            }
            thread::sleep(Duration::from_millis(150));
        }

        Ok(Self {
            base_url,
            client,
            _guard: guard,
        })
    }
}

#[test]
#[serial(live_forgejo)]
fn live_smoke_then_add_issue_round_trip() -> Result<()> {
    if !live_tests_enabled() {
        eprintln!(
            "skipping live Forgejo integration test; enable with {}=1",
            LIVE_TESTS_ENV
        );
        return Ok(());
    }

    let harness = LiveHarness::bootstrap("live_smoke_then_add_issue_round_trip")?;

    let create_output = harness.run_json_timed(
        "issue.create",
        &[
            "issue",
            "create",
            harness.repo_ref.as_str(),
            "--title",
            "itest title",
            "--body",
            "itest body",
            "--workflow",
            "triage",
            "--json",
        ],
    )?;

    let issue_number = json_u64_field(&create_output, "number")?;
    let issue_title = json_str_field(&create_output, "title")?;
    if issue_title != "itest title" {
        bail!("issue create returned unexpected title: {issue_title}");
    }

    let read_back = harness.get_issue(issue_number)?;
    let read_back_title = json_str_field(&read_back, "title")?;
    if read_back_title != "itest title" {
        bail!("read-back issue title mismatch: {read_back_title}");
    }

    let read_back_body = json_str_field(&read_back, "body")?;
    if read_back_body != "itest body" {
        bail!("read-back issue body mismatch: {read_back_body}");
    }

    let read_back_state = json_str_field(&read_back, "state")?;
    if read_back_state != "open" {
        bail!("read-back issue state mismatch: expected open, got {read_back_state}");
    }

    let author_login = read_back
        .get("user")
        .and_then(Value::as_object)
        .and_then(|user| user.get("login"))
        .and_then(Value::as_str)
        .ok_or_else(|| anyhow!("read-back issue JSON missing author login"))?;
    if author_login != harness.fixture.owner {
        bail!(
            "read-back author mismatch: expected {}, got {author_login}",
            harness.fixture.owner
        );
    }

    Ok(())
}

#[test]
#[serial(live_forgejo)]
fn live_issue_claim_release_and_transition_round_trip() -> Result<()> {
    if !live_tests_enabled() {
        eprintln!(
            "skipping live Forgejo integration test; enable with {}=1",
            LIVE_TESTS_ENV
        );
        return Ok(());
    }

    let harness = LiveHarness::bootstrap("live_issue_claim_release_and_transition_round_trip")?;
    let issue_number = harness.create_issue("claim lifecycle", "claim lifecycle body", "ready")?;
    let issue_ref = harness.issue_ref(issue_number);

    let claim_output = harness.run_plain_timed(
        "issue.claim",
        &["issue", "claim", &issue_ref, "--agent", "codex-a"],
    )?;
    ensure_contains(&claim_output, "claimed:", "claim output")?;

    let claimed = harness.get_issue(issue_number)?;
    if !issue_has_label(&claimed, "state/in-progress")? {
        bail!("expected state/in-progress after claim");
    }
    if !issue_has_label(&claimed, "claimed/codex-a")? {
        bail!("expected claimed/codex-a label after claim");
    }

    let conflict = harness.run_failure_timed(
        "issue.claim_non_ready_rejected",
        &["issue", "claim", &issue_ref, "--agent", "codex-b"],
    )?;
    let conflict_stderr = decode_output_stderr(&conflict)?;
    ensure_contains(&conflict_stderr, "is not ready", "non-ready claim stderr")?;

    let release_output = harness.run_plain_timed(
        "issue.release",
        &["issue", "release", &issue_ref, "--agent", "codex-a"],
    )?;
    ensure_contains(&release_output, "released:", "release output")?;

    let released = harness.get_issue(issue_number)?;
    if issue_has_label(&released, "claimed/codex-a")? {
        bail!("claim label should be removed after release");
    }
    if !issue_has_label(&released, "state/ready")? {
        bail!("expected state/ready after release");
    }

    let illegal_transition = harness.run_failure_timed(
        "issue.transition_illegal",
        &["issue", "transition", &issue_ref, "--to", "review"],
    )?;
    let illegal_stderr = decode_output_stderr(&illegal_transition)?;
    ensure_contains(
        &illegal_stderr,
        "illegal workflow transition",
        "illegal transition stderr",
    )?;

    let to_in_progress = harness.run_plain_timed(
        "issue.transition_in_progress",
        &["issue", "transition", &issue_ref, "--to", "in-progress"],
    )?;
    ensure_contains(&to_in_progress, "transitioned:", "transition output")?;

    let to_review = harness.run_plain_timed(
        "issue.transition_review",
        &["issue", "transition", &issue_ref, "--to", "review"],
    )?;
    ensure_contains(&to_review, "transitioned:", "transition output")?;

    let close_output = harness.run_plain_timed("issue.close", &["issue", "close", &issue_ref])?;
    ensure_contains(&close_output, "closed:", "close output")?;

    let closed_issue = harness.get_issue(issue_number)?;
    let closed_state = json_str_field(&closed_issue, "state")?;
    if closed_state != "closed" {
        bail!("expected closed issue state, got {closed_state}");
    }

    let reopen_output = harness.run_plain_timed(
        "issue.reopen",
        &["issue", "reopen", &issue_ref, "--workflow", "triage"],
    )?;
    ensure_contains(&reopen_output, "reopened:", "reopen output")?;

    let reopened = harness.get_issue(issue_number)?;
    let reopened_state = json_str_field(&reopened, "state")?;
    if reopened_state != "open" {
        bail!("expected open issue state after reopen, got {reopened_state}");
    }
    if !issue_has_label(&reopened, "state/triage")? {
        bail!("expected state/triage after reopen");
    }

    Ok(())
}

#[test]
#[serial(live_forgejo)]
fn live_issue_blocker_and_orchd_state_round_trip() -> Result<()> {
    if !live_tests_enabled() {
        eprintln!(
            "skipping live Forgejo integration test; enable with {}=1",
            LIVE_TESTS_ENV
        );
        return Ok(());
    }

    let harness = LiveHarness::bootstrap("live_issue_blocker_and_orchd_state_round_trip")?;
    let parent_issue_number = harness.create_issue("parent issue", "parent body", "ready")?;
    let parent_issue_ref = harness.issue_ref(parent_issue_number);

    let blocker_output = harness.run_plain_timed(
        "issue.blocker",
        &[
            "issue",
            "blocker",
            &parent_issue_ref,
            "--title",
            "Need upstream design",
            "--body",
            "blocked on upstream discussion",
        ],
    )?;
    ensure_contains(&blocker_output, "blocker:", "blocker output")?;

    let parent = harness.get_issue(parent_issue_number)?;
    if !issue_has_label(&parent, "state/blocked")? {
        bail!("expected parent issue to transition to state/blocked");
    }

    let open_issues = harness.list_open_issues()?;
    let blocker_issue = open_issues
        .iter()
        .find(|issue| {
            issue
                .get("title")
                .and_then(Value::as_str)
                .is_some_and(|title| title == "[BLOCKER] Need upstream design")
        })
        .ok_or_else(|| anyhow!("failed to locate blocker issue in list output"))?;
    let blocker_number = json_u64_field(blocker_issue, "number")?;
    let blocker_details = harness.get_issue(blocker_number)?;
    if !issue_has_label(&blocker_details, "type/blocker")? {
        bail!("expected blocker issue to have type/blocker label");
    }
    if !issue_has_label(&blocker_details, "state/triage")? {
        bail!("expected blocker issue to start in state/triage");
    }

    let orchd_queued = harness.run_plain_timed(
        "issue.orchd_state_queued",
        &["issue", "orchd-state", &parent_issue_ref, "--to", "queued"],
    )?;
    ensure_contains(&orchd_queued, "orchd-state:", "orchd queued output")?;

    let parent_queued = harness.get_issue(parent_issue_number)?;
    if !issue_has_label(&parent_queued, "orchd/state/queued")? {
        bail!("expected orchd/state/queued after orchd-state transition");
    }
    if issue_label_prefix_count(&parent_queued, "orchd/state/")? != 1 {
        bail!("expected exactly one orchd/state/* label after queued transition");
    }

    let orchd_running = harness.run_plain_timed(
        "issue.orchd_state_running",
        &["issue", "orchd-state", &parent_issue_ref, "--to", "running"],
    )?;
    ensure_contains(&orchd_running, "orchd-state:", "orchd running output")?;

    let parent_running = harness.get_issue(parent_issue_number)?;
    if !issue_has_label(&parent_running, "orchd/state/running")? {
        bail!("expected orchd/state/running after orchd-state transition");
    }
    if issue_has_label(&parent_running, "orchd/state/queued")? {
        bail!("orchd/state/queued should be replaced by orchd/state/running");
    }
    if issue_label_prefix_count(&parent_running, "orchd/state/")? != 1 {
        bail!("expected exactly one orchd/state/* label after running transition");
    }

    Ok(())
}

#[test]
#[serial(live_forgejo)]
fn live_orchd_local_backend_smoke() -> Result<()> {
    if !live_tests_enabled() {
        eprintln!(
            "skipping live orchd integration test; enable with {}=1",
            LIVE_TESTS_ENV
        );
        return Ok(());
    }

    let harness = LiveHarness::bootstrap("live_orchd_local_backend_smoke")?;

    let issue_number = harness.create_issue("orchd smoke", "issue body", "ready")?;

    let dispatch_cfg_path = harness.fixture.work_path.join("orchd-dispatch.toml");
    let db_path = harness.fixture.work_path.join("orchd.sqlite");
    let orchd_stdout = harness.fixture.work_path.join("orchd-stdout.log");
    let orchd_stderr = harness.fixture.work_path.join("orchd-stderr.log");

    let fake_codex = ensure_fake_codex_bin()?;
    let forgejoctl = forgejo_agent_bin()?;

    write_orchd_dispatch_toml(
        &dispatch_cfg_path,
        OrchdDispatchTomlInputs {
            actor: harness.fixture.owner.as_str(),
            forgejo_login: harness.fixture.owner.as_str(),
            repo_ref: harness.repo_ref.as_str(),
            principal_workdir: &harness.principal_workdir,
            codex_bin: &fake_codex,
            token_file: &harness.token_path,
            forgejoctl: &forgejoctl,
            directives: &[OrchdTestDirective::Poke],
            timeout_sec: 10,
        },
    )?;

    let port = pick_unused_port().ok_or_else(|| anyhow!("failed to pick unused port"))?;
    let orchd = OrchdTestProcess::spawn(OrchdSpawnInputs {
        listen_port: port,
        db_path: &db_path,
        repo_ref: &harness.repo_ref,
        dispatch_cfg_path: &dispatch_cfg_path,
        config_path: &harness.config_path,
        token_path: &harness.token_path,
        stdout_path: &orchd_stdout,
        stderr_path: &orchd_stderr,
        env: &[],
    })?;

    post_orchd_issue_comment_webhook(
        &orchd.client,
        &orchd.base_url,
        &harness.repo_ref,
        issue_number,
        harness.fixture.owner.as_str(),
        "@codex-orch poke",
    )?;

    let _ = wait_for_issue_label(
        &harness,
        issue_number,
        "orchd/state/completed",
        Duration::from_secs(30),
    )?;

    Ok(())
}

#[test]
#[serial(live_forgejo)]
fn live_orchd_reply_autodispatches_to_assignee() -> Result<()> {
    if !live_tests_enabled() {
        eprintln!(
            "skipping live orchd integration test; enable with {}=1",
            LIVE_TESTS_ENV
        );
        return Ok(());
    }

    let harness = LiveHarness::bootstrap("live_orchd_reply_autodispatches_to_assignee")?;

    let issue_number = harness.create_issue("orchd reply", "issue body", "ready")?;

    let dispatch_cfg_path = harness.fixture.work_path.join("orchd-dispatch-reply.toml");
    let db_path = harness.fixture.work_path.join("orchd-reply.sqlite");
    let orchd_stdout = harness.fixture.work_path.join("orchd-reply-stdout.log");
    let orchd_stderr = harness.fixture.work_path.join("orchd-reply-stderr.log");

    let fake_codex = ensure_fake_codex_bin()?;
    let forgejoctl = forgejo_agent_bin()?;

    write_orchd_dispatch_toml(
        &dispatch_cfg_path,
        OrchdDispatchTomlInputs {
            // Deliberately *not* the real actor: reply is expected to bypass allowlist.
            actor: "nobody",
            forgejo_login: harness.fixture.owner.as_str(),
            repo_ref: harness.repo_ref.as_str(),
            principal_workdir: &harness.principal_workdir,
            codex_bin: &fake_codex,
            token_file: &harness.token_path,
            forgejoctl: &forgejoctl,
            directives: &[OrchdTestDirective::Reply],
            timeout_sec: 10,
        },
    )?;

    let port = pick_unused_port().ok_or_else(|| anyhow!("failed to pick unused port"))?;
    let orchd = OrchdTestProcess::spawn(OrchdSpawnInputs {
        listen_port: port,
        db_path: &db_path,
        repo_ref: &harness.repo_ref,
        dispatch_cfg_path: &dispatch_cfg_path,
        config_path: &harness.config_path,
        token_path: &harness.token_path,
        stdout_path: &orchd_stdout,
        stderr_path: &orchd_stderr,
        env: &[],
    })?;

    if orchd_dispatch_count(&db_path)? != 0 {
        bail!("expected empty dispatch table at test start");
    }

    let non_codex = post_orchd_issue_comment_webhook_with_issue(
        &orchd.client,
        &orchd.base_url,
        &harness.repo_ref,
        serde_json::json!({
            "number": issue_number,
            "assignees": [{ "login": harness.fixture.owner.as_str() }],
        }),
        "random-human",
        "hello",
    )?;
    if json_str_field(&non_codex, "decision")? != "ignored" {
        bail!("expected ignored decision for non-codex assignee: {non_codex}");
    }
    if orchd_dispatch_count(&db_path)? != 0 {
        bail!("expected no dispatch for non-codex assignee");
    }

    let self_comment = post_orchd_issue_comment_webhook_with_issue(
        &orchd.client,
        &orchd.base_url,
        &harness.repo_ref,
        serde_json::json!({
            "number": issue_number,
            "assignees": [{ "login": "codex-orch" }],
        }),
        "codex-orch",
        "hello",
    )?;
    if json_str_field(&self_comment, "decision")? != "ignored" {
        bail!("expected ignored decision for assignee self-comment: {self_comment}");
    }
    if orchd_dispatch_count(&db_path)? != 0 {
        bail!("expected no dispatch for assignee self-comment");
    }

    let multi = post_orchd_issue_comment_webhook_with_issue(
        &orchd.client,
        &orchd.base_url,
        &harness.repo_ref,
        serde_json::json!({
            "number": issue_number,
            "assignees": [{ "login": "codex-orch" }, { "login": "codex-dev" }],
        }),
        "random-human",
        "hello",
    )?;
    if json_str_field(&multi, "decision")? != "ignored" {
        bail!("expected ignored decision for multi-assignee: {multi}");
    }
    if orchd_dispatch_count(&db_path)? != 0 {
        bail!("expected no dispatch for multi-assignee");
    }

    let reply = post_orchd_issue_comment_webhook_with_issue(
        &orchd.client,
        &orchd.base_url,
        &harness.repo_ref,
        serde_json::json!({
            "number": issue_number,
            "assignees": [{ "login": "codex-orch" }],
        }),
        harness.fixture.owner.as_str(),
        "please take this",
    )?;
    if json_str_field(&reply, "decision")? != "accepted" {
        bail!("expected accepted decision for assignee reply: {reply}");
    }
    if json_str_field(&reply, "reason_code")? != "assignee_reply" {
        bail!("expected assignee_reply reason for assignee reply: {reply}");
    }

    let _ = wait_for_issue_label(
        &harness,
        issue_number,
        "orchd/state/completed",
        Duration::from_secs(30),
    )?;

    let dispatch =
        orchd_latest_dispatch_directive_and_role(&db_path, &harness.repo_ref, issue_number)?
            .ok_or_else(|| anyhow!("expected a dispatch row after reply"))?;
    if dispatch.0 != "reply" || dispatch.1 != "codex-orch" {
        bail!("unexpected dispatch directive/role: {:?}", dispatch);
    }

    Ok(())
}

#[test]
#[serial(live_forgejo)]
fn live_orchd_prompt_template_failure_marks_failed_start() -> Result<()> {
    if !live_tests_enabled() {
        eprintln!(
            "skipping live orchd integration test; enable with {}=1",
            LIVE_TESTS_ENV
        );
        return Ok(());
    }

    let harness = LiveHarness::bootstrap("live_orchd_prompt_template_failure_marks_failed_start")?;
    let issue_number = harness.create_issue("orchd bad prompt", "issue body", "ready")?;

    let dispatch_cfg_path = harness
        .fixture
        .work_path
        .join("orchd-dispatch-bad-template.toml");
    let db_path = harness.fixture.work_path.join("orchd-bad-template.sqlite");
    let orchd_stdout = harness
        .fixture
        .work_path
        .join("orchd-bad-template-stdout.log");
    let orchd_stderr = harness
        .fixture
        .work_path
        .join("orchd-bad-template-stderr.log");

    let fake_codex = ensure_fake_codex_bin()?;
    let forgejoctl = forgejo_agent_bin()?;

    write_orchd_dispatch_toml(
        &dispatch_cfg_path,
        OrchdDispatchTomlInputs {
            actor: "nobody",
            forgejo_login: harness.fixture.owner.as_str(),
            repo_ref: harness.repo_ref.as_str(),
            principal_workdir: &harness.principal_workdir,
            codex_bin: &fake_codex,
            token_file: &harness.token_path,
            forgejoctl: &forgejoctl,
            directives: &[OrchdTestDirective::Reply],
            timeout_sec: 10,
        },
    )?;

    let bad_fresh = harness
        .fixture
        .work_path
        .join("orchd-bad-fresh-envelope.md");
    fs::write(
        &bad_fresh,
        "## Broken\n{{dispatch_md}}\n{{orders_md}}\n{{issue_md}}\n{{role_card_md}}\n",
    )
    .with_context(|| format!("failed writing {}", bad_fresh.display()))?;

    let original_cfg = fs::read_to_string(&dispatch_cfg_path)
        .with_context(|| format!("failed reading {}", dispatch_cfg_path.display()))?;
    let mut rewritten_cfg = String::new();
    for line in original_cfg.lines() {
        if line.trim_start().starts_with("fresh_envelope = ") {
            writeln!(
                &mut rewritten_cfg,
                "fresh_envelope = \"{}\"",
                bad_fresh.display()
            )
            .map_err(|err| anyhow!("failed to format rewritten config: {err}"))?;
        } else {
            rewritten_cfg.push_str(line);
            rewritten_cfg.push('\n');
        }
    }
    fs::write(&dispatch_cfg_path, rewritten_cfg)
        .with_context(|| format!("failed rewriting {}", dispatch_cfg_path.display()))?;

    let port = pick_unused_port().ok_or_else(|| anyhow!("failed to pick unused port"))?;
    let orchd = OrchdTestProcess::spawn(OrchdSpawnInputs {
        listen_port: port,
        db_path: &db_path,
        repo_ref: &harness.repo_ref,
        dispatch_cfg_path: &dispatch_cfg_path,
        config_path: &harness.config_path,
        token_path: &harness.token_path,
        stdout_path: &orchd_stdout,
        stderr_path: &orchd_stderr,
        env: &[],
    })?;

    let reply = post_orchd_issue_comment_webhook_with_issue(
        &orchd.client,
        &orchd.base_url,
        &harness.repo_ref,
        serde_json::json!({
            "number": issue_number,
            "assignees": [{ "login": "codex-orch" }],
        }),
        harness.fixture.owner.as_str(),
        "follow-up with no directive",
    )?;
    if json_str_field(&reply, "decision")? != "accepted" {
        bail!("expected accepted decision for assignee reply: {reply}");
    }

    let _ = wait_for_issue_label(
        &harness,
        issue_number,
        "orchd/state/failed",
        Duration::from_secs(30),
    )?;

    let status_reason =
        orchd_latest_dispatch_status_reason(&db_path, &harness.repo_ref, issue_number)?
            .ok_or_else(|| anyhow!("expected latest dispatch row for issue"))?;
    if status_reason.0 != "failed_start" {
        bail!(
            "expected failed_start terminal dispatch, got status={} reason={:?}",
            status_reason.0,
            status_reason.1
        );
    }
    if status_reason.1.as_deref() != Some("prompt_template_error") {
        bail!(
            "expected prompt_template_error reason code, got {:?}",
            status_reason.1
        );
    }

    let starting_count = orchd_starting_dispatch_count(&db_path, &harness.repo_ref, issue_number)?;
    if starting_count != 0 {
        bail!("expected no starting dispatch rows, found {starting_count}");
    }

    Ok(())
}

#[test]
#[serial(live_forgejo)]
fn live_orchd_impl_autoland_updates_remote_main() -> Result<()> {
    if !live_tests_enabled() {
        eprintln!(
            "skipping live orchd integration test; enable with {}=1",
            LIVE_TESTS_ENV
        );
        return Ok(());
    }

    let harness = LiveHarness::bootstrap("live_orchd_impl_autoland_updates_remote_main")?;
    let issue_number = harness.create_issue("orchd impl smoke", "issue body", "ready")?;

    let git = GitWorkspace::from_fixture(&harness.fixture, &harness.repo_name)?;
    let before_head = git.bare_head_main()?;
    let before_count = git.bare_commit_count_main()?;
    let backup_origin = harness
        .fixture
        .work_path
        .join(format!("{}-origin-backup.git", harness.repo_name));
    let mut init_backup = Command::new("git");
    init_backup.args(["init", "--bare"]).arg(&backup_origin);
    run_command_checked(&mut init_backup, "git init --bare backup origin remote")?;
    let backup_origin_str = backup_origin.to_string_lossy().into_owned();
    git_output_checked(
        &harness.principal_workdir,
        &["push", &backup_origin_str, "main:main"],
        "git push main to backup origin remote",
    )?;
    git_output_checked(
        &harness.principal_workdir,
        &["remote", "set-url", "origin", &backup_origin_str],
        "git remote set-url origin <backup>",
    )?;
    let forgejo_bare = harness.fixture.repo_git_dir(&harness.repo_name);
    let forgejo_bare_str = forgejo_bare.to_string_lossy().into_owned();
    git_output_checked(
        &harness.principal_workdir,
        &["remote", "add", "forgejo", &forgejo_bare_str],
        "git remote add forgejo <fixture bare>",
    )?;
    git_output_checked(
        &harness.principal_workdir,
        &["fetch", "origin", "main"],
        "git fetch origin main (backup remote)",
    )?;

    let fake_codex = ensure_fake_codex_bin()?;
    let forgejoctl = forgejo_agent_bin()?;

    let dispatch_cfg_path = harness.fixture.work_path.join("orchd-dispatch-impl.toml");
    write_orchd_dispatch_toml(
        &dispatch_cfg_path,
        OrchdDispatchTomlInputs {
            actor: harness.fixture.owner.as_str(),
            forgejo_login: harness.fixture.owner.as_str(),
            repo_ref: harness.repo_ref.as_str(),
            principal_workdir: &harness.principal_workdir,
            codex_bin: &fake_codex,
            token_file: &harness.token_path,
            forgejoctl: &forgejoctl,
            directives: &[OrchdTestDirective::Impl],
            timeout_sec: 30,
        },
    )?;

    let db_path = harness.fixture.work_path.join("orchd-impl.sqlite");
    let orchd_stdout = harness.fixture.work_path.join("orchd-impl-stdout.log");
    let orchd_stderr = harness.fixture.work_path.join("orchd-impl-stderr.log");
    let port = pick_unused_port().ok_or_else(|| anyhow!("failed to pick unused port"))?;

    let orchd = OrchdTestProcess::spawn(OrchdSpawnInputs {
        listen_port: port,
        db_path: &db_path,
        repo_ref: &harness.repo_ref,
        dispatch_cfg_path: &dispatch_cfg_path,
        config_path: &harness.config_path,
        token_path: &harness.token_path,
        stdout_path: &orchd_stdout,
        stderr_path: &orchd_stderr,
        env: &[
            ("FAKE_CODEX_MODE", "git_append_commit"),
            ("FAKE_CODEX_GIT_FILE", "orchd-itest.txt"),
        ],
    })?;

    post_orchd_issue_comment_webhook(
        &orchd.client,
        &orchd.base_url,
        &harness.repo_ref,
        issue_number,
        harness.fixture.owner.as_str(),
        "@codex-orch impl",
    )?;

    let _ = wait_for_issue_label(
        &harness,
        issue_number,
        "orchd/state/completed",
        Duration::from_secs(60),
    )?;

    let after_head = git.bare_head_main()?;
    let after_count = git.bare_commit_count_main()?;
    if after_head == before_head {
        bail!("expected autoland to advance main, but head stayed at {after_head}");
    }
    if after_count != before_count + 1 {
        bail!("expected main commit count to increase by 1 ({before_count} -> {after_count})");
    }
    let principal_head = stdout_trim(&git_output_checked(
        &harness.principal_workdir,
        &["rev-parse", "HEAD"],
        "git rev-parse HEAD (principal workspace)",
    )?)?;
    if principal_head != after_head {
        bail!(
            "expected principal workspace to sync to landed commit (principal={principal_head} remote={after_head})"
        );
    }
    let origin_head = stdout_trim(&git_output_checked(
        &harness.principal_workdir,
        &["rev-parse", "origin/main"],
        "git rev-parse origin/main (backup remote)",
    )?)?;
    if origin_head == after_head {
        bail!(
            "expected principal sync source to be Forgejo, but principal landed on origin/main (origin={origin_head} landed={after_head})"
        );
    }

    let final_issue = harness.get_issue(issue_number)?;
    if !issue_has_label(&final_issue, "state/review")? {
        bail!("expected impl success to transition issue to state/review");
    }
    if issue_label_prefix_count(&final_issue, "orchd/state/")? != 1 {
        bail!("expected exactly one orchd/state/* label after impl completion");
    }

    Ok(())
}

#[test]
#[serial(live_forgejo)]
fn live_orchd_impl_allows_parallel_dispatches_per_repo() -> Result<()> {
    if !live_tests_enabled() {
        eprintln!(
            "skipping live orchd integration test; enable with {}=1",
            LIVE_TESTS_ENV
        );
        return Ok(());
    }

    let harness = LiveHarness::bootstrap("live_orchd_impl_allows_parallel_dispatches_per_repo")?;
    let issue1 = harness.create_issue("orchd impl a", "issue body", "ready")?;
    let issue2 = harness.create_issue("orchd impl b", "issue body", "ready")?;

    let git = GitWorkspace::from_fixture(&harness.fixture, &harness.repo_name)?;
    let before_count = git.bare_commit_count_main()?;

    let fake_codex = ensure_fake_codex_bin()?;
    let forgejoctl = forgejo_agent_bin()?;

    let dispatch_cfg_path = harness.fixture.work_path.join("orchd-dispatch-queue.toml");
    write_orchd_dispatch_toml(
        &dispatch_cfg_path,
        OrchdDispatchTomlInputs {
            actor: harness.fixture.owner.as_str(),
            forgejo_login: harness.fixture.owner.as_str(),
            repo_ref: harness.repo_ref.as_str(),
            principal_workdir: &harness.principal_workdir,
            codex_bin: &fake_codex,
            token_file: &harness.token_path,
            forgejoctl: &forgejoctl,
            directives: &[OrchdTestDirective::Impl],
            timeout_sec: 60,
        },
    )?;

    let db_path = harness.fixture.work_path.join("orchd-queue.sqlite");
    let orchd_stdout = harness.fixture.work_path.join("orchd-queue-stdout.log");
    let orchd_stderr = harness.fixture.work_path.join("orchd-queue-stderr.log");
    let port = pick_unused_port().ok_or_else(|| anyhow!("failed to pick unused port"))?;

    let orchd = OrchdTestProcess::spawn(OrchdSpawnInputs {
        listen_port: port,
        db_path: &db_path,
        repo_ref: &harness.repo_ref,
        dispatch_cfg_path: &dispatch_cfg_path,
        config_path: &harness.config_path,
        token_path: &harness.token_path,
        stdout_path: &orchd_stdout,
        stderr_path: &orchd_stderr,
        env: &[
            ("FAKE_CODEX_MODE", "git_append_commit"),
            ("FAKE_CODEX_GIT_FILE", "orchd-itest.txt"),
            ("FAKE_CODEX_SLEEP_MS", "1500"),
        ],
    })?;

    post_orchd_issue_comment_webhook(
        &orchd.client,
        &orchd.base_url,
        &harness.repo_ref,
        issue1,
        harness.fixture.owner.as_str(),
        "@codex-orch impl",
    )?;

    let _ = wait_for_issue_label(
        &harness,
        issue1,
        "orchd/state/running",
        Duration::from_secs(30),
    )?;

    post_orchd_issue_comment_webhook(
        &orchd.client,
        &orchd.base_url,
        &harness.repo_ref,
        issue2,
        harness.fixture.owner.as_str(),
        "@codex-orch impl",
    )?;

    let _ = wait_for_issue_label(
        &harness,
        issue2,
        "orchd/state/running",
        Duration::from_secs(30),
    )?;

    let _ = wait_for_issue_label(
        &harness,
        issue1,
        "orchd/state/completed",
        Duration::from_secs(90),
    )?;

    let _ = wait_for_issue_label(
        &harness,
        issue2,
        "orchd/state/blocked",
        Duration::from_secs(90),
    )?;

    // Live harness posts synthetic webhooks directly to orchd. In production,
    // the autoland-conflict retry comment would trigger this follow-up webhook.
    post_orchd_issue_comment_webhook(
        &orchd.client,
        &orchd.base_url,
        &harness.repo_ref,
        issue2,
        harness.fixture.owner.as_str(),
        "@codex-orch impl",
    )?;

    let _ = wait_for_issue_label(
        &harness,
        issue2,
        "orchd/state/completed",
        Duration::from_secs(90),
    )?;

    let issue2_dispatch_statuses =
        orchd_issue_dispatch_statuses(&db_path, &harness.repo_ref, issue2)?;
    if !issue2_dispatch_statuses
        .iter()
        .any(|status| status == "blocked")
    {
        bail!(
            "expected parallel impl conflict retry path to produce a blocked dispatch before recovery; statuses={issue2_dispatch_statuses:?}"
        );
    }
    if issue2_dispatch_statuses.last().map(String::as_str) != Some("completed") {
        bail!(
            "expected final dispatch status for issue2 to be completed; statuses={issue2_dispatch_statuses:?}"
        );
    }

    let after_count = git.bare_commit_count_main()?;
    if after_count != before_count + 2 {
        bail!(
            "expected two parallel impl runs to autoland two commits ({before_count} -> {after_count})"
        );
    }
    let principal_count = stdout_trim(&git_output_checked(
        &harness.principal_workdir,
        &["rev-list", "--count", "HEAD"],
        "git rev-list --count HEAD (principal workspace)",
    )?)?
    .parse::<u64>()
    .context("principal commit count was not a u64")?;
    if principal_count != after_count {
        bail!(
            "principal workspace diverged from remote main count (principal_count={principal_count} remote_count={after_count})"
        );
    }

    Ok(())
}
