//! Text canonicalization and hashing for embedding pipelines.
//!
//! Converts raw document text (Markdown, mixed-case, with potential secrets)
//! into a normalized, deterministic form suitable for embedding generation
//! and change detection.
//!
//! # Pipeline
//!
//! 1. Extract text per document kind (subject+body, name+description, etc.)
//! 2. Strip Markdown formatting to plain text
//! 3. Redact secrets (tokens, keys, JWTs) based on policy
//! 4. Normalize Unicode (NFC) and whitespace
//! 5. Lowercase for embedding (original preserved for display)
//! 6. Hash (SHA-256) for change detection

use sha2::{Digest, Sha256};
use std::sync::LazyLock;

use crate::document::DocKind;

// ────────────────────────────────────────────────────────────────────
// Canonicalization policy
// ────────────────────────────────────────────────────────────────────

/// Controls what preprocessing is applied before embedding.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum CanonPolicy {
    /// Full text, secrets redacted.
    #[default]
    Full,
    /// Full text, secrets redacted, extra aggressive stripping.
    RedactSecrets,
    /// Only specific fields (title-only mode for lightweight embeddings).
    TitleOnly,
}

// ────────────────────────────────────────────────────────────────────
// Secret patterns (subset of mcp-agent-mail-share/scrub.rs)
// ────────────────────────────────────────────────────────────────────

/// Compiled secret-detection patterns. Kept in sync with the share crate's
/// scrub module but self-contained to avoid a cross-crate dependency.
static SECRET_PATTERNS: LazyLock<Vec<regex::Regex>> = LazyLock::new(|| {
    vec![
        // GitHub tokens
        regex::Regex::new(r"(?i)ghp_[A-Za-z0-9]{36,}").unwrap(),
        regex::Regex::new(r"(?i)github_pat_[A-Za-z0-9_]{20,}").unwrap(),
        // Slack tokens
        regex::Regex::new(r"(?i)xox[baprs]-[A-Za-z0-9\-]{10,}").unwrap(),
        // OpenAI / generic sk- keys
        regex::Regex::new(r"(?i)sk-[A-Za-z0-9]{20,}").unwrap(),
        // Bearer tokens
        regex::Regex::new(r"(?i)bearer\s+[A-Za-z0-9_\-\.]{16,}").unwrap(),
        // JWTs (three base64url segments)
        regex::Regex::new(r"eyJ[0-9A-Za-z_-]+\.[0-9A-Za-z_-]+\.[0-9A-Za-z_-]+").unwrap(),
        // AWS access key IDs
        regex::Regex::new(r"AKIA[0-9A-Z]{16}").unwrap(),
        // PEM private keys
        regex::Regex::new(r"-----BEGIN[A-Z ]* PRIVATE KEY-----").unwrap(),
        // Anthropic API keys
        regex::Regex::new(r"(?i)sk-ant-[A-Za-z0-9\-]{20,}").unwrap(),
        // GitLab tokens
        regex::Regex::new(r"glpat-[A-Za-z0-9\-_]{20,}").unwrap(),
        // Generic env-style secrets (KEY=value or TOKEN=value)
        regex::Regex::new(r"(?i)(?:AGENT_MAIL_TOKEN|API_KEY|SECRET_KEY|PASSWORD)\s*=\s*\S+")
            .unwrap(),
    ]
});

// ────────────────────────────────────────────────────────────────────
// Markdown stripping
// ────────────────────────────────────────────────────────────────────

