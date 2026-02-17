use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, anyhow, bail};
use chrono::Utc;
use reqwest::Method;
use reqwest::blocking::Client;
use serde::{Deserialize, Serialize};
use serde_json::json;
use toml_edit::{Array, DocumentMut, Item, Table, Value, value};

use forgejo_agent::api::ForgejoClient;
use forgejo_agent::config::AgentConfig;
use forgejo_agent::types::RepoRef;

use super::cli::{Cli, RoleAddArgs, RoleCheckArgs, RoleListArgs};
use super::dispatch_config::{DispatchConfig, load_dispatch_config};
use super::paths::expand_tilde_path;

const DEFAULT_CODEX_BIN: &str = "/home/main/forgejo-agent/bin/codex-role";
const DEFAULT_ROLE_TEMPLATE: &str = "templates/role-card-template.md";
const OWNER_FALLBACK_TOKEN: &str = "~/.config/forgejo-agent/token";

#[derive(Debug, Clone, Serialize)]
struct RoleSummary {
    role: String,
    forgejo_login: String,
    codex_role_arg: String,
    token_file: String,
    role_card_file: String,
    rank: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
struct RoleCheckSummary {
    role: String,
    forgejo_login: String,
    token_file: String,
    role_card_file: String,
    expected_rank: Option<String>,
    token_login: Option<String>,
    user_active: Option<bool>,
    user_admin: Option<bool>,
    errors: Vec<String>,
    warnings: Vec<String>,
}

#[derive(Debug, Clone, Serialize)]
struct RoleCheckReport {
    ok: bool,
    roles: Vec<RoleCheckSummary>,
}

#[derive(Debug, Clone, Deserialize)]
struct ForgejoTokenResponse {
    #[serde(default)]
    id: Option<i64>,
    #[serde(default)]
    name: Option<String>,
    sha1: String,
}

#[derive(Debug, Clone, Deserialize)]
struct ForgejoApiErrorBody {
    message: Option<String>,
}

#[derive(Debug, Clone)]
struct RoleTokenPrincipal {
    login: String,
    active: Option<bool>,
    is_admin: Option<bool>,
}

#[derive(Debug, Clone)]
struct MintedRoleToken {
    token: String,
    revoke_ref: String,
    created_user: bool,
}

#[derive(Debug, Clone)]
struct RemoteProvisioningReceipt {
    login: String,
    revoke_token_ref: String,
    created_user: bool,
}

pub(super) fn role_list_command(cli: &Cli, args: RoleListArgs) -> Result<()> {
    let dispatch_config_path = resolve_dispatch_config_path(cli)?;
    let dispatch_config = load_dispatch_config(&dispatch_config_path)?;
    let mut rows = collect_role_summaries(&dispatch_config);
    rows.sort_by(|left, right| left.role.cmp(&right.role));

    if args.json {
        println!("{}", serde_json::to_string_pretty(&rows)?);
        return Ok(());
    }

    println!(
        "{:<18} {:<18} {:<8} {:<8} {}",
        "role", "forgejo_login", "rank", "arg", "token_file"
    );
    for row in rows {
        let rank = row.rank.unwrap_or_else(|| "-".to_string());
        println!(
            "{:<18} {:<18} {:<8} {:<8} {}",
            row.role, row.forgejo_login, rank, row.codex_role_arg, row.token_file
        );
    }
    Ok(())
}

pub(super) fn role_check_command(cli: &Cli, args: RoleCheckArgs) -> Result<()> {
    let dispatch_config_path = resolve_dispatch_config_path(cli)?;
    let dispatch_config = load_dispatch_config(&dispatch_config_path)?;
    let cfg = AgentConfig::load(cli.config.clone(), cli.token_file.clone())?;
    let report = run_role_check(&dispatch_config, &cfg, args.role.as_deref())?;
    if report.roles.is_empty() {
        if let Some(role_name) = args.role.as_deref() {
            bail!(
                "role '{}' not found in dispatch config",
                normalize_name(role_name)
            );
        }
        bail!("no roles found in dispatch config");
    }

    if args.json {
        println!("{}", serde_json::to_string_pretty(&report)?);
    } else {
        for role in &report.roles {
            let status = if role.errors.is_empty() {
                "ok"
            } else {
                "error"
            };
            println!(
                "role={} login={} status={} token={} rank={}",
                role.role,
                role.forgejo_login,
                status,
                role.token_file,
                role.expected_rank.as_deref().unwrap_or("-")
            );
            if !role.errors.is_empty() {
                for error in &role.errors {
                    println!("  error: {error}");
                }
            }
            if !role.warnings.is_empty() {
                for warning in &role.warnings {
                    println!("  warning: {warning}");
                }
            }
        }
    }

    if report.ok {
        Ok(())
    } else {
        bail!("role check failed")
    }
}

pub(super) fn enforce_startup_role_check(
    dispatch_config: &DispatchConfig,
    cfg: &AgentConfig,
) -> Result<()> {
    let report = run_role_check(dispatch_config, cfg, None)?;
    if report.roles.is_empty() {
        bail!("startup role check found no roles in dispatch config");
    }
    if report.ok {
        return Ok(());
    }

    let mut lines = vec!["startup role check failed:".to_string()];
    for role in report.roles.iter().filter(|entry| !entry.errors.is_empty()) {
        lines.push(format!("- {}:", role.role));
        for error in &role.errors {
            lines.push(format!("  - {error}"));
        }
    }
    bail!("{}", lines.join("\n"));
}

pub(super) fn role_add_command(cli: &Cli, args: RoleAddArgs) -> Result<()> {
    let dispatch_config_path = resolve_dispatch_config_path(cli)?;
    let dispatch_config = load_dispatch_config(&dispatch_config_path)?;
    let cfg = AgentConfig::load(cli.config.clone(), cli.token_file.clone())?;
    let client = ForgejoClient::new(&cfg)?;

    let role = normalize_role_name(&args.role)?;
    let forgejo_login = normalize_name(args.forgejo_login.as_str());
    let rank = normalize_rank(args.rank.as_str())?;
    let codex_role_arg = args
        .codex_role_arg
        .as_deref()
        .map(normalize_name)
        .unwrap_or_else(|| {
            role.strip_prefix("codex-")
                .unwrap_or(role.as_str())
                .to_string()
        });

    if dispatch_config.roles.contains_key(&role) {
        bail!("role '{role}' already exists in dispatch config");
    }

    let token_file = resolve_role_token_path(args.token_file.as_deref(), role.as_str())?;
    let codex_bin = args.codex_bin.clone().unwrap_or_else(|| {
        dispatch_config
            .roles
            .values()
            .next()
            .map(|role_cfg| role_cfg.codex_bin.clone())
            .unwrap_or_else(|| PathBuf::from(DEFAULT_CODEX_BIN))
    });

    let role_card_file = dispatch_config
        .prompt_envelopes
        .role_card_file_for(role.as_str());
    if role_card_file.exists() {
        bail!(
            "role card already exists for role '{}' at {}",
            role,
            role_card_file.display()
        );
    }

    let config_before = fs::read_to_string(&dispatch_config_path)
        .with_context(|| format!("failed reading {}", dispatch_config_path.display()))?;
    assert_rank_defined(&config_before, rank.as_str())?;

    if let Some(owner_fallback) = owner_fallback_token_path()?
        && owner_fallback == token_file
    {
        bail!(
            "token path {} is owner fallback token path; use dedicated role credential file",
            token_file.display()
        );
    }

    let updated_config = render_role_added_config(
        &config_before,
        role.as_str(),
        forgejo_login.as_str(),
        token_file.as_path(),
        codex_bin.as_path(),
        codex_role_arg.as_str(),
        args.can_dispatch,
    )?;

    let role_card_body = render_role_card(role.as_str(), rank.as_str(), &role_card_file)?;

    if args.dry_run {
        if args.json {
            println!(
                "{}",
                serde_json::to_string_pretty(&json!({
                    "role": role,
                    "forgejo_login": forgejo_login,
                    "token_file": token_file,
                    "role_card_file": role_card_file,
                    "dispatch_config": dispatch_config_path,
                    "can_dispatch": args.can_dispatch,
                }))?
            );
        } else {
            println!("dry-run role add plan:");
            println!("- role: {role}");
            println!("- forgejo_login: {forgejo_login}");
            println!("- rank: {rank}");
            println!("- token_file: {}", token_file.display());
            println!("- role_card_file: {}", role_card_file.display());
            println!("- dispatch_config: {}", dispatch_config_path.display());
            println!("- can_dispatch: {}", args.can_dispatch);
        }
        return Ok(());
    }

    let admin_token = read_admin_token(args.admin_token_file.as_deref())?;
    let mut created_token_file = false;
    let mut previous_token_contents = read_token_if_exists(&token_file)?;
    let mut remote_receipt = None;

    let desired_token = if previous_token_contents.is_some() && !args.rotate_token {
        read_token_file(&token_file)?
    } else {
        let minted = mint_role_token(
            cfg.base_url.as_str(),
            admin_token.as_str(),
            forgejo_login.as_str(),
            args.create_user,
        )?;
        write_secret_token_file(&token_file, minted.token.as_str())?;
        created_token_file = previous_token_contents.is_none();
        previous_token_contents = previous_token_contents.take();
        remote_receipt = Some(RemoteProvisioningReceipt {
            login: forgejo_login.clone(),
            revoke_token_ref: minted.revoke_ref,
            created_user: minted.created_user,
        });
        minted.token
    };

    let remote_ready = (|| -> Result<()> {
        verify_token_maps_to_login(
            &client,
            &cfg,
            desired_token.as_str(),
            forgejo_login.as_str(),
        )?;
        ensure_scream_acl(
            cfg.base_url.as_str(),
            admin_token.as_str(),
            &args.scream_repo,
            forgejo_login.as_str(),
            args.scream_permission.as_str(),
        )?;
        Ok(())
    })();
    if let Err(err) = remote_ready {
        if created_token_file {
            let _ = fs::remove_file(&token_file);
        } else if args.rotate_token
            && let Some(previous) = previous_token_contents.as_deref()
        {
            let _ = write_secret_token_file(&token_file, previous);
        }

        let compensation_notes = compensate_remote_role_add(
            cfg.base_url.as_str(),
            admin_token.as_str(),
            remote_receipt.as_ref(),
        );
        let mut context = "role add failed before local writes".to_string();
        if !compensation_notes.is_empty() {
            context.push_str("; remote compensation: ");
            context.push_str(&compensation_notes.join("; "));
        }
        return Err(err).context(context);
    }

    let mut wrote_config = false;
    let mut wrote_role_card = false;

    let apply_result = (|| -> Result<()> {
        if let Some(parent) = role_card_file.parent() {
            fs::create_dir_all(parent)
                .with_context(|| format!("failed creating {}", parent.display()))?;
        }
        fs::write(&role_card_file, role_card_body)
            .with_context(|| format!("failed writing {}", role_card_file.display()))?;
        wrote_role_card = true;

        fs::write(&dispatch_config_path, updated_config)
            .with_context(|| format!("failed writing {}", dispatch_config_path.display()))?;
        wrote_config = true;

        let reloaded = load_dispatch_config(&dispatch_config_path)?;
        if !reloaded.roles.contains_key(role.as_str()) {
            bail!("new role '{}' missing after config write", role);
        }
        let persisted_token = read_token_file(&token_file)?;
        verify_token_maps_to_login(
            &client,
            &cfg,
            persisted_token.as_str(),
            forgejo_login.as_str(),
        )?;

        let report = run_role_check(&reloaded, &cfg, Some(role.as_str()))?;
        if !report.ok {
            bail!("new role '{}' failed post-write role check", role);
        }
        Ok(())
    })();

    if let Err(err) = apply_result {
        if wrote_config {
            let _ = fs::write(&dispatch_config_path, &config_before);
        }
        if wrote_role_card {
            let _ = fs::remove_file(&role_card_file);
        }
        if created_token_file {
            let _ = fs::remove_file(&token_file);
        } else if args.rotate_token
            && let Some(previous) = previous_token_contents.as_deref()
        {
            let _ = write_secret_token_file(&token_file, previous);
        }
        let compensation_notes = compensate_remote_role_add(
            cfg.base_url.as_str(),
            admin_token.as_str(),
            remote_receipt.as_ref(),
        );
        let mut context = "role add failed and local edits were rolled back".to_string();
        if !compensation_notes.is_empty() {
            context.push_str("; remote compensation: ");
            context.push_str(&compensation_notes.join("; "));
        }
        return Err(err).context(context);
    }

    if args.json {
        println!(
            "{}",
            serde_json::to_string_pretty(&json!({
                "added": true,
                "role": role,
                "forgejo_login": forgejo_login,
                "token_file": token_file,
                "role_card_file": role_card_file,
                "dispatch_config": dispatch_config_path,
            }))?
        );
    } else {
        println!("added role {role}");
        println!("- forgejo_login: {forgejo_login}");
        println!("- token_file: {}", token_file.display());
        println!("- role_card: {}", role_card_file.display());
        println!("- dispatch_config: {}", dispatch_config_path.display());
    }

    Ok(())
}

fn collect_role_summaries(config: &DispatchConfig) -> Vec<RoleSummary> {
    config
        .roles
        .iter()
        .map(|(role_name, role_cfg)| {
            let role_card_file = config.prompt_envelopes.role_card_file_for(role_name);
            let rank = read_rank_from_role_card(&role_card_file).ok().flatten();
            RoleSummary {
                role: role_name.clone(),
                forgejo_login: role_cfg.forgejo_login.clone(),
                codex_role_arg: role_cfg.codex_role_arg.clone(),
                token_file: role_cfg.token_file.to_string_lossy().into_owned(),
                role_card_file: role_card_file.to_string_lossy().into_owned(),
                rank,
            }
        })
        .collect()
}

fn run_role_check(
    dispatch_config: &DispatchConfig,
    cfg: &AgentConfig,
    role_filter: Option<&str>,
) -> Result<RoleCheckReport> {
    let client = ForgejoClient::new(cfg)?;
    Ok(evaluate_roles(
        dispatch_config,
        cfg,
        &client,
        role_filter.map(normalize_name),
    ))
}

fn evaluate_roles(
    config: &DispatchConfig,
    cfg: &AgentConfig,
    client: &ForgejoClient,
    role_filter: Option<String>,
) -> RoleCheckReport {
    let mut checks = Vec::new();

    for (role_name, role_cfg) in &config.roles {
        if let Some(filter) = role_filter.as_ref()
            && filter != role_name
        {
            continue;
        }

        let role_card_file = config.prompt_envelopes.role_card_file_for(role_name);
        let mut errors = Vec::new();
        let mut warnings = Vec::new();

        let expected_rank = match read_rank_from_role_card(&role_card_file) {
            Ok(rank) => rank,
            Err(err) => {
                errors.push(format!("role card: {err}"));
                None
            }
        };

        let token_contents = match read_token_file(&role_cfg.token_file) {
            Ok(token) => Some(token),
            Err(err) => {
                errors.push(format!("token file: {err}"));
                None
            }
        };

        if let Ok(meta) = fs::metadata(&role_cfg.token_file) {
            let mode = meta.permissions().mode() & 0o777;
            if (mode & 0o077) != 0 {
                errors.push(format!(
                    "token file mode {:o} is too permissive; expected 600 or stricter",
                    mode
                ));
            }
        }

        let (token_login, user_active, user_admin) = if let Some(token) = token_contents.as_deref()
        {
            match whoami_with_token(client, cfg, token) {
                Ok(principal) => {
                    if principal.login != role_cfg.forgejo_login {
                        errors.push(format!(
                            "token resolves to '{}' but role forgejo_login is '{}'",
                            principal.login, role_cfg.forgejo_login
                        ));
                    }
                    if principal.active.is_some_and(|active| !active) {
                        errors.push("forgejo user is inactive".to_string());
                    }
                    let expected_admin = expected_admin_for_role(role_name.as_str());
                    if let Some(is_admin) = principal.is_admin {
                        if is_admin != expected_admin {
                            errors.push(format!(
                                "forgejo admin flag mismatch (expected {}, got {})",
                                expected_admin, is_admin
                            ));
                        }
                    } else {
                        warnings.push(
                            "token whoami response missing admin flag; admin posture unverified"
                                .to_string(),
                        );
                    }
                    (Some(principal.login), principal.active, principal.is_admin)
                }
                Err(err) => {
                    errors.push(format!("token whoami failed: {err}"));
                    (None, None, None)
                }
            }
        } else {
            (None, None, None)
        };

        checks.push(RoleCheckSummary {
            role: role_name.clone(),
            forgejo_login: role_cfg.forgejo_login.clone(),
            token_file: role_cfg.token_file.to_string_lossy().into_owned(),
            role_card_file: role_card_file.to_string_lossy().into_owned(),
            expected_rank,
            token_login,
            user_active,
            user_admin,
            errors,
            warnings,
        });
    }

    checks.sort_by(|left, right| left.role.cmp(&right.role));

    let ok = checks.iter().all(|check| check.errors.is_empty());
    RoleCheckReport { ok, roles: checks }
}

fn expected_admin_for_role(role_name: &str) -> bool {
    matches!(role_name, "main" | "codex-orch")
}

fn resolve_dispatch_config_path(cli: &Cli) -> Result<PathBuf> {
    let raw = cli.dispatch_config.as_str();
    expand_tilde_path(raw)
}

fn normalize_name(raw: &str) -> String {
    raw.trim().to_ascii_lowercase()
}

fn normalize_role_name(raw: &str) -> Result<String> {
    let role = normalize_name(raw);
    if role.is_empty() {
        bail!("role cannot be empty");
    }
    if !role.starts_with("codex-") {
        bail!("role must start with 'codex-' (got '{role}')");
    }
    if !role
        .chars()
        .all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '-' | '_'))
    {
        bail!("role contains unsupported characters: '{role}'");
    }
    Ok(role)
}

