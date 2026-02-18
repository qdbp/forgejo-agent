use anyhow::{Context, Result, anyhow};
use axum::http::HeaderMap;
use chrono::Utc;
use hmac::{Hmac, Mac};
use sha2::{Digest, Sha256};

use super::dispatch_config::{
    DispatchTriggerActorGuard, DispatchTriggerAssigneeGuard, DispatchTriggerClass,
    DispatchTriggerConfig, DispatchTriggerDirectiveGuard, DispatchTriggerDirectiveSource,
    DispatchTriggerPrincipalSource, DispatchTriggerRoleSource, legacy_trigger_pack,
};
use super::lexicon::{DECISION_ACCEPTED, EVENT_ISSUE_COMMENT, EVENT_ISSUES, directive_is_known};
use super::paths::expand_tilde_path;
use super::state::{DecisionRecord, EventContext, EventRecord, ParsedDirective, WebhookPayload};

type HmacSha256 = Hmac<Sha256>;

#[derive(Debug)]
struct TriggerCandidate {
    id: String,
    class: DispatchTriggerClass,
    priority: i32,
    order: usize,
    directive: String,
    target_role: String,
    principal_login: String,
    reason_code: String,
    apply_guardrails: bool,
}

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

    let assignees = payload
        .issue
        .as_ref()
        .map(|issue| {
            if let Some(assignees) = issue.assignees.as_ref() {
                assignees
                    .iter()
                    .map(|user| user.login.to_ascii_lowercase())
                    .collect::<Vec<_>>()
            } else if let Some(assignee) = issue.assignee.as_ref() {
                vec![assignee.login.to_ascii_lowercase()]
            } else {
                Vec::new()
            }
        })
        .unwrap_or_default();

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

    let source_issue_id = payload.issue.as_ref().and_then(|issue| issue.id);
    let source_issue_anchor_at = payload
        .issue
        .as_ref()
        .and_then(|issue| {
            issue
                .updated_at
                .clone()
                .or_else(|| issue.closed_at.clone())
                .or_else(|| issue.created_at.clone())
        })
        .or_else(|| source_created_at.clone());

    Some(EventContext {
        repo_full_name,
        issue_number,
        source_issue_id,
        source_issue_anchor_at,
        actor_login,
        text,
        source_comment_id,
        source_created_at,
        assignees,
    })
}

pub(super) fn decide(
    event_type: &str,
    action: Option<&str>,
    context: Option<&EventContext>,
    configured_triggers: Option<&[DispatchTriggerConfig]>,
) -> DecisionRecord {
    let Some(context) = context else {
        return DecisionRecord::ignored("missing_context");
    };

    let action = action.map(|value| value.trim().to_ascii_lowercase());
    let parsed_directive = context.text.as_deref().and_then(parse_directive);

    let fallback_legacy;
    let triggers: &[DispatchTriggerConfig] = if let Some(configured_triggers) = configured_triggers
    {
        configured_triggers
    } else {
        fallback_legacy = legacy_trigger_pack();
        fallback_legacy.as_slice()
    };

    let actionable = triggers.iter().any(|trigger| {
        trigger.matcher.event_type == event_type
            && action.as_deref().is_some_and(|candidate| {
                trigger.matcher.actions.iter().any(|item| item == candidate)
            })
    });
    if !actionable {
        return DecisionRecord::ignored("unactionable_action");
    }

    let mut best: Option<TriggerCandidate> = None;
    for (order, trigger) in triggers.iter().enumerate() {
        let Some(action_value) = action.as_deref() else {
            continue;
        };
        if trigger.matcher.event_type != event_type
            || !trigger
                .matcher
                .actions
                .iter()
                .any(|item| item == action_value)
        {
            continue;
        }

        if !trigger_guards_hold(trigger, context, parsed_directive.as_ref()) {
            continue;
        }

        let Some(directive) = resolve_trigger_directive(trigger, parsed_directive.as_ref()) else {
            continue;
        };
        let Some(target_role) = resolve_trigger_role(trigger, context, parsed_directive.as_ref())
        else {
            continue;
        };
        let Some(principal_login) = resolve_trigger_principal(trigger, context) else {
            continue;
        };

        let candidate = TriggerCandidate {
            id: trigger.id.clone(),
            class: trigger.class.clone(),
            priority: trigger.priority,
            order,
            directive,
            target_role,
            principal_login,
            reason_code: trigger.action.reason_code.clone(),
            apply_guardrails: trigger.apply_guardrails,
        };
        if prefer_candidate(&candidate, best.as_ref()) {
            best = Some(candidate);
        }
    }

    let Some(best) = best else {
        return DecisionRecord::ignored("no_directive");
    };

    DecisionRecord {
        decision: DECISION_ACCEPTED.to_string(),
        reason_code: best.reason_code,
        directive: Some(best.directive),
        target_role: Some(best.target_role),
        principal_login: Some(best.principal_login),
        would_dispatch: true,
        decision_source: match best.class {
            DispatchTriggerClass::ExplicitDirective => "explicit_directive".to_string(),
            DispatchTriggerClass::AssigneeReply => "assignee_reply".to_string(),
            DispatchTriggerClass::Registered => "registered_trigger".to_string(),
        },
        trigger_id: Some(best.id),
        trigger_dedupe_key: None,
        trigger_apply_guardrails: best.apply_guardrails,
    }
}

