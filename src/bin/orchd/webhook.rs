use anyhow::{Context, Result, anyhow};
use axum::http::HeaderMap;
use chrono::Utc;
use hmac::{Hmac, Mac};
use sha2::{Digest, Sha256};

use super::lexicon::{
    DECISION_ACCEPTED, DECISION_IGNORED, EVENT_ISSUE_COMMENT, EVENT_ISSUES, directive_is_known,
};
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
        EVENT_ISSUE_COMMENT => payload.comment.as_ref().map(|comment| comment.body.clone()),
        EVENT_ISSUES => payload.issue.as_ref().and_then(|issue| issue.body.clone()),
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

pub(super) fn decide(
    event_type: &str,
    action: Option<&str>,
    context: Option<&EventContext>,
) -> DecisionRecord {
    let Some(context) = context else {
        return DecisionRecord {
            decision: DECISION_IGNORED.to_string(),
            reason_code: "missing_context".to_string(),
            directive: None,
            target_role: None,
            would_dispatch: false,
        };
    };

    if event_type == EVENT_ISSUE_COMMENT
        && context.text.as_deref().is_some_and(is_orchd_echo_comment)
    {
        return DecisionRecord {
            decision: DECISION_IGNORED.to_string(),
            reason_code: "orchd_echo_comment".to_string(),
            directive: None,
            target_role: None,
            would_dispatch: false,
        };
    }

    if !action_is_actionable(event_type, action) {
        return DecisionRecord {
            decision: DECISION_IGNORED.to_string(),
            reason_code: "unactionable_action".to_string(),
            directive: None,
            target_role: None,
            would_dispatch: false,
        };
    }

    if let Some(text) = context.text.as_deref()
        && let Some(parsed) = parse_directive(text)
    {
        return DecisionRecord {
            decision: DECISION_ACCEPTED.to_string(),
            reason_code: "explicit_directive".to_string(),
            directive: Some(parsed.directive),
            target_role: Some(parsed.role),
            would_dispatch: true,
        };
    }

    DecisionRecord {
        decision: DECISION_IGNORED.to_string(),
        reason_code: "no_directive".to_string(),
        directive: None,
        target_role: None,
        would_dispatch: false,
    }
}

fn action_is_actionable(event_type: &str, action: Option<&str>) -> bool {
    match event_type {
        EVENT_ISSUES => matches!(action, Some("opened" | "edited")),
        EVENT_ISSUE_COMMENT => matches!(action, Some("created" | "edited")),
        _ => false,
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

    if !directive_is_known(directive.as_str()) {
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

#[cfg(test)]
mod tests {
    use crate::orchd::lexicon::{
        DECISION_ACCEPTED, DECISION_IGNORED, DIRECTIVE_DESIGN, DIRECTIVE_POKE, EVENT_ISSUE_COMMENT,
        EVENT_ISSUES,
    };
    use crate::orchd::state::EventContext;

    use super::{decide, parse_directive};

    #[test]
    fn owner_comment_without_directive_is_ignored() {
        let context = EventContext {
            repo_full_name: "main/orchd-debug".to_string(),
            issue_number: Some(1),
            actor_login: Some("main".to_string()),
            text: Some("just checking in".to_string()),
            source_comment_id: None,
            source_created_at: None,
        };
        let decision = decide(EVENT_ISSUE_COMMENT, Some("created"), Some(&context));
        assert_eq!(decision.decision, DECISION_IGNORED);
        assert_eq!(decision.reason_code, "no_directive");
        assert!(!decision.would_dispatch);
    }

    #[test]
    fn explicit_directive_is_still_accepted() {
        let context = EventContext {
            repo_full_name: "main/orchd-debug".to_string(),
            issue_number: Some(1),
            actor_login: Some("main".to_string()),
            text: Some("@codex-orch poke".to_string()),
            source_comment_id: None,
            source_created_at: None,
        };
        let decision = decide(EVENT_ISSUE_COMMENT, Some("created"), Some(&context));
        assert_eq!(decision.decision, DECISION_ACCEPTED);
        assert_eq!(decision.reason_code, "explicit_directive");
        assert_eq!(decision.directive.as_deref(), Some(DIRECTIVE_POKE));
        assert_eq!(decision.target_role.as_deref(), Some("codex-orch"));
        assert!(decision.would_dispatch);
    }

    #[test]
    fn codex_alias_maps_to_orch_role() {
        let parsed = parse_directive("@codex poke").expect("directive should parse");
        assert_eq!(parsed.role, "codex-orch");
        assert_eq!(parsed.directive, DIRECTIVE_POKE);
    }

    #[test]
    fn issue_body_directive_is_ignored_for_label_updates() {
        let context = EventContext {
            repo_full_name: "main/forgejo-work".to_string(),
            issue_number: Some(16),
            actor_login: Some("codex-orch".to_string()),
            text: Some("@codex-orch design".to_string()),
            source_comment_id: None,
            source_created_at: None,
        };
        let decision = decide(EVENT_ISSUES, Some("label_updated"), Some(&context));
        assert_eq!(decision.decision, DECISION_IGNORED);
        assert_eq!(decision.reason_code, "unactionable_action");
        assert!(!decision.would_dispatch);
    }

    #[test]
    fn issue_body_directive_is_accepted_on_open() {
        let context = EventContext {
            repo_full_name: "main/forgejo-work".to_string(),
            issue_number: Some(16),
            actor_login: Some("main".to_string()),
            text: Some("@codex-orch design".to_string()),
            source_comment_id: None,
            source_created_at: None,
        };
        let decision = decide(EVENT_ISSUES, Some("opened"), Some(&context));
        assert_eq!(decision.decision, DECISION_ACCEPTED);
        assert_eq!(decision.reason_code, "explicit_directive");
        assert_eq!(decision.directive.as_deref(), Some(DIRECTIVE_DESIGN));
        assert_eq!(decision.target_role.as_deref(), Some("codex-orch"));
        assert!(decision.would_dispatch);
    }
}