fn normalize_rank(raw: &str) -> Result<String> {
    let rank = raw
        .trim()
        .trim_matches(|ch: char| [',', ';', ':', '.'].contains(&ch))
        .to_ascii_uppercase();
    let Some(digits) = rank.strip_prefix("OF-") else {
        bail!("rank must be OF-<n>, got '{raw}'");
    };
    let _: u8 = digits
        .parse()
        .with_context(|| format!("invalid rank digits in '{raw}'"))?;
    Ok(rank)
}

fn resolve_role_token_path(path: Option<&Path>, role: &str) -> Result<PathBuf> {
    if let Some(path) = path {
        let raw = path.to_string_lossy();
        return expand_tilde_path(raw.as_ref());
    }

    let home = expand_tilde_path("~")?;
    Ok(home
        .join(".config/forgejo-agent/creds")
        .join(format!("{role}.token")))
}

fn owner_fallback_token_path() -> Result<Option<PathBuf>> {
    Ok(Some(expand_tilde_path(OWNER_FALLBACK_TOKEN)?))
}

fn read_rank_from_role_card(path: &Path) -> Result<Option<String>> {
    let text = fs::read_to_string(path)
        .with_context(|| format!("failed reading role card {}", path.display()))?;
    Ok(parse_rank_from_role_card(&text))
}

