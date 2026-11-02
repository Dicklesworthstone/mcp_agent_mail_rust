//! Template rendering for the Mail SSR UI.
//!
//! We embed the legacy Jinja templates at compile time to keep the binary self-contained.

#![forbid(unsafe_code)]

use std::sync::LazyLock;

use include_dir::{Dir, include_dir};
use minijinja::value::Value;
use minijinja::{AutoEscape, Environment, Error, ErrorKind};

static TEMPLATE_DIR: Dir<'static> = include_dir!("$CARGO_MANIFEST_DIR/templates");

pub const MAIL_TAILWIND_ASSET_PATH: &str = "/mail/static/tailwind-3.4.17.min.js";
pub const MAIL_TAILWIND_INTEGRITY: &str = "sha256-F26JRmGqnNyaXLpscgBEy797i9gNHJoUKnwksbbFDRU=";

/// Strip HTML tags from a string (Jinja2 `striptags` filter).
#[allow(clippy::needless_pass_by_value)]
fn striptags(value: Value) -> Value {
    let s = value.to_string();
    let mut out = String::with_capacity(s.len());
    let mut in_tag = false;
    for c in s.chars() {
        match c {
            '<' => in_tag = true,
            '>' if in_tag => in_tag = false,
            _ if !in_tag => out.push(c),
            _ => {}
        }
    }
    Value::from(out)
}

/// Truncate a string (Jinja2 `truncate` filter).
///
/// Usage: `truncate(150)` — truncates at word boundary, appends `...`.
#[allow(clippy::needless_pass_by_value)]
fn truncate(value: Value, length: Option<usize>) -> Value {
    let s = value.to_string();
    let length = length.unwrap_or(255);

    if s.len() <= length {
        return Value::from(s);
    }

    let end = "...";
    let end_len = end.len();
    if length < end_len {
        let mut boundary = length;
        while boundary > 0 && !s.is_char_boundary(boundary) {
            boundary -= 1;
        }
        return Value::from(s[..boundary].to_string());
    }
    let trunc_len = length.saturating_sub(end_len);

    // Find a char boundary.
    let mut boundary = trunc_len;
    while boundary > 0 && !s.is_char_boundary(boundary) {
        boundary -= 1;
    }

    // Default: truncate at last word boundary (killwords=false).
    if let Some(pos) = s[..boundary].rfind(' ') {
        boundary = pos;
    }

    Value::from(format!("{}{end}", &s[..boundary]))
}

#[allow(clippy::needless_pass_by_value)] // MiniJinja filter signature uses owned `Value`.
fn tojson(value: Value) -> Result<Value, Error> {
    let s = serde_json::to_string(&value).map_err(|err| {
        Error::new(ErrorKind::InvalidOperation, "cannot serialize to JSON").with_source(err)
    })?;

    // Ensure the output is safe for embedding in HTML and inline JS contexts.
    // Mirrors MiniJinja's built-in `tojson` filter behavior when the `json`
    // feature is enabled.
    let mut rv = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '<' => rv.push_str("\\u003c"),
            '>' => rv.push_str("\\u003e"),
            '&' => rv.push_str("\\u0026"),
            '\'' => rv.push_str("\\u0027"),
            _ => rv.push(c),
        }
    }

    Ok(Value::from_safe_string(rv))
}

/// The calendar year at render time, for the shared page footer. Evaluated on
/// every render so a long-running server never shows a stale year.
fn current_year() -> i32 {
    chrono::Datelike::year(&chrono::Utc::now())
}

