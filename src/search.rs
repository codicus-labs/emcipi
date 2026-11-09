//! Tool ranking for `search_tools`: fielded Okapi BM25, modelled on Pi's tool search
//! (see docs/research/tool-search.md).
//!
//! - name: binary hit, weighted `NAME_WEIGHT × idf`, so a name match beats any number of
//!   description mentions
//! - text: title, description and input-schema property names/descriptions, BM25 with
//!   length normalization
//! - namespace: server name, description and instructions, binary hit at `NAMESPACE_WEIGHT × idf`
//!
//! Results are ordered by exact name match, then by how many query terms a tool covers,
//! then by score.

use std::collections::{HashMap, HashSet};

use rmcp::model::{JsonObject, Tool};
use serde_json::Value;

const K1: f64 = 1.2;
const B: f64 = 0.75;
const NAME_WEIGHT: f64 = 3.0;
const NAMESPACE_WEIGHT: f64 = 0.5;
/// Weight of a prefix hit (`repo` → `repository`) for terms without any exact hit.
const PREFIX_WEIGHT: f64 = 0.5;
const PREFIX_MIN_LEN: usize = 4;
const MAX_SCHEMA_DEPTH: usize = 8;

const STOPWORDS: &[&str] = &[
    "a", "an", "and", "are", "as", "at", "be", "by", "for", "from", "in", "is", "it", "of", "on",
    "or", "that", "the", "this", "to", "with",
];

/// What the ranker sees of one tool.
pub struct Document<'a> {
    /// Qualified name, e.g. `github__create_issue`.
    pub name: &'a str,
    /// Upstream tool name, e.g. `create_issue`.
    pub local_name: &'a str,
    pub tool: &'a Tool,
    /// Server name, description and instructions.
    pub namespace: &'a str,
}

#[derive(Debug, Clone, Copy)]
pub struct Hit {
    pub index: usize,
    pub exact: bool,
    pub coverage: usize,
    pub score: f64,
}

struct Indexed {
    name: HashSet<String>,
    local: HashSet<String>,
    text: HashMap<String, u32>,
    text_len: usize,
    namespace: HashSet<String>,
}

impl Indexed {
    fn new(doc: &Document<'_>) -> Self {
        let mut text = HashMap::new();
        let mut text_len = 0;
        for term in tokenize(&document_text(doc.tool)) {
            *text.entry(term).or_insert(0) += 1;
            text_len += 1;
        }
        Self {
            name: tokenize(doc.name).into_iter().collect(),
            local: tokenize(doc.local_name).into_iter().collect(),
            text,
            text_len,
            namespace: tokenize(doc.namespace).into_iter().collect(),
        }
    }

    fn contains_exact(&self, term: &str) -> bool {
        self.name.contains(term) || self.text.contains_key(term) || self.namespace.contains(term)
    }

    fn contains_prefix(&self, prefix: &str) -> bool {
        let starts = |t: &String| t.starts_with(prefix);
        self.name.iter().any(starts)
            || self.text.keys().any(starts)
            || self.namespace.iter().any(starts)
    }
}

/// How a query term is matched against document terms.
#[derive(Clone, Copy)]
enum Match {
    Exact,
    Prefix,
}

impl Match {
    fn set(self, set: &HashSet<String>, term: &str) -> bool {
        match self {
            Match::Exact => set.contains(term),
            Match::Prefix => set.iter().any(|t| t.starts_with(term)),
        }
    }

    fn tf(self, text: &HashMap<String, u32>, term: &str) -> u32 {
        match self {
            Match::Exact => text.get(term).copied().unwrap_or(0),
            Match::Prefix => text
                .iter()
                .filter(|(t, _)| t.starts_with(term))
                .map(|(_, n)| n)
                .sum(),
        }
    }
}