fn parse_rank_from_role_card(role_card_md: &str) -> Option<String> {
    role_card_md.lines().find_map(|line| {
        let line = line.trim();
        let line = line
            .strip_prefix('-')
            .or_else(|| line.strip_prefix('*'))
            .map(str::trim_start)?;
        let token = line.split_whitespace().next()?;
        normalize_rank(token).ok()
    })
}

fn read_token_file(path: &Path) -> Result<String> {
    let token = fs::read_to_string(path)
        .with_context(|| format!("failed reading token file {}", path.display()))?;
    let token = token.trim().to_string();
    if token.is_empty() {
        bail!("token file {} is empty", path.display());
    }
    Ok(token)
}

fn read_token_if_exists(path: &Path) -> Result<Option<String>> {
    if !path.exists() {
        return Ok(None);
    }
    let token = read_token_file(path)?;
    Ok(Some(token))
}

fn write_secret_token_file(path: &Path, token: &str) -> Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("failed creating {}", parent.display()))?;
    }
    fs::write(path, format!("{token}\n"))
        .with_context(|| format!("failed writing token file {}", path.display()))?;
    fs::set_permissions(path, fs::Permissions::from_mode(0o600))
        .with_context(|| format!("failed setting token file permissions {}", path.display()))?;
    Ok(())
}