pub(super) fn trigger_dedupe_key(record: &EventRecord, trigger_id: &str) -> String {
    let subject_anchor = if let Some(comment_id) = record.source_comment_id {
        format!("comment:{comment_id}")
    } else if let Some(issue_id) = record.source_issue_id {
        let anchored_at = record.source_issue_anchor_at.as_deref().unwrap_or("-");
        let action = record.action.as_deref().unwrap_or("-");
        format!("issue:{issue_id}:{action}:{anchored_at}")
    } else if let Some(created_at) = record.source_created_at.as_deref() {
        format!("event_created_at:{created_at}")
    } else {
        let payload_hash = Sha256::digest(record.raw_json.as_bytes());
        format!("payload:{}", &hex::encode(payload_hash)[..20])
    };

    let issue_number = record
        .issue_number
        .map(|value| value.to_string())
        .unwrap_or_else(|| "-".to_string());
    let action = record.action.as_deref().unwrap_or("-");
    format!(
        "trigger:{trigger_id}:{}:{}:{}:{}:{subject_anchor}",
        record.repo_full_name, issue_number, record.event_type, action
    )
}

const fn prefer_candidate(
    candidate: &TriggerCandidate,
    current: Option<&TriggerCandidate>,
) -> bool {
    let Some(current) = current else {
        return true;
    };

    let candidate_rank = candidate.class.precedence_rank();
    let current_rank = current.class.precedence_rank();
    if candidate_rank != current_rank {
        return candidate_rank > current_rank;
    }
    if candidate.priority != current.priority {
        return candidate.priority > current.priority;
    }
    candidate.order < current.order
}

fn trigger_guards_hold(
    trigger: &DispatchTriggerConfig,
    context: &EventContext,
    parsed_directive: Option<&ParsedDirective>,
) -> bool {
    let guards = &trigger.guards;
    if guards.directive == DispatchTriggerDirectiveGuard::RequireParsed
        && parsed_directive.is_none()
    {
        return false;
    }
    if guards.directive == DispatchTriggerDirectiveGuard::RequireAbsent
        && parsed_directive.is_some()
    {
        return false;
    }

    let assignee = single_codex_assignee(context);
    if guards.assignee == DispatchTriggerAssigneeGuard::RequireSingleCodex && assignee.is_none() {
        return false;
    }
    if guards.actor == DispatchTriggerActorGuard::RequireNotAssignee {
        let Some(assignee) = assignee else {
            return false;
        };
        let actor = context
            .actor_login
            .as_deref()
            .unwrap_or_default()
            .to_ascii_lowercase();
        if actor.is_empty() || actor == assignee {
            return false;
        }
    }
    true
}