/// Ranks `docs` for `query`. Only tools with a positive score are returned, best first.
pub fn rank(query: &str, docs: &[Document<'_>]) -> Vec<Hit> {
    let terms: Vec<String> = unique(tokenize(query));
    if terms.is_empty() || docs.is_empty() {
        return Vec::new();
    }
    let normalized = normalize_name(query);
    let indexed: Vec<Indexed> = docs.iter().map(Indexed::new).collect();
    let n = indexed.len() as f64;
    let avg_len = (indexed.iter().map(|d| d.text_len).sum::<usize>() as f64 / n).max(1.0);

    let matchers: Vec<(&str, Match, f64, f64)> = terms
        .iter()
        .filter_map(|term| {
            let exact_df = indexed.iter().filter(|d| d.contains_exact(term)).count();
            let (matcher, weight, df) = if exact_df > 0 {
                (Match::Exact, 1.0, exact_df)
            } else if term.chars().count() >= PREFIX_MIN_LEN {
                let df = indexed.iter().filter(|d| d.contains_prefix(term)).count();
                (Match::Prefix, PREFIX_WEIGHT, df)
            } else {
                return None;
            };
            (df > 0).then(|| (term.as_str(), matcher, weight, idf(n, df as f64)))
        })
        .collect();

    let mut hits: Vec<Hit> = indexed
        .iter()
        .enumerate()
        .filter_map(|(index, doc)| {
            let mut score = 0.0;
            let mut coverage = 0;
            let mut direct_hit = false;
            for &(term, matcher, weight, idf) in &matchers {
                let in_name = if matcher.set(&doc.name, term) {
                    NAME_WEIGHT
                } else {
                    0.0
                };
                let in_namespace = if matcher.set(&doc.namespace, term) {
                    NAMESPACE_WEIGHT
                } else {
                    0.0
                };
                let tf = matcher.tf(&doc.text, term) as f64;
                let norm = K1 * (1.0 - B + B * doc.text_len as f64 / avg_len);
                let in_text = tf * (K1 + 1.0) / (tf + norm);
                let term_score = in_name + in_namespace + in_text;
                direct_hit |= in_name + in_text > 0.0;
                if term_score > 0.0 {
                    coverage += 1;
                    score += idf * weight * term_score;
                }
            }
            // A query naming the tool's own words (e.g. "create issue") gets a bonus.
            let names_tool = !doc.local.is_empty() && doc.local.iter().all(|t| terms.contains(t));
            if names_tool {
                score += matchers
                    .iter()
                    .filter(|(t, ..)| doc.local.contains(*t))
                    .map(|(.., idf)| idf)
                    .sum::<f64>();
            }
            let name = docs[index].name.to_lowercase();
            let exact = normalized == name || normalized == docs[index].local_name.to_lowercase();
            // Matching only the server's name or description boosts, but never selects, a tool:
            // otherwise "issue" would return every tool of a server described as "Issues, …".
            ((direct_hit && score > 0.0) || exact).then_some(Hit {
                index,
                exact,
                coverage,
                score,
            })
        })
        .collect();

    hits.sort_by(|a, b| {
        b.exact
            .cmp(&a.exact)
            .then(b.coverage.cmp(&a.coverage))
            .then(b.score.total_cmp(&a.score))
            .then_with(|| docs[a.index].name.cmp(docs[b.index].name))
    });
    hits
}

fn idf(n: f64, df: f64) -> f64 {
    (1.0 + (n - df + 0.5) / (df + 0.5)).ln()
}

fn unique(terms: Vec<String>) -> Vec<String> {
    let mut seen = HashSet::new();
    terms
        .into_iter()
        .filter(|t| seen.insert(t.clone()))
        .collect()
}

/// Lowercase, everything outside `[a-z0-9_]` becomes `_`, for exact name comparison.
fn normalize_name(query: &str) -> String {
    query
        .trim()
        .to_lowercase()
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '_' {
                c
            } else {
                '_'
            }
        })
        .collect()
}

/// Splits camelCase/PascalCase and any non-alphanumeric separator, lowercases, drops
/// stopwords and reduces plurals to a naive singular. Used for queries and documents alike.
pub fn tokenize(text: &str) -> Vec<String> {
    split_camel_case(text)
        .split(|c: char| !c.is_alphanumeric())
        .filter(|t| !t.is_empty())
        .map(|t| strip_acronym_plural(t).to_lowercase())
        .filter(|t| !STOPWORDS.contains(&t.as_str()))
        .map(singular)
        .collect()
}

/// `PRs` → `PR`, `URLs` → `URL`: too short for [`singular`], but unambiguous.
fn strip_acronym_plural(token: &str) -> &str {
    match token.strip_suffix('s') {
        Some(stem) if stem.len() >= 2 && stem.chars().all(|c| c.is_ascii_uppercase()) => stem,
        _ => token,
    }
}