fn verify_token_maps_to_login(
    client: &ForgejoClient,
    cfg: &AgentConfig,
    token: &str,
    expected_login: &str,
) -> Result<()> {
    let actual = whoami_with_token(client, cfg, token)?.login;
    if actual != expected_login {
        bail!(
            "token identity mismatch: expected '{}', got '{}'",
            expected_login,
            actual
        );
    }
    Ok(())
}

fn whoami_with_token(
    client: &ForgejoClient,
    cfg: &AgentConfig,
    token: &str,
) -> Result<RoleTokenPrincipal> {
    let mut cfg = cfg.clone();
    cfg.token = token.to_string();
    let who = client.whoami(&cfg)?;
    let login = who
        .get("login")
        .and_then(serde_json::Value::as_str)
        .ok_or_else(|| anyhow!("whoami response missing login field"))?;
    let active = who.get("active").and_then(serde_json::Value::as_bool);
    let is_admin = who.get("is_admin").and_then(serde_json::Value::as_bool);
    Ok(RoleTokenPrincipal {
        login: normalize_name(login),
        active,
        is_admin,
    })
}

fn user_exists_with_token(base_url: &str, token: &str, login: &str) -> Result<bool> {
    let http = Client::builder()
        .user_agent("forgejo-agent/role")
        .build()
        .context("failed creating HTTP client")?;
    let url = format!("{}/api/v1/users/{}", base_url.trim_end_matches('/'), login);
    let response = http
        .request(Method::GET, &url)
        .header("Accept", "application/json")
        .header("Authorization", format!("token {token}"))
        .send()
        .with_context(|| format!("request failed: GET {url}"))?;

    if response.status().as_u16() == 404 {
        return Ok(false);
    }
    if response.status().is_success() {
        return Ok(true);
    }
    let status = response.status().as_u16();
    let body = response.text().unwrap_or_else(|_| "<no body>".to_string());
    bail!("GET {url} failed with {status}: {body}");
}