static ENV: LazyLock<Environment<'static>> = LazyLock::new(|| {
    let mut env = Environment::new();
    env.add_function("current_year", current_year);
    env.add_global("mail_tailwind_asset_path", MAIL_TAILWIND_ASSET_PATH);
    env.add_global("mail_tailwind_integrity", MAIL_TAILWIND_INTEGRITY);

    env.set_auto_escape_callback(|name| {
        let is_html = std::path::Path::new(name)
            .extension()
            .and_then(|ext| ext.to_str())
            .is_some_and(|ext| ext.eq_ignore_ascii_case("html"));
        if is_html {
            AutoEscape::Html
        } else {
            AutoEscape::None
        }
    });

    // Jinja2 compatibility: templates rely on `|tojson` extensively.
    env.add_filter("tojson", tojson);
    // Jinja2 built-ins used in mail_thread.html and elsewhere.
    env.add_filter("striptags", striptags);
    env.add_filter("truncate", truncate);

    // Load all embedded templates.
    for file in TEMPLATE_DIR.files() {
        // include_dir stores paths as static, so this can be a `&'static str`.
        let Some(name) = file.path().to_str() else {
            continue;
        };
        let contents =
            std::str::from_utf8(file.contents()).unwrap_or("<!-- invalid utf-8 template -->");
        // Ignore duplicates; the directory is flat.
        let _ = env.add_template(name, contents);
    }

    env
});

pub fn render_template<T: serde::Serialize>(name: &str, ctx: T) -> Result<String, Error> {
    let tpl = ENV.get_template(name)?;
    let json = serde_json::to_value(ctx).map_err(|err| {
        Error::new(ErrorKind::BadSerialization, "cannot serialize Mail context").with_source(err)
    })?;
    tpl.render(json_template_value(json)?)
}