/// `getHTTPResponse` → `get HTTP Response`.
fn split_camel_case(text: &str) -> String {
    let chars: Vec<char> = text.chars().collect();
    let mut out = String::with_capacity(text.len() + 8);
    for (i, &c) in chars.iter().enumerate() {
        if i > 0 && c.is_uppercase() {
            let prev = chars[i - 1];
            // `PRs`/`URLs` are plural acronyms, not `P Rs`.
            let plural_s = chars.get(i + 1) == Some(&'s')
                && chars.get(i + 2).is_none_or(|n| !n.is_alphanumeric());
            let next_lower = !plural_s && chars.get(i + 1).is_some_and(|n| n.is_lowercase());
            if prev.is_lowercase() || prev.is_ascii_digit() || (prev.is_uppercase() && next_lower) {
                out.push(' ');
            }
        }
        out.push(c);
    }
    out
}

fn singular(word: String) -> String {
    if !word.is_ascii() {
        return word;
    }
    let len = word.len();
    if len > 4 && word.ends_with("ies") {
        return format!("{}y", &word[..len - 3]);
    }
    if len > 4
        && ["ches", "shes", "sses", "xes", "zes"]
            .iter()
            .any(|s| word.ends_with(s))
    {
        return word[..len - 2].to_owned();
    }
    let keeps_s = ["ss", "us", "is"].iter().any(|s| word.ends_with(s));
    if len > 3 && word.ends_with('s') && !keeps_s {
        return word[..len - 1].to_owned();
    }
    word
}

/// Title, description and everything the input schema says about its parameters.
fn document_text(tool: &Tool) -> String {
    let mut out = String::new();
    for part in [tool.title.as_deref(), tool.description.as_deref()]
        .into_iter()
        .flatten()
    {
        out.push_str(part);
        out.push(' ');
    }
    let root = Value::Object((*tool.input_schema).clone());
    let mut seen_refs = HashSet::new();
    schema_text(&root, &root, &mut out, &mut seen_refs, 0);
    out
}

fn schema_text(
    value: &Value,
    root: &Value,
    out: &mut String,
    seen_refs: &mut HashSet<String>,
    depth: usize,
) {
    let Some(obj) = value.as_object() else { return };
    if depth > MAX_SCHEMA_DEPTH {
        return;
    }
    if let Some(reference) = obj.get("$ref").and_then(Value::as_str)
        && let Some(pointer) = reference.strip_prefix('#')
        && seen_refs.insert(reference.to_owned())
        && let Some(target) = root.pointer(pointer)
    {
        schema_text(target, root, out, seen_refs, depth + 1);
    }
    if let Some(props) = obj.get("properties").and_then(Value::as_object) {
        for (name, prop) in props {
            out.push_str(name);
            out.push(' ');
            if let Some(desc) = prop.get("description").and_then(Value::as_str) {
                out.push_str(desc);
                out.push(' ');
            }
            schema_text(prop, root, out, seen_refs, depth + 1);
        }
    }
    for key in ["items", "additionalProperties"] {
        if let Some(inner) = obj.get(key) {
            schema_text(inner, root, out, seen_refs, depth + 1);
        }
    }
    for key in ["anyOf", "oneOf", "allOf"] {
        for inner in obj.get(key).and_then(Value::as_array).into_iter().flatten() {
            schema_text(inner, root, out, seen_refs, depth + 1);
        }
    }
}

/// `name(required, args, optional?)`: required parameters first, capped at ~120 chars.
pub fn signature(name: &str, schema: &JsonObject) -> String {
    const MAX: usize = 120;
    let props: Vec<&String> = schema
        .get("properties")
        .and_then(Value::as_object)
        .map(|p| p.keys().collect())
        .unwrap_or_default();
    let required: Vec<&str> = schema
        .get("required")
        .and_then(Value::as_array)
        .map(|r| r.iter().filter_map(Value::as_str).collect())
        .unwrap_or_default();
    let mut params: Vec<String> = required
        .iter()
        .filter(|r| props.iter().any(|p| p == *r))
        .map(|r| (*r).to_owned())
        .collect();
    params.extend(
        props
            .iter()
            .filter(|p| !required.contains(&p.as_str()))
            .map(|p| format!("{p}?")),
    );
    let mut out = format!("{name}(");
    for (i, param) in params.iter().enumerate() {
        let sep = if i == 0 { "" } else { ", " };
        if out.len() + sep.len() + param.len() > MAX {
            out.push_str(sep);
            out.push('…');
            break;
        }
        out.push_str(sep);
        out.push_str(param);
    }
    out.push(')');
    out
}