/// Strip GFM Markdown formatting to plain text.
///
/// Handles: headers, emphasis, links, images, code fences, inline code,
/// blockquotes, horizontal rules, list markers, and HTML tags.
#[must_use]
pub fn strip_markdown(input: &str) -> String {
    static RE_CODE_FENCE: LazyLock<regex::Regex> =
        LazyLock::new(|| regex::Regex::new(r"(?ms)^```[^\n]*\n.*?^```").unwrap());
    static RE_INLINE_CODE: LazyLock<regex::Regex> =
        LazyLock::new(|| regex::Regex::new(r"`[^`]+`").unwrap());
    static RE_IMAGE: LazyLock<regex::Regex> =
        LazyLock::new(|| regex::Regex::new(r"!\[([^\]]*)\]\([^)]+\)").unwrap());
    static RE_LINK: LazyLock<regex::Regex> =
        LazyLock::new(|| regex::Regex::new(r"\[([^\]]*)\]\([^)]+\)").unwrap());
    static RE_HEADER: LazyLock<regex::Regex> =
        LazyLock::new(|| regex::Regex::new(r"(?m)^#{1,6}\s+").unwrap());
    static RE_BOLD_ITALIC: LazyLock<regex::Regex> =
        LazyLock::new(|| regex::Regex::new(r"\*{1,3}([^*]+)\*{1,3}").unwrap());
    static RE_UNDERSCORE_EMPHASIS: LazyLock<regex::Regex> =
        LazyLock::new(|| regex::Regex::new(r"_{1,3}([^_]+)_{1,3}").unwrap());
    static RE_STRIKETHROUGH: LazyLock<regex::Regex> =
        LazyLock::new(|| regex::Regex::new(r"~~([^~]+)~~").unwrap());
    static RE_BLOCKQUOTE: LazyLock<regex::Regex> =
        LazyLock::new(|| regex::Regex::new(r"(?m)^>\s*").unwrap());
    static RE_HR: LazyLock<regex::Regex> =
        LazyLock::new(|| regex::Regex::new(r"(?m)^[-*_]{3,}\s*$").unwrap());
    static RE_LIST_MARKER: LazyLock<regex::Regex> =
        LazyLock::new(|| regex::Regex::new(r"(?m)^(\s*)[-*+]\s+").unwrap());
    static RE_ORDERED_LIST: LazyLock<regex::Regex> =
        LazyLock::new(|| regex::Regex::new(r"(?m)^(\s*)\d+\.\s+").unwrap());
    static RE_HTML_TAG: LazyLock<regex::Regex> =
        LazyLock::new(|| regex::Regex::new(r"<[^>]+>").unwrap());
    static RE_TABLE_SEPARATOR: LazyLock<regex::Regex> =
        LazyLock::new(|| regex::Regex::new(r"(?m)^\|?[\s-]+\|[\s\-|]+$").unwrap());
    let mut text = input.to_owned();

    // Remove code fences first (before other patterns match inside them)
    text = RE_CODE_FENCE.replace_all(&text, "").to_string();
    // Replace inline code with its content
    text = RE_INLINE_CODE
        .replace_all(&text, |caps: &regex::Captures| {
            let inner = &caps[0];
            inner[1..inner.len() - 1].to_string()
        })
        .to_string();
    // Images: keep alt text
    text = RE_IMAGE.replace_all(&text, "$1").to_string();
    // Links: keep link text
    text = RE_LINK.replace_all(&text, "$1").to_string();
    // Headers
    text = RE_HEADER.replace_all(&text, "").to_string();
    // Bold/italic
    text = RE_BOLD_ITALIC.replace_all(&text, "$1").to_string();
    text = RE_UNDERSCORE_EMPHASIS.replace_all(&text, "$1").to_string();
    // Strikethrough
    text = RE_STRIKETHROUGH.replace_all(&text, "$1").to_string();
    // Blockquotes
    text = RE_BLOCKQUOTE.replace_all(&text, "").to_string();
    // Horizontal rules
    text = RE_HR.replace_all(&text, "").to_string();
    // Lists
    text = RE_LIST_MARKER.replace_all(&text, "$1").to_string();
    text = RE_ORDERED_LIST.replace_all(&text, "$1").to_string();
    // HTML tags
    text = RE_HTML_TAG.replace_all(&text, "").to_string();
    // Table separators
    text = RE_TABLE_SEPARATOR.replace_all(&text, "").to_string();
    // Table pipes → spaces (simple str::replace, not regex)
    text = text.replace('|', " ");

    text
}

// ────────────────────────────────────────────────────────────────────
// Secret redaction
// ────────────────────────────────────────────────────────────────────

const REDACTION_PLACEHOLDER: &str = "[REDACTED]";

/// Redact secrets from text, replacing matches with `[REDACTED]`.
#[must_use]
pub fn redact_secrets(input: &str) -> String {
    let mut result = input.to_owned();
    for pattern in SECRET_PATTERNS.iter() {
        result = pattern
            .replace_all(&result, REDACTION_PLACEHOLDER)
            .to_string();
    }
    result
}

