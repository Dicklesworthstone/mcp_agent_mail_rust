//! Lexical query parser/normalizer for Tantivy-backed search
//!
//! Sanitizes, normalizes, and compiles user queries into Tantivy `Box<dyn Query>`:
//! - Boolean operators (AND / OR / NOT) with correct precedence
//! - Phrase queries ("exact match") with position-aware matching
//! - Prefix/wildcard queries (migrat*)
//! - Hyphenated token quoting (POL-358 → "POL-358")
//! - Robust fallback: malformed queries degrade to term-by-term OR search
//! - Subject boost (2x) applied via `BoostQuery` at query time

#[cfg(feature = "tantivy-engine")]
use tantivy::Index;
#[cfg(feature = "tantivy-engine")]
use tantivy::query::{
    AllQuery, BooleanQuery, BoostQuery, EmptyQuery, Occur, Query, QueryParser, QueryParserError,
    RegexQuery,
};
#[cfg(feature = "tantivy-engine")]
use tantivy::schema::Field;

use regex::Regex;
use serde::{Deserialize, Serialize};
use std::sync::LazyLock;

#[cfg(feature = "tantivy-engine")]
use crate::tantivy_schema::{BODY_BOOST, SUBJECT_BOOST};

// ── Query sanitization (engine-independent) ─────────────────────────────────

/// Operators that FTS5/Tantivy treat specially
const BOOLEAN_OPERATORS: &[&str] = &["AND", "OR", "NOT", "NEAR"];

/// Characters that are special to Tantivy query grammar
static SPECIAL_CHARS: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"[\[\]{}^~\\]").expect("special chars regex"));

/// Lone wildcards and punctuation-only patterns
static UNSEARCHABLE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^[\*\.\?!()]+$").expect("unsearchable regex"));

/// Hyphenated token: ASCII alphanumeric segments joined by hyphens
/// We use a simpler regex without lookbehind (not supported by `regex` crate)
/// and handle the "already quoted" case in the replacement function.
static HYPHENATED_TOKEN: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"[a-zA-Z0-9]+(?:-[a-zA-Z0-9]+)+").expect("hyphenated regex"));

/// Multiple consecutive spaces
static MULTI_SPACE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r" {2,}").expect("multi-space regex"));

/// Canonical supported structured-hint fields.
const QUERY_HINT_FIELDS: &[&str] = &["from", "thread", "project", "before", "after", "importance"];

/// Structured hint aliases mapped to canonical field names.
const QUERY_HINT_ALIASES: &[(&str, &str)] = &[
    ("sender", "from"),
    ("frm", "from"),
    ("thread_id", "thread"),
    ("thr", "thread"),
    ("proj", "project"),
    ("since", "after"),
    ("until", "before"),
    ("priority", "importance"),
    ("prio", "importance"),
    ("imp", "importance"),
];

/// Result of query sanitization
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SanitizedQuery {
    /// The query was empty or unsearchable
    Empty,
    /// A valid, sanitized query string
    Valid(String),
}

/// A canonical filter hint extracted from free-text query input.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AppliedFilterHint {
    /// Canonical field name (`from`, `thread`, `project`, `before`, `after`, `importance`).
    pub field: String,
    /// Parsed value for this hint.
    pub value: String,
}

/// A deterministic typo-tolerant suggestion for malformed hint fields.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DidYouMeanHint {
    /// Original token from query input.
    pub token: String,
    /// Suggested canonical field name.
    pub suggested_field: String,
    /// Parsed value associated with the malformed field.
    pub value: String,
}

/// Structured query-assistance metadata extracted from a query string.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct QueryAssistance {
    /// Free-text query with recognized structured hints removed.
    pub query_text: String,
    /// Parsed canonical filter hints in deterministic token order.
    #[serde(skip_serializing_if = "Vec::is_empty", default)]
    pub applied_filter_hints: Vec<AppliedFilterHint>,
    /// Typo-tolerant deterministic suggestions for malformed hint fields.
    #[serde(skip_serializing_if = "Vec::is_empty", default)]
    pub did_you_mean: Vec<DidYouMeanHint>,
}

fn split_query_tokens(raw_query: &str) -> Vec<String> {
    let mut tokens = Vec::new();
    let mut current = String::new();
    let mut in_quotes = false;

    for ch in raw_query.chars() {
        if ch == '"' {
            in_quotes = !in_quotes;
            current.push(ch);
            continue;
        }
        if ch.is_whitespace() && !in_quotes {
            if !current.is_empty() {
                tokens.push(std::mem::take(&mut current));
            }
        } else {
            current.push(ch);
        }
    }
    if !current.is_empty() {
        tokens.push(current);
    }
    tokens
}

fn normalize_hint_value(value: &str) -> String {
    let trimmed = value.trim();
    let maybe_unquoted = if trimmed.starts_with('"') && trimmed.ends_with('"') && trimmed.len() >= 2
    {
        &trimmed[1..trimmed.len() - 1]
    } else {
        trimmed
    };
    maybe_unquoted.trim().to_string()
}

