use anyhow::{Context, Result, anyhow};
use axum::http::HeaderMap;
use chrono::Utc;
use hmac::{Hmac, Mac};
use sha2::{Digest, Sha256};

use super::paths::expand_tilde_path;
use super::state::{DecisionRecord, EventContext, ParsedDirective, WebhookPayload};

type HmacSha256 = Hmac<Sha256>;

pub(super) fn verify_signature(
    secret: Option<&[u8]>,
    headers: &HeaderMap,
    body: &[u8],
) -> Result<()> {
    let Some(secret) = secret else {
        return Ok(());
    };
    let signature = extract_header(headers, &["x-forgejo-signature", "x-gitea-signature"])
        .ok_or_else(|| anyhow!("missing webhook signature header"))?;
    let signature = signature.trim();
    let signature = signature.strip_prefix("sha256=").unwrap_or(signature);
    let provided = hex::decode(signature).context("signature is not valid hex")?;

    let mut mac = HmacSha256::new_from_slice(secret).context("invalid webhook secret")?;
    mac.update(body);
    mac.verify_slice(&provided)
        .map_err(|_| anyhow!("webhook signature verification failed"))?;
    Ok(())
}

pub(super) fn synthetic_delivery_id(body: &[u8]) -> String {
    let hash = Sha256::digest(body);
    let hash_hex = hex::encode(hash);
    format!(
        "synthetic-{}-{}",
        Utc::now().timestamp_micros(),
        &hash_hex[..12]
    )
}

pub(super) fn load_secret(secret_file: Option<&str>) -> Result<Option<Vec<u8>>> {
    let Some(secret_file) = secret_file else {
        return Ok(None);
    };
    let path = expand_tilde_path(secret_file)?;
    let secret = std::fs::read_to_string(&path)
        .with_context(|| format!("failed to read webhook secret file: {}", path.display()))?;
    let secret = secret.trim().as_bytes().to_vec();
    if secret.is_empty() {
        return Err(anyhow!("webhook secret file is empty: {}", path.display()));
    }
    Ok(Some(secret))
}

pub(super) fn extract_event_context(
    event_type: &str,
    payload: &WebhookPayload,
) -> Option<EventContext> {
    let repo_full_name = payload.repository.as_ref()?.full_name.clone();
    let issue_number = payload.issue.as_ref().map(|issue| issue.number);

    let actor_login = payload
        .sender
        .as_ref()
        .map(|sender| sender.login.clone())
        .or_else(|| {
            payload
                .comment
                .as_ref()
                .and_then(|comment| comment.user.as_ref().map(|user| user.login.clone()))
        });

    let text = match event_type {
        "issue_comment" => payload.comment.as_ref().map(|comment| comment.body.clone()),
        "issues" => payload.issue.as_ref().and_then(|issue| issue.body.clone()),
        _ => None,
    };
    let source_comment_id = payload.comment.as_ref().and_then(|comment| comment.id);
    let source_created_at = payload
        .comment
        .as_ref()
        .and_then(|comment| comment.created_at.clone());

    Some(EventContext {
        repo_full_name,
        issue_number,
        actor_login,
        text,
        source_comment_id,
        source_created_at,
    })
}

pub(super) fn decide(event_type: &str, context: Option<&EventContext>) -> DecisionRecord {
    let Some(context) = context else {
        return DecisionRecord {
            decision: "ignored".to_string(),
            reason_code: "missing_context".to_string(),
            directive: None,
            target_role: None,
            would_dispatch: false,
        };
    };

    if event_type == "issue_comment" && context.text.as_deref().is_some_and(is_orchd_echo_comment) {
        return DecisionRecord {
            decision: "ignored".to_string(),
            reason_code: "orchd_echo_comment".to_string(),
            directive: None,
            target_role: None,
            would_dispatch: false,
        };
    }

    if let Some(text) = context.text.as_deref()
        && let Some(parsed) = parse_directive(text)
    {
        return DecisionRecord {
            decision: "accepted".to_string(),
            reason_code: "explicit_directive".to_string(),
            directive: Some(parsed.directive),
            target_role: Some(parsed.role),
            would_dispatch: true,
        };
    }

    DecisionRecord {
        decision: "ignored".to_string(),
        reason_code: "no_directive".to_string(),
        directive: None,
        target_role: None,
        would_dispatch: false,
    }
}

pub(super) fn parse_directive(text: &str) -> Option<ParsedDirective> {
    text.lines().find_map(parse_directive_line)
}

fn parse_directive_line(line: &str) -> Option<ParsedDirective> {
    let mut parts = line.split_whitespace();
    let role_token = parts.next()?;
    let directive_token = parts.next()?;
    if parts.next().is_some() {
        return None;
    }

    let role_token = role_token
        .trim_matches(|ch: char| [',', ';', ':'].contains(&ch))
        .trim_start_matches('@')
        .to_ascii_lowercase();
    let directive = directive_token
        .trim_matches(|ch: char| [',', ';', ':', '.'].contains(&ch))
        .to_ascii_lowercase();

    let role = if role_token == "codex" {
        "codex-orch".to_string()
    } else if role_token.starts_with("codex-") {
        role_token
    } else {
        return None;
    };

    if !matches!(directive.as_str(), "design" | "impl" | "pr" | "poke") {
        return None;
    }

    Some(ParsedDirective { role, directive })
}

pub(super) fn is_orchd_echo_comment(text: &str) -> bool {
    text.trim_start().starts_with("orchd:")
}

pub(super) fn extract_header(headers: &HeaderMap, names: &[&str]) -> Option<String> {
    names.iter().find_map(|name| {
        headers
            .get(*name)
            .and_then(|value| value.to_str().ok())
            .map(ToOwned::to_owned)
    })
}