/// First line of a description, capped at `max` characters.
pub fn summary(description: &str, max: usize) -> String {
    let line = description
        .lines()
        .find(|l| !l.trim().is_empty())
        .unwrap_or("")
        .trim();
    if line.chars().count() <= max {
        return line.to_owned();
    }
    if let Some(i) = line.find(". ")
        && line[..=i].chars().count() <= max
    {
        return line[..=i].to_owned();
    }
    let cut: String = line.chars().take(max.saturating_sub(1)).collect();
    format!("{}…", cut.trim_end())
}

/// Up to `n` candidates closest to `wanted` by edit distance; `(qualified, local)` pairs
/// are compared by their closer form, so `create_isue` finds `github__create_issue`.
pub fn closest<'a>(wanted: &str, candidates: &[(&'a str, &'a str)], n: usize) -> Vec<&'a str> {
    let wanted = wanted.to_lowercase();
    let max_distance = (wanted.chars().count() / 2).max(2);
    let mut scored: Vec<(usize, &str)> = candidates
        .iter()
        .map(|(full, local)| {
            let d = levenshtein(&wanted, &full.to_lowercase())
                .min(levenshtein(&wanted, &local.to_lowercase()));
            (d, *full)
        })
        .filter(|(d, _)| *d <= max_distance)
        .collect();
    scored.sort();
    scored.into_iter().take(n).map(|(_, name)| name).collect()
}