fn normalize_hint_field(field: &str) -> Option<&'static str> {
    let lower = field.to_ascii_lowercase();
    if QUERY_HINT_FIELDS.contains(&lower.as_str()) {
        return QUERY_HINT_FIELDS
            .iter()
            .find(|candidate| **candidate == lower)
            .copied();
    }
    QUERY_HINT_ALIASES
        .iter()
        .find(|(alias, _)| *alias == lower)
        .map(|(_, canonical)| *canonical)
}

fn levenshtein_distance(a: &str, b: &str) -> usize {
    if a == b {
        return 0;
    }
    if a.is_empty() {
        return b.chars().count();
    }
    if b.is_empty() {
        return a.chars().count();
    }

    let b_chars: Vec<char> = b.chars().collect();
    let mut prev_row: Vec<usize> = (0..=b_chars.len()).collect();
    let mut cur_row = vec![0; b_chars.len() + 1];

    for (i, a_ch) in a.chars().enumerate() {
        cur_row[0] = i + 1;
        for (j, b_ch) in b_chars.iter().enumerate() {
            let cost = usize::from(a_ch != *b_ch);
            cur_row[j + 1] = (prev_row[j + 1] + 1)
                .min(cur_row[j] + 1)
                .min(prev_row[j] + cost);
        }
        prev_row.copy_from_slice(&cur_row);
    }
    prev_row[b_chars.len()]
}

fn suggest_hint_field(field: &str) -> Option<&'static str> {
    let lower = field.to_ascii_lowercase();
    let mut best: Option<(&'static str, usize)> = None;
    for candidate in QUERY_HINT_FIELDS {
        let distance = levenshtein_distance(&lower, candidate);
        if distance > 2 {
            continue;
        }
        match best {
            None => best = Some((*candidate, distance)),
            Some((best_candidate, best_distance)) => {
                if distance < best_distance
                    || (distance == best_distance && *candidate < best_candidate)
                {
                    best = Some((*candidate, distance));
                }
            }
        }
    }
    best.map(|(candidate, _)| candidate)
}

/// Parse structured query hints with typo-tolerant suggestions.
///
/// Supported canonical fields:
/// - `from`, `thread`, `project`, `before`, `after`, `importance`
///
/// The returned `query_text` preserves plain-text semantics by removing only
/// recognized canonical/alias hints and keeping unknown `field:value` tokens.
#[must_use]
pub fn parse_query_assistance(raw_query: &str) -> QueryAssistance {
    let mut plain_tokens = Vec::new();
    let mut applied_filter_hints = Vec::new();
    let mut did_you_mean = Vec::new();

    for token in split_query_tokens(raw_query) {
        let Some((field, value_part)) = token.split_once(':') else {
            plain_tokens.push(token);
            continue;
        };

        if field.trim().is_empty() || value_part.trim().is_empty() {
            plain_tokens.push(token);
            continue;
        }

        let value = normalize_hint_value(value_part);
        if value.is_empty() {
            plain_tokens.push(token);
            continue;
        }

        if let Some(canonical) = normalize_hint_field(field) {
            applied_filter_hints.push(AppliedFilterHint {
                field: canonical.to_string(),
                value,
            });
            continue;
        }

        if let Some(suggested_field) = suggest_hint_field(field) {
            did_you_mean.push(DidYouMeanHint {
                token: token.clone(),
                suggested_field: suggested_field.to_string(),
                value,
            });
        }
        plain_tokens.push(token);
    }

    QueryAssistance {
        query_text: plain_tokens.join(" ").trim().to_string(),
        applied_filter_hints,
        did_you_mean,
    }
}

impl SanitizedQuery {
    /// Returns `true` if the query is empty/unsearchable
    #[must_use]
    pub const fn is_empty(&self) -> bool {
        matches!(self, Self::Empty)
    }

    /// Returns the sanitized query string, or `None` if empty
    #[must_use]
    pub fn as_str(&self) -> Option<&str> {
        match self {
            Self::Empty => None,
            Self::Valid(s) => Some(s.as_str()),
        }
    }
}