fn read_admin_token(path: Option<&Path>) -> Result<String> {
    let path = if let Some(path) = path {
        let raw = path.to_string_lossy();
        expand_tilde_path(raw.as_ref())?
    } else {
        bail!("admin token file is required (use --admin-token-file)");
    };
    read_token_file(&path)
}

fn mint_role_token(
    base_url: &str,
    admin_token: &str,
    login: &str,
    create_user: bool,
) -> Result<MintedRoleToken> {
    let http = Client::builder()
        .user_agent("forgejo-agent/role")
        .build()
        .context("failed creating HTTP client")?;

    let user_exists = user_exists_with_token(base_url, admin_token, login)?;
    let user_created_in_run = if user_exists {
        false
    } else {
        if !create_user {
            bail!(
                "forgejo user '{}' does not exist; rerun with --create-user to provision",
                login
            );
        }
        create_forgejo_user(&http, base_url, admin_token, login)?;
        true
    };

    let temp_password = format!(
        "{}-{}",
        login,
        Utc::now().timestamp_nanos_opt().unwrap_or_default()
    );

    update_forgejo_user_password(&http, base_url, admin_token, login, temp_password.as_str())?;
    let token = create_forgejo_user_token(&http, base_url, login, temp_password.as_str())?;
    let revoke_ref = token
        .id
        .map(|id| id.to_string())
        .or(token.name)
        .ok_or_else(|| anyhow!("minted token response missing both id and name"))?;
    Ok(MintedRoleToken {
        token: token.sha1,
        revoke_ref,
        created_user: user_created_in_run,
    })
}