fn levenshtein(a: &str, b: &str) -> usize {
    let b: Vec<char> = b.chars().collect();
    let mut prev: Vec<usize> = (0..=b.len()).collect();
    for (i, ca) in a.chars().enumerate() {
        let mut cur = vec![i + 1; b.len() + 1];
        for (j, cb) in b.iter().enumerate() {
            let cost = usize::from(ca != *cb);
            cur[j + 1] = (prev[j] + cost).min(prev[j + 1] + 1).min(cur[j] + 1);
        }
        prev = cur;
    }
    prev[b.len()]
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    fn tool(name: &str, desc: &str, schema: Value) -> Tool {
        let Value::Object(schema) = schema else {
            panic!()
        };
        Tool::new(name.to_owned(), desc.to_owned(), schema)
    }

    fn ranked(query: &str, tools: &[(&str, &str, Tool)]) -> Vec<String> {
        let docs: Vec<Document<'_>> = tools
            .iter()
            .map(|(name, ns, tool)| Document {
                name,
                local_name: name.split_once("__").map_or(*name, |(_, l)| l),
                tool,
                namespace: ns,
            })
            .collect();
        rank(query, &docs)
            .into_iter()
            .map(|h| docs[h.index].name.to_owned())
            .collect()
    }

    fn corpus() -> Vec<(&'static str, &'static str, Tool)> {
        let empty = || json!({"type": "object"});
        vec![
            (
                "github__create_issue",
                "github GitHub issues and pull requests",
                tool(
                    "create_issue",
                    "Create a new issue in a repository.",
                    json!({
                        "type": "object",
                        "properties": {"owner": {"type": "string"}, "repo": {"type": "string"}, "title": {"type": "string"}},
                        "required": ["owner", "repo", "title"]
                    }),
                ),
            ),
            (
                "github__list_pull_requests",
                "github GitHub issues and pull requests",
                tool(
                    "list_pull_requests",
                    "List pull requests. Mentions issue, issue, issue linking.",
                    empty(),
                ),
            ),
            (
                "github__get_status",
                "github GitHub issues and pull requests",
                tool(
                    "get_status",
                    "Get the combined CI status of a commit.",
                    empty(),
                ),
            ),
            (
                "browser__take_screenshot",
                "browser Browser automation",
                tool("take_screenshot", "Capture the page.", empty()),
            ),
            (
                "browser__resize_window",
                "browser Browser automation",
                tool(
                    "resize_window",
                    "Resize the browser window before a screenshot.",
                    empty(),
                ),
            ),
            (
                "fs__read_file",
                "fs Files",
                tool(
                    "read_file",
                    "Read a file.",
                    json!({
                        "type": "object",
                        "properties": {"path": {"$ref": "#/$defs/path"}},
                        "$defs": {"path": {"type": "object", "properties": {"encoding": {"description": "Text encoding like utf8"}}}}
                    }),
                ),
            ),
        ]
    }

    #[test]
    fn tokenizer_splits_and_normalizes() {
        assert_eq!(
            tokenize("open PRs, URLs and IDs"),
            ["open", "pr", "url", "id"]
        );
        assert_eq!(tokenize("HTTPServer"), ["http", "server"]);
        assert_eq!(tokenize("getHTTPResponse"), ["get", "http", "response"]);
        assert_eq!(tokenize("list_pull_requests"), ["list", "pull", "request"]);
        assert_eq!(
            tokenize("Search the Repositories"),
            ["search", "repository"]
        );
        assert_eq!(
            tokenize("status boxes class analysis"),
            ["status", "box", "class", "analysis"]
        );
        assert_eq!(
            tokenize("kebab-case.v2/path"),
            ["kebab", "case", "v2", "path"]
        );
    }

    #[test]
    fn name_match_beats_repeated_description_mentions() {
        let order = ranked("issue", &corpus());
        assert_eq!(order[0], "github__create_issue");
    }

    #[test]
    fn coverage_beats_single_strong_hit() {
        // Only resize_window mentions both words.
        assert_eq!(
            ranked("screenshot window", &corpus())[0],
            "browser__resize_window"
        );
    }

    #[test]
    fn exact_name_ranks_first() {
        assert_eq!(ranked("get_status", &corpus())[0], "github__get_status");
        assert_eq!(
            ranked("browser__take_screenshot", &corpus())[0],
            "browser__take_screenshot"
        );
    }

    #[test]
    fn schema_fields_and_refs_are_searchable() {
        assert_eq!(ranked("repo owner", &corpus())[0], "github__create_issue");
        assert_eq!(ranked("encoding", &corpus()), ["fs__read_file"]);
    }

    #[test]
    fn prefix_fallback_for_unknown_terms() {
        assert_eq!(
            ranked("screenshots capture", &corpus())[0],
            "browser__take_screenshot"
        );
        assert_eq!(ranked("brows", &corpus()).len(), 2);
    }

    #[test]
    fn namespace_alone_does_not_select() {
        // Every github tool has "issues" in its namespace, only create_issue in its name.
        assert_eq!(ranked("issue", &corpus())[..1], ["github__create_issue"]);
        assert!(!ranked("issue", &corpus()).contains(&"github__get_status".to_owned()));
    }

    #[test]
    fn no_match_returns_nothing() {
        assert!(ranked("kubernetes", &corpus()).is_empty());
        assert!(ranked("the of", &corpus()).is_empty());
    }

    #[test]
    fn signature_lists_required_first() {
        let schema = json!({
            "properties": {"body": {}, "title": {}, "owner": {}, "repo": {}},
            "required": ["owner", "repo", "title"]
        });
        let Value::Object(schema) = schema else {
            panic!()
        };
        assert_eq!(
            signature("gh__create_issue", &schema),
            "gh__create_issue(owner, repo, title, body?)"
        );
        assert_eq!(signature("x", &JsonObject::new()), "x()");
    }

    #[test]
    fn summary_caps_length() {
        assert_eq!(summary("Does X. More.\nSecond", 200), "Does X. More.");
        assert_eq!(
            summary("Short first. Then a very long sentence", 15),
            "Short first."
        );
        assert_eq!(summary("abcdefghij", 5), "abcd…");
    }

    #[test]
    fn closest_names() {
        let candidates = [
            ("github__create_issue", "create_issue"),
            ("github__close_issue", "close_issue"),
            ("fs__read_file", "read_file"),
        ];
        assert_eq!(
            closest("create_isue", &candidates, 3)[0],
            "github__create_issue"
        );
        assert_eq!(
            closest("github__create_issues", &candidates, 1),
            ["github__create_issue"]
        );
        assert!(closest("kubernetes_apply", &candidates, 3).is_empty());
    }
}