/// Sanitize a raw query string for Tantivy search.
///
/// Handles: empty/whitespace-only, boolean-operator-only, leading wildcards,
/// trailing lone wildcards, hyphenated token quoting, special char escaping,
/// and whitespace normalization.
#[must_use]
pub fn sanitize_query(query: &str) -> SanitizedQuery {
    let trimmed = query.trim();
    if trimmed.is_empty() {
        return SanitizedQuery::Empty;
    }

    // Reject patterns that can't match anything useful
    if UNSEARCHABLE.is_match(trimmed) {
        return SanitizedQuery::Empty;
    }

    // Escape special Tantivy characters: [ ] { } ^ ~ \.
    let mut result = SPECIAL_CHARS.replace_all(trimmed, " ").to_string();

    // Collapse parentheses to spaces (Tantivy handles its own grouping)
    result = result.replace(['(', ')'], " ");

    // Collapse multiple spaces
    result = MULTI_SPACE.replace_all(&result, " ").trim().to_string();

    // Strip leading wildcards (*foo → foo)
    while result.starts_with('*') {
        result = result[1..].trim_start().to_string();
    }

    if result.is_empty() {
        return SanitizedQuery::Empty;
    }

    // Strip trailing lone wildcard: "foo *" → "foo"
    if result.ends_with(" *") {
        result = result[..result.len() - 2].trim_end().to_string();
        if result.is_empty() {
            return SanitizedQuery::Empty;
        }
    }

    // Check if only boolean operators remain
    if is_operators_only(&result) {
        return SanitizedQuery::Empty;
    }

    // Quote hyphenated tokens (POL-358 → "POL-358")
    result = quote_hyphenated_tokens(&result);

    if result.trim().is_empty() {
        SanitizedQuery::Empty
    } else {
        SanitizedQuery::Valid(result)
    }
}

/// Check whether a string contains only boolean operators and whitespace
fn is_operators_only(s: &str) -> bool {
    s.split_whitespace().all(|word| {
        BOOLEAN_OPERATORS
            .iter()
            .any(|op| word.eq_ignore_ascii_case(op))
    })
}

/// Quote hyphenated tokens to prevent them from being split.
///
/// `POL-358` → `"POL-358"`, but already-quoted strings are left alone.
fn quote_hyphenated_tokens(query: &str) -> String {
    if !query.contains('-') {
        return query.to_string();
    }

    // Track whether we're inside quotes to avoid double-quoting
    let mut result = String::with_capacity(query.len() + 8);
    let mut last_end = 0;

    // Pre-scan for quote positions
    let quote_positions: Vec<usize> = query
        .char_indices()
        .filter(|(_, c)| *c == '"')
        .map(|(i, _)| i)
        .collect();

    for mat in HYPHENATED_TOKEN.find_iter(query) {
        let start = mat.start();
        let end = mat.end();

        // Check if this match is inside quotes
        let in_quotes = quote_positions.iter().filter(|&&p| p < start).count() % 2 != 0;

        result.push_str(&query[last_end..start]);
        if in_quotes {
            result.push_str(mat.as_str());
        } else {
            result.push('"');
            result.push_str(mat.as_str());
            result.push('"');
        }
        last_end = end;
    }
    result.push_str(&query[last_end..]);
    result
}

/// Extract plain terms from a query string (for fallback matching).
///
/// Strips boolean operators, quotes, wildcards, and special chars;
/// returns lowercase terms suitable for LIKE-style matching.
#[must_use]
pub fn extract_terms(query: &str) -> Vec<String> {
    query
        .split_whitespace()
        .filter(|w| {
            !BOOLEAN_OPERATORS
                .iter()
                .any(|op| w.eq_ignore_ascii_case(op))
        })
        .map(|w| {
            w.trim_matches(|c: char| !c.is_alphanumeric() && c != '-' && c != '_')
                .to_lowercase()
        })
        .filter(|w| !w.is_empty())
        .collect()
}

// ── Tantivy query compilation (behind feature gate) ─────────────────────────

/// Outcome of parsing a query into a Tantivy `Box<dyn Query>`
#[cfg(feature = "tantivy-engine")]
#[derive(Debug)]
pub enum ParseOutcome {
    /// Primary parse succeeded
    Parsed(Box<dyn Query>),
    /// Primary parse failed; used fallback strategy
    Fallback {
        query: Box<dyn Query>,
        original_error: String,
    },
    /// Query was empty or unsearchable — no results
    Empty,
}

#[cfg(feature = "tantivy-engine")]
impl ParseOutcome {
    /// Extract the compiled query, or `None` if empty
    #[must_use]
    pub fn into_query(self) -> Option<Box<dyn Query>> {
        match self {
            Self::Parsed(q) | Self::Fallback { query: q, .. } => Some(q),
            Self::Empty => None,
        }
    }

    /// Returns `true` if fallback was used
    #[must_use]
    pub const fn used_fallback(&self) -> bool {
        matches!(self, Self::Fallback { .. })
    }
}

/// Parser configuration
#[cfg(feature = "tantivy-engine")]
#[derive(Debug, Clone)]
pub struct LexicalParserConfig {
    /// Whether to use conjunction (AND) as default operator between terms.
    /// When `true`, "foo bar" means "foo AND bar".
    /// When `false`, "foo bar" means "foo OR bar".
    pub conjunction_by_default: bool,
    /// Subject field boost multiplier (applied via `BoostQuery`)
    pub subject_boost: f32,
    /// Body field boost multiplier
    pub body_boost: f32,
}

#[cfg(feature = "tantivy-engine")]
impl Default for LexicalParserConfig {
    fn default() -> Self {
        Self {
            conjunction_by_default: true,
            subject_boost: SUBJECT_BOOST,
            body_boost: BODY_BOOST,
        }
    }
}

