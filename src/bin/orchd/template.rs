use std::collections::HashSet;
use std::fs;
use std::path::Path;

use super::errors::DispatchError;

pub(super) fn unresolved_prompt_tokens(text: &str) -> Vec<String> {
    let mut tokens = Vec::new();
    let mut remainder = text;
    loop {
        let Some(start) = remainder.find("{{") else {
            break;
        };
        let after_start = &remainder[start + 2..];
        let Some(end) = after_start.find("}}") else {
            break;
        };
        let key = &after_start[..end];
        if !key.is_empty()
            && key
                .bytes()
                .all(|ch| ch.is_ascii_alphanumeric() || ch == b'_')
        {
            tokens.push(format!("{{{{{key}}}}}"));
        }
        remainder = &after_start[end + 2..];
    }
    tokens.sort();
    tokens.dedup();
    tokens
}

pub(super) fn render_prompt(
    template: &str,
    values: &[(&str, &str)],
) -> Result<String, DispatchError> {
    let provided_keys: HashSet<&str> = values.iter().map(|(key, _)| *key).collect();
    let unresolved: Vec<String> = unresolved_prompt_tokens(template)
        .into_iter()
        .filter(|token| {
            let key = token
                .strip_prefix("{{")
                .and_then(|value| value.strip_suffix("}}"))
                .unwrap_or_default();
            !provided_keys.contains(key)
        })
        .collect();
    if !unresolved.is_empty() {
        return Err(DispatchError::PromptTemplate(format!(
            "unresolved prompt tokens: {}",
            unresolved.join(", ")
        )));
    }

    let mut text = template.to_string();
    for (key, value) in values {
        let token = format!("{{{{{key}}}}}");
        text = text.replace(&token, value);
    }
    Ok(text)
}

pub(super) fn render_prompt_file(
    template_path: &Path,
    values: &[(&str, &str)],
    label: &str,
) -> Result<String, DispatchError> {
    let template = fs::read_to_string(template_path).map_err(|err| {
        DispatchError::Io(format!(
            "failed reading {label} template {}: {err}",
            template_path.display()
        ))
    })?;
    render_prompt(&template, values)
}