fn resolve_trigger_directive(
    trigger: &DispatchTriggerConfig,
    parsed_directive: Option<&ParsedDirective>,
) -> Option<String> {
    match &trigger.action.directive {
        DispatchTriggerDirectiveSource::Literal(directive) => Some(directive.clone()),
        DispatchTriggerDirectiveSource::ParsedDirective => {
            parsed_directive.map(|directive| directive.directive.to_ascii_lowercase())
        }
    }
}

fn resolve_trigger_role(
    trigger: &DispatchTriggerConfig,
    context: &EventContext,
    parsed_directive: Option<&ParsedDirective>,
) -> Option<String> {
    match &trigger.action.target_role {
        DispatchTriggerRoleSource::Literal(role) => Some(role.clone()),
        DispatchTriggerRoleSource::ParsedDirectiveRole => {
            parsed_directive.map(|directive| directive.role.to_ascii_lowercase())
        }
        DispatchTriggerRoleSource::SingleAssignee => single_codex_assignee(context),
    }
}

fn resolve_trigger_principal(
    trigger: &DispatchTriggerConfig,
    context: &EventContext,
) -> Option<String> {
    match &trigger.action.principal {
        DispatchTriggerPrincipalSource::EventActor => Some(
            context
                .actor_login
                .as_deref()
                .unwrap_or_default()
                .to_ascii_lowercase(),
        ),
        DispatchTriggerPrincipalSource::Literal(principal) => Some(principal.clone()),
        DispatchTriggerPrincipalSource::SingleAssignee => single_codex_assignee(context),
    }
}

fn single_codex_assignee(context: &EventContext) -> Option<String> {
    if context.assignees.len() != 1 {
        return None;
    }
    let assignee = context.assignees[0].trim().to_ascii_lowercase();
    if assignee.starts_with("codex-") {
        Some(assignee)
    } else {
        None
    }
}

pub(super) fn parse_directive(text: &str) -> Option<ParsedDirective> {
    text.lines().find_map(parse_directive_line)
}

