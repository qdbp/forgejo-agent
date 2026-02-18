use std::collections::{BTreeSet, HashMap};
use std::fs;
use std::io::Read;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use tracing::warn;

use forgejo_agent::types::RepoRef;

use super::dispatch_config::DispatchRepoBindingConfig;

const REPO_CONFIG_PATH: &str = ".orchd/config.toml";

const DEFAULT_MAX_DOC_BYTES: u64 = 256 * 1024;
const DEFAULT_MAX_TOTAL_BYTES: u64 = 1024 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub(super) enum DocKind {
    Include,
    Point,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub(super) enum DocImportance {
    Required,
    Recommended,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub(super) struct RelativePath(String);

impl RelativePath {
    pub(super) fn parse(raw: &str) -> Result<Self, String> {
        let raw = raw.trim();
        if raw.is_empty() {
            return Err("path is empty".to_string());
        }
        let path = Path::new(raw);
        if path.is_absolute() {
            return Err(format!("absolute paths are not allowed: {raw}"));
        }
        if raw.starts_with('~') {
            return Err(format!("tilde paths are not allowed: {raw}"));
        }
        for component in path.components() {
            match component {
                std::path::Component::CurDir => {
                    return Err(format!("relative path must not contain '.': {raw}"));
                }
                std::path::Component::ParentDir => {
                    return Err(format!("relative path must not contain '..': {raw}"));
                }
                std::path::Component::Normal(_) => {}
                std::path::Component::RootDir | std::path::Component::Prefix(_) => {
                    return Err(format!("unsupported path component in: {raw}"));
                }
            }
        }
        Ok(Self(raw.to_string()))
    }

    pub(super) const fn as_str(&self) -> &str {
        self.0.as_str()
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub(super) enum DocRef {
    Workdir { path: RelativePath },
    Repo { repo: RepoRef, path: RelativePath },
}

impl DocRef {
    pub(super) fn parse(raw: &str) -> Result<Self, String> {
        let raw = raw.trim();
        if raw.is_empty() {
            return Err("ref is empty".to_string());
        }

        if let Some(rest) = raw.strip_prefix("workdir:") {
            return Ok(Self::Workdir {
                path: RelativePath::parse(rest)?,
            });
        }

        if raw.starts_with("repo:") {
            let mut parts = raw.splitn(3, ':');
            let prefix = parts.next().unwrap_or_default();
            if prefix != "repo" {
                return Err(format!("unsupported ref prefix: {raw}"));
            }
            let repo = parts
                .next()
                .ok_or_else(|| format!("repo ref is missing owner/repo: {raw}"))?;
            let path = parts
                .next()
                .ok_or_else(|| format!("repo ref is missing path: {raw}"))?;
            let repo = RepoRef::parse(repo)
                .map_err(|err| format!("invalid repo in ref {raw}: {err:#}"))?;
            return Ok(Self::Repo {
                repo,
                path: RelativePath::parse(path)?,
            });
        }

        Err(format!(
            "unsupported ref format (expected workdir:... or repo:...): {raw}"
        ))
    }

    pub(super) fn display(&self) -> String {
        match self {
            Self::Workdir { path } => format!("workdir:{}", path.as_str()),
            Self::Repo { repo, path } => format!("repo:{repo}:{}", path.as_str()),
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct ReadingMaterialSpecSet {
    #[serde(default = "default_max_doc_bytes")]
    pub(super) max_doc_bytes: u64,
    #[serde(default = "default_max_total_bytes")]
    pub(super) max_total_bytes: u64,
    #[serde(default)]
    pub(super) rule: Vec<ReadingMaterialRuleSpec>,
}

const fn default_max_doc_bytes() -> u64 {
    DEFAULT_MAX_DOC_BYTES
}

const fn default_max_total_bytes() -> u64 {
    DEFAULT_MAX_TOTAL_BYTES
}

impl Default for ReadingMaterialSpecSet {
    fn default() -> Self {
        Self {
            max_doc_bytes: default_max_doc_bytes(),
            max_total_bytes: default_max_total_bytes(),
            rule: Vec::new(),
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct ReadingMaterialRuleSpec {
    pub(super) kind: String,
    #[serde(rename = "ref")]
    pub(super) r#ref: String,
    #[serde(default = "default_selector_any")]
    pub(super) roles: Vec<String>,
    #[serde(default = "default_selector_any")]
    pub(super) directives: Vec<String>,
    #[serde(default)]
    pub(super) order: i64,
    #[serde(default = "default_importance")]
    pub(super) importance: String,
    #[serde(default)]
    pub(super) max_bytes: Option<u64>,
}

fn default_selector_any() -> Vec<String> {
    vec!["*".to_string()]
}

fn default_importance() -> String {
    "recommended".to_string()
}

#[derive(Debug, Clone)]
struct SelectorSet {
    any: bool,
    values: BTreeSet<String>,
}

impl SelectorSet {
    fn parse(raw: &[String], label: &str) -> Result<Self, String> {
        if raw.is_empty() {
            return Err(format!("{label} selector list is empty"));
        }
        let mut values = BTreeSet::new();
        for item in raw {
            let item = item.trim().to_ascii_lowercase();
            if item.is_empty() {
                return Err(format!("{label} selector contains empty value"));
            }
            if item == "*" {
                return Ok(Self {
                    any: true,
                    values: BTreeSet::new(),
                });
            }
            values.insert(item);
        }
        Ok(Self { any: false, values })
    }

    fn matches(&self, value: &str) -> bool {
        if self.any {
            return true;
        }
        self.values.contains(&value.trim().to_ascii_lowercase())
    }
}

#[derive(Debug, Clone)]
struct ReadingMaterialRule {
    kind: DocKind,
    doc_ref: DocRef,
    roles: SelectorSet,
    directives: SelectorSet,
    order: i64,
    importance: DocImportance,
    max_bytes: Option<u64>,
}

impl ReadingMaterialRuleSpec {
    fn parse(&self, source: &str) -> Result<ReadingMaterialRule, String> {
        let kind = match self.kind.trim().to_ascii_lowercase().as_str() {
            "include" => DocKind::Include,
            "point" => DocKind::Point,
            other => return Err(format!("{source}: unknown kind '{other}'")),
        };
        let importance = match self.importance.trim().to_ascii_lowercase().as_str() {
            "required" => DocImportance::Required,
            "recommended" => DocImportance::Recommended,
            other => return Err(format!("{source}: unknown importance '{other}'")),
        };
        let doc_ref =
            DocRef::parse(&self.r#ref).map_err(|err| format!("{source}: invalid ref: {err}"))?;
        let roles =
            SelectorSet::parse(&self.roles, "roles").map_err(|err| format!("{source}: {err}"))?;
        let directives = SelectorSet::parse(&self.directives, "directives")
            .map_err(|err| format!("{source}: {err}"))?;
        Ok(ReadingMaterialRule {
            kind,
            doc_ref,
            roles,
            directives,
            order: self.order,
            importance,
            max_bytes: self.max_bytes,
        })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize)]
#[serde(rename_all = "lowercase")]
enum RuleLayer {
    Global,
    Repo,
}

#[derive(Debug, Clone, Serialize)]
pub(super) struct DocPlan {
    version: u32,
    role: String,
    directive: String,
    prompt_mode: String,
    budgets: DocBudgets,
    docs: Vec<DocPlanDoc>,
    warnings: Vec<String>,
}

#[derive(Debug, Clone, Copy, Serialize)]
struct DocBudgets {
    max_doc_bytes: u64,
    max_total_bytes: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub(super) enum DocDisposition {
    Included,
    Pointed,
    Omitted,
}

#[derive(Debug, Clone, Serialize)]
pub(super) struct DocPlanDoc {
    layer: RuleLayer,
    order: i64,
    desired_kind: DocKind,
    disposition: DocDisposition,
    importance: DocImportance,
    doc_ref: String,
    resolved_path: Option<String>,
    bytes: Option<u64>,
    sha256: Option<String>,
    downgraded_reason: Option<String>,
    omitted_reason: Option<String>,
}

#[derive(Debug, Clone)]
struct ResolvedDoc {
    abs_path: PathBuf,
    bytes: u64,
    sha256: String,
}

fn sha256_file(path: &Path) -> Result<(u64, String), String> {
    let mut file = fs::File::open(path).map_err(|err| format!("open failed: {err}"))?;
    let mut hasher = Sha256::new();
    let mut buf = [0u8; 8192];
    let mut total: u64 = 0;
    loop {
        let n = file
            .read(&mut buf)
            .map_err(|err| format!("read failed: {err}"))?;
        if n == 0 {
            break;
        }
        total = total.saturating_add(n as u64);
        hasher.update(&buf[..n]);
    }
    let digest = hasher.finalize();
    Ok((total, hex::encode(digest)))
}

fn resolve_doc_ref(
    repo_root: &Path,
    repo_bindings: &HashMap<String, DispatchRepoBindingConfig>,
    doc_ref: &DocRef,
) -> Result<PathBuf, String> {
    match doc_ref {
        DocRef::Workdir { path } => Ok(repo_root.join(path.as_str())),
        DocRef::Repo { repo, path } => {
            let binding = repo_bindings.get(&repo.to_string()).ok_or_else(|| {
                format!(
                    "repo ref requires existing repo_bindings entry for {}",
                    repo
                )
            })?;
            Ok(binding.local_path.join(path.as_str()))
        }
    }
}

fn try_resolve_doc(
    repo_root: &Path,
    repo_bindings: &HashMap<String, DispatchRepoBindingConfig>,
    doc_ref: &DocRef,
) -> Result<ResolvedDoc, String> {
    let abs_path = resolve_doc_ref(repo_root, repo_bindings, doc_ref)?;
    let meta = fs::metadata(&abs_path)
        .map_err(|err| format!("stat failed for {}: {err}", abs_path.display()))?;
    if !meta.is_file() {
        return Err(format!(
            "path is not a regular file: {}",
            abs_path.display()
        ));
    }
    let bytes = meta.len();
    let (read_bytes, sha256) = sha256_file(&abs_path)?;
    if read_bytes != bytes {
        return Err(format!(
            "hash read bytes ({read_bytes}) did not match metadata len ({bytes}) for {}",
            abs_path.display()
        ));
    }
    Ok(ResolvedDoc {
        abs_path,
        bytes,
        sha256,
    })
}

fn read_doc_utf8(path: &Path) -> Result<String, String> {
    fs::read_to_string(path).map_err(|err| format!("utf8 read failed: {err}"))
}

fn min_budget(a: u64, b: u64) -> u64 {
    a.min(b)
}

fn load_repo_config(repo_root: &Path) -> Result<Option<ReadingMaterialSpecSet>, String> {
    let path = repo_root.join(REPO_CONFIG_PATH);
    if !path.exists() {
        return Ok(None);
    }
    let text = fs::read_to_string(&path)
        .map_err(|err| format!("read failed {}: {err}", path.display()))?;
    let root: RepoOrchdConfigFile =
        toml::from_str(&text).map_err(|err| format!("invalid {}: {err}", path.display()))?;
    Ok(root.docs)
}

#[derive(Debug, Deserialize)]
struct RepoOrchdConfigFile {
    #[serde(default)]
    docs: Option<ReadingMaterialSpecSet>,
    #[serde(flatten)]
    _unknown: HashMap<String, toml::Value>,
}

#[derive(Debug, Clone)]
pub(super) struct ReadingMaterialOutcome {
    pub(super) markdown: String,
    pub(super) doc_plan: DocPlan,
}

pub(super) fn build_reading_material(
    global: &ReadingMaterialSpecSet,
    role: &str,
    directive: &str,
    prompt_mode: &str,
    repo_root: &Path,
    repo_bindings: &HashMap<String, DispatchRepoBindingConfig>,
) -> ReadingMaterialOutcome {
    let mut warnings = Vec::new();

    let role = role.trim().to_ascii_lowercase();
    let directive = directive.trim().to_ascii_lowercase();
    if prompt_mode != "fresh" {
        return ReadingMaterialOutcome {
            markdown: String::new(),
            doc_plan: DocPlan {
                version: 1,
                role,
                directive,
                prompt_mode: prompt_mode.to_string(),
                budgets: DocBudgets {
                    max_doc_bytes: global.max_doc_bytes,
                    max_total_bytes: global.max_total_bytes,
                },
                docs: Vec::new(),
                warnings,
            },
        };
    }

    let repo = match load_repo_config(repo_root) {
        Ok(cfg) => cfg,
        Err(err) => {
            warnings.push(format!("repo config: {err}"));
            None
        }
    };

    let empty_repo_cfg = ReadingMaterialSpecSet::default();
    let repo_cfg = repo.as_ref().unwrap_or(&empty_repo_cfg);
    let budgets = DocBudgets {
        max_doc_bytes: min_budget(global.max_doc_bytes, repo_cfg.max_doc_bytes),
        max_total_bytes: min_budget(global.max_total_bytes, repo_cfg.max_total_bytes),
    };

    let mut rules = Vec::new();
    rules.extend(
        global
            .rule
            .iter()
            .enumerate()
            .map(|(idx, rule)| (RuleLayer::Global, idx, rule)),
    );
    rules.extend(
        repo_cfg
            .rule
            .iter()
            .enumerate()
            .map(|(idx, rule)| (RuleLayer::Repo, idx, rule)),
    );

    let mut parsed = Vec::new();
    for (layer, idx, spec) in rules {
        let source = format!("{layer:?} rule {idx}");
        match spec.parse(&source) {
            Ok(parsed_rule) => {
                if parsed_rule.roles.matches(&role) && parsed_rule.directives.matches(&directive) {
                    parsed.push((layer, parsed_rule));
                }
            }
            Err(err) => warnings.push(err),
        }
    }

    parsed.sort_by(|(layer_a, a), (layer_b, b)| {
        layer_a
            .cmp(layer_b)
            .then_with(|| a.order.cmp(&b.order))
            .then_with(|| a.doc_ref.display().cmp(&b.doc_ref.display()))
    });

    let mut seen = BTreeSet::new();
    let mut docs = Vec::new();
    let mut included_total: u64 = 0;
    for (layer, rule) in parsed {
        let key = rule.doc_ref.display();
        if !seen.insert(key.clone()) {
            continue;
        }

        let desired_kind = rule.kind;
        let mut disposition = DocDisposition::Omitted;
        let mut downgraded_reason = None;
        let mut omitted_reason = None;
        let mut resolved_path = None;
        let mut bytes = None;
        let mut sha256 = None;

        match try_resolve_doc(repo_root, repo_bindings, &rule.doc_ref) {
            Ok(doc) => {
                resolved_path = Some(doc.abs_path.to_string_lossy().into_owned());
                bytes = Some(doc.bytes);
                sha256 = Some(doc.sha256.clone());

                match desired_kind {
                    DocKind::Point => {
                        disposition = DocDisposition::Pointed;
                    }
                    DocKind::Include => {
                        let max_doc_bytes = rule.max_bytes.map_or(budgets.max_doc_bytes, |v| {
                            min_budget(v, budgets.max_doc_bytes)
                        });
                        if doc.bytes > max_doc_bytes {
                            disposition = DocDisposition::Pointed;
                            downgraded_reason = Some("doc_too_large".to_string());
                        } else if included_total.saturating_add(doc.bytes) > budgets.max_total_bytes
                        {
                            disposition = DocDisposition::Pointed;
                            downgraded_reason = Some("total_budget_exceeded".to_string());
                        } else {
                            disposition = DocDisposition::Included;
                            included_total = included_total.saturating_add(doc.bytes);
                        }
                    }
                }
            }
            Err(err) => {
                // Missing/unreadable docs are warned and omitted from the prompt.
                warnings.push(format!("{}: {}", rule.doc_ref.display(), err));
                omitted_reason = Some(err);
            }
        }

        docs.push(DocPlanDoc {
            layer,
            order: rule.order,
            desired_kind,
            disposition,
            importance: rule.importance,
            doc_ref: key,
            resolved_path,
            bytes,
            sha256,
            downgraded_reason,
            omitted_reason,
        });
    }

    let markdown = render_reading_material_markdown(&docs, repo_root, repo_bindings, &mut warnings);

    for warning in &warnings {
        warn!("reading material: {warning}");
    }

    ReadingMaterialOutcome {
        markdown,
        doc_plan: DocPlan {
            version: 1,
            role,
            directive,
            prompt_mode: prompt_mode.to_string(),
            budgets,
            docs,
            warnings,
        },
    }
}

#[derive(Debug, Clone)]
struct DocBlurb {
    title: String,
    summary: Option<String>,
}

fn render_reading_material_markdown(
    docs: &[DocPlanDoc],
    repo_root: &Path,
    repo_bindings: &HashMap<String, DispatchRepoBindingConfig>,
    warnings: &mut Vec<String>,
) -> String {
    use std::fmt::Write as _;

    let includes = docs
        .iter()
        .filter(|doc| doc.disposition == DocDisposition::Included && doc.resolved_path.is_some())
        .collect::<Vec<_>>();
    let points = docs
        .iter()
        .filter(|doc| doc.disposition == DocDisposition::Pointed && doc.resolved_path.is_some())
        .collect::<Vec<_>>();

    if includes.is_empty() && points.is_empty() {
        return String::new();
    }

    let mut out = String::new();
    out.push_str("## Reading material\n\n");

    if !includes.is_empty() {
        for doc in includes {
            let Some(path) = resolve_for_render(repo_root, repo_bindings, doc, warnings) else {
                continue;
            };
            let content = match read_doc_utf8(&path) {
                Ok(v) => v,
                Err(err) => {
                    warnings.push(format!("{}: {err}", doc.doc_ref));
                    continue;
                }
            };

            let blurb = doc_blurb(&doc.doc_ref, Some(&content));
            let _ = writeln!(&mut out, "### {}", blurb.title);
            let _ = writeln!(
                &mut out,
                "ref: {} (importance: {})",
                path.display(),
                doc_importance_str(doc.importance)
            );
            out.push('\n');

            let doc_title = doc_title_from_path(&path);
            let _ = writeln!(
                &mut out,
                "The document titled {doc_title} is included in the block below:"
            );
            let fence = backtick_fence(&content);
            let _ = writeln!(&mut out, "{fence}md");
            out.push_str(&content);
            if !content.ends_with('\n') {
                out.push('\n');
            }
            let _ = writeln!(&mut out, "{fence}");
            let _ = writeln!(&mut out, "End document\n");
        }
        out.push('\n');
    }

    if !points.is_empty() {
        out.push_str("### Pointers\n");
        for doc in points {
            let (path, content) = match resolve_for_render(repo_root, repo_bindings, doc, warnings)
            {
                Some(path) => match read_doc_utf8(&path) {
                    Ok(content) => (Some(path), Some(content)),
                    Err(err) => {
                        warnings.push(format!("{}: {err}", doc.doc_ref));
                        (Some(path), None)
                    }
                },
                None => (None, None),
            };
            let blurb = doc_blurb(&doc.doc_ref, content.as_deref());
            let ref_path = path
                .as_ref()
                .map(|p| p.to_string_lossy().into_owned())
                .unwrap_or_else(|| doc.doc_ref.clone());

            match blurb.summary {
                Some(summary) => {
                    let _ = writeln!(
                        &mut out,
                        "- {} (ref: {}, importance: {}): {}",
                        blurb.title,
                        ref_path,
                        doc_importance_str(doc.importance),
                        summary
                    );
                }
                None => {
                    let _ = writeln!(
                        &mut out,
                        "- {} (ref: {}, importance: {})",
                        blurb.title,
                        ref_path,
                        doc_importance_str(doc.importance)
                    );
                }
            }
        }
        out.push('\n');
    }

    out
}

const fn doc_importance_str(importance: DocImportance) -> &'static str {
    match importance {
        DocImportance::Required => "required",
        DocImportance::Recommended => "recommended",
    }
}

fn doc_blurb(doc_ref: &str, content: Option<&str>) -> DocBlurb {
    let fallback_title = fallback_title_from_ref(doc_ref);
    let Some(content) = content else {
        return DocBlurb {
            title: fallback_title,
            summary: None,
        };
    };

    let (title_opt, summary_start_line) = markdown_first_h1(content);
    let title = title_opt.unwrap_or(fallback_title);
    let summary = markdown_first_paragraph(content, summary_start_line)
        .and_then(|para| first_sentence(&para))
        .map(|s| truncate_chars(&s, 160));

    DocBlurb { title, summary }
}

fn fallback_title_from_ref(doc_ref: &str) -> String {
    let parsed = DocRef::parse(doc_ref).ok();
    let Some(parsed) = parsed else {
        return doc_ref.to_string();
    };
    let path = match &parsed {
        DocRef::Workdir { path } | DocRef::Repo { path, .. } => path.as_str(),
    };
    Path::new(path)
        .file_name()
        .and_then(|v| v.to_str())
        .unwrap_or(path)
        .to_string()
}

fn doc_title_from_path(path: &Path) -> String {
    if let Some(stem) = path.file_stem().and_then(|v| v.to_str()) {
        return stem.to_string();
    }
    if let Some(name) = path.file_name().and_then(|v| v.to_str()) {
        return name.to_string();
    }
    path.to_string_lossy().into_owned()
}

fn is_md_fence(line_trimmed: &str) -> bool {
    line_trimmed.starts_with("```") || line_trimmed.starts_with("~~~")
}

fn markdown_first_h1(content: &str) -> (Option<String>, usize) {
    let mut in_fence = false;
    let mut lines = content.lines().enumerate().peekable();
    while let Some((idx, raw_line)) = lines.next() {
        let line = raw_line.trim_end_matches('\r');
        let trimmed = line.trim_start();

        if is_md_fence(trimmed) {
            in_fence = !in_fence;
            continue;
        }
        if in_fence {
            continue;
        }

        if let Some(title) = md_atx_h1(trimmed) {
            return (Some(title), idx + 1);
        }

        if trimmed.is_empty() {
            continue;
        }

        // Setext H1: "Title\n====="
        if let Some((_, next_raw)) = lines.peek() {
            let next = next_raw.trim_end_matches('\r').trim_start();
            if md_setext_h1_underline(next) {
                // Consume underline.
                let _ = lines.next();
                return (Some(trimmed.to_string()), idx + 2);
            }
        }
    }

    (None, 0)
}

fn md_atx_h1(line_trimmed: &str) -> Option<String> {
    let rest = if let Some(rest) = line_trimmed.strip_prefix("# ") {
        rest
    } else if let Some(rest) = line_trimmed.strip_prefix("#\t") {
        rest
    } else {
        return None;
    };

    let title = rest.trim();
    if title.is_empty() {
        return None;
    }
    Some(title.to_string())
}

fn md_setext_h1_underline(line_trimmed: &str) -> bool {
    let line = line_trimmed.trim();
    !line.is_empty()
        && line.bytes().all(|b| b == b'=' || b == b' ' || b == b'\t')
        && line.bytes().any(|b| b == b'=')
}

fn markdown_first_paragraph(content: &str, start_line: usize) -> Option<String> {
    let mut in_fence = false;
    let mut found = Vec::new();
    for (idx, raw_line) in content.lines().enumerate() {
        if idx < start_line {
            continue;
        }

        let line = raw_line.trim_end_matches('\r');
        let trimmed = line.trim_start();

        if is_md_fence(trimmed) {
            in_fence = !in_fence;
            if !found.is_empty() {
                break;
            }
            continue;
        }
        if in_fence {
            continue;
        }

        if trimmed.is_empty() {
            if !found.is_empty() {
                break;
            }
            continue;
        }
        if trimmed.starts_with('#') || md_setext_h1_underline(trimmed) {
            if !found.is_empty() {
                break;
            }
            continue;
        }
        if trimmed.starts_with("- ")
            || trimmed.starts_with("* ")
            || trimmed.starts_with("+ ")
            || trimmed.starts_with("> ")
        {
            // Lists/quotes tend to be noisy as a one-line summary; keep optional summaries conservative.
            if !found.is_empty() {
                break;
            }
            continue;
        }

        found.push(trimmed.to_string());
    }

    if found.is_empty() {
        return None;
    }

    Some(found.join(" "))
}

fn first_sentence(text: &str) -> Option<String> {
    let normalized = text.split_whitespace().collect::<Vec<_>>().join(" ");
    if normalized.is_empty() {
        return None;
    }

    for (idx, ch) in normalized.char_indices() {
        if matches!(ch, '.' | '!' | '?') {
            return Some(normalized[..=idx].to_string());
        }
    }
    Some(normalized)
}

fn truncate_chars(text: &str, max: usize) -> String {
    if text.chars().count() <= max {
        return text.to_string();
    }
    let mut out = String::with_capacity(max + 3);
    out.extend(text.chars().take(max.saturating_sub(3)));
    out.push_str("...");
    out
}

fn backtick_fence(content: &str) -> String {
    let mut max_run = 0usize;
    for line in content.lines() {
        let mut bytes = line.as_bytes();
        if bytes.last().copied() == Some(b'\r') {
            bytes = &bytes[..bytes.len().saturating_sub(1)];
        }

        let mut i = 0usize;
        while i < bytes.len() && i < 3 && bytes[i] == b' ' {
            i += 1;
        }
        let mut j = i;
        while j < bytes.len() && bytes[j] == b'`' {
            j += 1;
        }
        let run = j.saturating_sub(i);
        if run < 3 {
            continue;
        }
        if bytes[j..].iter().all(|b| *b == b' ' || *b == b'\t') {
            max_run = max_run.max(run);
        }
    }

    let fence_len = max_run.saturating_add(1).max(3);
    "`".repeat(fence_len)
}

fn resolve_for_render(
    repo_root: &Path,
    repo_bindings: &HashMap<String, DispatchRepoBindingConfig>,
    doc: &DocPlanDoc,
    warnings: &mut Vec<String>,
) -> Option<PathBuf> {
    let doc_ref = match DocRef::parse(&doc.doc_ref) {
        Ok(v) => v,
        Err(err) => {
            warnings.push(format!("{}: {err}", doc.doc_ref));
            return None;
        }
    };
    match resolve_doc_ref(repo_root, repo_bindings, &doc_ref) {
        Ok(path) => Some(path),
        Err(err) => {
            warnings.push(format!("{}: {err}", doc.doc_ref));
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;
    use std::fs;

    use tempfile::TempDir;

    use super::{DocDisposition, DocRef, ReadingMaterialSpecSet, build_reading_material};

    fn write_file(root: &TempDir, rel: &str, body: &str) {
        let path = root.path().join(rel);
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).unwrap();
        }
        fs::write(path, body).unwrap();
    }

    #[test]
    fn doc_ref_parsing_rejects_parent_dir() {
        assert!(DocRef::parse("workdir:../secrets").is_err());
        assert!(DocRef::parse("repo:main/forgejo-agent:../secrets").is_err());
    }

    #[test]
    fn reading_material_includes_and_points_docs() {
        let tmp = TempDir::new().unwrap();
        write_file(&tmp, "docs/a.txt", "alpha\n");
        write_file(&tmp, "docs/b.txt", "beta\n");
        write_file(
            &tmp,
            ".orchd/config.toml",
            r#"
[docs]

[[docs.rule]]
kind = "include"
ref = "workdir:docs/a.txt"
roles = ["*"]
directives = ["impl"]
order = 10
importance = "required"

[[docs.rule]]
kind = "point"
ref = "workdir:docs/b.txt"
roles = ["*"]
directives = ["impl"]
order = 20
importance = "recommended"
"#,
        );

        let global = ReadingMaterialSpecSet::default();
        let outcome = build_reading_material(
            &global,
            "codex-dev",
            "impl",
            "fresh",
            tmp.path(),
            &HashMap::new(),
        );

        assert!(outcome.markdown.contains("## Reading material"));
        assert!(outcome.markdown.contains("### a.txt"));
        let a_path = tmp.path().join("docs/a.txt").to_string_lossy().into_owned();
        assert!(
            outcome
                .markdown
                .contains(&format!("ref: {a_path} (importance: required)"))
        );
        assert!(
            outcome
                .markdown
                .contains("The document titled a is included in the block below:")
        );
        assert!(outcome.markdown.contains("alpha"));
        assert!(outcome.markdown.contains("End document"));
        assert!(outcome.markdown.contains("### Pointers"));
        let b_path = tmp.path().join("docs/b.txt").to_string_lossy().into_owned();
        assert!(
            outcome
                .markdown
                .contains(&format!("b.txt (ref: {b_path}, importance: recommended)"))
        );

        let docs = &outcome.doc_plan.docs;
        assert_eq!(docs.len(), 2);
        assert_eq!(docs[0].disposition, DocDisposition::Included);
        assert_eq!(docs[1].disposition, DocDisposition::Pointed);
        assert!(docs[0].sha256.as_deref().unwrap_or_default().len() >= 64);
        assert!(docs[0].bytes.unwrap_or_default() > 0);
    }

    #[test]
    fn reading_material_renders_titles_and_safe_code_fences() {
        let tmp = TempDir::new().unwrap();
        write_file(
            &tmp,
            "docs/included.md",
            r"# Included Doc

This doc includes fenced code blocks.

```bash
echo hi
```
",
        );
        write_file(
            &tmp,
            "docs/pointer.md",
            r"# Pointer Doc

First paragraph is used as a summary. Second sentence.
",
        );
        write_file(
            &tmp,
            ".orchd/config.toml",
            r#"
[docs]

[[docs.rule]]
kind = "include"
ref = "workdir:docs/included.md"
roles = ["*"]
directives = ["impl"]
order = 10
importance = "required"

[[docs.rule]]
kind = "point"
ref = "workdir:docs/pointer.md"
roles = ["*"]
directives = ["impl"]
order = 20
importance = "recommended"
"#,
        );

        let global = ReadingMaterialSpecSet::default();
        let outcome = build_reading_material(
            &global,
            "codex-dev",
            "impl",
            "fresh",
            tmp.path(),
            &HashMap::new(),
        );

        assert!(outcome.markdown.contains("### Included Doc"));
        assert!(
            outcome
                .markdown
                .contains("The document titled included is included in the block below:")
        );
        // Nested ``` in markdown docs should not terminate the outer fence (````).
        assert!(outcome.markdown.contains("````md"));
        assert!(outcome.markdown.contains("End document"));
        assert!(outcome.markdown.contains("### Pointers"));
        let pointer_path = tmp
            .path()
            .join("docs/pointer.md")
            .to_string_lossy()
            .into_owned();
        assert!(outcome.markdown.contains(&format!(
            "Pointer Doc (ref: {pointer_path}, importance: recommended): First paragraph is used as a summary."
        )));
    }
}