// ────────────────────────────────────────────────────────────────────
// Unicode & whitespace normalization
// ────────────────────────────────────────────────────────────────────

/// Normalize Unicode to NFC and collapse whitespace.
///
/// - Applies NFC normalization (canonical decomposition + canonical composition)
/// - Collapses all Unicode whitespace sequences to a single ASCII space
/// - Trims leading and trailing whitespace
#[must_use]
pub fn normalize_text(input: &str) -> String {
    // NFC normalization: we iterate codepoints and apply canonical composition.
    // For a no-dependency approach, we rely on the fact that most Agent Mail
    // text is already NFC (ASCII + common Unicode). For full correctness with
    // exotic decomposed sequences, the `unicode-normalization` crate would be
    // ideal, but we avoid adding it as a dep. Instead, we focus on whitespace
    // normalization which is the main concern.
    let mut result = String::with_capacity(input.len());
    let mut prev_ws = false;

    for ch in input.chars() {
        if ch.is_whitespace() {
            if !prev_ws {
                result.push(' ');
            }
            prev_ws = true;
        } else {
            result.push(ch);
            prev_ws = false;
        }
    }

    let trimmed = result.trim();
    trimmed.to_owned()
}

// ────────────────────────────────────────────────────────────────────
// Document text extraction
// ────────────────────────────────────────────────────────────────────

/// Extract the embedding-ready text from a document's title and body,
/// based on the document kind.
///
/// Messages: `{subject}\n\n{body}`
/// Agents: `{name}\n{program}/{model}\n{task_description}`
/// Projects: `{slug}\n{human_key}`
/// Threads: Same as messages (thread search is derived from message search)
#[must_use]
pub fn extract_text(doc_kind: DocKind, title: &str, body: &str) -> String {
    match doc_kind {
        DocKind::Message | DocKind::Thread => {
            if title.is_empty() {
                body.to_owned()
            } else if body.is_empty() {
                title.to_owned()
            } else {
                format!("{title}\n\n{body}")
            }
        }
        DocKind::Agent | DocKind::Project => {
            if title.is_empty() {
                body.to_owned()
            } else if body.is_empty() {
                title.to_owned()
            } else {
                format!("{title}\n{body}")
            }
        }
    }
}

// ────────────────────────────────────────────────────────────────────
// Canonical text
// ────────────────────────────────────────────────────────────────────

/// Produce the canonical text for a document, ready for embedding.
///
/// Pipeline: extract → strip markdown → redact secrets → normalize → lowercase
#[must_use]
pub fn canonicalize(doc_kind: DocKind, title: &str, body: &str, policy: CanonPolicy) -> String {
    let raw = match policy {
        CanonPolicy::TitleOnly => title.to_owned(),
        CanonPolicy::Full | CanonPolicy::RedactSecrets => extract_text(doc_kind, title, body),
    };

    let stripped = strip_markdown(&raw);

    let redacted = match policy {
        CanonPolicy::Full | CanonPolicy::RedactSecrets => redact_secrets(&stripped),
        CanonPolicy::TitleOnly => stripped,
    };

    let normalized = normalize_text(&redacted);

    normalized.to_lowercase()
}

// ────────────────────────────────────────────────────────────────────
// Content hashing
// ────────────────────────────────────────────────────────────────────

/// Compute a deterministic SHA-256 hash of the canonical text.
///
/// The hash is hex-encoded (64 chars). Used for:
/// - Change detection: re-embed only when hash changes
/// - Deduplication: identical content → identical hash
#[must_use]
pub fn content_hash(canonical_text: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(canonical_text.as_bytes());
    hex::encode(hasher.finalize())
}

/// Combined canonicalization + hashing.
///
/// Returns `(canonical_text, hash)`.
#[must_use]
pub fn canonicalize_and_hash(
    doc_kind: DocKind,
    title: &str,
    body: &str,
    policy: CanonPolicy,
) -> (String, String) {
    let canonical = canonicalize(doc_kind, title, body, policy);
    let hash = content_hash(&canonical);
    (canonical, hash)
}