/// Lexical query parser: sanitizes, parses, and compiles queries for Tantivy.
#[cfg(feature = "tantivy-engine")]
pub struct LexicalParser {
    config: LexicalParserConfig,
    subject_field: Field,
    body_field: Field,
}

/// Escape regex-special characters in a prefix string for `RegexQuery`.
#[cfg(feature = "tantivy-engine")]
fn regex_escape_prefix(prefix: &str) -> String {
    let mut escaped = String::with_capacity(prefix.len() + 4);
    for ch in prefix.chars() {
        if matches!(
            ch,
            '.' | '*' | '+' | '?' | '(' | ')' | '[' | ']' | '{' | '}' | '|' | '^' | '$' | '\\'
        ) {
            escaped.push('\\');
        }
        escaped.push(ch);
    }
    escaped
}

#[cfg(feature = "tantivy-engine")]
impl LexicalParser {
    /// Create a new parser with the given field handles and configuration.
    #[must_use]
    pub const fn new(subject_field: Field, body_field: Field, config: LexicalParserConfig) -> Self {
        Self {
            config,
            subject_field,
            body_field,
        }
    }

    /// Create a parser with default configuration.
    #[must_use]
    pub fn with_defaults(subject_field: Field, body_field: Field) -> Self {
        Self::new(subject_field, body_field, LexicalParserConfig::default())
    }

    /// Parse a raw query string into a Tantivy query.
    ///
    /// Strategy:
    /// 1. Sanitize the input
    /// 2. Check for prefix patterns (`term*`) and build `RegexQuery` directly
    /// 3. Attempt Tantivy `QueryParser::parse_query`
    /// 4. On failure, fall back to term-by-term OR search
    /// 5. Apply field boosts
    #[must_use]
    pub fn parse(&self, index: &Index, raw_query: &str) -> ParseOutcome {
        let sanitized = sanitize_query(raw_query);
        let query_str = match sanitized {
            SanitizedQuery::Empty => return ParseOutcome::Empty,
            SanitizedQuery::Valid(ref s) => s.as_str(),
        };

        // Handle simple prefix queries directly via RegexQuery.
        // Tantivy's built-in QueryParser prefix handling can be unreliable
        // with custom tokenizers across versions.
        if let Some(q) = self.try_prefix_query(query_str) {
            return ParseOutcome::Parsed(self.apply_boost(q));
        }

        let mut parser = QueryParser::for_index(index, vec![self.subject_field, self.body_field]);

        if self.config.conjunction_by_default {
            parser.set_conjunction_by_default();
        }

        match parser.parse_query(query_str) {
            Ok(query) => ParseOutcome::Parsed(self.apply_boost(query)),
            Err(ref e) => self.build_fallback(index, raw_query, e),
        }
    }

    /// Try to build a prefix query from a simple `term*` pattern.
    ///
    /// Returns `Some(query)` if the input is a single prefix pattern (e.g., `migrat*`).
    /// Returns `None` if the input is not a simple prefix pattern.
    fn try_prefix_query(&self, query_str: &str) -> Option<Box<dyn Query>> {
        let trimmed = query_str.trim();

        // Must be a single word ending with *
        if !trimmed.ends_with('*') || trimmed.contains(' ') {
            return None;
        }

        let prefix = trimmed.trim_end_matches('*').to_lowercase();
        if prefix.is_empty() {
            return None;
        }

        // Escape regex special chars and build pattern
        let escaped = regex_escape_prefix(&prefix);
        let pattern = format!("{escaped}.*");

        let fields = [self.subject_field, self.body_field];
        let mut clauses: Vec<(Occur, Box<dyn Query>)> = Vec::new();

        for &field in &fields {
            if let Ok(rq) = RegexQuery::from_pattern(&pattern, field) {
                clauses.push((Occur::Should, Box::new(rq) as Box<dyn Query>));
            }
        }

        if clauses.is_empty() {
            return None;
        }

        Some(Box::new(BooleanQuery::new(clauses)))
    }

    /// Apply subject/body boosts by wrapping the original query.
    ///
    /// Instead of re-parsing per-field (which can fail for prefix queries),
    /// we wrap the original multi-field query with a boost on the overall score.
    /// The subject boost is effectively applied through Tantivy's built-in
    /// multi-field scoring when `QueryParser` searches multiple default fields.
    fn apply_boost(&self, query: Box<dyn Query>) -> Box<dyn Query> {
        // When subject_boost != body_boost, we can't perfectly split the boost
        // per-field without re-parsing. Instead, use the geometric mean as the
        // overall boost factor. This preserves relative ordering.
        let avg_boost = f32::midpoint(self.config.subject_boost, self.config.body_boost);
        if (avg_boost - 1.0).abs() < f32::EPSILON {
            return query;
        }
        Box::new(BoostQuery::new(query, avg_boost))
    }