fn create_forgejo_user(
    http: &Client,
    base_url: &str,
    admin_token: &str,
    login: &str,
) -> Result<()> {
    let url = format!("{}/api/v1/admin/users", base_url.trim_end_matches('/'));
    let payload = json!({
        "username": login,
        "email": format!("{login}@localhost"),
        "password": format!("{login}-bootstrap-Password-123"),
        "must_change_password": false,
        "send_notify": false,
        "visibility": "private",
    });
    send_token_json(http, Method::POST, &url, admin_token, &payload)
}

fn update_forgejo_user_password(
    http: &Client,
    base_url: &str,
    admin_token: &str,
    login: &str,
    password: &str,
) -> Result<()> {
    let url = format!(
        "{}/api/v1/admin/users/{}",
        base_url.trim_end_matches('/'),
        login
    );
    let payload = json!({
        "active": true,
        "must_change_password": false,
        "password": password,
    });
    send_token_json(http, Method::PATCH, &url, admin_token, &payload)
}

fn create_forgejo_user_token(
    http: &Client,
    base_url: &str,
    login: &str,
    password: &str,
) -> Result<ForgejoTokenResponse> {
    let url = format!(
        "{}/api/v1/users/{}/tokens",
        base_url.trim_end_matches('/'),
        login
    );

    let token_name = format!("{}-{}", login, Utc::now().format("%Y%m%d-%H%M%S"));

    let scoped_payload = json!({
        "name": token_name,
        "scopes": ["all"],
    });
    if let Ok(response) = send_basic_json::<ForgejoTokenResponse>(
        http,
        Method::POST,
        &url,
        login,
        password,
        &scoped_payload,
    ) {
        let mut response = response;
        response.name.get_or_insert(token_name);
        Ok(response)
    } else {
        let payload = json!({ "name": token_name });
        let mut response = send_basic_json::<ForgejoTokenResponse>(
            http,
            Method::POST,
            &url,
            login,
            password,
            &payload,
        )?;
        response.name.get_or_insert(token_name);
        Ok(response)
    }
}

fn compensate_remote_role_add(
    base_url: &str,
    admin_token: &str,
    receipt: Option<&RemoteProvisioningReceipt>,
) -> Vec<String> {
    let Some(receipt) = receipt else {
        return Vec::new();
    };

    let http = match Client::builder().user_agent("forgejo-agent/role").build() {
        Ok(client) => client,
        Err(err) => {
            return vec![format!(
                "failed building HTTP client for compensation: {err}"
            )];
        }
    };

    let mut notes = Vec::new();
    if let Err(err) = revoke_forgejo_user_token(
        &http,
        base_url,
        admin_token,
        receipt.login.as_str(),
        receipt.revoke_token_ref.as_str(),
    ) {
        notes.push(format!("token revoke failed: {err}"));
    }
    if receipt.created_user
        && let Err(err) =
            set_forgejo_user_active(&http, base_url, admin_token, receipt.login.as_str(), false)
    {
        notes.push(format!("user deactivate failed: {err}"));
    }
    notes
}