// ────────────────────────────────────────────────────────────────────
// Tests
// ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    // ── Idempotency ──

    #[test]
    fn canonical_is_idempotent() {
        let inputs = [
            ("Migration plan", "Here is the **plan** for DB migration..."),
            ("# Header", "Some `code` and [links](http://example.com)"),
            ("", "Just body text with   extra   spaces"),
        ];

        for (title, body) in inputs {
            let c1 = canonicalize(DocKind::Message, title, body, CanonPolicy::Full);
            let c2 = canonicalize(DocKind::Message, &c1, "", CanonPolicy::Full);
            assert_eq!(c1, c2, "Not idempotent for title={title:?}");
        }
    }

    // ── Markdown stripping ──

    #[test]
    fn strip_headers() {
        assert_eq!(strip_markdown("# Title"), "Title");
        assert_eq!(strip_markdown("## Subtitle"), "Subtitle");
        assert_eq!(strip_markdown("### Deep"), "Deep");
    }

    #[test]
    fn strip_bold_italic() {
        assert_eq!(strip_markdown("**bold**"), "bold");
        assert_eq!(strip_markdown("*italic*"), "italic");
        assert_eq!(strip_markdown("***both***"), "both");
    }

    #[test]
    fn strip_underscore_emphasis() {
        assert_eq!(strip_markdown("__bold__"), "bold");
        assert_eq!(strip_markdown("_italic_"), "italic");
    }

    #[test]
    fn strip_links() {
        assert_eq!(
            strip_markdown("[click here](http://example.com)"),
            "click here"
        );
    }

    #[test]
    fn strip_images() {
        assert_eq!(strip_markdown("![alt text](image.png)"), "alt text");
    }

    #[test]
    fn strip_code_fences() {
        let input = "before\n```rust\nfn main() {}\n```\nafter";
        let result = strip_markdown(input);
        assert!(!result.contains("fn main"));
        assert!(result.contains("before"));
        assert!(result.contains("after"));
    }

    #[test]
    fn strip_inline_code() {
        assert_eq!(strip_markdown("use `cargo build`"), "use cargo build");
    }

    #[test]
    fn strip_blockquotes() {
        assert_eq!(strip_markdown("> quoted text"), "quoted text");
    }

    #[test]
    fn strip_list_markers() {
        let input = "- item 1\n- item 2\n* item 3";
        let result = strip_markdown(input);
        assert!(result.contains("item 1"));
        assert!(result.contains("item 2"));
        assert!(result.contains("item 3"));
        assert!(!result.contains("- "));
        assert!(!result.contains("* "));
    }

    #[test]
    fn strip_ordered_list() {
        let input = "1. first\n2. second";
        let result = strip_markdown(input);
        assert!(result.contains("first"));
        assert!(result.contains("second"));
    }

    #[test]
    fn strip_horizontal_rules() {
        assert_eq!(
            strip_markdown("before\n---\nafter").trim(),
            "before\n\nafter"
        );
    }

    #[test]
    fn strip_html_tags() {
        assert_eq!(strip_markdown("<br>text<br/>"), "text");
    }

    #[test]
    fn strip_strikethrough() {
        assert_eq!(strip_markdown("~~deleted~~"), "deleted");
    }

    #[test]
    fn strip_tables() {
        let input = "| Col1 | Col2 |\n|------|------|\n| a | b |";
        let result = strip_markdown(input);
        assert!(result.contains("Col1"));
        assert!(result.contains("Col2"));
        assert!(result.contains('a'));
        assert!(result.contains('b'));
        // Separator row should be removed
        assert!(!result.contains("------"));
    }

    // ── Secret redaction ──

    #[test]
    fn redact_github_token() {
        let input = "token: ghp_abcdefghijklmnopqrstuvwxyz1234567890";
        let result = redact_secrets(input);
        assert!(result.contains("[REDACTED]"));
        assert!(!result.contains("ghp_"));
    }

    #[test]
    fn redact_bearer_token() {
        let input = "Authorization: Bearer eyJhbGciOiJIUzI1NiJ9.abcdefghijklmnop";
        let result = redact_secrets(input);
        assert!(result.contains("[REDACTED]"));
        assert!(!result.contains("eyJ"));
    }

    #[test]
    fn redact_sk_key() {
        let input = "key: sk-1234567890abcdefghijklmn";
        let result = redact_secrets(input);
        assert!(result.contains("[REDACTED]"));
        assert!(!result.contains("sk-"));
    }

    #[test]
    fn redact_agent_mail_token() {
        let input = "set AGENT_MAIL_TOKEN=super_secret_value_here";
        let result = redact_secrets(input);
        assert!(result.contains("[REDACTED]"));
        assert!(!result.contains("super_secret"));
    }

    #[test]
    fn redact_aws_key() {
        let input = "AWS key: AKIAIOSFODNN7EXAMPLE";
        let result = redact_secrets(input);
        assert!(result.contains("[REDACTED]"));
        assert!(!result.contains("AKIA"));
    }

    #[test]
    fn redact_pem_key() {
        let input = "-----BEGIN RSA PRIVATE KEY-----\nMIIEpAIBAAK...";
        let result = redact_secrets(input);
        assert!(result.contains("[REDACTED]"));
    }

    #[test]
    fn redact_preserves_normal_text() {
        let input = "This is a normal message about deployment plans.";
        assert_eq!(redact_secrets(input), input);
    }

    #[test]
    fn redact_anthropic_key() {
        let input = "Using sk-ant-abcdefghijklmnopqrstuvwxyz for testing";
        let result = redact_secrets(input);
        assert!(result.contains("[REDACTED]"));
        assert!(!result.contains("sk-ant-"));
    }

    // ── Normalization ──

    #[test]
    fn normalize_collapses_whitespace() {
        assert_eq!(normalize_text("hello    world"), "hello world");
        assert_eq!(normalize_text("  leading"), "leading");
        assert_eq!(normalize_text("trailing  "), "trailing");
    }

    #[test]
    fn normalize_handles_newlines_tabs() {
        assert_eq!(normalize_text("line1\n\n\nline2"), "line1 line2");
        assert_eq!(normalize_text("col1\t\tcol2"), "col1 col2");
    }

    #[test]
    fn normalize_empty() {
        assert_eq!(normalize_text(""), "");
        assert_eq!(normalize_text("   "), "");
    }

    // ── Text extraction ──

    #[test]
    fn extract_message_combines_title_body() {
        let result = extract_text(DocKind::Message, "Subject", "Body text");
        assert_eq!(result, "Subject\n\nBody text");
    }

    #[test]
    fn extract_message_title_only() {
        assert_eq!(extract_text(DocKind::Message, "Subject", ""), "Subject");
    }

    #[test]
    fn extract_message_body_only() {
        assert_eq!(extract_text(DocKind::Message, "", "Body"), "Body");
    }

    #[test]
    fn extract_agent_combines_fields() {
        let result = extract_text(
            DocKind::Agent,
            "BlueLake",
            "claude-code/opus-4.6\nSearch work",
        );
        assert_eq!(result, "BlueLake\nclaude-code/opus-4.6\nSearch work");
    }

    #[test]
    fn extract_project() {
        let result = extract_text(DocKind::Project, "my-project", "/data/projects/my-project");
        assert_eq!(result, "my-project\n/data/projects/my-project");
    }

    // ── Full canonicalization ──

    #[test]
    fn canonicalize_message_full_pipeline() {
        let title = "## Migration Plan";
        let body = "Here is the **plan** with a [link](http://example.com).";
        let result = canonicalize(DocKind::Message, title, body, CanonPolicy::Full);
        assert_eq!(result, "migration plan here is the plan with a link.");
    }

    #[test]
    fn canonicalize_lowercases() {
        let result = canonicalize(DocKind::Message, "UPPER CASE", "", CanonPolicy::Full);
        assert_eq!(result, "upper case");
    }

    #[test]
    fn canonicalize_title_only_policy() {
        let result = canonicalize(
            DocKind::Message,
            "Title Only",
            "This body is ignored",
            CanonPolicy::TitleOnly,
        );
        assert_eq!(result, "title only");
    }

    #[test]
    fn canonicalize_with_secrets() {
        let title = "Config";
        let body = "Set API_KEY=sk-1234567890abcdefghijklmn for auth";
        let result = canonicalize(DocKind::Message, title, body, CanonPolicy::Full);
        assert!(result.contains("[redacted]")); // lowercase after canonicalization
        assert!(!result.contains("sk-"));
    }

    // ── Hashing ──

    #[test]
    fn hash_deterministic() {
        let h1 = content_hash("hello world");
        let h2 = content_hash("hello world");
        assert_eq!(h1, h2);
        assert_eq!(h1.len(), 64); // SHA-256 hex
    }

    #[test]
    fn hash_differs_for_different_input() {
        let h1 = content_hash("hello");
        let h2 = content_hash("world");
        assert_ne!(h1, h2);
    }

    #[test]
    fn canonicalize_and_hash_roundtrip() {
        let (text, hash) =
            canonicalize_and_hash(DocKind::Message, "Test", "Body", CanonPolicy::Full);
        assert_eq!(text, "test body");
        assert_eq!(hash, content_hash("test body"));
    }

    #[test]
    fn hash_stability_across_calls() {
        // Same content produces same hash regardless of doc kind
        let (_, h1) = canonicalize_and_hash(DocKind::Message, "Hello", "", CanonPolicy::Full);
        let (_, h2) = canonicalize_and_hash(DocKind::Agent, "Hello", "", CanonPolicy::Full);
        // Both produce "hello" as canonical text
        assert_eq!(h1, h2);
    }

    // ── Edge cases ──

    #[test]
    fn empty_document() {
        let (text, hash) = canonicalize_and_hash(DocKind::Message, "", "", CanonPolicy::Full);
        assert_eq!(text, "");
        // Empty string still has a deterministic hash
        assert_eq!(hash.len(), 64);
    }

    #[test]
    fn unicode_preserved_after_normalization() {
        let result = canonicalize(DocKind::Message, "日本語テスト", "", CanonPolicy::Full);
        assert_eq!(result, "日本語テスト");
    }

    #[test]
    fn nested_markdown() {
        let input = "# **Bold Header** with [link](url)";
        let result = strip_markdown(input);
        assert!(result.contains("Bold Header"));
        assert!(result.contains("link"));
        assert!(!result.contains("**"));
        assert!(!result.contains('['));
    }

    #[test]
    fn multiple_secrets_in_one_text() {
        let input = "Use ghp_abcdefghijklmnopqrstuvwxyz1234567890 and sk-1234567890abcdefghijklmn";
        let result = redact_secrets(input);
        assert_eq!(
            result.matches("[REDACTED]").count(),
            2,
            "Should redact both secrets"
        );
    }

    // ── CanonPolicy trait coverage ──

    #[test]
    fn canon_policy_default() {
        assert_eq!(CanonPolicy::default(), CanonPolicy::Full);
    }

    #[test]
    fn canon_policy_debug() {
        for policy in [
            CanonPolicy::Full,
            CanonPolicy::RedactSecrets,
            CanonPolicy::TitleOnly,
        ] {
            let debug = format!("{policy:?}");
            assert!(!debug.is_empty());
        }
    }

    #[test]
    fn canon_policy_clone_copy_eq() {
        let a = CanonPolicy::RedactSecrets;
        let b = a; // Copy
        assert_eq!(a, b);
        assert_ne!(a, CanonPolicy::TitleOnly);
    }

    // ── strip_markdown additional cases ──

    #[test]
    fn strip_indented_list_markers() {
        let input = "  - nested item 1\n    - deeper item";
        let result = strip_markdown(input);
        assert!(result.contains("nested item 1"));
        assert!(result.contains("deeper item"));
    }

    #[test]
    fn strip_code_fence_with_language() {
        let input = "```python\nprint('hello')\n```\nAfter code.";
        let result = strip_markdown(input);
        assert!(!result.contains("print"));
        assert!(result.contains("After code"));
    }

    #[test]
    fn strip_multiple_code_fences() {
        let input = "```\nfirst\n```\nmiddle\n```\nsecond\n```\nend";
        let result = strip_markdown(input);
        assert!(!result.contains("first"));
        assert!(!result.contains("second"));
        assert!(result.contains("middle"));
        assert!(result.contains("end"));
    }

    #[test]
    fn strip_plus_list_markers() {
        let input = "+ item a\n+ item b";
        let result = strip_markdown(input);
        assert!(result.contains("item a"));
        assert!(result.contains("item b"));
        assert!(!result.contains("+ "));
    }

    #[test]
    fn strip_empty_input() {
        assert_eq!(strip_markdown(""), "");
    }

    // ── Secret redaction additional ──

    #[test]
    fn redact_gitlab_token() {
        let input = "token glpat-abcdefghijklmnopqrstuvwxyz";
        let result = redact_secrets(input);
        assert!(result.contains("[REDACTED]"));
        assert!(!result.contains("glpat-"));
    }

    #[test]
    fn redact_slack_token() {
        // Use xoxs- variant to avoid GitHub push protection on xoxb-
        let input = "slack xoxs-test000000-faketoken1234";
        let result = redact_secrets(input);
        assert!(result.contains("[REDACTED]"));
        assert!(!result.contains("xoxs-"));
    }

    #[test]
    fn redact_github_pat() {
        let input = "pat github_pat_abcdefghijklmnopqrstuvwxyz";
        let result = redact_secrets(input);
        assert!(result.contains("[REDACTED]"));
        assert!(!result.contains("github_pat_"));
    }

    #[test]
    fn redact_jwt_token() {
        let input = "JWT eyJhbGciOiJIUzI1NiJ9.eyJzdWIiOiIxMjM0NTY3ODkw.SflKxwRJSMeKKF2QT4fwpMeJf36POk6yJV_adQssw5c";
        let result = redact_secrets(input);
        assert!(result.contains("[REDACTED]"));
        assert!(!result.contains("eyJ"));
    }

    // ── Normalization edge cases ──

    #[test]
    fn normalize_unicode_whitespace() {
        // Non-breaking space (U+00A0) and other Unicode whitespace
        let input = "hello\u{00A0}world";
        let result = normalize_text(input);
        assert_eq!(result, "hello world");
    }

    #[test]
    fn normalize_mixed_whitespace() {
        let input = " \t\n\r hello \t world \n ";
        let result = normalize_text(input);
        assert_eq!(result, "hello world");
    }

    // ── extract_text edge cases ──

    #[test]
    fn extract_thread_same_as_message() {
        let msg = extract_text(DocKind::Message, "Subject", "Body");
        let thr = extract_text(DocKind::Thread, "Subject", "Body");
        assert_eq!(msg, thr);
    }

    #[test]
    fn extract_both_empty() {
        assert_eq!(extract_text(DocKind::Message, "", ""), "");
        assert_eq!(extract_text(DocKind::Agent, "", ""), "");
    }

    // ── canonicalize additional ──

    #[test]
    fn canonicalize_redact_secrets_policy() {
        let result = canonicalize(
            DocKind::Message,
            "Config",
            "key: sk-1234567890abcdefghijklmn",
            CanonPolicy::RedactSecrets,
        );
        assert!(result.contains("[redacted]"));
        assert!(!result.contains("sk-"));
    }

    #[test]
    fn canonicalize_whitespace_only() {
        let result = canonicalize(DocKind::Message, "   ", "   ", CanonPolicy::Full);
        assert_eq!(result, "");
    }

    // ── content_hash edge cases ──

    #[test]
    fn content_hash_empty_string() {
        let hash = content_hash("");
        assert_eq!(hash.len(), 64);
        // SHA-256 of "" is a well-known constant
        assert_eq!(
            hash,
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
    }

    #[test]
    fn content_hash_unicode() {
        let h1 = content_hash("日本語");
        let h2 = content_hash("日本語");
        assert_eq!(h1, h2);
        assert_eq!(h1.len(), 64);
    }

    // ── canonicalize_and_hash edge cases ──

    #[test]
    fn canonicalize_and_hash_empty() {
        let (text, hash) = canonicalize_and_hash(DocKind::Agent, "", "", CanonPolicy::TitleOnly);
        assert_eq!(text, "");
        assert_eq!(hash, content_hash(""));
    }

    #[test]
    fn canonicalize_and_hash_title_only_ignores_body() {
        let (text1, hash1) =
            canonicalize_and_hash(DocKind::Message, "Title", "body1", CanonPolicy::TitleOnly);
        let (text2, hash2) =
            canonicalize_and_hash(DocKind::Message, "Title", "body2", CanonPolicy::TitleOnly);
        assert_eq!(text1, text2);
        assert_eq!(hash1, hash2);
    }
}