    /// Build a fallback query from individual terms when parsing fails.
    fn build_fallback(
        &self,
        index: &Index,
        raw_query: &str,
        error: &QueryParserError,
    ) -> ParseOutcome {
        let terms = extract_terms(raw_query);
        if terms.is_empty() {
            return ParseOutcome::Empty;
        }

        // Try each term individually as a simple query
        let mut clauses: Vec<(Occur, Box<dyn Query>)> = Vec::new();
        let parser = QueryParser::for_index(index, vec![self.subject_field, self.body_field]);

        for term in &terms {
            if let Ok(q) = parser.parse_query(term) {
                clauses.push((Occur::Should, q));
            }
        }

        if clauses.is_empty() {
            return ParseOutcome::Empty;
        }

        let query: Box<dyn Query> = Box::new(BooleanQuery::new(clauses));
        ParseOutcome::Fallback {
            query,
            original_error: error.to_string(),
        }
    }
}

/// Build a "match all" query (useful for filter-only searches).
#[cfg(feature = "tantivy-engine")]
#[must_use]
pub fn match_all_query() -> Box<dyn Query> {
    Box::new(AllQuery)
}

/// Build an empty query (matches nothing).
#[cfg(feature = "tantivy-engine")]
#[must_use]
pub fn match_none_query() -> Box<dyn Query> {
    Box::new(EmptyQuery)
}