fn revoke_forgejo_user_token(
    http: &Client,
    base_url: &str,
    admin_token: &str,
    login: &str,
    token_ref: &str,
) -> Result<()> {
    let url = format!(
        "{}/api/v1/users/{}/tokens/{}",
        base_url.trim_end_matches('/'),
        login,
        token_ref
    );
    let response = http
        .request(Method::DELETE, &url)
        .header("Accept", "application/json")
        .header("Authorization", format!("token {admin_token}"))
        .send()
        .with_context(|| format!("request failed: DELETE {url}"))?;

    if response.status().is_success() || response.status().as_u16() == 404 {
        return Ok(());
    }
    let status = response.status().as_u16();
    let body_text = response.text().unwrap_or_else(|_| "<no body>".to_string());
    let body_message = serde_json::from_str::<ForgejoApiErrorBody>(&body_text)
        .ok()
        .and_then(|body| body.message)
        .unwrap_or(body_text);
    bail!("DELETE {url} failed with {status}: {body_message}");
}

fn set_forgejo_user_active(
    http: &Client,
    base_url: &str,
    admin_token: &str,
    login: &str,
    active: bool,
) -> Result<()> {
    let url = format!(
        "{}/api/v1/admin/users/{}",
        base_url.trim_end_matches('/'),
        login
    );
    let payload = json!({
        "active": active,
        "must_change_password": false,
    });
    send_token_json(http, Method::PATCH, &url, admin_token, &payload)
}

fn ensure_scream_acl(
    base_url: &str,
    admin_token: &str,
    repo: &RepoRef,
    login: &str,
    permission: &str,
) -> Result<()> {
    let permission = normalize_name(permission);
    if !matches!(permission.as_str(), "read" | "write" | "admin") {
        bail!(
            "unsupported scream permission '{}'; use read/write/admin",
            permission
        );
    }

    let http = Client::builder()
        .user_agent("forgejo-agent/role")
        .build()
        .context("failed creating HTTP client")?;
    let url = format!(
        "{}/api/v1/repos/{}/{}/collaborators/{}",
        base_url.trim_end_matches('/'),
        repo.owner,
        repo.repo,
        login
    );
    let payload = json!({ "permission": permission });
    send_token_json(&http, Method::PUT, &url, admin_token, &payload)
}

fn send_token_json(
    http: &Client,
    method: Method,
    url: &str,
    token: &str,
    payload: &serde_json::Value,
) -> Result<()> {
    let response = http
        .request(method.clone(), url)
        .header("Accept", "application/json")
        .header("Authorization", format!("token {token}"))
        .header("Content-Type", "application/json")
        .json(payload)
        .send()
        .with_context(|| format!("request failed: {} {}", method, url))?;

    if response.status().is_success() {
        return Ok(());
    }

    let status = response.status().as_u16();
    let body_text = response.text().unwrap_or_else(|_| "<no body>".to_string());
    let body_message = serde_json::from_str::<ForgejoApiErrorBody>(&body_text)
        .ok()
        .and_then(|body| body.message)
        .unwrap_or(body_text);
    bail!(
        "{} {} failed with {}: {}",
        method,
        url,
        status,
        body_message
    )
}

fn send_basic_json<T: for<'de> Deserialize<'de>>(
    http: &Client,
    method: Method,
    url: &str,
    username: &str,
    password: &str,
    payload: &serde_json::Value,
) -> Result<T> {
    let response = http
        .request(method.clone(), url)
        .header("Accept", "application/json")
        .header("Content-Type", "application/json")
        .basic_auth(username, Some(password))
        .json(payload)
        .send()
        .with_context(|| format!("request failed: {} {}", method, url))?;

    if !response.status().is_success() {
        let status = response.status().as_u16();
        let body_text = response.text().unwrap_or_else(|_| "<no body>".to_string());
        let body_message = serde_json::from_str::<ForgejoApiErrorBody>(&body_text)
            .ok()
            .and_then(|body| body.message)
            .unwrap_or(body_text);
        bail!(
            "{} {} failed with {}: {}",
            method,
            url,
            status,
            body_message
        );
    }

    response
        .json::<T>()
        .with_context(|| format!("invalid JSON from {} {}", method, url))
}