fn parse_directive_line(line: &str) -> Option<ParsedDirective> {
    // Commands must start at the beginning of a line (no leading whitespace). We support two
    // syntaxes:
    // 1) `@codex-<role> [/<codex_profile>] <directive>` optionally followed by `,.;:` and then
    //    additional text.
    //    If additional text is present, the directive token must be suffixed with one of `,.;:`
    //    to avoid accidental parsing of normal prose.
    // 2) `cc @codex-<role>` as an alias for `reply` (optionally followed by more text).
    const TRAILER_PUNCT: &[char] = &[',', '.', ';', ':'];

    if line.is_empty() {
        return None;
    }

    // `cc @codex-orch ...` -> `@codex-orch reply ...`
    if line.len() >= 3
        && line.as_bytes()[0..2].eq_ignore_ascii_case(b"cc")
        && line.as_bytes()[2].is_ascii_whitespace()
    {
        let mut parts = line.split_whitespace();
        let cc_token = parts.next()?;
        if !cc_token.eq_ignore_ascii_case("cc") {
            return None;
        }
        let role_token = parts.next()?;
        if role_token.contains(['\'', '"']) {
            return None;
        }

        let role_token = role_token
            .trim_end_matches(|ch: char| TRAILER_PUNCT.contains(&ch))
            .trim_start_matches('@')
            .to_ascii_lowercase();
        let role = if role_token == "codex" {
            "codex-orch".to_string()
        } else if role_token.starts_with("codex-") {
            role_token
        } else {
            return None;
        };

        return Some(ParsedDirective {
            role,
            directive: "reply".to_string(),
            profile: None,
        });
    }

    if !line.starts_with('@') {
        return None;
    }

    let mut parts = line.split_whitespace();
    let role_token = parts.next()?;
    let token2 = parts.next()?;
    let (profile, directive_token) = if let Some(raw_profile) = token2.strip_prefix('/') {
        let profile = (!raw_profile.is_empty()
            && raw_profile
                .bytes()
                .all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, b'_' | b'-')))
        .then(|| raw_profile.to_string());
        let directive_token = parts.next()?;
        (profile, directive_token)
    } else {
        (None, token2)
    };
    let has_tail = parts.next().is_some();

    if role_token.contains(['\'', '"'])
        || directive_token.contains(['\'', '"'])
        || profile
            .as_deref()
            .is_some_and(|value| value.contains(['\'', '"']))
    {
        return None;
    }

    let directive_has_trailer_punct = directive_token
        .chars()
        .last()
        .is_some_and(|ch| TRAILER_PUNCT.contains(&ch));
    if has_tail && !directive_has_trailer_punct {
        return None;
    }

    let role_token = role_token
        .trim_end_matches(|ch: char| TRAILER_PUNCT.contains(&ch))
        .trim_start_matches('@')
        .to_ascii_lowercase();
    let directive = directive_token
        .trim_end_matches(|ch: char| TRAILER_PUNCT.contains(&ch))
        .to_ascii_lowercase();

    let role = if role_token == "codex" {
        "codex-orch".to_string()
    } else if role_token.starts_with("codex-") {
        role_token
    } else {
        return None;
    };

    // `poke` is a user-facing alias for the canonical `reply` directive.
    let directive = if directive == "poke" {
        "reply".to_string()
    } else {
        directive
    };

    if !directive_is_known(directive.as_str()) {
        return None;
    }

    Some(ParsedDirective {
        role,
        directive,
        profile,
    })
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
    use crate::orchd::dispatch_config::{
        DispatchTriggerAction, DispatchTriggerClass, DispatchTriggerConfig,
        DispatchTriggerDirectiveSource, DispatchTriggerGuards, DispatchTriggerMatcher,
        DispatchTriggerPrincipalSource, DispatchTriggerRoleSource,
    };
    use crate::orchd::lexicon::{DECISION_ACCEPTED, DECISION_IGNORED, DIRECTIVE_REPLY};
    use crate::orchd::state::{EventContext, EventRecord};

    use super::{decide, parse_directive, trigger_dedupe_key};

    #[test]
    fn owner_comment_without_directive_is_ignored() {
        let context = EventContext {
            repo_full_name: "main/orchd-debug".to_string(),
            issue_number: Some(1),
            source_issue_id: Some(99),
            source_issue_anchor_at: Some("2026-02-16T09:00:00Z".to_string()),
            actor_login: Some("main".to_string()),
            text: Some("just checking in".to_string()),
            source_comment_id: None,
            source_created_at: None,
            assignees: Vec::new(),
        };
        let decision = decide("issue_comment", Some("created"), Some(&context), None);
        assert_eq!(decision.decision, DECISION_IGNORED);
        assert_eq!(decision.reason_code, "no_directive");
        assert!(!decision.would_dispatch);
    }

    #[test]
    fn explicit_directive_is_still_accepted() {
        let context = EventContext {
            repo_full_name: "main/orchd-debug".to_string(),
            issue_number: Some(1),
            source_issue_id: Some(99),
            source_issue_anchor_at: Some("2026-02-16T09:00:00Z".to_string()),
            actor_login: Some("main".to_string()),
            text: Some("@codex-orch poke".to_string()),
            source_comment_id: Some(123),
            source_created_at: Some("2026-02-16T09:00:01Z".to_string()),
            assignees: Vec::new(),
        };
        let decision = decide("issue_comment", Some("created"), Some(&context), None);
        assert_eq!(decision.decision, DECISION_ACCEPTED);
        assert_eq!(decision.reason_code, "explicit_directive");
        assert_eq!(decision.directive.as_deref(), Some(DIRECTIVE_REPLY));
        assert_eq!(decision.target_role.as_deref(), Some("codex-orch"));
        assert!(decision.would_dispatch);
    }

    #[test]
    fn comment_without_directive_dispatches_reply_to_single_codex_assignee() {
        let context = EventContext {
            repo_full_name: "main/orchd-debug".to_string(),
            issue_number: Some(1),
            source_issue_id: Some(99),
            source_issue_anchor_at: Some("2026-02-16T09:00:00Z".to_string()),
            actor_login: Some("main".to_string()),
            text: Some("please take a look".to_string()),
            source_comment_id: Some(123),
            source_created_at: Some("2026-02-16T09:00:01Z".to_string()),
            assignees: vec!["codex-orch".to_string()],
        };
        let decision = decide("issue_comment", Some("created"), Some(&context), None);
        assert_eq!(decision.decision, DECISION_ACCEPTED);
        assert_eq!(decision.reason_code, "assignee_reply");
        assert_eq!(decision.directive.as_deref(), Some("reply"));
        assert_eq!(decision.target_role.as_deref(), Some("codex-orch"));
        assert!(decision.would_dispatch);
    }

    #[test]
    fn poke_alias_maps_to_reply_and_codex_alias_maps_to_orch_role() {
        let parsed = parse_directive("@codex poke").expect("directive should parse");
        assert_eq!(parsed.role, "codex-orch");
        assert_eq!(parsed.directive, DIRECTIVE_REPLY);
        assert_eq!(parsed.profile.as_deref(), None);
    }

    #[test]
    fn directive_with_trailing_punct_allows_same_line_text() {
        let parsed = parse_directive("@codex-orch reply: please review")
            .expect("directive with punctuation should parse");
        assert_eq!(parsed.role, "codex-orch");
        assert_eq!(parsed.directive, DIRECTIVE_REPLY);
        assert_eq!(parsed.profile.as_deref(), None);
    }

    #[test]
    fn directive_with_tail_without_punct_is_not_parsed() {
        assert!(parse_directive("@codex-orch reply please review").is_none());
    }

    #[test]
    fn directive_with_profile_parses() {
        let parsed = parse_directive("@codex-orch /opus45 impl").expect("profile directive parses");
        assert_eq!(parsed.role, "codex-orch");
        assert_eq!(parsed.directive, "impl");
        assert_eq!(parsed.profile.as_deref(), Some("opus45"));
    }

    #[test]
    fn directive_with_profile_and_tail_requires_punct() {
        assert!(parse_directive("@codex-orch /opus45 impl please").is_none());
    }

    #[test]
    fn directive_with_profile_and_tail_allows_punct() {
        let parsed = parse_directive("@codex-orch /opus45 impl: please")
            .expect("profile directive parses with punctuation");
        assert_eq!(parsed.role, "codex-orch");
        assert_eq!(parsed.directive, "impl");
        assert_eq!(parsed.profile.as_deref(), Some("opus45"));
    }

    #[test]
    fn cc_alias_maps_to_reply() {
        let parsed =
            parse_directive("cc @codex-orch please review").expect("cc alias should parse");
        assert_eq!(parsed.role, "codex-orch");
        assert_eq!(parsed.directive, DIRECTIVE_REPLY);
        assert_eq!(parsed.profile.as_deref(), None);
    }

    #[test]
    fn directive_requires_start_of_line() {
        assert!(parse_directive(" @codex-orch reply: please review").is_none());
    }

    #[test]
    fn issue_body_directive_is_ignored_for_label_updates() {
        let context = EventContext {
            repo_full_name: "main/forgejo-work".to_string(),
            issue_number: Some(16),
            source_issue_id: Some(99),
            source_issue_anchor_at: Some("2026-02-16T09:00:00Z".to_string()),
            actor_login: Some("codex-orch".to_string()),
            text: Some("@codex-orch design".to_string()),
            source_comment_id: None,
            source_created_at: None,
            assignees: Vec::new(),
        };
        let decision = decide("issues", Some("label_updated"), Some(&context), None);
        assert_eq!(decision.decision, DECISION_IGNORED);
        assert_eq!(decision.reason_code, "unactionable_action");
        assert!(!decision.would_dispatch);
    }

    #[test]
    fn issue_body_directive_is_accepted_on_open() {
        let context = EventContext {
            repo_full_name: "main/forgejo-work".to_string(),
            issue_number: Some(16),
            source_issue_id: Some(99),
            source_issue_anchor_at: Some("2026-02-16T09:00:00Z".to_string()),
            actor_login: Some("main".to_string()),
            text: Some("@codex-orch design".to_string()),
            source_comment_id: None,
            source_created_at: None,
            assignees: Vec::new(),
        };
        let decision = decide("issues", Some("opened"), Some(&context), None);
        assert_eq!(decision.decision, DECISION_ACCEPTED);
        assert_eq!(decision.reason_code, "explicit_directive");
        assert_eq!(decision.directive.as_deref(), Some("design"));
        assert_eq!(decision.target_role.as_deref(), Some("codex-orch"));
        assert!(decision.would_dispatch);
    }

    #[test]
    fn explicit_directive_precedes_registered_trigger() {
        let trigger = DispatchTriggerConfig {
            id: "custom.closed".to_string(),
            class: DispatchTriggerClass::Registered,
            priority: 999,
            matcher: DispatchTriggerMatcher {
                event_type: "issue_comment".to_string(),
                actions: vec!["created".to_string()],
            },
            guards: DispatchTriggerGuards::default(),
            action: DispatchTriggerAction {
                directive: DispatchTriggerDirectiveSource::Literal("reply".to_string()),
                target_role: DispatchTriggerRoleSource::Literal("codex-orch".to_string()),
                principal: DispatchTriggerPrincipalSource::EventActor,
                reason_code: "registered_trigger:custom.closed".to_string(),
            },
            apply_guardrails: true,
        };

        let context = EventContext {
            repo_full_name: "main/orchd-debug".to_string(),
            issue_number: Some(4),
            source_issue_id: Some(4),
            source_issue_anchor_at: Some("2026-02-16T09:00:00Z".to_string()),
            actor_login: Some("main".to_string()),
            text: Some("@codex-orch poke".to_string()),
            source_comment_id: Some(500),
            source_created_at: Some("2026-02-16T09:00:00Z".to_string()),
            assignees: Vec::new(),
        };

        let decision = decide(
            "issue_comment",
            Some("created"),
            Some(&context),
            Some(&[trigger]),
        );
        assert_eq!(decision.reason_code, "registered_trigger:custom.closed");

        let decision_with_legacy = decide("issue_comment", Some("created"), Some(&context), None);
        assert_eq!(decision_with_legacy.reason_code, "explicit_directive");
    }

    #[test]
    fn trigger_dedupe_key_uses_comment_id_when_present() {
        let record = EventRecord {
            delivery_id: "delivery-1".to_string(),
            event_type: "issue_comment".to_string(),
            repo_full_name: "main/forgejo-agent".to_string(),
            issue_number: Some(26),
            source_issue_id: Some(1234),
            source_issue_anchor_at: Some("2026-02-16T09:00:00Z".to_string()),
            action: Some("created".to_string()),
            actor_login: Some("main".to_string()),
            event_text: Some("hello".to_string()),
            source_comment_id: Some(4321),
            source_created_at: Some("2026-02-16T09:00:01Z".to_string()),
            raw_json: "{}".to_string(),
        };
        let key = trigger_dedupe_key(&record, "legacy.assignee.reply");
        assert!(key.contains("comment:4321"));
    }
}
