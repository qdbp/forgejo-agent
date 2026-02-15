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
use serde_json::Value;
use serial_test::serial;
use tempfile::TempDir;

const LIVE_TESTS_ENV: &str = "FORGEJO_LIVE_TESTS";
const TIMINGS_PATH_ENV: &str = "FORGEJO_LIVE_TIMINGS_PATH";
const KEEP_FIXTURE_ENV: &str = "FORGEJO_LIVE_KEEP_FIXTURE";
const FORGEJO_BIN_ENV: &str = "FORGEJO_BIN";

static TIMINGS_LOCK: LazyLock<Mutex<()>> = LazyLock::new(|| Mutex::new(()));

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

#[derive(Debug)]
struct LiveHarness {
    timer: StepTimer,
    fixture: ForgejoFixture,
    repo_name: String,
    repo_ref: String,
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

        Ok(Self {
            timer,
            fixture,
            repo_name,
            repo_ref,
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

    let fake_codex = fake_codex_bin().or_else(|_| {
        let status = Command::new("cargo")
            .current_dir(env!("CARGO_MANIFEST_DIR"))
            .args(["build", "--quiet", "--bin", "fake-codex"])
            .status()
            .context("failed to build fake-codex")?;
        if !status.success() {
            bail!("failed building fake-codex");
        }
        fake_codex_bin()
    })?;
    let forgejoctl = forgejo_agent_bin()?;

    let prompts_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("prompts");
    let dispatch_toml = format!(
        r#"
version = 1
allowed_actors = ["{actor}"]

[tmux]
session = "itest"
remain_on_exit = false

[prompt_envelopes]
fresh_envelope_file = "{fresh_env}"
followup_envelope_file = "{follow_env}"

[roles.codex-orch]
codex_bin = "{codex_bin}"
codex_role_arg = "orch"
token_file = "{token_file}"
workdir = "{workdir}"

[directives.poke]
role = "codex-orch"
prompt_file = "{poke_prompt}"
timeout_sec = 10

forgejoctl_bin = "{forgejoctl}"
"#,
        actor = harness.fixture.owner.as_str(),
        fresh_env = prompts_dir.join("orchd-envelope-fresh.md").display(),
        follow_env = prompts_dir.join("orchd-envelope-followup.md").display(),
        poke_prompt = prompts_dir.join("orchd-poke.md").display(),
        codex_bin = fake_codex.display(),
        token_file = harness.token_path.display(),
        workdir = env!("CARGO_MANIFEST_DIR"),
        forgejoctl = forgejoctl.display(),
    );
    fs::write(&dispatch_cfg_path, dispatch_toml)
        .with_context(|| format!("failed writing {}", dispatch_cfg_path.display()))?;

    let port = pick_unused_port().ok_or_else(|| anyhow!("failed to pick unused port"))?;
    let base_url = format!("http://127.0.0.1:{port}");
    let listen = format!("127.0.0.1:{port}");

    let stdout = fs::File::create(&orchd_stdout)
        .with_context(|| format!("failed creating {}", orchd_stdout.display()))?;
    let stderr = fs::File::create(&orchd_stderr)
        .with_context(|| format!("failed creating {}", orchd_stderr.display()))?;

    let orchd = Command::new(orchd_bin()?)
        .arg("--listen")
        .arg(&listen)
        .arg("--db-path")
        .arg(&db_path)
        .arg("--reconcile-repo")
        .arg(&harness.repo_ref)
        .arg("--dispatch-mode")
        .arg("tmux-exec")
        .arg("--dispatch-backend")
        .arg("local")
        .arg("--dispatch-config")
        .arg(&dispatch_cfg_path)
        .arg("--config")
        .arg(&harness.config_path)
        .arg("--token-file")
        .arg(&harness.token_path)
        .stdout(Stdio::from(stdout))
        .stderr(Stdio::from(stderr))
        .spawn()
        .context("failed spawning orchd")?;
    let mut _orchd_guard = ChildGuard(orchd);

    let client = Client::builder()
        .timeout(Duration::from_secs(3))
        .build()
        .context("failed to build orchd HTTP client")?;
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        if Instant::now() > deadline {
            let stdout = fs::read_to_string(&orchd_stdout).unwrap_or_default();
            let stderr = fs::read_to_string(&orchd_stderr).unwrap_or_default();
            bail!("orchd did not become ready\nstdout:\n{stdout}\nstderr:\n{stderr}");
        }
        if let Ok(resp) = client.get(format!("{base_url}/healthz")).send()
            && resp.status() == StatusCode::OK
        {
            break;
        }
        thread::sleep(Duration::from_millis(150));
    }

    let delivery_id = format!("itest-webhook-{}", unique_suffix()?);
    let webhook_body = serde_json::json!({
        "action": "created",
        "repository": { "full_name": harness.repo_ref.as_str() },
        "issue": { "number": issue_number },
        "comment": { "body": "@codex-orch poke", "user": { "login": harness.fixture.owner.as_str() } },
        "sender": { "login": harness.fixture.owner.as_str() },
    });

    let webhook_resp = client
        .post(format!("{base_url}/webhook"))
        .header("Content-Type", "application/json")
        .header("X-Forgejo-Event", "issue_comment")
        .header("X-Forgejo-Delivery", delivery_id)
        .body(webhook_body.to_string())
        .send()
        .context("failed POSTing webhook to orchd")?;
    let webhook_status = webhook_resp.status();
    if webhook_status != StatusCode::ACCEPTED && webhook_status != StatusCode::OK {
        let body = webhook_resp.text().unwrap_or_default();
        bail!("orchd webhook returned {} body={body}", webhook_status);
    }

    let issue_ref = harness.issue_ref(issue_number);
    let poll_deadline = Instant::now() + Duration::from_secs(30);
    loop {
        let issue = harness.get_issue(issue_number)?;
        if issue_has_label(&issue, "orchd/state/completed")? {
            break;
        }
        if Instant::now() > poll_deadline {
            bail!(
                "orchd dispatch did not complete; issue={} labels={:?}",
                issue_ref,
                issue_label_names(&issue)?
            );
        }
        thread::sleep(Duration::from_millis(250));
    }

    Ok(())
}
