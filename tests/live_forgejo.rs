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

fn run_cli_json(config_path: &Path, token_path: &Path, args: &[&str]) -> Result<Value> {
    let mut cmd = Command::new(forgejo_agent_bin()?);

    cmd.arg("--config")
        .arg(config_path)
        .arg("--token-file")
        .arg(token_path)
        .args(args);

    let output = cmd.output().context("failed to run forgejo-agent CLI")?;
    if !output.status.success() {
        let stdout = String::from_utf8_lossy(&output.stdout);
        let stderr = String::from_utf8_lossy(&output.stderr);
        bail!(
            "forgejo-agent command failed (status={:?})\nstdout:\n{stdout}\nstderr:\n{stderr}",
            output.status.code()
        );
    }

    let stdout = String::from_utf8(output.stdout).context("forgejo-agent stdout not utf-8")?;
    serde_json::from_str(&stdout).with_context(|| format!("stdout was not JSON: {stdout}"))
}

fn run_cli_plain(config_path: &Path, token_path: &Path, args: &[&str]) -> Result<String> {
    let mut cmd = Command::new(forgejo_agent_bin()?);

    cmd.arg("--config")
        .arg(config_path)
        .arg("--token-file")
        .arg(token_path)
        .args(args);

    let output = cmd.output().context("failed to run forgejo-agent CLI")?;
    if !output.status.success() {
        let stdout = String::from_utf8_lossy(&output.stdout);
        let stderr = String::from_utf8_lossy(&output.stderr);
        bail!(
            "forgejo-agent command failed (status={:?})\nstdout:\n{stdout}\nstderr:\n{stderr}",
            output.status.code()
        );
    }

    String::from_utf8(output.stdout).context("forgejo-agent stdout not utf-8")
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

    let timer = StepTimer::new("live_smoke_then_add_issue_round_trip");

    let fixture = ForgejoFixture::spawn(&timer)?;

    let repo_name = format!("itest-{}", unique_suffix()?);
    let repo_ref = format!("{}/{}", fixture.owner, repo_name);
    let (config_path, token_path) = fixture.write_agent_config(&repo_name, &timer)?;

    let ensure_started = Instant::now();
    let ensure_output = run_cli_plain(&config_path, &token_path, &["repo", "ensure", &repo_ref])?;
    timer.record("repo.ensure", ensure_started)?;
    if !ensure_output.contains("repo ensured") {
        bail!("unexpected repo ensure output: {ensure_output}");
    }

    let create_started = Instant::now();
    let create_output = run_cli_json(
        &config_path,
        &token_path,
        &[
            "issue",
            "create",
            &repo_ref,
            "--title",
            "itest title",
            "--body",
            "itest body",
            "--workflow",
            "triage",
            "--json",
        ],
    )?;
    timer.record("issue.create", create_started)?;

    let issue_number = create_output
        .get("number")
        .and_then(Value::as_u64)
        .ok_or_else(|| anyhow!("issue create JSON missing numeric 'number'"))?;

    let issue_title = create_output
        .get("title")
        .and_then(Value::as_str)
        .ok_or_else(|| anyhow!("issue create JSON missing string 'title'"))?;
    if issue_title != "itest title" {
        bail!("issue create returned unexpected title: {issue_title}");
    }

    let verify_started = Instant::now();
    let read_back = fixture.authed_get(&format!(
        "/api/v1/repos/{}/{}/issues/{issue_number}",
        fixture.owner, repo_name
    ))?;
    timer.record("issue.verify_read_back", verify_started)?;

    let read_back_title = read_back
        .get("title")
        .and_then(Value::as_str)
        .ok_or_else(|| anyhow!("read-back issue JSON missing string 'title'"))?;
    if read_back_title != "itest title" {
        bail!("read-back issue title mismatch: {read_back_title}");
    }

    let read_back_body = read_back
        .get("body")
        .and_then(Value::as_str)
        .ok_or_else(|| anyhow!("read-back issue JSON missing string 'body'"))?;
    if read_back_body != "itest body" {
        bail!("read-back issue body mismatch: {read_back_body}");
    }

    let read_back_state = read_back
        .get("state")
        .and_then(Value::as_str)
        .ok_or_else(|| anyhow!("read-back issue JSON missing string 'state'"))?;
    if read_back_state != "open" {
        bail!("read-back issue state mismatch: expected open, got {read_back_state}");
    }

    let author_login = read_back
        .get("user")
        .and_then(Value::as_object)
        .and_then(|user| user.get("login"))
        .and_then(Value::as_str)
        .ok_or_else(|| anyhow!("read-back issue JSON missing author login"))?;
    if author_login != fixture.owner {
        bail!(
            "read-back author mismatch: expected {}, got {author_login}",
            fixture.owner
        );
    }

    Ok(())
}