fn render_role_added_config(
    original: &str,
    role: &str,
    forgejo_login: &str,
    token_file: &Path,
    codex_bin: &Path,
    codex_role_arg: &str,
    can_dispatch: bool,
) -> Result<String> {
    let mut doc = original
        .parse::<DocumentMut>()
        .map_err(|err| anyhow!("invalid TOML in dispatch config: {err}"))?;

    if !doc.contains_key("roles") {
        doc["roles"] = Item::Table(Table::new());
    }
    let roles_item = doc
        .get_mut("roles")
        .ok_or_else(|| anyhow!("missing [roles] table"))?;
    let roles = roles_item
        .as_table_like_mut()
        .ok_or_else(|| anyhow!("[roles] must be a table"))?;

    if roles.get(role).is_some() {
        bail!("role '{}' already exists", role);
    }

    let mut role_table = Table::new();
    role_table["codex_bin"] = value(codex_bin.to_string_lossy().into_owned());
    role_table["codex_role_arg"] = value(codex_role_arg);
    role_table["forgejo_login"] = value(forgejo_login);
    role_table["token_file"] = value(token_file.to_string_lossy().into_owned());
    roles.insert(role, Item::Table(role_table));

    if can_dispatch {
        let actors_item = doc
            .entry("allowed_actors")
            .or_insert(Item::Value(Value::Array(Array::new())));
        let actors = actors_item
            .as_array_mut()
            .ok_or_else(|| anyhow!("allowed_actors must be an array"))?;
        if !actors
            .iter()
            .any(|value| value.as_str().is_some_and(|actor| actor == role))
        {
            actors.push(role);
        }
    }

    Ok(doc.to_string())
}

fn assert_rank_defined(dispatch_config_toml: &str, rank: &str) -> Result<()> {
    let doc = dispatch_config_toml
        .parse::<DocumentMut>()
        .map_err(|err| anyhow!("invalid TOML in dispatch config: {err}"))?;

    let rank_acl_enabled = doc
        .get("rank_acl")
        .and_then(Item::as_table)
        .and_then(|table| table.get("enabled"))
        .and_then(Item::as_bool)
        .unwrap_or(true);

    if !rank_acl_enabled {
        return Ok(());
    }

    let has_rank = doc
        .get("rank_acl")
        .and_then(Item::as_table)
        .and_then(|table| table.get("ranks"))
        .and_then(Item::as_table)
        .and_then(|table| table.get(rank))
        .is_some();
    if !has_rank {
        bail!(
            "rank '{}' is not configured under [rank_acl.ranks] in dispatch config",
            rank
        );
    }
    Ok(())
}

fn render_role_card(role: &str, rank: &str, role_card_file: &Path) -> Result<String> {
    let template_path = PathBuf::from(DEFAULT_ROLE_TEMPLATE);
    let template = fs::read_to_string(&template_path).with_context(|| {
        format!(
            "failed reading role template {} (needed for {})",
            template_path.display(),
            role_card_file.display()
        )
    })?;
    Ok(template.replace("{{role}}", role).replace("{{rank}}", rank))
}

#[cfg(test)]
mod tests {
    use super::{
        assert_rank_defined, normalize_rank, parse_rank_from_role_card, render_role_added_config,
    };
    use std::path::Path;

    #[test]
    fn normalize_rank_accepts_of_prefix() {
        let rank = normalize_rank("of-8").expect("rank should parse");
        assert_eq!(rank, "OF-8");
    }

    #[test]
    fn parse_rank_from_role_card_reads_bullet() {
        let role_card = "# role\n\n- OF-6\n";
        assert_eq!(
            parse_rank_from_role_card(role_card),
            Some("OF-6".to_string())
        );
    }

    #[test]
    fn assert_rank_defined_checks_acl_table() {
        let dispatch_config = r#"
version = 1
allowed_actors = ["main"]

[rank_acl]
enabled = true

[rank_acl.ranks."OF-10"]
directives = ["poke"]
"#;
        assert!(assert_rank_defined(dispatch_config, "OF-10").is_ok());
        assert!(assert_rank_defined(dispatch_config, "OF-8").is_err());
    }

    #[test]
    fn render_role_added_config_inserts_role_block() {
        let source = r#"
version = 1
allowed_actors = ["main"]

[roles.codex-orch]
codex_bin = "/tmp/codex-role"
codex_role_arg = "orch"
forgejo_login = "codex-orch"
token_file = "/tmp/orch.token"
"#;

        let rendered = render_role_added_config(
            source,
            "codex-dev",
            "codex-dev",
            Path::new("/tmp/codex-dev.token"),
            Path::new("/tmp/codex-role"),
            "dev",
            true,
        )
        .expect("render role config");

        assert!(rendered.contains("[roles.codex-dev]"));
        assert!(rendered.contains("forgejo_login = \"codex-dev\""));
        assert!(rendered.contains("token_file = \"/tmp/codex-dev.token\""));
        assert!(rendered.contains("\"codex-dev\""));
    }

    #[test]
    fn render_role_added_config_rejects_duplicate_role() {
        let source = r#"
version = 1
[roles.codex-orch]
token_file = "/tmp/orch.token"
"#;
        let result = render_role_added_config(
            source,
            "codex-orch",
            "codex-orch",
            Path::new("/tmp/orch.token"),
            Path::new("/tmp/codex-role"),
            "orch",
            false,
        );
        assert!(result.is_err());
    }
}