/// Convert through JSON's public value API: its arbitrary-precision numbers
/// otherwise serialize as private structs that `MiniJinja` treats as maps.
fn json_template_value(json: serde_json::Value) -> Result<Value, Error> {
    match json {
        serde_json::Value::Null => Ok(Value::from(())),
        serde_json::Value::Bool(value) => Ok(Value::from(value)),
        serde_json::Value::String(value) => Ok(Value::from(value)),
        serde_json::Value::Number(number) => {
            if let Some(value) = number.as_i64() {
                return Ok(Value::from(value));
            }
            if let Some(value) = number.as_u64() {
                return Ok(Value::from(value));
            }
            if let Some(value) = number.as_i128() {
                return Ok(Value::from(value));
            }
            if let Some(value) = number.as_u128() {
                return Ok(Value::from(value));
            }
            // Do not silently round an oversized integer into a floating ID.
            if number.is_f64()
                && let Some(value) = number.as_f64().filter(|value| value.is_finite())
            {
                return Ok(Value::from(value));
            }
            Err(Error::new(
                ErrorKind::BadSerialization,
                "Mail context number is outside the template numeric range",
            ))
        }
        serde_json::Value::Array(values) => values
            .into_iter()
            .map(json_template_value)
            .collect::<Result<Vec<_>, _>>()
            .map(Value::from),
        serde_json::Value::Object(values) => values
            .into_iter()
            .map(|(key, value)| json_template_value(value).map(|value| (key, value)))
            .collect::<Result<std::collections::BTreeMap<_, _>, _>>()
            .map(Value::from_object),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn json_context_preserves_numeric_types_and_exact_integer_ids() {
        let ctx = json_template_value(serde_json::json!({
            "rows": [{"count": 1, "id": u64::MAX}],
            "minimum": i64::MIN,
            "fraction": 2.5,
            "enabled": true,
            "absent": null,
            "text": "<message>",
        }))
        .unwrap();
        let html = ENV
            .render_str(
                "{% for row in rows %}{% if row.count == 1 %}one{% endif %}:{{ row.count + 1 }}:{{ row.id }}{% endfor %};{{ minimum }};{{ fraction + 0.5 }};{{ enabled }};{{ absent is none }};{{ text }}",
                ctx,
            )
            .unwrap();
        assert_eq!(
            html,
            "one:2:18446744073709551615;-9223372036854775808;3.0;True;True;<message>"
        );
    }

    #[test]
    fn typed_context_and_json_context_render_the_same_exact_numeric_message() {
        #[derive(serde::Serialize)]
        struct Context {
            message: u64,
        }
        let typed = Context { message: u64::MAX };
        let json = serde_json::to_value(&typed).unwrap();
        let typed_html = render_template("error.html", &typed).unwrap();
        let json_html = render_template("error.html", json).unwrap();
        assert_eq!(typed_html, json_html);
        assert!(json_html.contains("18446744073709551615"));
        assert!(!json_html.contains("$serde_json::private::Number"));
    }

    // --- striptags ---

    #[test]
    fn striptags_removes_html_tags() {
        let result = striptags(Value::from("<b>bold</b> and <i>italic</i>"));
        assert_eq!(result.to_string(), "bold and italic");
    }

    #[test]
    fn striptags_handles_no_tags() {
        let result = striptags(Value::from("plain text"));
        assert_eq!(result.to_string(), "plain text");
    }

    #[test]
    fn striptags_handles_nested_tags() {
        let result = striptags(Value::from("<div><p>hello</p></div>"));
        assert_eq!(result.to_string(), "hello");
    }

    #[test]
    fn striptags_handles_empty() {
        let result = striptags(Value::from(""));
        assert_eq!(result.to_string(), "");
    }

    #[test]
    fn striptags_handles_self_closing() {
        let result = striptags(Value::from("text<br>more<hr>end"));
        assert_eq!(result.to_string(), "textmoreend");
    }

    // --- truncate ---

    #[test]
    fn truncate_short_string_unchanged() {
        let result = truncate(Value::from("hello"), Some(100));
        assert_eq!(result.to_string(), "hello");
    }

    #[test]
    fn truncate_long_string() {
        let result = truncate(Value::from("this is a long string for testing"), Some(15));
        let s = result.to_string();
        assert!(s.ends_with("..."));
        assert!(s.len() <= 15);
    }

    #[test]
    fn truncate_at_word_boundary() {
        let result = truncate(Value::from("one two three four five"), Some(12));
        let s = result.to_string();
        assert!(s.ends_with("..."));
        // Should truncate at a word boundary, not mid-word
        assert!(!s.contains("thre"));
    }

    #[test]
    fn truncate_default_length() {
        // Default is 255
        let short = "short text";
        let result = truncate(Value::from(short), None);
        assert_eq!(result.to_string(), short);
    }

    #[test]
    fn truncate_exact_length() {
        let s = "abcde";
        let result = truncate(Value::from(s), Some(5));
        assert_eq!(result.to_string(), "abcde");
    }

    #[test]
    fn truncate_tiny_length_never_exceeds_limit() {
        let result = truncate(Value::from("abcdef"), Some(2));
        assert_eq!(result.to_string(), "ab");

        let ellipsis = truncate(Value::from("abcdef"), Some(3));
        assert_eq!(ellipsis.to_string(), "...");

        let empty = truncate(Value::from("abcdef"), Some(0));
        assert_eq!(empty.to_string(), "");
    }

    // --- tojson ---

    #[test]
    fn tojson_string_value() {
        let result = tojson(Value::from("hello")).expect("tojson");
        let s = result.to_string();
        assert_eq!(s, "\"hello\"");
    }

    #[test]
    fn tojson_number_value() {
        let result = tojson(Value::from(42)).expect("tojson");
        assert_eq!(result.to_string(), "42");
    }

    #[test]
    fn tojson_escapes_html_chars() {
        let result = tojson(Value::from("<script>&'test'</script>")).expect("tojson");
        let s = result.to_string();
        assert!(!s.contains('<'));
        assert!(!s.contains('>'));
        assert!(s.contains("\\u003c"));
        assert!(s.contains("\\u003e"));
        assert!(s.contains("\\u0026"));
        assert!(s.contains("\\u0027"));
    }

    #[test]
    fn tojson_bool_value() {
        let result = tojson(Value::from(true)).expect("tojson");
        assert_eq!(result.to_string(), "true");
    }

    // --- render_template ---

    #[test]
    fn render_missing_template_returns_error() {
        let err = render_template("nonexistent.html", serde_json::json!({}));
        assert!(err.is_err());
    }

    #[test]
    fn archive_commit_template_renders_vec_trailers_and_empty_author() {
        let html = render_template(
            "archive_commit.html",
            serde_json::json!({
                "commit": {
                    "sha": "abcdef1234567890abcdef1234567890abcdef12",
                    "short_sha": "abcdef12",
                    "author": "",
                    "email": "",
                    "date": "2026-06-01T00:00:00Z",
                    "subject": "repair archive template",
                    "body": "",
                    "trailers": [["Co-Authored-By", "Agent <agent@example.com>"]],
                    "files_changed": [{
                        "path": "src/lib.rs",
                        "change_type": "modified",
                        "a_path": "src/lib.rs",
                        "b_path": "src/lib.rs"
                    }],
                    "diff": "diff --git a/src/lib.rs b/src/lib.rs\n",
                    "stats": {
                        "files": 1,
                        "insertions": 2,
                        "deletions": 1
                    }
                }
            }),
        )
        .expect("archive commit template should render");

        assert!(html.contains(r#"<span class="text-sm font-bold">?</span>"#));
        assert!(html.contains("Co-Authored-By:"));
        assert!(html.contains("Agent &lt;agent@example.com&gt;"));
    }

    fn conversation_template_context() -> serde_json::Value {
        serde_json::json!({
            "project": {"id": 1, "slug": "alpha", "human_key": "Workspace Alpha"},
            "threads_url": "/mail/alpha/threads",
            "threads": [],
            "has_more_threads": false,
            "previous_page_url": "",
            "next_page_url": "",
            "page_size": 50,
            "agents": [],
            "retired_agents": [],
            "results": [],
            "recipes": [],
            "messages": [],
            "items": [],
            "q": "",
            "scope": "",
            "field_scope": "",
            "order": "relevance",
            "boost": false,
            "importance": [],
            "ack": "any",
            "agent": "BlueLake",
            "thread": "",
            "direction": "",
            "from_date": "",
            "to_date": "",
            "static_export": false
        })
    }

    fn has_template_href(html: &str, href: &str) -> bool {
        let attribute = format!("href=\"{}\"", href.replace('&', "&amp;"));
        html.contains(&attribute) || html.contains(&attribute.replace('/', "&#x2f;"))
    }

    /// Inspect rendered content before scripts, including the inbox's Alpine
    /// template. Nested anchors change the browser's DOM and break card links.
    fn assert_independent_content_anchors(html: &str) {
        let content = html
            .split_once("<main ")
            .expect("rendered base has main content")
            .1
            .split("</main>")
            .next()
            .expect("main content")
            .split("<script")
            .next()
            .expect("content before scripts");
        let mut anchor_open = false;
        for tag in content.split('<').skip(1) {
            if tag.starts_with("a ") {
                assert!(!anchor_open, "nested anchor in rendered content: {tag}");
                anchor_open = true;
            } else if tag.starts_with("/a>") {
                assert!(anchor_open, "closing anchor has no opening anchor");
                anchor_open = false;
            }
        }
        assert!(!anchor_open, "unclosed anchor in rendered content");
    }

    #[test]
    fn conversation_project_preview_renders_metadata_and_escaped_links() {
        let mut ctx = conversation_template_context();
        ctx["has_more_threads"] = serde_json::json!(true);
        ctx["threads"] = serde_json::json!([{
            "thread_id": "release <tag> ?#&",
            "subject": "<script>alert(1)</script>",
            "message_count": 3,
            "first_activity": "2026-01-01T00:00:00Z",
            "last_activity": "2026-01-02T00:00:00Z",
            "last_activity_relative": "2 minutes ago",
            "thread_url": "/mail/alpha/thread/release%20%3Ctag%3E%20%3F%23%26"
        }]);
        let html = render_template("mail_project.html", &ctx).expect("project renders");
        assert!(html.contains("Active Conversations"));
        assert!(html.contains("Browse all threads"));
        assert!(html.contains("3 messages"));
        assert!(html.contains("2 minutes ago"));
        assert!(html.contains("datetime=\"2026-01-02T00:00:00Z\""));
        assert!(html.contains("release &lt;tag&gt; ?#&amp;"));
        assert!(html.contains("&lt;script&gt;alert(1)"));
        assert!(!html.contains("<script>alert(1)</script>"));
        assert!(has_template_href(&html, "/mail/alpha/threads"));
        assert!(has_template_href(
            &html,
            "/mail/alpha/thread/release%20%3Ctag%3E%20%3F%23%26"
        ));
        assert_independent_content_anchors(&html);
    }

    #[test]
    fn conversation_empty_project_does_not_claim_an_unsent_search_failed() {
        let ctx = conversation_template_context();
        let html = render_template("mail_project.html", &ctx).expect("empty project renders");
        assert!(html.contains("No conversations yet"));
        assert!(!html.contains("No matching messages"));
        assert!(has_template_href(&html, "/mail/alpha/threads"));

        let mut searched = ctx;
        searched["q"] = serde_json::json!("missing");
        let html = render_template("mail_project.html", &searched).expect("search renders");
        assert!(html.contains("No matching messages"));
    }

    #[test]
    fn conversation_browser_preserves_cursor_links_and_canonical_identifiers() {
        let mut ctx = conversation_template_context();
        let previous = "/mail/alpha/threads?before=cursor%2Bvalue&limit=2";
        let next = "/mail/alpha/threads?after=cursor%2Bvalue&limit=2";
        ctx["previous_page_url"] = serde_json::json!(previous);
        ctx["next_page_url"] = serde_json::json!(next);
        ctx["page_size"] = serde_json::json!(2);
        ctx["threads"] = serde_json::json!([
            {
                "thread_id": "..", "subject": "", "message_count": 1,
                "first_activity": "2026-01-01T00:00:00Z",
                "last_activity": "2026-01-01T00:00:00Z",
                "last_activity_relative": "1 hour ago",
                "thread_url": "/mail/alpha/thread?thread_id=.."
            },
            {
                "thread_id": "0042", "subject": "Numeric key", "message_count": 4,
                "first_activity": "2025-12-01T00:00:00Z",
                "last_activity": "2025-12-31T00:00:00Z",
                "last_activity_relative": "1 day ago",
                "thread_url": "/mail/alpha/thread/0042"
            }
        ]);
        let html = render_template("mail_threads.html", &ctx).expect("thread browser renders");
        assert!(html.contains("Project Threads"));
        assert!(html.contains("(No subject)"));
        assert!(html.contains("1 message"));
        assert!(html.contains("4 messages"));
        assert!(html.contains("Started:"));
        assert!(html.contains("datetime=\"2025-12-01T00:00:00Z\""));
        assert!(html.contains("Up to 2 conversations per page"));
        for href in [
            previous,
            next,
            "/mail/alpha/thread?thread_id=..",
            "/mail/alpha/thread/0042",
        ] {
            assert!(has_template_href(&html, href), "missing {href}");
        }
        assert!(html.contains("rel=\"prev\""));
        assert!(html.contains("rel=\"next\""));
        assert_independent_content_anchors(&html);
    }

    #[test]
    fn conversation_browser_empty_and_exhausted_pages_keep_navigation() {
        let mut ctx = conversation_template_context();
        let html = render_template("mail_threads.html", &ctx).expect("empty browser renders");
        assert!(html.contains("No conversations yet"));
        assert!(!html.contains("aria-label=\"Thread pagination\""));
        assert!(has_template_href(&html, "/mail/alpha"));

        ctx["previous_page_url"] = serde_json::json!("/mail/alpha/threads?before=earlier&limit=50");
        let html = render_template("mail_threads.html", &ctx).expect("exhausted page renders");
        assert!(html.contains("No more conversations"));
        assert!(html.contains("rel=\"prev\""));
        assert!(!html.contains("rel=\"next\""));
    }

    #[test]
    fn conversation_search_results_have_independent_message_and_thread_links() {
        for template in ["mail_project.html", "mail_search.html"] {
            let mut ctx = conversation_template_context();
            ctx["q"] = serde_json::json!("needle");
            ctx["results"] = serde_json::json!([
                {
                    "id": 42, "subject": "", "sender": "RemoteSender", "from": "RemoteSender",
                    "created": "2026-01-01T00:00:00Z", "created_relative": "1 hour ago",
                    "thread_id": "42", "thread_url": "/mail/alpha/thread/42", "snippet": ""
                },
                {
                    "id": 43, "subject": "Escaped <subject>", "sender": "RemoteSender", "from": "RemoteSender",
                    "created": "2026-01-01T00:00:00Z", "created_relative": "1 hour ago",
                    "thread_id": "literal%2F\" <thread>",
                    "thread_url": "/mail/alpha/thread/literal%252F%22%20%3Cthread%3E", "snippet": ""
                }
            ]);
            let html = render_template(template, &ctx).expect("search template renders");
            assert!(html.contains("(No subject)"), "{template}");
            assert!(html.contains("Escaped &lt;subject&gt;"), "{template}");
            assert!(!html.contains("<thread>"), "{template}");
            for href in [
                "/mail/alpha/threads",
                "/mail/alpha/message/42",
                "/mail/alpha/thread/42",
                "/mail/alpha/message/43",
                "/mail/alpha/thread/literal%252F%22%20%3Cthread%3E",
            ] {
                assert!(has_template_href(&html, href), "{template}: missing {href}");
            }
            assert_independent_content_anchors(&html);
        }
    }

    #[test]
    fn conversation_message_and_inbox_keep_independent_thread_navigation() {
        let mut ctx = conversation_template_context();
        ctx["message"] = serde_json::json!({
            "id": 42, "subject": "Standalone starter", "sender": "BlueLake",
            "created": "2026-01-01T00:00:00Z", "thread_id": "42",
            "thread_url": "/mail/alpha/thread/42"
        });
        let html = render_template("mail_message.html", &ctx).expect("message renders");
        assert!(html.contains("View Thread"));
        assert!(has_template_href(&html, "/mail/alpha/thread/42"));
        assert!(has_template_href(&html, "/mail/alpha/threads"));
        assert_independent_content_anchors(&html);

        let html = render_template("mail_inbox.html", &ctx).expect("inbox renders");
        assert!(has_template_href(&html, "/mail/alpha/threads"));
        assert!(html.contains("window.agentMailAppendAuth(item.thread_url)"));
        assert_independent_content_anchors(&html);
    }

    #[test]
    fn conversation_static_exports_do_not_offer_unexported_thread_browsers() {
        let mut ctx = conversation_template_context();
        ctx["static_export"] = serde_json::json!(true);
        ctx["message"] = serde_json::json!({"id": 42, "subject": "Exported message"});
        for template in [
            "mail_project.html",
            "mail_search.html",
            "mail_message.html",
            "mail_thread.html",
            "mail_inbox.html",
        ] {
            let html = render_template(template, &ctx).expect("static template renders");
            assert!(
                !has_template_href(&html, "/mail/alpha/threads"),
                "{template} links to a dynamic-only browser"
            );
            assert!(!html.contains("Active Conversations"), "{template}");
        }
    }

    #[test]
    fn conversation_thread_view_links_back_to_its_project_browser() {
        let mut ctx = conversation_template_context();
        ctx["thread_id"] = serde_json::json!("0042");
        ctx["thread_subject"] = serde_json::json!("Named numeric conversation");
        ctx["message_count"] = serde_json::json!(0);
        let html = render_template("mail_thread.html", &ctx).expect("thread renders");
        assert!(has_template_href(&html, "/mail/alpha/threads"));
        assert!(has_template_href(&html, "/mail/alpha"));
        assert!(html.contains("Browse Threads"));
        assert!(html.contains("Back to Threads"));
        assert_independent_content_anchors(&html);
    }

    // --- ENV initialization ---

    #[test]
    fn env_initializes_without_panic() {
        // Force lazy initialization
        let _ = &*ENV;
    }

    // --- third-party assets (br-kp1in.43) ---

    /// Whether a CDN URL names an exact `x.y.z` version: every `name@version`
    /// segment must be exact (`@latest`, `@3` and `@3.x.x` float), and some
    /// segment must carry one (cdnjs and Tailwind use a bare `x.y.z` segment).
    fn version_pinned(url: &str) -> bool {
        let is_semver = |text: &str| {
            let parts: Vec<&str> = text.split('.').collect();
            parts.len() == 3
                && parts
                    .iter()
                    .all(|part| !part.is_empty() && part.bytes().all(|b| b.is_ascii_digit()))
        };
        let mut versioned = false;
        for segment in url.trim_start_matches("https://").split(['/', '?']) {
            // A leading `@` is an npm scope (`@mojs/core`), not a version.
            if segment
                .split_once('@')
                .is_some_and(|(name, _)| !name.is_empty())
            {
                if !segment.rsplit('@').next().is_some_and(is_semver) {
                    return false;
                }
                versioned = true;
            } else if is_semver(segment) {
                versioned = true;
            }
        }
        versioned
    }

    #[test]
    fn version_pinning_rejects_floating_tags() {
        for floating in [
            "https://unpkg.com/lucide@latest",
            "https://cdn.jsdelivr.net/npm/alpinejs@3.x.x/dist/cdn.min.js",
            "https://cdn.jsdelivr.net/npm/marked@11/marked.min.js",
            "https://unpkg.com/@popperjs/core@2",
            "https://cdn.tailwindcss.com",
        ] {
            assert!(
                !version_pinned(floating),
                "{floating} must count as floating"
            );
        }
        for pinned in [
            "https://unpkg.com/@popperjs/core@2.11.8/dist/umd/popper.min.js",
            "https://cdnjs.cloudflare.com/ajax/libs/prism/1.29.0/prism.min.js",
            "https://cdn.tailwindcss.com/3.4.17",
        ] {
            assert!(version_pinned(pinned), "{pinned} is pinned");
        }
    }

    fn validate_external_assets(html: &str) -> Result<usize, String> {
        let mut with_sri = 0;
        for tag in html.split('<').skip(1) {
            let tag = tag.split('>').next().unwrap_or_default();
            let attr = if tag.starts_with("script") {
                "src=\""
            } else if tag.starts_with("link") && tag.contains("stylesheet") {
                "href=\""
            } else {
                continue;
            };
            let Some(url) = tag
                .split(attr)
                .nth(1)
                .and_then(|rest| rest.split('"').next())
            else {
                continue;
            };
            if url.starts_with("http://") || url.starts_with("//") {
                return Err(format!("{url} must use HTTPS"));
            }
            if !url.starts_with("https://") {
                continue;
            }
            if !version_pinned(url) {
                return Err(format!("{url} is not pinned to an exact version"));
            }
            if !tag.contains("integrity=\"sha384-") || !tag.contains("crossorigin=\"anonymous\"") {
                return Err(format!("{url} loads without SRI"));
            }
            with_sri += 1;
        }
        // A module import cannot carry SRI, so none may load from a CDN.
        for quote in ['\'', '"'] {
            if html.contains(&format!("from {quote}https://"))
                || html.contains(&format!("import({quote}https://"))
            {
                return Err("a module import loads a CDN URL without SRI".to_string());
            }
        }
        Ok(with_sri)
    }

    #[test]
    fn external_asset_validation_rejects_the_old_tailwind_exemption() {
        let legacy = "<script src=\"https://cdn.tailwindcss.com/3.4.17\"></script>";
        assert_eq!(
            validate_external_assets(legacy).unwrap_err(),
            "https://cdn.tailwindcss.com/3.4.17 loads without SRI"
        );
        for url in [
            "http://cdn.example.test/library/1.2.3/script.js",
            "//cdn.example.test/library/1.2.3/script.js",
        ] {
            assert!(validate_external_assets(&format!("<script src=\"{url}\"></script>")).is_err());
        }
    }

    /// Every remote script/stylesheet names an exact version and carries SRI.
    /// The original missing-integrity Tailwind tag is a failing negative control.
    #[test]
    fn third_party_assets_are_version_pinned_with_sri() {
        let mut with_sri = 0;
        for file in TEMPLATE_DIR.files() {
            let Some(html) = file.contents_utf8() else {
                continue;
            };
            with_sri += validate_external_assets(html)
                .unwrap_or_else(|error| panic!("{}: {error}", file.path().display()));
        }
        assert!(
            with_sri >= 30,
            "only {with_sri} SRI-checked assets: the scan no longer matches base.html"
        );
    }
}