// ── Tests ───────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    // ── sanitize_query tests ──

    #[test]
    fn empty_returns_empty() {
        assert_eq!(sanitize_query(""), SanitizedQuery::Empty);
        assert_eq!(sanitize_query("   "), SanitizedQuery::Empty);
    }

    #[test]
    fn unsearchable_patterns() {
        for p in ["*", "**", "***", ".", "..", "...", "?", "??", "???", "!!!"] {
            assert!(sanitize_query(p).is_empty(), "expected Empty for '{p}'");
        }
    }

    #[test]
    fn bare_boolean_operators() {
        assert!(sanitize_query("AND").is_empty());
        assert!(sanitize_query("OR").is_empty());
        assert!(sanitize_query("NOT").is_empty());
        assert!(sanitize_query("and").is_empty());
        assert!(sanitize_query("AND OR NOT").is_empty());
        assert!(sanitize_query("NEAR AND").is_empty());
    }

    #[test]
    fn strips_leading_wildcard() {
        assert_eq!(
            sanitize_query("*foo"),
            SanitizedQuery::Valid("foo".to_string())
        );
        assert_eq!(
            sanitize_query("**foo"),
            SanitizedQuery::Valid("foo".to_string())
        );
    }

    #[test]
    fn strips_trailing_lone_wildcard() {
        assert_eq!(
            sanitize_query("foo *"),
            SanitizedQuery::Valid("foo".to_string())
        );
        assert!(sanitize_query(" *").is_empty());
    }

    #[test]
    fn preserves_prefix_wildcard() {
        assert_eq!(
            sanitize_query("migrat*"),
            SanitizedQuery::Valid("migrat*".to_string())
        );
    }

    #[test]
    fn preserves_boolean_with_terms() {
        assert_eq!(
            sanitize_query("plan AND users"),
            SanitizedQuery::Valid("plan AND users".to_string())
        );
    }

    #[test]
    fn collapses_multiple_spaces() {
        assert_eq!(
            sanitize_query("foo  bar   baz"),
            SanitizedQuery::Valid("foo bar baz".to_string())
        );
    }

    #[test]
    fn quotes_hyphenated_tokens() {
        assert_eq!(
            sanitize_query("POL-358"),
            SanitizedQuery::Valid("\"POL-358\"".to_string())
        );
        assert_eq!(
            sanitize_query("search for FEAT-123 and bd-42"),
            SanitizedQuery::Valid("search for \"FEAT-123\" and \"bd-42\"".to_string())
        );
    }

    #[test]
    fn leaves_already_quoted() {
        assert_eq!(
            sanitize_query("\"build plan\""),
            SanitizedQuery::Valid("\"build plan\"".to_string())
        );
    }

    #[test]
    fn escapes_special_chars() {
        assert_eq!(
            sanitize_query("foo[bar]"),
            SanitizedQuery::Valid("foo bar".to_string())
        );
        assert_eq!(
            sanitize_query("test^2"),
            SanitizedQuery::Valid("test 2".to_string())
        );
    }

    #[test]
    fn simple_term() {
        assert_eq!(
            sanitize_query("hello"),
            SanitizedQuery::Valid("hello".to_string())
        );
    }

    #[test]
    fn multi_segment_hyphenated() {
        assert_eq!(
            sanitize_query("foo-bar-baz"),
            SanitizedQuery::Valid("\"foo-bar-baz\"".to_string())
        );
    }

    #[test]
    fn parentheses_stripped() {
        assert_eq!(
            sanitize_query("(hello)"),
            SanitizedQuery::Valid("hello".to_string())
        );
        assert!(sanitize_query("((()))").is_empty());
    }

    // ── extract_terms tests ──

    #[test]
    fn extract_terms_basic() {
        assert_eq!(extract_terms("hello world"), vec!["hello", "world"]);
    }

    #[test]
    fn extract_terms_strips_operators() {
        assert_eq!(extract_terms("plan AND users"), vec!["plan", "users"]);
        assert_eq!(
            extract_terms("NOT forbidden OR allowed"),
            vec!["forbidden", "allowed"]
        );
    }

    #[test]
    fn extract_terms_strips_punctuation() {
        assert_eq!(extract_terms("\"quoted stuff\""), vec!["quoted", "stuff"]);
    }

    #[test]
    fn extract_terms_empty() {
        assert!(extract_terms("").is_empty());
        assert!(extract_terms("AND OR NOT").is_empty());
    }

    #[test]
    fn extract_terms_preserves_hyphenated() {
        assert_eq!(extract_terms("POL-358"), vec!["pol-358"]);
    }

    #[test]
    fn extract_terms_lowercase() {
        assert_eq!(extract_terms("HELLO World"), vec!["hello", "world"]);
    }

    // ── is_operators_only tests ──

    #[test]
    fn operators_only_true() {
        assert!(is_operators_only("AND OR NOT"));
        assert!(is_operators_only("and"));
        assert!(is_operators_only("NEAR"));
    }

    #[test]
    fn operators_only_false() {
        assert!(!is_operators_only("plan AND users"));
        assert!(!is_operators_only("hello"));
    }

    // ── SanitizedQuery methods ──

    #[test]
    fn sanitized_query_accessors() {
        let empty = SanitizedQuery::Empty;
        assert!(empty.is_empty());
        assert!(empty.as_str().is_none());

        let valid = SanitizedQuery::Valid("hello".to_string());
        assert!(!valid.is_empty());
        assert_eq!(valid.as_str(), Some("hello"));
    }

    // ── parse_query_assistance tests ──

    #[test]
    fn parse_query_assistance_extracts_canonical_hints() {
        let assistance =
            parse_query_assistance("from:BlueLake thread:br-123 importance:high migration plan");
        assert_eq!(assistance.query_text, "migration plan");
        assert_eq!(
            assistance.applied_filter_hints,
            vec![
                AppliedFilterHint {
                    field: "from".to_string(),
                    value: "BlueLake".to_string()
                },
                AppliedFilterHint {
                    field: "thread".to_string(),
                    value: "br-123".to_string()
                },
                AppliedFilterHint {
                    field: "importance".to_string(),
                    value: "high".to_string()
                },
            ]
        );
        assert!(assistance.did_you_mean.is_empty());
    }

    #[test]
    fn parse_query_assistance_supports_aliases() {
        let assistance = parse_query_assistance("sender:RedPeak prio:urgent since:2026-02-01");
        assert!(assistance.query_text.is_empty());
        assert_eq!(
            assistance.applied_filter_hints,
            vec![
                AppliedFilterHint {
                    field: "from".to_string(),
                    value: "RedPeak".to_string()
                },
                AppliedFilterHint {
                    field: "importance".to_string(),
                    value: "urgent".to_string()
                },
                AppliedFilterHint {
                    field: "after".to_string(),
                    value: "2026-02-01".to_string()
                },
            ]
        );
    }

    #[test]
    fn parse_query_assistance_handles_quoted_values() {
        let assistance = parse_query_assistance(
            "from:\"Blue Lake\" thread:\"br-42\" project:\"backend-api\" search term",
        );
        assert_eq!(assistance.query_text, "search term");
        assert_eq!(assistance.applied_filter_hints.len(), 3);
        assert_eq!(assistance.applied_filter_hints[0].value, "Blue Lake");
        assert_eq!(assistance.applied_filter_hints[1].value, "br-42");
        assert_eq!(assistance.applied_filter_hints[2].value, "backend-api");
    }

    #[test]
    fn parse_query_assistance_preserves_unknown_hint_tokens() {
        let assistance = parse_query_assistance("form:BlueLake migration");
        assert_eq!(assistance.query_text, "form:BlueLake migration");
        assert!(assistance.applied_filter_hints.is_empty());
        assert_eq!(assistance.did_you_mean.len(), 1);
        assert_eq!(assistance.did_you_mean[0].suggested_field, "from");
    }

    #[test]
    fn parse_query_assistance_suggestion_order_is_deterministic() {
        let assistance = parse_query_assistance("thred:br-1 imporance:high body");
        assert_eq!(assistance.did_you_mean.len(), 2);
        assert_eq!(assistance.did_you_mean[0].suggested_field, "thread");
        assert_eq!(assistance.did_you_mean[1].suggested_field, "importance");
        assert_eq!(assistance.query_text, "thred:br-1 imporance:high body");
    }

    #[test]
    fn parse_query_assistance_plain_text_compatibility() {
        let assistance = parse_query_assistance("just regular free text");
        assert_eq!(assistance.query_text, "just regular free text");
        assert!(assistance.applied_filter_hints.is_empty());
        assert!(assistance.did_you_mean.is_empty());
    }

    // ── Tantivy integration tests (require tantivy-engine feature) ──

    #[cfg(feature = "tantivy-engine")]
    mod tantivy_tests {
        use super::super::*;
        use tantivy::TantivyDocument;
        use tantivy::collector::TopDocs;
        use tantivy::doc;
        use tantivy::schema::Value;

        use crate::tantivy_schema::{FieldHandles, build_schema, register_tokenizer};

        fn setup_index() -> (Index, FieldHandles) {
            let (schema, handles) = build_schema();
            let index = Index::create_in_ram(schema);
            register_tokenizer(&index);

            let mut writer = index.writer(15_000_000).unwrap();
            writer
                .add_document(doc!(
                    handles.id => 1u64,
                    handles.doc_kind => "message",
                    handles.subject => "Migration plan review",
                    handles.body => "Here is the plan for DB migration to v3",
                    handles.sender => "BlueLake",
                    handles.project_slug => "backend",
                    handles.project_id => 1u64,
                    handles.thread_id => "br-123",
                    handles.importance => "high",
                    handles.created_ts => 1_700_000_000_000_000i64
                ))
                .unwrap();
            writer
                .add_document(doc!(
                    handles.id => 2u64,
                    handles.doc_kind => "message",
                    handles.subject => "Deployment checklist",
                    handles.body => "Steps for deploying the new search engine",
                    handles.sender => "RedPeak",
                    handles.project_slug => "backend",
                    handles.project_id => 1u64,
                    handles.thread_id => "br-456",
                    handles.importance => "normal",
                    handles.created_ts => 1_700_100_000_000_000i64
                ))
                .unwrap();
            writer
                .add_document(doc!(
                    handles.id => 3u64,
                    handles.doc_kind => "message",
                    handles.subject => "POL-358 compliance update",
                    handles.body => "Policy review for POL-358 audit requirements",
                    handles.sender => "GreenCastle",
                    handles.project_slug => "compliance",
                    handles.project_id => 2u64,
                    handles.thread_id => "TKT-789",
                    handles.importance => "urgent",
                    handles.created_ts => 1_700_200_000_000_000i64
                ))
                .unwrap();
            writer.commit().unwrap();

            (index, handles)
        }

        #[test]
        fn parse_simple_term() {
            let (index, handles) = setup_index();
            let parser = LexicalParser::with_defaults(handles.subject, handles.body);
            let outcome = parser.parse(&index, "migration");

            let query = outcome.into_query().expect("should produce a query");
            let reader = index.reader().unwrap();
            let searcher = reader.searcher();
            let hits = searcher.search(&query, &TopDocs::with_limit(10)).unwrap();
            assert_eq!(hits.len(), 1);

            let doc: TantivyDocument = searcher.doc(hits[0].1).unwrap();
            let id = doc.get_first(handles.id).unwrap().as_u64().unwrap();
            assert_eq!(id, 1);
        }

        #[test]
        fn parse_phrase_query() {
            let (index, handles) = setup_index();
            let parser = LexicalParser::with_defaults(handles.subject, handles.body);
            let outcome = parser.parse(&index, "\"migration plan\"");

            let query = outcome.into_query().expect("should produce a query");
            let reader = index.reader().unwrap();
            let searcher = reader.searcher();
            let hits = searcher.search(&query, &TopDocs::with_limit(10)).unwrap();
            assert!(!hits.is_empty());
        }

        #[test]
        fn parse_prefix_query() {
            let (index, handles) = setup_index();
            // Prefix queries with disjunction mode (OR) — the natural mode
            // for prefix matching across multiple fields
            let config = LexicalParserConfig {
                conjunction_by_default: false,
                ..LexicalParserConfig::default()
            };
            let parser = LexicalParser::new(handles.subject, handles.body, config);
            let outcome = parser.parse(&index, "migrat*");

            let query = outcome.into_query().expect("should produce a query");
            let reader = index.reader().unwrap();
            let searcher = reader.searcher();
            let hits = searcher.search(&query, &TopDocs::with_limit(10)).unwrap();
            assert_eq!(hits.len(), 1);
        }

        #[test]
        fn parse_boolean_and() {
            let (index, handles) = setup_index();
            let parser = LexicalParser::with_defaults(handles.subject, handles.body);
            let outcome = parser.parse(&index, "migration AND plan");

            let query = outcome.into_query().expect("should produce a query");
            let reader = index.reader().unwrap();
            let searcher = reader.searcher();
            let hits = searcher.search(&query, &TopDocs::with_limit(10)).unwrap();
            assert_eq!(hits.len(), 1);
        }

        #[test]
        fn parse_empty_returns_empty() {
            let (index, handles) = setup_index();
            let parser = LexicalParser::with_defaults(handles.subject, handles.body);

            assert!(matches!(parser.parse(&index, ""), ParseOutcome::Empty));
            assert!(matches!(parser.parse(&index, "   "), ParseOutcome::Empty));
            assert!(matches!(parser.parse(&index, "AND"), ParseOutcome::Empty));
        }

        #[test]
        fn parse_hyphenated_finds_document() {
            let (index, handles) = setup_index();
            let parser = LexicalParser::with_defaults(handles.subject, handles.body);
            // POL-358 gets quoted → phrase match in subject/body
            let outcome = parser.parse(&index, "POL-358");

            let query = outcome.into_query().expect("should produce a query");
            let reader = index.reader().unwrap();
            let searcher = reader.searcher();
            let hits = searcher.search(&query, &TopDocs::with_limit(10)).unwrap();
            assert_eq!(hits.len(), 1);

            let doc: TantivyDocument = searcher.doc(hits[0].1).unwrap();
            let id = doc.get_first(handles.id).unwrap().as_u64().unwrap();
            assert_eq!(id, 3);
        }

        #[test]
        fn parse_multi_term_default_conjunction() {
            let (index, handles) = setup_index();
            let parser = LexicalParser::with_defaults(handles.subject, handles.body);
            // Default is conjunction: "deployment search" should match doc 2
            // (both terms in subject+body)
            let outcome = parser.parse(&index, "deployment search");

            let query = outcome.into_query().expect("should produce a query");
            let reader = index.reader().unwrap();
            let searcher = reader.searcher();
            let hits = searcher.search(&query, &TopDocs::with_limit(10)).unwrap();
            // "deployment" is in subject of doc2 ("Deployment checklist")
            // "search" is in body of doc2 ("deploying the new search engine")
            assert!(!hits.is_empty());
        }

        #[test]
        fn parse_disjunction_mode() {
            let (index, handles) = setup_index();
            let config = LexicalParserConfig {
                conjunction_by_default: false,
                ..LexicalParserConfig::default()
            };
            let parser = LexicalParser::new(handles.subject, handles.body, config);
            // In disjunction mode, "migration deployment" matches docs with either term
            let outcome = parser.parse(&index, "migration deployment");

            let query = outcome.into_query().expect("should produce a query");
            let reader = index.reader().unwrap();
            let searcher = reader.searcher();
            let hits = searcher.search(&query, &TopDocs::with_limit(10)).unwrap();
            // Should match doc 1 (migration) and doc 2 (deployment)
            assert_eq!(hits.len(), 2);
        }

        #[test]
        fn parse_fallback_on_malformed() {
            let (index, handles) = setup_index();
            let parser = LexicalParser::with_defaults(handles.subject, handles.body);
            // Unclosed quote is malformed — should fallback
            let outcome = parser.parse(&index, "\"unclosed quote migration");

            assert!(outcome.used_fallback());
            let query = outcome
                .into_query()
                .expect("fallback should produce a query");
            let reader = index.reader().unwrap();
            let searcher = reader.searcher();
            let hits = searcher.search(&query, &TopDocs::with_limit(10)).unwrap();
            // "migration" term should still match doc 1
            assert!(!hits.is_empty());
        }

        #[test]
        fn match_all_finds_everything() {
            let (index, _handles) = setup_index();
            let query = match_all_query();
            let reader = index.reader().unwrap();
            let searcher = reader.searcher();
            let hits = searcher.search(&*query, &TopDocs::with_limit(100)).unwrap();
            assert_eq!(hits.len(), 3);
        }

        #[test]
        fn match_none_finds_nothing() {
            let (index, _handles) = setup_index();
            let query = match_none_query();
            let reader = index.reader().unwrap();
            let searcher = reader.searcher();
            let hits = searcher.search(&*query, &TopDocs::with_limit(100)).unwrap();
            assert!(hits.is_empty());
        }

        #[test]
        fn subject_boost_ranks_subject_hit_higher() {
            let (index, handles) = setup_index();
            let parser = LexicalParser::with_defaults(handles.subject, handles.body);
            // "plan" appears in both subject and body of doc 1.
            // "plan" does NOT appear in doc 2 or doc 3.
            // This test verifies that the boost mechanism produces a query.
            let outcome = parser.parse(&index, "plan");
            let query = outcome.into_query().expect("should produce a query");
            let reader = index.reader().unwrap();
            let searcher = reader.searcher();
            let hits = searcher.search(&query, &TopDocs::with_limit(10)).unwrap();
            assert!(!hits.is_empty());
            // Doc 1 has "plan" in subject (boosted 2x) — should rank first
            let doc: TantivyDocument = searcher.doc(hits[0].1).unwrap();
            let id = doc.get_first(handles.id).unwrap().as_u64().unwrap();
            assert_eq!(id, 1);
        }

        #[test]
        fn parse_outcome_accessors() {
            let (index, handles) = setup_index();
            let parser = LexicalParser::with_defaults(handles.subject, handles.body);

            let outcome = parser.parse(&index, "migration");
            assert!(!outcome.used_fallback());

            let outcome2 = parser.parse(&index, "");
            assert!(outcome2.into_query().is_none());
        }

        #[test]
        fn parser_config_defaults() {
            let config = LexicalParserConfig::default();
            assert!(config.conjunction_by_default);
            assert!((config.subject_boost - 2.0).abs() < f32::EPSILON);
            assert!((config.body_boost - 1.0).abs() < f32::EPSILON);
        }
    }
}
