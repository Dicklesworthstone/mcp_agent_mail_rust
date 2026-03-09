//! Mail UI HTTP route handlers.
//!
//! Implements the `/mail/*` HTML routes that display the agent mail web interface.
//! Each route loads data from the DB, renders a Jinja template, and returns HTML.

#![forbid(unsafe_code)]

use std::collections::{BTreeMap, HashSet};
use std::path::Path;

use asupersync::Cx;
use fastmcp_core::block_on;
use mcp_agent_mail_core::config::Config;
use mcp_agent_mail_db::models::{AgentRow, ProjectRow};
use mcp_agent_mail_db::pool::DbPool;
use mcp_agent_mail_db::timestamps::{micros_to_iso, micros_to_naive, now_micros};
use mcp_agent_mail_db::{DbPoolConfig, get_or_create_pool, queries};
use mcp_agent_mail_storage::{self as storage, ensure_archive_root};
use serde::Serialize;

use crate::markdown;
use crate::templates;

/// Dispatch a mail UI request to the correct handler.
///
/// Returns `Some(html_or_json_string)` if the route was handled, `None` for unrecognized paths.
/// Returns `Err(status, message)` for errors.
///
/// `method` should be `"GET"` or `"POST"`.
/// `body` is the raw request body (only relevant for POST requests).
pub fn dispatch(
    path: &str,
    query: &str,
    method: &str,
    body: &str,
) -> Result<Option<String>, (u16, String)> {
    let cx = Cx::for_testing();
    let pool = get_pool()?;

    // Strip leading "/mail" prefix.
    let sub = path.strip_prefix("/mail").unwrap_or(path);

    match sub {
        // Python parity: GET /mail and /mail/unified-inbox → unified inbox.
        "" | "/" | "/unified-inbox" => {
            let limit = extract_query_int(query, "limit", 1000).clamp(1, 1000);
            let filter_importance = extract_query_str(query, "filter_importance");
            render_unified_inbox(
                &cx,
                &pool,
                limit,
                filter_importance.as_deref(),
                is_static_export_request(query),
            )
        }
        // Explicit projects list route (legacy Python: GET /mail/projects).
        "/projects" => render_projects_list(&cx, &pool),
        _ if sub.starts_with("/api/") => handle_api_route(sub, query, method, body, &cx, &pool),
        _ if sub.starts_with("/archive/") => render_archive_route(sub, query, method, &cx, &pool),
        _ => dispatch_project_route(sub, method, body, &cx, &pool, query),
    }
}

#[cfg(test)]
fn initialized_test_pool(prefix: &str) -> DbPool {
    let nonce = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("clock should be after unix epoch")
        .as_nanos();
    let db_path = std::env::temp_dir().join(format!("{prefix}-{nonce}.sqlite3"));
    let conn = mcp_agent_mail_db::DbConn::open_file(db_path.display().to_string())
        .expect("test sqlite file should initialize");
    conn.execute_raw("PRAGMA journal_mode=WAL")
        .expect("test sqlite journal mode should initialize");
    let cfg = DbPoolConfig {
        database_url: format!("sqlite:///{}", db_path.display()),
        ..DbPoolConfig::default()
    };
    get_or_create_pool(&cfg).expect("test pool should initialize")
}

#[cfg(test)]
mod route_regressions {
    use super::*;
    use asupersync::Outcome;
    use std::collections::BTreeSet;
    use std::time::{SystemTime, UNIX_EPOCH};

    fn outcome_ok<T>(outcome: Outcome<T, mcp_agent_mail_db::DbError>) -> T {
        match outcome {
            Outcome::Ok(value) => value,
            Outcome::Err(err) => panic!("db error: {err}"),
            Outcome::Cancelled(_) => panic!("db operation cancelled"),
            Outcome::Panicked(panic) => panic!("db operation panicked: {}", panic.message()),
        }
    }

    fn unique_nonce() -> u128 {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("clock should be after unix epoch")
            .as_nanos()
    }

    fn make_test_pool(label: &str) -> DbPool {
        initialized_test_pool(&format!("mail-ui-{label}"))
    }

    #[test]
    fn unified_message_view_populates_client_fields() {
        let aggregate = UnifiedMessageAggregate {
            id: 7,
            subject: "Weekly update".to_string(),
            body_md: "# Hello\nThis is the body".to_string(),
            created_ts: now_micros(),
            importance: "high".to_string(),
            thread_id: "br-7".to_string(),
            project_slug: "demo".to_string(),
            project_name: "Demo".to_string(),
            sender: "GreenCastle".to_string(),
            recipients: BTreeSet::from(["BlueLake".to_string(), "AmberPeak".to_string()]),
            recipient_read: BTreeMap::from([
                ("AmberPeak".to_string(), true),
                ("BlueLake".to_string(), true),
            ]),
            all_read: true,
        };

        let view = aggregate.into_view();
        assert_eq!(view.recipients, "AmberPeak, BlueLake");
        assert_eq!(view.recipient_names, vec!["AmberPeak", "BlueLake"]);
        assert_eq!(
            view.recipient_read,
            BTreeMap::from([
                ("AmberPeak".to_string(), true),
                ("BlueLake".to_string(), true),
            ])
        );
        assert!(view.read);
        assert!(view.excerpt.contains("Hello"));
        assert!(!view.created_full.is_empty());
        assert!(!view.created_relative.is_empty());
    }

    #[test]
    fn unified_api_message_value_preserves_client_fields() {
        let message = UnifiedMessageAggregate {
            id: 9,
            subject: "API parity".to_string(),
            body_md: "Body for unified inbox parity".to_string(),
            created_ts: now_micros(),
            importance: "normal".to_string(),
            thread_id: "br-unified".to_string(),
            project_slug: "demo".to_string(),
            project_name: "Demo".to_string(),
            sender: "GreenCastle".to_string(),
            recipients: BTreeSet::from(["BlueLake".to_string()]),
            recipient_read: BTreeMap::from([("BlueLake".to_string(), false)]),
            all_read: false,
        }
        .into_view();

        let payload = unified_api_message_value(&message);
        assert_eq!(payload["recipients"], "BlueLake");
        assert_eq!(payload["recipient_names"], serde_json::json!(["BlueLake"]));
        assert_eq!(
            payload["recipient_read"],
            serde_json::json!({ "BlueLake": false })
        );
        assert_eq!(payload["read"], false);
        assert!(
            payload["excerpt"]
                .as_str()
                .is_some_and(|value| !value.is_empty())
        );
    }

    #[test]
    fn render_attachments_lists_message_attachments() {
        let cx = Cx::for_testing();
        let pool = make_test_pool("attachments");
        let project = outcome_ok(block_on(queries::ensure_project(
            &cx,
            &pool,
            &format!("/tmp/mail-ui-attachments-{}", unique_nonce()),
        )));
        let project_id = project.id.unwrap_or(0);

        let sender = outcome_ok(block_on(queries::register_agent(
            &cx,
            &pool,
            project_id,
            "GreenCastle",
            "test",
            "test",
            None,
            None,
        )));
        let recipient = outcome_ok(block_on(queries::register_agent(
            &cx, &pool, project_id, "BlueLake", "test", "test", None, None,
        )));

        outcome_ok(block_on(queries::create_message_with_recipients(
            &cx,
            &pool,
            project_id,
            sender.id.unwrap_or(0),
            "Attachment delivery",
            "See attached artifact",
            Some("br-attachments"),
            "normal",
            false,
            r#"[{"name":"artifact.txt","path":"attachments/demo.txt","content_type":"text/plain","size":"128"}]"#,
            &[(recipient.id.unwrap_or(0), "to")],
        )));

        let html = render_attachments(&cx, &pool, &project.slug)
            .expect("attachments render should succeed")
            .expect("attachments route should return html");
        assert!(html.contains("artifact.txt"));
        assert!(html.contains("128"));
    }

    #[test]
    fn parse_attachment_views_accepts_content_type_and_size_aliases() {
        let attachments = parse_attachment_views(
            r#"[{"name":"artifact.txt","path":"attachments/demo.txt","content_type":"text/plain","size":"128"}]"#,
        );
        assert_eq!(attachments.len(), 1);
        assert_eq!(attachments[0].name.as_deref(), Some("artifact.txt"));
        assert_eq!(attachments[0].path.as_deref(), Some("attachments/demo.txt"));
        assert_eq!(attachments[0].media_type.as_deref(), Some("text/plain"));
        assert_eq!(attachments[0].bytes, Some(128));
    }

    #[test]
    fn render_message_renders_sender_recipients_and_thread_preview() {
        let cx = Cx::for_testing();
        let pool = make_test_pool("message");
        let project = outcome_ok(block_on(queries::ensure_project(
            &cx,
            &pool,
            &format!("/tmp/mail-ui-message-{}", unique_nonce()),
        )));
        let project_id = project.id.unwrap_or(0);

        let sender = outcome_ok(block_on(queries::register_agent(
            &cx,
            &pool,
            project_id,
            "GreenCastle",
            "test",
            "test",
            None,
            None,
        )));
        let blue = outcome_ok(block_on(queries::register_agent(
            &cx, &pool, project_id, "BlueLake", "test", "test", None, None,
        )));
        let amber = outcome_ok(block_on(queries::register_agent(
            &cx,
            &pool,
            project_id,
            "AmberPeak",
            "test",
            "test",
            None,
            None,
        )));

        let root = outcome_ok(block_on(queries::create_message_with_recipients(
            &cx,
            &pool,
            project_id,
            sender.id.unwrap_or(0),
            "Thread root",
            "First message",
            Some("br-message"),
            "high",
            false,
            "[]",
            &[(blue.id.unwrap_or(0), "to"), (amber.id.unwrap_or(0), "cc")],
        )));
        outcome_ok(block_on(queries::create_message_with_recipients(
            &cx,
            &pool,
            project_id,
            blue.id.unwrap_or(0),
            "Thread reply",
            "Reply body",
            Some("br-message"),
            "normal",
            false,
            "[]",
            &[(sender.id.unwrap_or(0), "to")],
        )));

        let html = render_message(&cx, &pool, &project.slug, root.id.unwrap_or(0))
            .expect("message render should succeed")
            .expect("message route should return html");
        assert!(html.contains("GreenCastle"));
        assert!(html.contains("BlueLake"));
        assert!(html.contains("AmberPeak"));
        assert!(html.contains("Part of a Conversation Thread"));
    }

    #[test]
    fn render_message_root_seed_uses_numeric_thread_reference() {
        let cx = Cx::for_testing();
        let pool = make_test_pool("message-root-thread");
        let project = outcome_ok(block_on(queries::ensure_project(
            &cx,
            &pool,
            &format!("/tmp/mail-ui-message-root-thread-{}", unique_nonce()),
        )));
        let project_id = project.id.unwrap_or(0);

        let sender = outcome_ok(block_on(queries::register_agent(
            &cx,
            &pool,
            project_id,
            "GreenCastle",
            "test",
            "test",
            None,
            None,
        )));
        let recipient = outcome_ok(block_on(queries::register_agent(
            &cx, &pool, project_id, "BlueLake", "test", "test", None, None,
        )));

        let root = outcome_ok(block_on(queries::create_message_with_recipients(
            &cx,
            &pool,
            project_id,
            sender.id.unwrap_or(0),
            "Numeric thread root",
            "Kickoff",
            None,
            "normal",
            false,
            "[]",
            &[(recipient.id.unwrap_or(0), "to")],
        )));
        let root_id = root.id.unwrap_or(0);
        let root_thread_ref = root_id.to_string();
        outcome_ok(block_on(queries::create_message_with_recipients(
            &cx,
            &pool,
            project_id,
            recipient.id.unwrap_or(0),
            "Numeric thread reply",
            "Reply",
            Some(&root_thread_ref),
            "normal",
            false,
            "[]",
            &[(sender.id.unwrap_or(0), "to")],
        )));

        let html = render_message(&cx, &pool, &project.slug, root_id)
            .expect("message render should succeed")
            .expect("message route should return html");
        assert!(html.contains(&format!("/mail/{}/thread/{root_id}", project.slug)));
        assert!(html.contains("Part of a Conversation Thread"));
    }

    #[test]
    fn render_inbox_root_seed_uses_numeric_thread_reference() {
        let cx = Cx::for_testing();
        let pool = make_test_pool("inbox-root-thread");
        let project = outcome_ok(block_on(queries::ensure_project(
            &cx,
            &pool,
            &format!("/tmp/mail-ui-inbox-root-thread-{}", unique_nonce()),
        )));
        let project_id = project.id.unwrap_or(0);

        let sender = outcome_ok(block_on(queries::register_agent(
            &cx,
            &pool,
            project_id,
            "GreenCastle",
            "test",
            "test",
            None,
            None,
        )));
        let recipient = outcome_ok(block_on(queries::register_agent(
            &cx, &pool, project_id, "BlueLake", "test", "test", None, None,
        )));

        let root = outcome_ok(block_on(queries::create_message_with_recipients(
            &cx,
            &pool,
            project_id,
            sender.id.unwrap_or(0),
            "Inbox numeric thread root",
            "Kickoff",
            None,
            "normal",
            false,
            "[]",
            &[(recipient.id.unwrap_or(0), "to")],
        )));
        let root_id = root.id.unwrap_or(0);
        let root_thread_ref = root_id.to_string();
        outcome_ok(block_on(queries::create_message_with_recipients(
            &cx,
            &pool,
            project_id,
            recipient.id.unwrap_or(0),
            "Inbox numeric thread reply",
            "Reply",
            Some(&root_thread_ref),
            "normal",
            false,
            "[]",
            &[(sender.id.unwrap_or(0), "to")],
        )));

        let html = render_inbox(&cx, &pool, &project.slug, &recipient.name, 50, 1, false)
            .expect("inbox render should succeed")
            .expect("inbox route should return html");
        assert!(html.contains(&format!("/mail/{}/thread/{root_id}", project.slug)));
    }

    #[test]
    fn render_unified_inbox_static_export_marks_snapshot_mode() {
        let cx = Cx::for_testing();
        let pool = make_test_pool("unified-static");
        let html = render_unified_inbox(&cx, &pool, 10, None, true)
            .expect("unified inbox render should succeed")
            .expect("unified inbox should return html");
        assert!(html.contains("Static export snapshot"));
    }

    #[test]
    fn render_unified_inbox_serializes_normalized_importance_filter() {
        let cx = Cx::for_testing();
        let pool = make_test_pool("unified-importance-filter-template");
        let html = render_unified_inbox(&cx, &pool, 10, Some(" HIGH "), false)
            .expect("unified inbox render should succeed")
            .expect("unified inbox should return html");
        assert!(html.contains(r#"const initialImportanceFilter = "high";"#));
    }

    #[test]
    fn render_unified_inbox_wires_importance_filter_refresh_handler() {
        let cx = Cx::for_testing();
        let pool = make_test_pool("unified-importance-filter-handler");
        let html = render_unified_inbox(&cx, &pool, 10, None, false)
            .expect("unified inbox render should succeed")
            .expect("unified inbox should return html");
        assert!(html.contains(r#"@change="handleImportanceFilterChange()""#));
        assert!(html.contains("async handleImportanceFilterChange()"));
    }
}

fn get_pool() -> Result<DbPool, (u16, String)> {
    let cfg = DbPoolConfig::from_env();
    get_or_create_pool(&cfg).map_err(|e| (500, format!("Database error: {e}")))
}

fn block_on_outcome<T>(
    _cx: &Cx,
    fut: impl std::future::Future<Output = asupersync::Outcome<T, mcp_agent_mail_db::DbError>>,
) -> Result<T, (u16, String)> {
    match block_on(fut) {
        asupersync::Outcome::Ok(v) => Ok(v),
        asupersync::Outcome::Err(e) => {
            let status = if matches!(e, mcp_agent_mail_db::DbError::NotFound { .. }) {
                404
            } else {
                500
            };
            Err((status, e.to_string()))
        }
        asupersync::Outcome::Cancelled(_) => Err((503, "Request cancelled".to_string())),
        asupersync::Outcome::Panicked(p) => Err((500, format!("Internal error: {}", p.message()))),
    }
}

fn render(name: &str, ctx: impl Serialize) -> Result<Option<String>, (u16, String)> {
    templates::render_template(name, ctx)
        .map(Some)
        .map_err(|e| (500, format!("Template error: {e}")))
}

// ---------------------------------------------------------------------------
// Query-string helpers
// ---------------------------------------------------------------------------

fn extract_query_str(query: &str, key: &str) -> Option<String> {
    for pair in query.split('&') {
        if let Some((k, v)) = pair.split_once('=')
            && k == key
            && !v.is_empty()
        {
            return Some(percent_decode_component(v));
        }
    }
    None
}

fn extract_query_int(query: &str, key: &str, default: usize) -> usize {
    extract_query_str(query, key)
        .and_then(|v| v.parse().ok())
        .unwrap_or(default)
}

fn is_static_export_request(query: &str) -> bool {
    extract_query_str(query, "__static_export").is_some()
}

fn is_valid_project_slug(slug: &str) -> bool {
    !slug.is_empty()
        && slug
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_'))
}

fn is_valid_archive_agent_name(name: &str) -> bool {
    !name.is_empty() && name.bytes().all(|b| b.is_ascii_alphanumeric())
}

fn is_valid_time_travel_timestamp(timestamp: &str) -> bool {
    // Python parity: must match prefix YYYY-MM-DDTHH:MM, with optional trailing
    // seconds/fraction/timezone.
    let bytes = timestamp.as_bytes();
    if bytes.len() < 16 {
        return false;
    }
    if bytes[4] != b'-' || bytes[7] != b'-' || bytes[10] != b'T' || bytes[13] != b':' {
        return false;
    }

    bytes[0..4].iter().all(u8::is_ascii_digit)
        && bytes[5..7].iter().all(u8::is_ascii_digit)
        && bytes[8..10].iter().all(u8::is_ascii_digit)
        && bytes[11..13].iter().all(u8::is_ascii_digit)
        && bytes[14..16].iter().all(u8::is_ascii_digit)
}

fn archive_browser_file_project_slug(sub: &str) -> Option<&str> {
    let rest = sub.strip_prefix("/archive/browser/")?;
    let (project_slug, tail) = rest.split_once('/')?;
    if project_slug.is_empty() || tail != "file" {
        return None;
    }
    Some(project_slug)
}

/// Percent-decode a single URL query component.
///
/// This is intentionally minimal (no `;` separators, no nested decoding), but:
/// - preserves invalid/truncated `%` escapes verbatim
/// - decodes bytes and then interprets them as UTF-8 (lossy), so non-ASCII works
fn percent_decode_component(input: &str) -> String {
    percent_decode_impl(input, true)
}

fn percent_decode_path_segment(input: &str) -> String {
    percent_decode_impl(input, false)
}

fn percent_decode_impl(input: &str, plus_as_space: bool) -> String {
    let bytes = input.as_bytes();
    let mut out: Vec<u8> = Vec::with_capacity(bytes.len());
    let mut i = 0usize;
    while i < bytes.len() {
        match bytes[i] {
            b'+' if plus_as_space => {
                out.push(b' ');
                i += 1;
            }
            b'%' if i + 2 < bytes.len() => {
                let hi = bytes[i + 1];
                let lo = bytes[i + 2];
                let hex = [hi, lo];
                if let Ok(hex_str) = std::str::from_utf8(&hex)
                    && let Ok(value) = u8::from_str_radix(hex_str, 16)
                {
                    out.push(value);
                    i += 3;
                    continue;
                }
                out.push(bytes[i]);
                i += 1;
            }
            other => {
                out.push(other);
                i += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).to_string()
}

fn percent_encode_path_segment(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    for byte in input.as_bytes() {
        if byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'.' | b'_' | b'~') {
            out.push(char::from(*byte));
        } else {
            use std::fmt::Write as _;
            let _ = write!(out, "%{byte:02X}");
        }
    }
    out
}

fn mail_thread_href(project_slug: &str, thread_id: &str) -> String {
    format!(
        "/mail/{project_slug}/thread/{}",
        percent_encode_path_segment(thread_id)
    )
}

#[cfg(test)]
mod query_decode_tests {
    use super::{
        mail_thread_href, percent_decode_component, percent_decode_path_segment,
        percent_encode_path_segment,
    };

    #[test]
    fn percent_decode_basic() {
        assert_eq!(percent_decode_component("hello"), "hello");
        assert_eq!(percent_decode_component("hello+world"), "hello world");
        assert_eq!(percent_decode_component("hello%20world"), "hello world");
        assert_eq!(percent_decode_component("%40user"), "@user");
        assert_eq!(percent_decode_component("key%3Dvalue"), "key=value");
    }

    #[test]
    fn percent_decode_invalid_hex_is_preserved() {
        assert_eq!(percent_decode_component("%ZZ"), "%ZZ");
        assert_eq!(percent_decode_component("abc%2"), "abc%2");
    }

    #[test]
    fn percent_decode_utf8_multibyte() {
        // "€" U+20AC is UTF-8 bytes E2 82 AC.
        assert_eq!(percent_decode_component("%E2%82%AC"), "€");
    }

    #[test]
    fn percent_decode_path_segment_keeps_plus_literal() {
        assert_eq!(percent_decode_path_segment("topic+a%2Fb"), "topic+a/b");
    }

    #[test]
    fn percent_encode_path_segment_escapes_reserved_bytes() {
        assert_eq!(
            percent_encode_path_segment("topic/a b+"),
            "topic%2Fa%20b%2B"
        );
    }

    #[test]
    fn mail_thread_href_encodes_thread_path_segment() {
        assert_eq!(
            mail_thread_href("demo", "topic/a b+"),
            "/mail/demo/thread/topic%2Fa%20b%2B"
        );
    }
}

#[cfg(test)]
mod utility_tests {
    use super::*;

    // --- extract_query_str ---

    #[test]
    fn extract_query_str_found() {
        assert_eq!(
            extract_query_str("page=2&q=hello", "q"),
            Some("hello".to_string())
        );
    }

    #[test]
    fn extract_query_str_not_found() {
        assert_eq!(extract_query_str("page=2&q=hello", "missing"), None);
    }

    #[test]
    fn extract_query_str_empty_value_returns_none() {
        assert_eq!(extract_query_str("q=", "q"), None);
    }

    #[test]
    fn extract_query_str_with_encoding() {
        assert_eq!(
            extract_query_str("q=hello+world", "q"),
            Some("hello world".to_string())
        );
    }

    #[test]
    fn extract_query_str_first_match() {
        assert_eq!(
            extract_query_str("q=first&q=second", "q"),
            Some("first".to_string())
        );
    }

    #[test]
    fn extract_query_str_empty_query() {
        assert_eq!(extract_query_str("", "q"), None);
    }

    // --- extract_query_int ---

    #[test]
    fn extract_query_int_found() {
        assert_eq!(extract_query_int("page=5&limit=20", "limit", 10), 20);
    }

    #[test]
    fn extract_query_int_not_found_returns_default() {
        assert_eq!(extract_query_int("page=5", "limit", 10), 10);
    }

    #[test]
    fn extract_query_int_invalid_number_returns_default() {
        assert_eq!(extract_query_int("limit=abc", "limit", 10), 10);
    }

    // --- truncate_body ---

    #[test]
    fn truncate_body_short_unchanged() {
        assert_eq!(truncate_body("hello", 100), "hello");
    }

    #[test]
    fn truncate_body_long_truncated() {
        let result = truncate_body("hello world this is a long body", 10);
        assert!(result.ends_with('…'));
        assert!(result.len() <= 14); // 10 bytes + ellipsis char
    }

    #[test]
    fn truncate_body_at_char_boundary() {
        // "café" is 5 bytes (é is 2 bytes), max=4 should not split the é
        let result = truncate_body("café latte", 4);
        assert!(result.ends_with('…'));
        assert!(!result.contains('é')); // Should truncate before the multibyte char
    }

    #[test]
    fn truncate_body_exact_length() {
        assert_eq!(truncate_body("hello", 5), "hello");
    }

    // --- ts_display / ts_display_opt ---

    #[test]
    fn ts_display_formats_micros() {
        let result = ts_display(1_700_000_000_000_000); // ~2023-11-14
        assert!(result.contains("2023"));
    }

    #[test]
    fn ts_display_opt_none_returns_empty() {
        assert_eq!(ts_display_opt(None), "");
    }

    #[test]
    fn ts_display_opt_some_returns_formatted() {
        let result = ts_display_opt(Some(1_700_000_000_000_000));
        assert!(!result.is_empty());
    }

    #[test]
    fn matches_importance_filter_requires_exact_importance() {
        assert!(matches_importance_filter("high", Some("high")));
        assert!(matches_importance_filter("HIGH", Some("high")));
        assert!(matches_importance_filter("urgent", Some("urgent")));
        assert!(matches_importance_filter("low", None));
        assert!(!matches_importance_filter("urgent", Some("high")));
        assert!(!matches_importance_filter("high", Some("urgent")));
        assert!(!matches_importance_filter("normal", Some("low")));
    }

    // --- archive time-travel validation ---

    #[test]
    fn project_slug_validation_python_parity() {
        assert!(is_valid_project_slug("alpha"));
        assert!(is_valid_project_slug("alpha-beta_01"));
        assert!(!is_valid_project_slug("alpha-beta_01.v2"));
        assert!(!is_valid_project_slug(""));
        assert!(!is_valid_project_slug("../etc/passwd"));
        assert!(!is_valid_project_slug("project with spaces"));
    }

    #[test]
    fn archive_agent_name_validation_python_parity() {
        assert!(is_valid_archive_agent_name("Agent123"));
        assert!(is_valid_archive_agent_name("A"));
        assert!(!is_valid_archive_agent_name(""));
        assert!(!is_valid_archive_agent_name("agent-name"));
        assert!(!is_valid_archive_agent_name("agent name"));
        assert!(!is_valid_archive_agent_name("agent!"));
    }

    #[test]
    fn time_travel_timestamp_validation_python_parity() {
        assert!(is_valid_time_travel_timestamp("2026-02-11T05:43"));
        assert!(is_valid_time_travel_timestamp("2026-02-11T05:43:59Z"));
        assert!(is_valid_time_travel_timestamp("2026-02-11T05:43:59+05:30"));
        assert!(is_valid_time_travel_timestamp(
            "2026-02-11T05:43:59.123456-08:00"
        ));

        assert!(!is_valid_time_travel_timestamp(""));
        assert!(!is_valid_time_travel_timestamp("2026-02-11"));
        assert!(!is_valid_time_travel_timestamp("2026-02-11T05"));
        assert!(!is_valid_time_travel_timestamp("not-a-timestamp"));
    }

    #[test]
    fn archive_browser_file_project_slug_parser_enforces_exact_shape() {
        assert_eq!(
            archive_browser_file_project_slug("/archive/browser/demo/file"),
            Some("demo")
        );
        assert_eq!(
            archive_browser_file_project_slug("/archive/browser/demo/file/extra"),
            None
        );
        assert_eq!(
            archive_browser_file_project_slug("/archive/browser/demo"),
            None
        );
    }

    #[test]
    fn run_command_stdout_with_timeout_returns_stdout_for_fast_command() {
        let mut command = std::process::Command::new("sh");
        command.args(["-c", "printf '42K /tmp/archive\\n'"]);
        let stdout =
            run_command_stdout_with_timeout(&mut command, std::time::Duration::from_millis(250))
                .expect("stdout");
        assert_eq!(stdout, "42K /tmp/archive\n");
    }

    #[test]
    fn run_command_stdout_with_timeout_kills_hung_command() {
        let mut command = std::process::Command::new("sh");
        command.args(["-c", "sleep 1"]);
        let started = std::time::Instant::now();
        let stdout =
            run_command_stdout_with_timeout(&mut command, std::time::Duration::from_millis(50));
        assert!(stdout.is_none());
        assert!(
            started.elapsed() < std::time::Duration::from_millis(500),
            "timeout helper should return promptly instead of waiting for the full child runtime"
        );
    }

    #[test]
    fn time_travel_timestamp_rejects_invalid_separators() {
        assert!(!is_valid_time_travel_timestamp("2026/02/11T05:43"));
        assert!(!is_valid_time_travel_timestamp("2026-02-11 05:43"));
    }

    #[test]
    fn highlight_snippet_safely_escapes_and_matches() {
        // "mark" shouldn't match inside the inserted <mark> tag.
        let body = "This is a mark and another mark.";
        let html = highlight_snippet(body, "mark", 100);
        assert_eq!(
            html,
            "This is a <mark>mark</mark> and another <mark>mark</mark>."
        );

        // "amp" shouldn't match inside the &amp; entity.
        let body_with_amp = "amp & voltage";
        let html_amp = highlight_snippet(body_with_amp, "amp", 100);
        assert_eq!(html_amp, "<mark>amp</mark> &amp; voltage");

        // Overlapping terms are merged cleanly.
        let body_overlap = "authenticate";
        let html_overlap = highlight_snippet(body_overlap, "auth thenticate", 100);
        assert_eq!(html_overlap, "<mark>authenticate</mark>");
    }
}

#[cfg(test)]
mod route_hardening_tests {
    use super::*;

    fn make_test_pool() -> DbPool {
        initialized_test_pool("mail-ui-route")
    }

    // F2: Malformed project slugs are rejected with 400 before DB access.
    #[test]
    fn dispatch_project_rejects_path_traversal_slug() {
        let cx = Cx::for_testing();
        let pool = make_test_pool();
        let result = dispatch_project_route("/../../etc/passwd", "GET", "", &cx, &pool, "");
        let (status, _msg) = result.expect_err("path traversal slug should be rejected");
        assert_eq!(status, 400);
    }

    #[test]
    fn dispatch_project_rejects_dot_dot_slug() {
        let cx = Cx::for_testing();
        let pool = make_test_pool();
        let result = dispatch_project_route("/..", "GET", "", &cx, &pool, "");
        let (status, _msg) = result.expect_err(".. slug should be rejected");
        assert_eq!(status, 400);
    }

    #[test]
    fn dispatch_project_rejects_special_chars_in_slug() {
        let cx = Cx::for_testing();
        let pool = make_test_pool();
        for bad_slug in &["/foo bar", "/slug;drop", "/slug'inject", "/slug<xss>"] {
            let result = dispatch_project_route(bad_slug, "GET", "", &cx, &pool, "");
            let (status, _msg) =
                result.expect_err(&format!("slug {bad_slug:?} should be rejected"));
            assert_eq!(status, 400, "slug {bad_slug:?} should return 400");
        }
    }

    #[test]
    fn dispatch_project_accepts_valid_slug_format() {
        let cx = Cx::for_testing();
        let pool = make_test_pool();
        // Valid slug format passes validation (may then 404 on project lookup).
        let result = dispatch_project_route("/my-project_1", "GET", "", &cx, &pool, "");
        // Should not be a 400 — either Ok or a different error from DB lookup.
        if let Err((400, _)) = result {
            panic!("valid slug should not be rejected as 400");
        }
    }

    // F3: Unknown inbox sub-actions return Ok(None) → 404, not the inbox page.
    #[test]
    fn inbox_unknown_subaction_returns_none() {
        let cx = Cx::for_testing();
        let pool = make_test_pool();
        let result = dispatch_project_route(
            "/my-project/inbox/some-agent/unknown-action",
            "GET",
            "",
            &cx,
            &pool,
            "",
        );
        // Ok(None) means 404 at the HTTP handler level.
        assert_eq!(
            result.unwrap(),
            None,
            "unknown inbox sub-action should return None (404)"
        );
    }

    #[test]
    fn inbox_deep_subpath_returns_none() {
        let cx = Cx::for_testing();
        let pool = make_test_pool();
        let result = dispatch_project_route(
            "/my-project/inbox/agent/foo/bar/baz",
            "GET",
            "",
            &cx,
            &pool,
            "",
        );
        assert_eq!(
            result.unwrap(),
            None,
            "deep inbox sub-path should return None (404)"
        );
    }
}

/// F4 regression test suite: comprehensive authorization + route hardening
/// assertions to prevent regressions in slug validation, IDOR prevention,
/// route dispatch strictness, and archive input sanitization.
#[cfg(test)]
mod auth_route_hardening_regression_suite {
    use super::*;

    fn make_test_pool() -> DbPool {
        initialized_test_pool("mail-ui-f4")
    }

    // -- Slug validation regression (F2 scope) --

    #[test]
    fn regression_slug_url_encoded_traversal_rejected() {
        let cx = Cx::for_testing();
        let pool = make_test_pool();
        // Slugs containing non-alphanumeric/hyphen/underscore chars → 400.
        for slug in &["..%2F..%2Fetc", ".hidden", "slug with space"] {
            let path = format!("/{slug}");
            let result = dispatch_project_route(&path, "GET", "", &cx, &pool, "");
            match result {
                Err((400, _)) => {} // expected
                other => panic!("slug {slug:?} should yield 400, got {other:?}"),
            }
        }
    }

    #[test]
    fn regression_path_traversal_in_rest_yields_404_not_traversal() {
        let cx = Cx::for_testing();
        let pool = make_test_pool();
        // "foo/../bar" → slug="foo" (valid), rest="../bar" (unknown route → 404).
        // Path traversal in the rest segment can't escape because routes are
        // matched by exact string, not filesystem paths.
        let result = dispatch_project_route("/foo/../bar", "GET", "", &cx, &pool, "");
        assert_eq!(
            result.unwrap(),
            None,
            "path traversal in rest segment yields 404, not a directory escape"
        );
    }

    #[test]
    fn regression_archive_browser_file_slug_validated() {
        // archive_browser_file_project_slug must reject invalid slugs.
        assert!(archive_browser_file_project_slug("/archive/browser/../../../etc/file").is_none());
        assert!(archive_browser_file_project_slug("/archive/browser//file").is_none());
        assert!(archive_browser_file_project_slug("/archive/browser/ok/file").is_some());
    }

    #[test]
    fn regression_time_travel_rejects_bad_agent_name() {
        // is_valid_archive_agent_name rejects path-traversal and special chars.
        assert!(!is_valid_archive_agent_name(""));
        assert!(!is_valid_archive_agent_name("../etc"));
        assert!(!is_valid_archive_agent_name("agent;DROP"));
        assert!(!is_valid_archive_agent_name("a b c"));
        assert!(is_valid_archive_agent_name("BlueLake"));
        assert!(is_valid_archive_agent_name("Agent42"));
    }

    #[test]
    fn regression_time_travel_rejects_bad_timestamp() {
        assert!(!is_valid_time_travel_timestamp(""));
        assert!(!is_valid_time_travel_timestamp("not-a-date"));
        assert!(!is_valid_time_travel_timestamp("2026/02/11T05:43"));
        assert!(!is_valid_time_travel_timestamp("'; DROP TABLE messages;--"));
        assert!(is_valid_time_travel_timestamp("2026-02-11T05:43"));
    }

    // -- Route dispatch strictness (F3 scope) --

    #[test]
    fn regression_inbox_post_to_nonexistent_action_is_method_not_allowed() {
        let cx = Cx::for_testing();
        let pool = make_test_pool();
        // POST to an unknown inbox sub-action should be 405, not silently handled.
        let result = dispatch_project_route(
            "/my-project/inbox/some-agent/nonexistent",
            "POST",
            "",
            &cx,
            &pool,
            "",
        );
        let (status, _) = result.expect_err("unknown POST inbox action should be rejected");
        assert_eq!(status, 405);
    }

    #[test]
    fn regression_archive_routes_reject_post_method() {
        let cx = Cx::for_testing();
        let pool = make_test_pool();
        let result = render_archive_route("/archive/guide", "", "POST", &cx, &pool);
        let (status, _) = result.expect_err("POST to archive should be 405");
        assert_eq!(status, 405);
    }

    #[test]
    fn regression_unknown_archive_subpath_returns_none() {
        let cx = Cx::for_testing();
        let pool = make_test_pool();
        let result = render_archive_route("/archive/nonexistent", "", "GET", &cx, &pool);
        assert_eq!(
            result.unwrap(),
            None,
            "unknown archive path should return None (404)"
        );
    }

    // -- Aggregate: slug validation applies consistently across entry points --

    #[test]
    fn regression_all_slug_validators_agree_on_boundary_inputs() {
        let boundary_inputs = vec![
            ("valid-slug", true),
            ("slug_with_underscore", true),
            ("slug123", true),
            ("", false),
            (".", false),
            ("..", false),
            ("slug/path", false),
            ("slug space", false),
            ("slug;inject", false),
            ("slug<xss>", false),
            ("slug'quote", false),
            ("slug\"double", false),
            ("slug&param=val", false),
            ("slug%00null", false),
        ];

        for (input, expected_valid) in &boundary_inputs {
            assert_eq!(
                is_valid_project_slug(input),
                *expected_valid,
                "is_valid_project_slug({input:?}) mismatch"
            );
        }
    }

    // -- Cross-project route scoping (F1 scope) --

    #[test]
    fn regression_message_route_format_requires_numeric_id() {
        let cx = Cx::for_testing();
        let pool = make_test_pool();
        // message/{mid} where mid is not numeric → 404 (invalid parse returns None).
        let result = dispatch_project_route("/my-project/message/abc", "GET", "", &cx, &pool, "");
        // Non-numeric message IDs should gracefully fail (Ok(None) or Err(400/404)).
        match result {
            Ok(None) | Err((400 | 404, _)) => {} // acceptable
            other => panic!("non-numeric message id should fail gracefully, got {other:?}"),
        }
    }

    #[test]
    fn regression_thread_route_decodes_percent_encoded_thread_id() {
        let cx = Cx::for_testing();
        let pool = make_test_pool();
        let project = outcome_ok(block_on(queries::ensure_project(
            &cx,
            &pool,
            &format!("/tmp/mail-ui-encoded-thread-{}", unique_nonce()),
        )));
        let project_id = project.id.unwrap_or(0);

        let sender = outcome_ok(block_on(queries::register_agent(
            &cx, &pool, project_id, "BlueLake", "test", "test", None, None,
        )));
        let sender_id = sender.id.unwrap_or(0);
        let thread_id = "topic/with space+plus";

        outcome_ok(block_on(queries::create_message(
            &cx,
            &pool,
            project_id,
            sender_id,
            "Encoded thread subject",
            "Encoded thread body",
            Some(thread_id),
            "normal",
            false,
            "[]",
        )));

        let route = format!(
            "/{}/thread/{}",
            project.slug,
            percent_encode_path_segment(thread_id)
        );
        let html = dispatch_project_route(&route, "GET", "", &cx, &pool, "")
            .expect("thread route should succeed")
            .expect("thread route should render html");

        assert!(html.contains("Encoded thread subject"));
        assert!(html.contains("topic/with space+plus"));
    }

    // -- Method enforcement on known routes --

    #[test]
    fn regression_mark_read_only_accepts_post() {
        let cx = Cx::for_testing();
        let pool = make_test_pool();
        // GET on mark-read should be 405.
        let result = dispatch_project_route(
            "/my-project/inbox/agent/mark-read",
            "GET",
            "",
            &cx,
            &pool,
            "",
        );
        // GET on a POST-only action should return Ok(None) from the ("GET", _) arm.
        assert_eq!(
            result.unwrap(),
            None,
            "GET on mark-read should not render anything (404)"
        );
    }

    #[test]
    fn regression_overseer_send_only_accepts_post() {
        let cx = Cx::for_testing();
        let pool = make_test_pool();
        // GET /mail/{project}/overseer/send → should not render (only POST accepted).
        let result = dispatch_project_route("/my-project/overseer/send", "GET", "", &cx, &pool, "");
        assert_eq!(
            result.unwrap(),
            None,
            "GET on overseer/send should return None (404)"
        );
    }
}

// ---------------------------------------------------------------------------
// Timestamp formatting for templates
// ---------------------------------------------------------------------------

fn ts_display(micros: i64) -> String {
    micros_to_iso(micros)
}

fn ts_display_opt(micros: Option<i64>) -> String {
    micros.map_or_else(String::new, ts_display)
}

/// Format micros as "Month DD, YYYY at I:MM PM" (Python parity: `created_full`).
fn ts_display_full(micros: i64) -> String {
    micros_to_naive(micros)
        .format("%B %d, %Y at %l:%M %p")
        .to_string()
}

/// Format micros as relative time (e.g. "5m ago", "2h ago", "3d ago").
/// Python parity: `created_relative`.
fn ts_display_relative(micros: i64) -> String {
    let now = now_micros();
    let delta_secs = (now - micros).max(0) / 1_000_000;
    if delta_secs < 60 {
        "Just now".to_string()
    } else if delta_secs < 3600 {
        format!("{}m ago", delta_secs / 60)
    } else if delta_secs < 86400 {
        format!("{}h ago", delta_secs / 3600)
    } else {
        format!("{}d ago", delta_secs / 86400)
    }
}

/// Extract a plain-text excerpt from markdown body (first 150 chars).
fn body_excerpt(body: &str, max_len: usize) -> String {
    // Strip basic markdown/HTML for a clean excerpt.
    let plain: String = body
        .chars()
        .filter(|c| *c != '#' && *c != '*' && *c != '`')
        .collect();
    let trimmed = plain.trim();
    if trimmed.len() <= max_len {
        trimmed.to_string()
    } else {
        let mut end = max_len;
        while end > 0 && !trimmed.is_char_boundary(end) {
            end -= 1;
        }
        format!("{}…", &trimmed[..end])
    }
}

// ---------------------------------------------------------------------------
// Route: GET /mail — project index
// ---------------------------------------------------------------------------

#[derive(Serialize)]
struct IndexCtx {
    projects: Vec<IndexProject>,
}

#[derive(Serialize)]
struct IndexProject {
    slug: String,
    human_key: String,
    created_at: String,
    agent_count: usize,
}

fn render_index(cx: &Cx, pool: &DbPool) -> Result<Option<String>, (u16, String)> {
    let projects = block_on_outcome(cx, queries::list_projects(cx, pool))?;
    let mut items: Vec<IndexProject> = Vec::with_capacity(projects.len());
    for p in &projects {
        let agents = block_on_outcome(cx, queries::list_agents(cx, pool, p.id.unwrap_or(0)))?;
        items.push(IndexProject {
            slug: p.slug.clone(),
            human_key: p.human_key.clone(),
            created_at: ts_display(p.created_at),
            agent_count: agents.len(),
        });
    }
    render("mail_index.html", IndexCtx { projects: items })
}

// ---------------------------------------------------------------------------
// Route: GET /mail/projects — explicit projects list (Python parity)
// ---------------------------------------------------------------------------

fn render_projects_list(cx: &Cx, pool: &DbPool) -> Result<Option<String>, (u16, String)> {
    // Reuse the index renderer — Python's /mail/projects renders the same template
    // as the old /mail root (project list view).
    render_index(cx, pool)
}

// ---------------------------------------------------------------------------
// JSON response helper (for POST endpoints returning JSON)
// ---------------------------------------------------------------------------

fn json_ok(value: &serde_json::Value) -> Result<Option<String>, (u16, String)> {
    serde_json::to_string(value)
        .map(Some)
        .map_err(|e| (500, format!("JSON error: {e}")))
}

fn json_err(status: u16, detail: &str) -> Result<Option<String>, (u16, String)> {
    let body = serde_json::json!({ "error": detail });
    let s = serde_json::to_string(&body).unwrap_or_else(|_| format!("{{\"error\":\"{detail}\"}}"));
    // Return the JSON as a successful dispatch result so the caller can set the HTTP status.
    // We embed the status in the Err variant so the server can return the right code.
    Err((status, s))
}

fn json_detail_err(status: u16, detail: &str) -> Result<Option<String>, (u16, String)> {
    let body = serde_json::json!({ "detail": detail });
    let s = serde_json::to_string(&body).unwrap_or_else(|_| format!("{{\"detail\":\"{detail}\"}}"));
    Err((status, s))
}

// ---------------------------------------------------------------------------
// Route: GET /mail/unified-inbox
// ---------------------------------------------------------------------------

#[derive(Serialize)]
struct UnifiedInboxCtx {
    projects: Vec<UnifiedProject>,
    messages: Vec<UnifiedMessage>,
    total_agents: usize,
    total_messages: usize,
    filter_importance: String,
    static_export: bool,
}

#[derive(Serialize)]
struct UnifiedProject {
    id: i64,
    slug: String,
    human_key: String,
    created_at: String,
    agent_count: usize,
    agents: Vec<UnifiedAgent>,
}

#[derive(Serialize)]
struct UnifiedAgent {
    id: i64,
    name: String,
    program: String,
    model: String,
    last_active: String,
}

#[derive(Serialize)]
struct UnifiedMessage {
    id: i64,
    subject: String,
    body_md: String,
    body_html: String,
    created: String,
    created_full: String,
    created_relative: String,
    importance: String,
    thread_id: String,
    project_slug: String,
    project_name: String,
    sender: String,
    recipients: String,
    recipient_names: Vec<String>,
    recipient_read: BTreeMap<String, bool>,
    read: bool,
    excerpt: String,
}

#[derive(Debug)]
struct UnifiedMessageAggregate {
    id: i64,
    subject: String,
    body_md: String,
    created_ts: i64,
    importance: String,
    thread_id: String,
    project_slug: String,
    project_name: String,
    sender: String,
    recipients: std::collections::BTreeSet<String>,
    recipient_read: BTreeMap<String, bool>,
    all_read: bool,
}

impl UnifiedMessageAggregate {
    fn from_inbox_row(
        project: &ProjectRow,
        recipient_name: &str,
        row: &queries::InboxRow,
        root_ids_with_replies: &HashSet<i64>,
    ) -> Self {
        let message = &row.message;
        let mut recipients = std::collections::BTreeSet::new();
        recipients.insert(recipient_name.to_string());
        let mut recipient_read = BTreeMap::new();
        recipient_read.insert(recipient_name.to_string(), row.read_ts.is_some());
        Self {
            id: message.id.unwrap_or(0),
            subject: message.subject.clone(),
            body_md: message.body_md.clone(),
            created_ts: message.created_ts,
            importance: message.importance.clone(),
            thread_id: display_thread_ref_for_message(
                message.id.unwrap_or(0),
                message.thread_id.as_deref(),
                root_ids_with_replies,
            ),
            project_slug: project.slug.clone(),
            project_name: project.human_key.clone(),
            sender: row.sender_name.clone(),
            recipients,
            recipient_read,
            all_read: row.read_ts.is_some(),
        }
    }

    fn absorb(&mut self, recipient_name: &str, row: &queries::InboxRow) {
        self.recipients.insert(recipient_name.to_string());
        self.recipient_read
            .insert(recipient_name.to_string(), row.read_ts.is_some());
        self.all_read &= row.read_ts.is_some();
    }

    fn into_view(self) -> UnifiedMessage {
        let recipient_names = self.recipients.into_iter().collect::<Vec<_>>();
        let recipients = recipient_names.join(", ");
        let created = ts_display(self.created_ts);
        UnifiedMessage {
            id: self.id,
            subject: self.subject,
            body_html: markdown::render_markdown_to_safe_html(&self.body_md),
            excerpt: body_excerpt(&self.body_md, 150),
            body_md: self.body_md,
            created_full: ts_display_full(self.created_ts),
            created_relative: ts_display_relative(self.created_ts),
            created,
            importance: self.importance,
            thread_id: self.thread_id,
            project_slug: self.project_slug,
            project_name: self.project_name,
            sender: self.sender,
            recipients,
            recipient_names,
            recipient_read: self.recipient_read,
            read: self.all_read,
        }
    }
}

fn explicit_thread_ref(thread_id: Option<&str>) -> Option<String> {
    thread_id
        .map(str::trim)
        .filter(|thread_id| !thread_id.is_empty())
        .map(str::to_string)
}

fn root_ids_with_replies(
    cx: &Cx,
    pool: &DbPool,
    project_id: i64,
    root_message_ids: &[i64],
) -> Result<HashSet<i64>, (u16, String)> {
    Ok(block_on_outcome(
        cx,
        queries::list_numeric_thread_roots_with_replies(cx, pool, project_id, root_message_ids),
    )?
    .into_iter()
    .collect())
}

fn display_thread_ref_for_message(
    message_id: i64,
    thread_id: Option<&str>,
    root_ids_with_replies: &HashSet<i64>,
) -> String {
    if let Some(thread_ref) = explicit_thread_ref(thread_id) {
        return thread_ref;
    }
    if message_id > 0 && root_ids_with_replies.contains(&message_id) {
        return message_id.to_string();
    }
    String::new()
}

fn collect_unified_message_aggregates(
    cx: &Cx,
    pool: &DbPool,
    projects_rows: &[ProjectRow],
    limit: usize,
    filter_importance: Option<&str>,
) -> Result<Vec<UnifiedMessageAggregate>, (u16, String)> {
    // Applying the importance filter after a tight per-agent inbox window can
    // hide matching messages behind newer non-matching rows. Over-fetch when a
    // server-side importance filter is active so the filtered unified view stays
    // complete and then truncate after aggregation.
    let per_agent_limit = if filter_importance.is_some() {
        limit.max(10_000)
    } else {
        limit.max(1)
    };
    let mut messages = BTreeMap::new();

    for project in projects_rows {
        let pid = project.id.unwrap_or(0);
        let agents_rows = block_on_outcome(cx, queries::list_agents(cx, pool, pid))?;
        let mut project_inbox_rows = Vec::new();
        let mut candidate_root_ids = Vec::new();
        for agent in &agents_rows {
            let aid = agent.id.unwrap_or(0);
            let inbox = block_on_outcome(
                cx,
                queries::fetch_inbox(cx, pool, pid, aid, false, None, per_agent_limit),
            )?;
            for row in inbox {
                let message = &row.message;
                if !matches_importance_filter(&message.importance, filter_importance) {
                    continue;
                }
                if explicit_thread_ref(message.thread_id.as_deref()).is_none()
                    && let Some(message_id) = message.id
                    && message_id > 0
                {
                    candidate_root_ids.push(message_id);
                }
                project_inbox_rows.push((agent.name.clone(), row));
            }
        }

        let reply_root_ids = root_ids_with_replies(cx, pool, pid, &candidate_root_ids)?;
        for (agent_name, row) in project_inbox_rows {
            let message = &row.message;
            let Some(message_id) = message.id else {
                continue;
            };
            if let Some(entry) = messages.get_mut(&message_id) {
                entry.absorb(&agent_name, &row);
                continue;
            }

            let aggregate = UnifiedMessageAggregate::from_inbox_row(
                project,
                &agent_name,
                &row,
                &reply_root_ids,
            );
            messages.insert(message_id, aggregate);
        }
    }

    let mut out: Vec<UnifiedMessageAggregate> = messages.into_values().collect();
    out.sort_by(|a, b| b.created_ts.cmp(&a.created_ts).then(b.id.cmp(&a.id)));
    out.truncate(limit);
    Ok(out)
}

fn render_unified_inbox(
    cx: &Cx,
    pool: &DbPool,
    limit: usize,
    filter_importance: Option<&str>,
    static_export: bool,
) -> Result<Option<String>, (u16, String)> {
    let normalized_filter = filter_importance
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_ascii_lowercase);
    let projects_rows = block_on_outcome(cx, queries::list_projects(cx, pool))?;

    let mut projects = Vec::new();
    let mut total_agents: usize = 0;
    for p in &projects_rows {
        let pid = p.id.unwrap_or(0);
        let agents_rows = block_on_outcome(cx, queries::list_agents(cx, pool, pid))?;
        if agents_rows.is_empty() {
            continue;
        }
        total_agents += agents_rows.len();
        let agents: Vec<UnifiedAgent> = agents_rows
            .iter()
            .map(|a| UnifiedAgent {
                id: a.id.unwrap_or(0),
                name: a.name.clone(),
                program: a.program.clone(),
                model: a.model.clone(),
                last_active: ts_display(a.last_active_ts),
            })
            .collect();
        projects.push(UnifiedProject {
            id: pid,
            slug: p.slug.clone(),
            human_key: p.human_key.clone(),
            created_at: ts_display(p.created_at),
            agent_count: agents.len(),
            agents,
        });
    }

    let messages = collect_unified_message_aggregates(
        cx,
        pool,
        &projects_rows,
        limit,
        normalized_filter.as_deref(),
    )?
    .into_iter()
    .map(UnifiedMessageAggregate::into_view)
    .collect::<Vec<_>>();

    let total_messages = messages.len();
    render(
        "mail_unified_inbox.html",
        UnifiedInboxCtx {
            projects,
            messages,
            total_agents,
            total_messages,
            filter_importance: normalized_filter.unwrap_or_default(),
            static_export,
        },
    )
}

fn matches_importance_filter(message_importance: &str, filter_importance: Option<&str>) -> bool {
    filter_importance.is_none_or(|filter| message_importance.eq_ignore_ascii_case(filter))
}

fn unified_api_message_value(message: &UnifiedMessage) -> serde_json::Value {
    let body_length = message.body_md.len();
    serde_json::json!({
        "id": message.id,
        "subject": message.subject,
        "body_md": message.body_md,
        "body_html": message.body_html,
        "body_length": body_length,
        "excerpt": message.excerpt,
        "created": message.created,
        "created_ts": message.created,
        "created_full": message.created_full,
        "created_relative": message.created_relative,
        "importance": message.importance,
        "thread_id": message.thread_id,
        "sender": message.sender,
        "recipients": message.recipients,
        "recipient_names": message.recipient_names,
        "recipient_read": message.recipient_read,
        "project_slug": message.project_slug,
        "project_name": message.project_name,
        "read": message.read,
    })
}

// ---------------------------------------------------------------------------
// Route: GET /mail/{project} — project detail
// ---------------------------------------------------------------------------

#[derive(Serialize)]
struct ProjectCtx {
    project: ProjectView,
    agents: Vec<AgentView>,
    static_export: bool,
}

#[derive(Serialize)]
struct ProjectView {
    id: i64,
    slug: String,
    human_key: String,
    created_at: String,
}

#[derive(Serialize)]
struct AgentView {
    id: i64,
    name: String,
    program: String,
    model: String,
    task_description: String,
    last_active: String,
}

fn project_view(p: &ProjectRow) -> ProjectView {
    ProjectView {
        id: p.id.unwrap_or(0),
        slug: p.slug.clone(),
        human_key: p.human_key.clone(),
        created_at: ts_display(p.created_at),
    }
}

fn agent_view(a: &AgentRow) -> AgentView {
    AgentView {
        id: a.id.unwrap_or(0),
        name: a.name.clone(),
        program: a.program.clone(),
        model: a.model.clone(),
        task_description: a.task_description.clone(),
        last_active: ts_display(a.last_active_ts),
    }
}

fn render_project(
    cx: &Cx,
    pool: &DbPool,
    slug: &str,
    static_export: bool,
) -> Result<Option<String>, (u16, String)> {
    let p = block_on_outcome(cx, queries::get_project_by_slug(cx, pool, slug))?;
    let agents = block_on_outcome(cx, queries::list_agents(cx, pool, p.id.unwrap_or(0)))?;
    render(
        "mail_project.html",
        ProjectCtx {
            project: project_view(&p),
            agents: agents.iter().map(agent_view).collect(),
            static_export,
        },
    )
}

// ---------------------------------------------------------------------------
// Route: GET /mail/{project}/inbox/{agent}
// ---------------------------------------------------------------------------

#[derive(Serialize)]
struct InboxCtx {
    project: ProjectView,
    agent: String,
    items: Vec<InboxMessage>,
    page: usize,
    limit: usize,
    total: usize,
    prev_page: Option<usize>,
    next_page: Option<usize>,
    static_export: bool,
}

#[derive(Serialize)]
struct InboxMessage {
    id: i64,
    subject: String,
    body_html: String,
    sender: String,
    importance: String,
    thread_id: String,
    created: String,
    ack_required: bool,
    acked: bool,
    read: bool,
}

fn render_inbox(
    cx: &Cx,
    pool: &DbPool,
    project_slug: &str,
    agent_name: &str,
    limit: usize,
    page: usize,
    static_export: bool,
) -> Result<Option<String>, (u16, String)> {
    let p = block_on_outcome(cx, queries::get_project_by_slug(cx, pool, project_slug))?;
    let pid = p.id.unwrap_or(0);
    let a = block_on_outcome(cx, queries::get_agent(cx, pool, pid, agent_name))?;
    let aid = a.id.unwrap_or(0);

    // Fetch a generous amount, then paginate client-side (Python parity).
    let fetch_limit = 10_000;
    let inbox = block_on_outcome(
        cx,
        queries::fetch_inbox(cx, pool, pid, aid, false, None, fetch_limit),
    )?;
    let total = inbox.len();
    let candidate_root_ids: Vec<i64> = inbox
        .iter()
        .filter_map(|row| {
            if explicit_thread_ref(row.message.thread_id.as_deref()).is_none() {
                row.message.id.filter(|message_id| *message_id > 0)
            } else {
                None
            }
        })
        .collect();
    let reply_root_ids = root_ids_with_replies(cx, pool, pid, &candidate_root_ids)?;

    // Offset-based pagination (Python: offset = (page - 1) * limit).
    let page = page.max(1);
    let offset = (page - 1).saturating_mul(limit.max(1));
    let mut items = Vec::new();
    for row in inbox.iter().skip(offset).take(limit) {
        let m = &row.message;
        items.push(InboxMessage {
            id: m.id.unwrap_or(0),
            subject: m.subject.clone(),
            body_html: markdown::render_markdown_to_safe_html(&m.body_md),
            sender: row.sender_name.clone(),
            importance: m.importance.clone(),
            thread_id: display_thread_ref_for_message(
                m.id.unwrap_or(0),
                m.thread_id.as_deref(),
                &reply_root_ids,
            ),
            created: ts_display(m.created_ts),
            ack_required: m.ack_required_bool(),
            acked: row.ack_ts.is_some(),
            read: row.read_ts.is_some(),
        });
    }

    let prev_page = if page > 1 { Some(page - 1) } else { None };
    let next_page = if offset.saturating_add(limit) < total {
        Some(page + 1)
    } else {
        None
    };

    render(
        "mail_inbox.html",
        InboxCtx {
            project: project_view(&p),
            agent: a.name,
            items,
            page,
            limit,
            total,
            prev_page,
            next_page,
            static_export,
        },
    )
}

// ---------------------------------------------------------------------------
// Route: GET /mail/{project}/message/{mid}
// ---------------------------------------------------------------------------

#[derive(Serialize)]
struct MessageCtx {
    project: ProjectView,
    message: MessageView,
    recipients: Vec<MessageRecipientView>,
    thread_items: Vec<MessageThreadPreview>,
    other_thread_items: Vec<MessageThreadPreview>,
    extra_thread_count: usize,
    commit_sha: Option<String>,
}

#[derive(Serialize)]
struct MessageView {
    id: i64,
    subject: String,
    body_md: String,
    body_html: String,
    importance: String,
    thread_id: String,
    thread_url: String,
    created: String,
    ack_required: bool,
    sender: String,
}

#[derive(Serialize)]
struct MessageRecipientView {
    kind: String,
    name: String,
}

#[derive(Clone, Serialize)]
struct MessageThreadPreview {
    id: i64,
    from: String,
    subject: String,
}

fn render_message(
    cx: &Cx,
    pool: &DbPool,
    project_slug: &str,
    message_id: i64,
) -> Result<Option<String>, (u16, String)> {
    let p = block_on_outcome(cx, queries::get_project_by_slug(cx, pool, project_slug))?;
    let pid = p.id.unwrap_or(0);
    let m = block_on_outcome(cx, queries::get_message(cx, pool, message_id))?;
    if m.project_id != pid {
        return Err((404, "Message not found".to_string()));
    }
    let current_message_id = m.id.unwrap_or(0);
    let sender = block_on_outcome(cx, queries::get_agent_by_id(cx, pool, m.sender_id))?;
    let recipients = block_on_outcome(
        cx,
        queries::list_message_recipients_by_message(cx, pool, pid, message_id),
    )?;
    let stored_thread_ref = explicit_thread_ref(m.thread_id.as_deref());
    let candidate_thread_ref = stored_thread_ref
        .clone()
        .or_else(|| (current_message_id > 0).then(|| current_message_id.to_string()));
    let thread_items = if let Some(thread_id) = candidate_thread_ref.as_deref() {
        block_on_outcome(
            cx,
            queries::list_thread_messages(cx, pool, pid, thread_id, None),
        )?
        .into_iter()
        .map(|item| MessageThreadPreview {
            id: item.id,
            from: item.from,
            subject: item.subject,
        })
        .collect()
    } else {
        Vec::new()
    };
    let other_thread_items = thread_items
        .iter()
        .filter(|item| item.id != current_message_id)
        .cloned()
        .collect::<Vec<_>>();
    let extra_thread_count = other_thread_items.len().saturating_sub(5);
    let thread_ref = if stored_thread_ref.is_some() || thread_items.len() > 1 {
        candidate_thread_ref.unwrap_or_default()
    } else {
        String::new()
    };

    render(
        "mail_message.html",
        MessageCtx {
            project: project_view(&p),
            message: MessageView {
                id: current_message_id,
                subject: m.subject.clone(),
                body_md: m.body_md.clone(),
                body_html: markdown::render_markdown_to_safe_html(&m.body_md),
                importance: m.importance.clone(),
                thread_url: mail_thread_href(&p.slug, &thread_ref),
                thread_id: thread_ref,
                created: ts_display(m.created_ts),
                ack_required: m.ack_required_bool(),
                sender: sender.name,
            },
            recipients: recipients
                .into_iter()
                .map(|recipient| MessageRecipientView {
                    kind: recipient.kind,
                    name: recipient.name,
                })
                .collect(),
            thread_items,
            other_thread_items,
            extra_thread_count,
            commit_sha: None,
        },
    )
}

#[cfg(test)]
mod message_route_authorization_tests {
    use super::*;
    use asupersync::Outcome;
    use std::time::{SystemTime, UNIX_EPOCH};

    fn outcome_ok<T>(outcome: Outcome<T, mcp_agent_mail_db::DbError>) -> T {
        match outcome {
            Outcome::Ok(v) => v,
            Outcome::Err(e) => panic!("db error: {e}"),
            Outcome::Cancelled(_) => panic!("db operation cancelled"),
            Outcome::Panicked(p) => panic!("db operation panicked: {}", p.message()),
        }
    }

    fn make_test_pool() -> DbPool {
        initialized_test_pool("mail-ui-idor")
    }

    #[test]
    fn project_message_route_blocks_cross_project_idor_access() {
        let cx = Cx::for_testing();
        let pool = make_test_pool();
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("clock should be after unix epoch")
            .as_nanos();

        let project_a = outcome_ok(block_on(queries::ensure_project(
            &cx,
            &pool,
            &format!("/tmp/mail-ui-idor-a-{nonce}"),
        )));
        let project_b = outcome_ok(block_on(queries::ensure_project(
            &cx,
            &pool,
            &format!("/tmp/mail-ui-idor-b-{nonce}"),
        )));
        let project_a_id = project_a.id.unwrap_or(0);

        let sender = outcome_ok(block_on(queries::register_agent(
            &cx,
            &pool,
            project_a_id,
            "BlueLake",
            "test",
            "test",
            None,
            None,
        )));
        let sender_id = sender.id.unwrap_or(0);

        let message = outcome_ok(block_on(queries::create_message(
            &cx,
            &pool,
            project_a_id,
            sender_id,
            "Scoped subject",
            "Scoped body",
            Some("idor-thread"),
            "normal",
            false,
            "[]",
        )));
        let message_id = message.id.expect("message id should be present");

        let in_scope_route = format!("/{}/message/{message_id}", project_a.slug);
        let in_scope_result = dispatch_project_route(&in_scope_route, "GET", "", &cx, &pool, "");
        assert!(
            matches!(in_scope_result, Ok(Some(_))),
            "same-project route should render message: {in_scope_result:?}"
        );

        let cross_scope_route = format!("/{}/message/{message_id}", project_b.slug);
        let cross_scope_result =
            dispatch_project_route(&cross_scope_route, "GET", "", &cx, &pool, "");
        let (status, detail) =
            cross_scope_result.expect_err("cross-project tampering must be rejected");
        assert_eq!(status, 404);
        assert_eq!(detail, "Message not found");
    }
}

// ---------------------------------------------------------------------------
// Route: GET /mail/{project}/thread/{thread_id}
// ---------------------------------------------------------------------------

#[derive(Serialize)]
struct ThreadCtx {
    project: ProjectView,
    thread_id: String,
    thread_subject: String,
    message_count: usize,
    messages: Vec<ThreadMessage>,
}

#[derive(Serialize)]
struct ThreadMessage {
    id: i64,
    subject: String,
    body_md: String,
    body_html: String,
    sender: String,
    created: String,
    importance: String,
}

fn render_thread(
    cx: &Cx,
    pool: &DbPool,
    project_slug: &str,
    thread_id: &str,
) -> Result<Option<String>, (u16, String)> {
    let p = block_on_outcome(cx, queries::get_project_by_slug(cx, pool, project_slug))?;
    let pid = p.id.unwrap_or(0);
    let thread_msgs = block_on_outcome(
        cx,
        queries::list_thread_messages(cx, pool, pid, thread_id, None),
    )?;

    let messages: Vec<ThreadMessage> = thread_msgs
        .iter()
        .map(|tm| ThreadMessage {
            id: tm.id,
            subject: tm.subject.clone(),
            body_md: tm.body_md.clone(),
            body_html: markdown::render_markdown_to_safe_html(&tm.body_md),
            sender: tm.from.clone(),
            created: ts_display(tm.created_ts),
            importance: tm.importance.clone(),
        })
        .collect();

    let thread_subject = messages
        .first()
        .map_or_else(|| format!("Thread {thread_id}"), |m| m.subject.clone());
    let message_count = messages.len();

    render(
        "mail_thread.html",
        ThreadCtx {
            project: project_view(&p),
            thread_id: thread_id.to_string(),
            thread_subject,
            message_count,
            messages,
        },
    )
}

// ---------------------------------------------------------------------------
// Route: GET /mail/{project}/search
// ---------------------------------------------------------------------------

#[derive(Serialize)]
struct SearchCtx {
    project: ProjectView,
    q: String,
    results: Vec<WebSearchResult>,
    static_export: bool,
    static_search_index_path: String,
    // Facet state (round-trips through URL params)
    order: String,
    scope: String,
    boost: bool,
    importance: Vec<String>,
    agent: String,
    thread: String,
    ack: String,
    direction: String,
    from_date: String,
    to_date: String,
    // Pagination
    next_cursor: String,
    cursor: String,
    result_count: usize,
    // Agent list for facet dropdown
    agents: Vec<AgentView>,
    // Saved recipes
    recipes: Vec<RecipeView>,
    // Deep link for current search state
    deep_link: String,
}

#[derive(Serialize)]
struct WebSearchResult {
    id: i64,
    subject: String,
    snippet: String,
    #[serde(rename = "from")]
    from_name: String,
    created: String,
    created_relative: String,
    importance: String,
    thread_id: String,
    thread_url: String,
    ack_required: bool,
    score: String,
}

#[derive(Serialize)]
struct RecipeView {
    id: i64,
    name: String,
    description: String,
    route: String,
    pinned: bool,
    use_count: i64,
}

/// Extract all values for a repeated query param (e.g. `imp=high&imp=urgent`).
fn extract_query_str_all(query: &str, key: &str) -> Vec<String> {
    let mut out = Vec::new();
    for pair in query.split('&') {
        if let Some((k, v)) = pair.split_once('=')
            && k == key
            && !v.is_empty()
        {
            out.push(percent_decode_component(v));
        }
    }
    out
}

/// Build the deep-link URL for the current search state.
fn build_search_deep_link(project_slug: &str, query_str: &str) -> String {
    if query_str.is_empty() {
        return format!("/mail/{project_slug}/search");
    }
    format!("/mail/{project_slug}/search?{query_str}")
}

/// Highlight matched terms in a snippet by wrapping them in `<mark>` tags.
///
/// Uses a simple case-insensitive substring approach. The input body is
/// first HTML-escaped, then highlight `<mark>` tags are inserted.
fn highlight_snippet(body: &str, query: &str, max_len: usize) -> String {
    // Extract search terms from the query (strip field prefixes, quotes, operators).
    let terms: Vec<String> = query
        .split_whitespace()
        .filter(|t| !matches!(t.to_ascii_uppercase().as_str(), "AND" | "OR" | "NOT"))
        .map(|t| {
            // Strip field prefix like "subject:" or "body:"
            let t = t.split_once(':').map_or(t, |(_, v)| v);
            // Strip quotes
            t.trim_matches('"').to_string()
        })
        .filter(|t| t.len() >= 2)
        .collect();

    if terms.is_empty() {
        return html_escape(&truncate_body(body, max_len));
    }

    // Find the best window: center on the first matching term.
    let body_lower = body.to_ascii_lowercase();
    let mut best_pos = 0usize;
    for term in &terms {
        if let Some(pos) = body_lower.find(&term.to_ascii_lowercase()) {
            best_pos = pos;
            break;
        }
    }

    // Extract window around best_pos.
    let half = max_len / 2;
    let start = best_pos.saturating_sub(half);
    let mut end = (start + max_len).min(body.len());
    // Ensure char boundary.
    while end > start && !body.is_char_boundary(end) {
        end -= 1;
    }
    let mut s_start = start;
    while s_start < end && !body.is_char_boundary(s_start) {
        s_start += 1;
    }

    let window = &body[s_start..end];
    let prefix = if s_start > 0 { "…" } else { "" };
    let suffix = if end < body.len() { "…" } else { "" };

    // Find intervals in UNESCAPED window to avoid corrupting HTML entities or <mark> tags.
    let mut intervals = Vec::new();
    let lower_window = window.to_ascii_lowercase();
    for term in &terms {
        let pattern = term.to_ascii_lowercase();
        for (idx, _) in lower_window.match_indices(&pattern) {
            intervals.push((idx, idx + pattern.len()));
        }
    }

    // Merge overlapping intervals
    intervals.sort_unstable_by_key(|&(start, _)| start);
    let mut merged: Vec<(usize, usize)> = Vec::new();
    for interval in intervals {
        if let Some(last) = merged.last_mut()
            && interval.0 <= last.1
        {
            last.1 = last.1.max(interval.1);
            continue;
        }
        merged.push(interval);
    }

    // Build the final escaped HTML string
    let mut out = String::with_capacity(window.len() + merged.len() * 13 + 6);
    if !prefix.is_empty() {
        out.push_str(prefix); // "…" is safe to not escape
    }

    let mut last = 0;
    for (start, end) in merged {
        out.push_str(&html_escape(&window[last..start]));
        out.push_str("<mark>");
        out.push_str(&html_escape(&window[start..end]));
        out.push_str("</mark>");
        last = end;
    }
    out.push_str(&html_escape(&window[last..]));

    if !suffix.is_empty() {
        out.push_str(suffix);
    }

    out
}

/// Minimal HTML escaping for untrusted text (before inserting <mark> tags).
fn html_escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for ch in s.chars() {
        match ch {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&#x27;"),
            _ => out.push(ch),
        }
    }
    out
}

#[allow(clippy::too_many_lines)]
fn render_search(
    cx: &Cx,
    pool: &DbPool,
    project_slug: &str,
    query_str: &str,
) -> Result<Option<String>, (u16, String)> {
    use mcp_agent_mail_db::search_planner::{
        Direction, Importance, RankingMode, SearchQuery, TimeRange,
    };

    let p = block_on_outcome(cx, queries::get_project_by_slug(cx, pool, project_slug))?;
    let pid = p.id.unwrap_or(0);

    // ── Parse all query parameters ──────────────────────────────────
    let q = extract_query_str(query_str, "q").unwrap_or_default();
    let static_export = is_static_export_request(query_str);
    let limit = extract_query_int(query_str, "limit", 50);
    let order = extract_query_str(query_str, "order").unwrap_or_else(|| "relevance".to_string());
    let scope = extract_query_str(query_str, "scope").unwrap_or_default();
    let boost = extract_query_str(query_str, "boost").is_some();
    let cursor = extract_query_str(query_str, "cursor").unwrap_or_default();

    // Facets
    let imp_strs = extract_query_str_all(query_str, "imp");
    let importance_filter: Vec<Importance> = imp_strs
        .iter()
        .filter_map(|s| Importance::parse(s))
        .collect();

    let agent_filter = extract_query_str(query_str, "agent").unwrap_or_default();
    let thread_filter = extract_query_str(query_str, "thread").unwrap_or_default();
    let ack_filter = extract_query_str(query_str, "ack").unwrap_or_else(|| "any".to_string());
    let direction_filter = extract_query_str(query_str, "direction").unwrap_or_default();
    let from_date = extract_query_str(query_str, "from_date").unwrap_or_default();
    let to_date = extract_query_str(query_str, "to_date").unwrap_or_default();

    // ── Build search query ──────────────────────────────────────────
    let has_any_filter = !q.is_empty()
        || !importance_filter.is_empty()
        || !agent_filter.is_empty()
        || !thread_filter.is_empty()
        || ack_filter != "any"
        || !direction_filter.is_empty()
        || !from_date.is_empty()
        || !to_date.is_empty();

    let (results, next_cursor_val) = if has_any_filter {
        let time_range = TimeRange {
            min_ts: parse_date_to_micros(&from_date),
            max_ts: parse_date_to_micros_end(&to_date),
        };

        let ranking = if order == "time" {
            RankingMode::Recency
        } else {
            RankingMode::Relevance
        };

        let direction = match direction_filter.as_str() {
            "inbox" => Some(Direction::Inbox),
            "outbox" => Some(Direction::Outbox),
            _ => None,
        };

        let ack_required = match ack_filter.as_str() {
            "required" => Some(true),
            "not_required" => Some(false),
            _ => None,
        };

        let search_query = SearchQuery {
            text: q.clone(),
            doc_kind: mcp_agent_mail_db::search_planner::DocKind::Message,
            project_id: Some(pid),
            product_id: None,
            importance: importance_filter,
            direction,
            agent_name: if agent_filter.is_empty() {
                None
            } else {
                Some(agent_filter.clone())
            },
            thread_id: if thread_filter.is_empty() {
                None
            } else {
                Some(thread_filter.clone())
            },
            ack_required,
            time_range,
            ranking,
            limit: Some(limit),
            cursor: if cursor.is_empty() {
                None
            } else {
                Some(cursor.clone())
            },
            explain: false,
            ..Default::default()
        };

        let resp = block_on_outcome(
            cx,
            mcp_agent_mail_db::search_service::execute_search_simple(cx, pool, &search_query),
        )?;

        let web_results: Vec<WebSearchResult> = resp
            .results
            .iter()
            .map(|r| WebSearchResult {
                id: r.id,
                subject: r.title.clone(),
                snippet: highlight_snippet(&r.body, &q, 250),
                from_name: r.from_agent.clone().unwrap_or_default(),
                created: r.created_ts.map_or_else(String::new, ts_display),
                created_relative: r.created_ts.map_or_else(String::new, ts_display_relative),
                importance: r.importance.clone().unwrap_or_default(),
                thread_url: r
                    .thread_id
                    .as_deref()
                    .map_or_else(String::new, |thread_id| {
                        mail_thread_href(project_slug, thread_id)
                    }),
                thread_id: r.thread_id.clone().unwrap_or_default(),
                ack_required: r.ack_required.unwrap_or(false),
                score: r.score.map_or_else(String::new, |s| format!("{s:.2}")),
            })
            .collect();

        let nc = resp.next_cursor.unwrap_or_default();
        (web_results, nc)
    } else {
        (Vec::new(), String::new())
    };

    // ── Load agents for facet dropdown ──────────────────────────────
    let agents_rows = block_on_outcome(cx, queries::list_agents(cx, pool, pid))?;
    let agents: Vec<AgentView> = agents_rows.iter().map(agent_view).collect();

    // ── Load saved recipes ──────────────────────────────────────────
    let recipes = load_recipes(pool);

    let result_count = results.len();
    let deep_link = build_search_deep_link(project_slug, query_str);

    render(
        "mail_search.html",
        SearchCtx {
            project: project_view(&p),
            q,
            results,
            static_export,
            static_search_index_path: "../../../search-index.json".to_string(),
            order,
            scope,
            boost,
            importance: imp_strs,
            agent: agent_filter,
            thread: thread_filter,
            ack: ack_filter,
            direction: direction_filter,
            from_date,
            to_date,
            next_cursor: next_cursor_val,
            cursor,
            result_count,
            agents,
            recipes,
            deep_link,
        },
    )
}

/// Load saved search recipes from the DB (best-effort, returns empty on error).
fn load_recipes(pool: &DbPool) -> Vec<RecipeView> {
    let path = pool.sqlite_path();
    if path == ":memory:" {
        return Vec::new();
    }
    let Ok(conn) = mcp_agent_mail_db::open_sqlite_file_with_recovery(path) else {
        return Vec::new();
    };
    let recipes = mcp_agent_mail_db::search_recipes::list_recipes(&conn).unwrap_or_default();
    recipes
        .iter()
        .map(|r| RecipeView {
            id: r.id.unwrap_or(0),
            name: r.name.clone(),
            description: r.description.clone(),
            route: r.route_string(),
            pinned: r.pinned,
            use_count: r.use_count,
        })
        .collect()
}

/// Parse "YYYY-MM-DD" to start-of-day microseconds.
fn parse_date_to_micros(s: &str) -> Option<i64> {
    if s.is_empty() {
        return None;
    }
    // Parse YYYY-MM-DD
    let parts: Vec<&str> = s.splitn(3, '-').collect();
    if parts.len() != 3 {
        return None;
    }
    let y: i32 = parts[0].parse().ok()?;
    let m: u32 = parts[1].parse().ok()?;
    let d: u32 = parts[2].parse().ok()?;
    let dt = chrono::NaiveDate::from_ymd_opt(y, m, d)?;
    let ts = dt.and_hms_opt(0, 0, 0)?.and_utc().timestamp_micros();
    Some(ts)
}

/// Parse "YYYY-MM-DD" to end-of-day microseconds (23:59:59.999999).
fn parse_date_to_micros_end(s: &str) -> Option<i64> {
    parse_date_to_micros(s).map(|ts| ts + 86_400_000_000 - 1)
}

fn truncate_body(body: &str, max: usize) -> String {
    if body.len() <= max {
        return body.to_string();
    }
    let mut end = max;
    while end > 0 && !body.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}…", &body[..end])
}

// ---------------------------------------------------------------------------
// Route: GET /mail/{project}/file_reservations
// ---------------------------------------------------------------------------

#[derive(Serialize)]
struct FileReservationsCtx {
    project: ProjectView,
    reservations: Vec<ReservationView>,
}

#[derive(Serialize)]
struct ReservationView {
    id: i64,
    agent_name: String,
    path_pattern: String,
    exclusive: bool,
    reason: String,
    created: String,
    expires: String,
    released: String,
}

fn render_file_reservations(
    cx: &Cx,
    pool: &DbPool,
    project_slug: &str,
) -> Result<Option<String>, (u16, String)> {
    let p = block_on_outcome(cx, queries::get_project_by_slug(cx, pool, project_slug))?;
    let pid = p.id.unwrap_or(0);
    let rows = block_on_outcome(cx, queries::list_file_reservations(cx, pool, pid, false))?;

    let mut reservations = Vec::with_capacity(rows.len());
    for r in &rows {
        let agent = block_on_outcome(cx, queries::get_agent_by_id(cx, pool, r.agent_id))
            .map_or_else(|_| format!("agent#{}", r.agent_id), |a| a.name);
        reservations.push(ReservationView {
            id: r.id.unwrap_or(0),
            agent_name: agent,
            path_pattern: r.path_pattern.clone(),
            exclusive: r.exclusive != 0,
            reason: r.reason.clone(),
            created: ts_display(r.created_ts),
            expires: ts_display(r.expires_ts),
            released: ts_display_opt(r.released_ts),
        });
    }

    render(
        "mail_file_reservations.html",
        FileReservationsCtx {
            project: project_view(&p),
            reservations,
        },
    )
}

// ---------------------------------------------------------------------------
// Route: GET /mail/{project}/attachments
// ---------------------------------------------------------------------------

#[derive(Serialize)]
struct AttachmentsCtx {
    project: ProjectView,
    items: Vec<AttachmentMessageView>,
}

#[derive(Serialize)]
struct AttachmentMessageView {
    id: i64,
    subject: String,
    created: String,
    attachments: Vec<AttachmentView>,
}

#[derive(Serialize)]
struct AttachmentView {
    name: Option<String>,
    media_type: Option<String>,
    path: Option<String>,
    bytes: Option<u64>,
}

fn parse_attachment_views(attachments_json: &str) -> Vec<AttachmentView> {
    serde_json::from_str::<Vec<serde_json::Value>>(attachments_json)
        .map(|attachments| {
            attachments
                .into_iter()
                .filter_map(|attachment| {
                    let media_type = attachment
                        .get("media_type")
                        .or_else(|| attachment.get("content_type"))
                        .and_then(serde_json::Value::as_str)
                        .or_else(|| {
                            attachment
                                .get("type")
                                .and_then(serde_json::Value::as_str)
                                .filter(|kind| !matches!(*kind, "file" | "inline" | "auto"))
                        })
                        .map(str::to_string);
                    let path = attachment
                        .get("path")
                        .and_then(serde_json::Value::as_str)
                        .map(str::to_string);
                    let name = attachment
                        .get("name")
                        .and_then(serde_json::Value::as_str)
                        .map(str::to_string)
                        .or_else(|| {
                            path.as_deref().and_then(|attachment_path| {
                                Path::new(attachment_path)
                                    .file_name()
                                    .and_then(std::ffi::OsStr::to_str)
                                    .map(str::to_string)
                            })
                        });
                    let bytes = attachment
                        .get("bytes")
                        .and_then(serde_json::Value::as_u64)
                        .or_else(|| attachment.get("size").and_then(serde_json::Value::as_u64))
                        .or_else(|| {
                            attachment
                                .get("size")
                                .and_then(serde_json::Value::as_str)
                                .and_then(|raw| raw.parse::<u64>().ok())
                        });

                    if media_type.is_none() && path.is_none() && name.is_none() && bytes.is_none() {
                        return None;
                    }

                    Some(AttachmentView {
                        name,
                        media_type,
                        path,
                        bytes,
                    })
                })
                .collect()
        })
        .unwrap_or_default()
}

fn render_attachments(
    cx: &Cx,
    pool: &DbPool,
    project_slug: &str,
) -> Result<Option<String>, (u16, String)> {
    let p = block_on_outcome(cx, queries::get_project_by_slug(cx, pool, project_slug))?;
    let pid = p.id.unwrap_or(0);
    let agents = block_on_outcome(cx, queries::list_agents(cx, pool, pid))?;
    let mut items_by_id = BTreeMap::new();

    for agent in &agents {
        let aid = agent.id.unwrap_or(0);
        let inbox = block_on_outcome(
            cx,
            queries::fetch_inbox(cx, pool, pid, aid, false, None, 10_000),
        )?;
        for row in inbox {
            let message = row.message;
            let Some(message_id) = message.id else {
                continue;
            };
            items_by_id.entry(message_id).or_insert_with(|| {
                let attachments = parse_attachment_views(&message.attachments);
                (
                    message.created_ts,
                    AttachmentMessageView {
                        id: message_id,
                        subject: message.subject.clone(),
                        created: ts_display(message.created_ts),
                        attachments,
                    },
                )
            });
        }
    }

    let mut items = items_by_id
        .into_iter()
        .filter_map(|(_, (created_ts, item))| {
            if item.attachments.is_empty() {
                None
            } else {
                Some((created_ts, item))
            }
        })
        .collect::<Vec<_>>();
    items.sort_by(|(left_ts, left_item), (right_ts, right_item)| {
        right_ts.cmp(left_ts).then(right_item.id.cmp(&left_item.id))
    });
    let items = items.into_iter().map(|(_, item)| item).collect();

    render(
        "mail_attachments.html",
        AttachmentsCtx {
            project: project_view(&p),
            items,
        },
    )
}

// ---------------------------------------------------------------------------
// Route: GET /mail/{project}/overseer/compose
// ---------------------------------------------------------------------------

#[derive(Serialize)]
struct OverseerComposeCtx {
    project: ProjectView,
    agents: Vec<AgentView>,
}

fn render_overseer_compose(
    cx: &Cx,
    pool: &DbPool,
    project_slug: &str,
) -> Result<Option<String>, (u16, String)> {
    let p = block_on_outcome(cx, queries::get_project_by_slug(cx, pool, project_slug))?;
    let pid = p.id.unwrap_or(0);
    let agents = block_on_outcome(cx, queries::list_agents(cx, pool, pid))?;
    render(
        "overseer_compose.html",
        OverseerComposeCtx {
            project: project_view(&p),
            agents: agents.iter().map(agent_view).collect(),
        },
    )
}

// ---------------------------------------------------------------------------
// Project sub-route dispatch
// ---------------------------------------------------------------------------

fn dispatch_project_route(
    sub: &str,
    method: &str,
    body: &str,
    cx: &Cx,
    pool: &DbPool,
    query: &str,
) -> Result<Option<String>, (u16, String)> {
    // sub starts with "/" and has at least the project slug.
    let sub = sub.strip_prefix('/').unwrap_or(sub);
    let (project_slug, rest) = sub.split_once('/').unwrap_or((sub, ""));

    if project_slug.is_empty() {
        return Ok(None);
    }

    // F2: Reject malformed project slugs early (before any DB/archive access).
    if !is_valid_project_slug(project_slug) {
        return Err((400, "Invalid project identifier".to_string()));
    }

    match rest {
        "" => render_project(cx, pool, project_slug, is_static_export_request(query)),
        "search" => render_search(cx, pool, project_slug, query),
        "file_reservations" => render_file_reservations(cx, pool, project_slug),
        "attachments" => render_attachments(cx, pool, project_slug),
        "overseer/compose" => render_overseer_compose(cx, pool, project_slug),
        "overseer/send" if method == "POST" => handle_overseer_send(cx, pool, project_slug, body),
        _ if rest.starts_with("inbox/") => {
            let agent_rest = rest.strip_prefix("inbox/").unwrap_or("");
            if agent_rest.is_empty() {
                return Err((400, "Missing agent name".to_string()));
            }
            // Parse agent name and optional sub-action.
            let (agent_name, action) = agent_rest.split_once('/').unwrap_or((agent_rest, ""));

            if agent_name.is_empty() {
                return Err((400, "Missing agent name".to_string()));
            }

            match (method, action) {
                ("POST", "mark-read") => handle_mark_read(cx, pool, project_slug, agent_name, body),
                ("POST", "mark-all-read") => {
                    handle_mark_all_read(cx, pool, project_slug, agent_name)
                }
                // F3: Only accept GET on the inbox root, not unknown sub-paths.
                ("GET", "") => {
                    let limit = extract_query_int(query, "limit", 10000);
                    let page = extract_query_int(query, "page", 1);
                    render_inbox(
                        cx,
                        pool,
                        project_slug,
                        agent_name,
                        limit,
                        page,
                        is_static_export_request(query),
                    )
                }
                ("GET", _) => Ok(None), // Unknown inbox sub-action → 404
                _ => Err((405, "Method Not Allowed".to_string())),
            }
        }
        _ if rest.starts_with("message/") => {
            let mid_str = rest.strip_prefix("message/").unwrap_or("");
            let mid: i64 = mid_str
                .parse()
                .map_err(|_| (400, format!("Invalid message ID: {mid_str}")))?;
            render_message(cx, pool, project_slug, mid)
        }
        _ if rest.starts_with("thread/") => {
            let encoded_thread_id = rest.strip_prefix("thread/").unwrap_or("");
            let thread_id = percent_decode_path_segment(encoded_thread_id);
            if thread_id.is_empty() {
                return Err((400, "Missing thread ID".to_string()));
            }
            render_thread(cx, pool, project_slug, &thread_id)
        }
        _ => Ok(None),
    }
}

// ---------------------------------------------------------------------------
// API sub-routes under /mail/api/*
// ---------------------------------------------------------------------------

fn handle_api_route(
    sub: &str,
    query: &str,
    method: &str,
    body: &str,
    cx: &Cx,
    pool: &DbPool,
) -> Result<Option<String>, (u16, String)> {
    // /api/unified-inbox → JSON
    if sub == "/api/unified-inbox" {
        return render_api_unified_inbox(cx, pool, query);
    }
    // /api/projects/{project_id}/siblings/{other_id} → POST (sibling suggestion)
    if let Some(rest) = sub.strip_prefix("/api/projects/") {
        // Check for siblings route: {project_id}/siblings/{other_id}
        if let Some((project_id_str, siblings_rest)) = rest.split_once("/siblings/") {
            if method == "POST" {
                let project_id: i64 = project_id_str
                    .parse()
                    .map_err(|_| (400, "Invalid project ID".to_string()))?;
                let other_id: i64 = siblings_rest
                    .parse()
                    .map_err(|_| (400, "Invalid sibling project ID".to_string()))?;
                return handle_sibling_update(cx, pool, project_id, other_id, body);
            }
            return Err((405, "Method Not Allowed".to_string()));
        }
        // /api/projects/{project}/agents → JSON
        if let Some(project_slug) = rest.strip_suffix("/agents") {
            return render_api_project_agents(cx, pool, project_slug);
        }
    }
    // Other API routes handled elsewhere (e.g., /mail/api/locks is in handle_special_routes).
    Ok(None)
}

fn render_api_unified_inbox(
    cx: &Cx,
    pool: &DbPool,
    query: &str,
) -> Result<Option<String>, (u16, String)> {
    // Keep API and HTML route limits aligned so live refreshes never shrink
    // the already-rendered unified inbox dataset.
    let limit = extract_query_int(query, "limit", 1000).clamp(1, 1000);
    let include_projects =
        extract_query_str(query, "include_projects").is_some_and(|v| v == "true" || v == "1");
    let filter_importance = extract_query_str(query, "filter_importance");
    let normalized_filter = filter_importance
        .map(|value| value.trim().to_ascii_lowercase())
        .filter(|value| !value.is_empty());

    let projects = block_on_outcome(cx, queries::list_projects(cx, pool))?;
    let messages = collect_unified_message_aggregates(
        cx,
        pool,
        &projects,
        limit,
        normalized_filter.as_deref(),
    )?
    .into_iter()
    .map(|message| unified_api_message_value(&message.into_view()))
    .collect::<Vec<_>>();

    let mut result = serde_json::json!({ "messages": messages });
    if include_projects {
        let proj_list: Vec<serde_json::Value> = projects
            .iter()
            .map(|p| {
                serde_json::json!({
                    "id": p.id.unwrap_or(0),
                    "slug": p.slug,
                    "human_key": p.human_key,
                    "created_at": ts_display(p.created_at),
                })
            })
            .collect();
        result["projects"] = serde_json::json!(proj_list);
    }

    let json = serde_json::to_string(&result).map_err(|e| (500, format!("JSON error: {e}")))?;
    Ok(Some(json))
}

fn render_api_project_agents(
    cx: &Cx,
    pool: &DbPool,
    project_slug: &str,
) -> Result<Option<String>, (u16, String)> {
    let p = block_on_outcome(cx, queries::get_project_by_slug(cx, pool, project_slug))?;
    let agents = block_on_outcome(cx, queries::list_agents(cx, pool, p.id.unwrap_or(0)))?;
    let mut names: Vec<&str> = agents.iter().map(|a| a.name.as_str()).collect();
    names.sort_unstable(); // Python parity: ORDER BY name
    let json = serde_json::to_string(&serde_json::json!({ "agents": names }))
        .map_err(|e| (500, format!("JSON error: {e}")))?;
    Ok(Some(json))
}

// ---------------------------------------------------------------------------
// Archive routes
// ---------------------------------------------------------------------------

/// Get archive root path from Config (for git operations).
fn get_archive_root() -> Result<std::path::PathBuf, (u16, String)> {
    let config = Config::from_env();
    let (root, _) =
        ensure_archive_root(&config).map_err(|e| (500, format!("Archive error: {e}")))?;
    Ok(root)
}

/// Get a `ProjectArchive` handle for a specific project slug.
fn get_project_archive(slug: &str) -> Result<storage::ProjectArchive, (u16, String)> {
    let config = Config::from_env();
    storage::open_archive(&config, slug)
        .map_err(|e| (500, format!("Archive error: {e}")))?
        .ok_or_else(|| (404, "Archive not found".to_string()))
}

fn render_archive_route(
    sub: &str,
    query: &str,
    method: &str,
    cx: &Cx,
    pool: &DbPool,
) -> Result<Option<String>, (u16, String)> {
    if method != "GET" {
        return Err((405, "Method Not Allowed".to_string()));
    }
    match sub {
        "/archive/guide" => render_archive_guide(cx, pool),
        "/archive/activity" => {
            let limit = extract_query_int(query, "limit", 50).min(500);
            render_archive_activity(limit)
        }
        "/archive/timeline" => {
            let project = extract_query_str(query, "project");
            render_archive_timeline(cx, pool, project.as_deref())
        }
        "/archive/browser" => {
            let project = extract_query_str(query, "project");
            let path = extract_query_str(query, "path").unwrap_or_default();
            render_archive_browser(cx, pool, project.as_deref(), &path)
        }
        "/archive/network" => {
            let project = extract_query_str(query, "project");
            render_archive_network(cx, pool, project.as_deref())
        }
        "/archive/time-travel" => render_archive_time_travel(cx, pool),
        "/archive/time-travel/snapshot" => {
            let project = extract_query_str(query, "project").unwrap_or_default();
            let agent = extract_query_str(query, "agent").unwrap_or_default();
            let timestamp = extract_query_str(query, "timestamp").unwrap_or_default();
            render_archive_time_travel_snapshot(cx, pool, &project, &agent, &timestamp)
        }
        _ if archive_browser_file_project_slug(sub).is_some() => {
            // /archive/browser/{project}/file?path=...
            let project_slug = archive_browser_file_project_slug(sub).unwrap_or_default();
            let path = extract_query_str(query, "path").unwrap_or_default();
            render_archive_browser_file(cx, pool, project_slug, &path)
        }
        _ if sub.starts_with("/archive/commit/") => {
            let sha = sub.strip_prefix("/archive/commit/").unwrap_or("");
            render_archive_commit(sha)
        }
        _ => Ok(None),
    }
}

// -- Guide --

#[derive(Serialize)]
struct ArchiveGuideCtx {
    storage_root: String,
    total_commits: String,
    project_count: usize,
    repo_size: String,
    last_commit_time: String,
    projects: Vec<ArchiveGuideProject>,
}

#[derive(Serialize)]
struct ArchiveGuideProject {
    slug: String,
    human_key: String,
}

fn render_archive_guide(cx: &Cx, pool: &DbPool) -> Result<Option<String>, (u16, String)> {
    let config = Config::from_env();
    let storage_root = config.storage_root.display().to_string();

    let (total_commits, last_commit_time, repo_size) = get_archive_root().map_or_else(
        |_| ("0".to_string(), "Never".to_string(), "N/A".to_string()),
        |root| {
            // Count commits (cap at 10_000)
            let commits = match storage::get_recent_commits_extended(&root, 10_000) {
                Ok(commits) => commits,
                Err(err) => {
                    tracing::warn!(
                        archive_root = %root.display(),
                        error = %err,
                        "failed to read archive commits for archive guide"
                    );
                    Vec::new()
                }
            };
            let total = if commits.len() >= 10_000 {
                "10,000+".to_string()
            } else {
                format!("{}", commits.len())
            };
            let last = commits.first().map_or_else(
                || "Never".to_string(),
                |c| c.date.get(..10).unwrap_or(&c.date).to_string(),
            );

            let size = estimate_repo_size(&root);
            (total, last, size)
        },
    );

    let db_projects = block_on_outcome(cx, queries::list_projects(cx, pool))?;
    let projects: Vec<ArchiveGuideProject> = db_projects
        .iter()
        .map(|p| ArchiveGuideProject {
            slug: p.slug.clone(),
            human_key: p.human_key.clone(),
        })
        .collect();
    let project_count = projects.len();

    render(
        "archive_guide.html",
        ArchiveGuideCtx {
            storage_root,
            total_commits,
            project_count,
            repo_size,
            last_commit_time,
            projects,
        },
    )
}

/// Estimate the size of a directory tree, returned as a human-readable string.
fn estimate_repo_size(path: &std::path::Path) -> String {
    // Try `du -sh` with timeout to prevent server lockup on massive/networked NFS archives.
    const DU_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(1);

    let mut command = std::process::Command::new("du");
    command.args(["-sh", &path.display().to_string()]);
    run_command_stdout_with_timeout(&mut command, DU_TIMEOUT)
        .and_then(|stdout| stdout.split_whitespace().next().map(str::to_string))
        .unwrap_or_else(|| "Unknown".to_string())
}

fn run_command_stdout_with_timeout(
    command: &mut std::process::Command,
    timeout: std::time::Duration,
) -> Option<String> {
    use std::process::Stdio;
    use std::time::Instant;

    command.stdout(Stdio::piped()).stderr(Stdio::piped());
    let mut child = command.spawn().ok()?;
    let deadline = Instant::now() + timeout;

    loop {
        match child.try_wait() {
            Ok(Some(status)) => {
                if !status.success() {
                    return None;
                }
                let output = child.wait_with_output().ok()?;
                return Some(String::from_utf8_lossy(&output.stdout).into_owned());
            }
            Ok(None) if Instant::now() < deadline => {
                std::thread::sleep(std::time::Duration::from_millis(25));
            }
            Ok(None) | Err(_) => {
                let _ = child.kill();
                let _ = child.wait();
                return None;
            }
        }
    }
}

// -- Activity --

#[derive(Serialize)]
struct ArchiveActivityCtx {
    commits: Vec<storage::ExtendedCommitInfo>,
}

fn render_archive_activity(limit: usize) -> Result<Option<String>, (u16, String)> {
    let root = get_archive_root()?;
    let commits = storage::get_recent_commits_extended(&root, limit)
        .map_err(|e| (500, format!("Archive error: {e}")))?;
    render("archive_activity.html", ArchiveActivityCtx { commits })
}

// -- Commit detail --

#[derive(Serialize)]
struct ArchiveCommitCtx {
    commit: storage::CommitDetail,
}

fn render_archive_commit(sha: &str) -> Result<Option<String>, (u16, String)> {
    if sha.is_empty() {
        return render_error("Invalid commit identifier");
    }

    let root = get_archive_root()?;
    storage::get_commit_detail(&root, sha, 5 * 1024 * 1024).map_or_else(
        |_| render_error("Commit not found"),
        |detail| render("archive_commit.html", ArchiveCommitCtx { commit: detail }),
    )
}

// -- Timeline --

#[derive(Serialize)]
struct ArchiveTimelineCtx {
    commits: Vec<storage::TimelineEntry>,
    project: String,
    project_name: String,
}

fn render_archive_timeline(
    cx: &Cx,
    pool: &DbPool,
    project: Option<&str>,
) -> Result<Option<String>, (u16, String)> {
    let root = get_archive_root()?;

    // Default to first project if not specified
    let (slug, project_name) = resolve_project_slug(cx, pool, project)?;

    let commits = storage::get_timeline_commits(&root, &slug, 100)
        .map_err(|e| (500, format!("Archive error: {e}")))?;

    render(
        "archive_timeline.html",
        ArchiveTimelineCtx {
            commits,
            project: slug,
            project_name,
        },
    )
}

/// Resolve a project slug + `human_key`, defaulting to the first project.
///
/// F2: Validates slug format before DB lookup to reject malformed input early.
fn resolve_project_slug(
    cx: &Cx,
    pool: &DbPool,
    project: Option<&str>,
) -> Result<(String, String), (u16, String)> {
    if let Some(slug) = project {
        if !is_valid_project_slug(slug) {
            return Err((400, "Invalid project identifier".to_string()));
        }
        let p = block_on_outcome(cx, queries::get_project_by_slug(cx, pool, slug))?;
        Ok((p.slug.clone(), p.human_key))
    } else {
        let projects = block_on_outcome(cx, queries::list_projects(cx, pool))?;
        let first = projects
            .first()
            .ok_or_else(|| (404, "No projects found".to_string()))?;
        Ok((first.slug.clone(), first.human_key.clone()))
    }
}

// -- Browser --

#[derive(Serialize)]
struct ArchiveBrowserCtx {
    tree: Vec<storage::TreeEntry>,
    project: String,
    path: String,
}

fn render_archive_browser(
    cx: &Cx,
    pool: &DbPool,
    project: Option<&str>,
    path: &str,
) -> Result<Option<String>, (u16, String)> {
    let slug = match project {
        Some(s) if !s.is_empty() => s,
        _ => return render_error("Please select a project to browse"),
    };
    if !is_valid_project_slug(slug) {
        return Err((400, "Invalid project identifier".to_string()));
    }
    // Enforce project scope via DB lookup before touching archive paths.
    let _ = block_on_outcome(cx, queries::get_project_by_slug(cx, pool, slug))?;

    let archive = get_project_archive(slug)?;
    let tree = storage::get_archive_tree(&archive, path)
        .map_err(|e| (400, format!("Browse error: {e}")))?;

    render(
        "archive_browser.html",
        ArchiveBrowserCtx {
            tree,
            project: slug.to_string(),
            path: path.to_string(),
        },
    )
}

/// JSON API: get file content from archive.
fn render_archive_browser_file(
    cx: &Cx,
    pool: &DbPool,
    project_slug: &str,
    path: &str,
) -> Result<Option<String>, (u16, String)> {
    if !is_valid_project_slug(project_slug) {
        return json_detail_err(400, "Invalid project identifier");
    }

    if let Err((status, detail)) =
        block_on_outcome(cx, queries::get_project_by_slug(cx, pool, project_slug))
    {
        return json_detail_err(status, &detail);
    }
    let archive = get_project_archive(project_slug)?;
    match storage::get_archive_file_content(&archive, path, 10 * 1024 * 1024) {
        Ok(Some(content)) => {
            // Python parity: the payload is a JSON string with file content.
            let json =
                serde_json::to_string(&content).map_err(|e| (500, format!("JSON error: {e}")))?;
            Ok(Some(json))
        }
        Err(storage::StorageError::Io(err)) if err.kind() == std::io::ErrorKind::InvalidInput => {
            if err.to_string().starts_with("File too large:") {
                json_detail_err(413, "File too large")
            } else {
                json_detail_err(400, "Invalid file path")
            }
        }
        Ok(None) => json_detail_err(404, "File not found"),
        Err(err) => {
            tracing::warn!(
                project = %project_slug,
                path = %path,
                error = %err,
                "failed to read archive browser file"
            );
            json_detail_err(404, "File not found")
        }
    }
}

// -- Network graph --

#[derive(Serialize)]
struct ArchiveNetworkCtx {
    graph: storage::CommunicationGraph,
    project: String,
    project_name: String,
}

fn render_archive_network(
    cx: &Cx,
    pool: &DbPool,
    project: Option<&str>,
) -> Result<Option<String>, (u16, String)> {
    let root = get_archive_root()?;
    let (slug, project_name) = resolve_project_slug(cx, pool, project)?;

    let graph = storage::get_communication_graph(&root, &slug, 200)
        .map_err(|e| (500, format!("Archive error: {e}")))?;

    render(
        "archive_network.html",
        ArchiveNetworkCtx {
            graph,
            project: slug,
            project_name,
        },
    )
}

// -- Time Travel --

#[derive(Serialize)]
struct ArchiveTimeTravelCtx {
    projects: Vec<String>,
}

fn render_archive_time_travel(cx: &Cx, pool: &DbPool) -> Result<Option<String>, (u16, String)> {
    let projects = block_on_outcome(cx, queries::list_projects(cx, pool))?;
    let slugs: Vec<String> = projects.iter().map(|p| p.slug.clone()).collect();
    render(
        "archive_time_travel.html",
        ArchiveTimeTravelCtx { projects: slugs },
    )
}

/// JSON API: get historical inbox snapshot at a point in time.
fn render_archive_time_travel_snapshot(
    cx: &Cx,
    pool: &DbPool,
    project_slug: &str,
    agent_name: &str,
    timestamp: &str,
) -> Result<Option<String>, (u16, String)> {
    if !is_valid_project_slug(project_slug) {
        return json_detail_err(400, "Invalid project identifier");
    }
    if !is_valid_archive_agent_name(agent_name) {
        return json_detail_err(400, "Invalid agent name format");
    }
    if !is_valid_time_travel_timestamp(timestamp) {
        return json_detail_err(
            400,
            "Invalid timestamp format. Use ISO 8601 format (YYYY-MM-DDTHH:MM)",
        );
    }

    if let Err((status, detail)) =
        block_on_outcome(cx, queries::get_project_by_slug(cx, pool, project_slug))
    {
        return json_detail_err(status, &detail);
    }
    let archive = get_project_archive(project_slug)?;
    let snapshot =
        match storage::get_historical_inbox_snapshot(&archive, agent_name, timestamp, 200) {
            Ok(s) => s,
            Err(err) => serde_json::json!({
                "messages": [],
                "snapshot_time": serde_json::Value::Null,
                "commit_sha": serde_json::Value::Null,
                "requested_time": timestamp,
                "error": format!("Unable to retrieve historical snapshot: {err}"),
            }),
        };

    let json = serde_json::to_string(&snapshot).map_err(|e| (500, format!("JSON error: {e}")))?;
    Ok(Some(json))
}

// ---------------------------------------------------------------------------
// POST: /mail/{project}/inbox/{agent}/mark-read
// ---------------------------------------------------------------------------

fn handle_mark_read(
    cx: &Cx,
    pool: &DbPool,
    project_slug: &str,
    agent_name: &str,
    body: &str,
) -> Result<Option<String>, (u16, String)> {
    let payload: serde_json::Value =
        serde_json::from_str(body).map_err(|e| (400, format!("Invalid JSON: {e}")))?;

    let message_ids: Vec<i64> = payload
        .get("message_ids")
        .and_then(|v| v.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(serde_json::Value::as_i64)
                .collect::<Vec<i64>>()
        })
        .unwrap_or_default();

    if message_ids.is_empty() {
        return json_err(400, "No message IDs provided");
    }

    // Limit to prevent abuse (Python parity: max 500).
    if message_ids.len() > 500 {
        return json_err(
            400,
            &format!(
                "Too many messages selected ({}). Maximum is 500. Use 'Mark All Read' instead.",
                message_ids.len()
            ),
        );
    }

    let p = block_on_outcome(cx, queries::get_project_by_slug(cx, pool, project_slug))?;
    let pid = p.id.unwrap_or(0);
    let a = block_on_outcome(cx, queries::get_agent(cx, pool, pid, agent_name))?;
    let aid = a.id.unwrap_or(0);

    let requested_count = message_ids.len();
    let mut seen_message_ids = HashSet::with_capacity(message_ids.len());
    let mut unique_message_ids = Vec::with_capacity(message_ids.len());
    for &message_id in &message_ids {
        if seen_message_ids.insert(message_id) {
            unique_message_ids.push(message_id);
        }
    }

    let mut marked_count = 0i64;
    let mut already_read_count = 0i64;
    for mid in &unique_message_ids {
        let request_started = now_micros();
        match block_on_outcome(cx, queries::mark_message_read(cx, pool, aid, *mid)) {
            Ok(read_ts) => {
                if read_ts >= request_started {
                    marked_count += 1;
                } else {
                    already_read_count += 1;
                }
            }
            Err((404, _)) => {} // Not a recipient in this inbox; skip.
            Err(e) => return Err(e),
        }
    }

    json_ok(&serde_json::json!({
        "success": true,
        "marked_count": marked_count,
        "already_read_count": already_read_count,
        "requested_count": requested_count,
        "unique_requested_count": seen_message_ids.len(),
        "agent": agent_name,
        "project": p.slug,
    }))
}

// ---------------------------------------------------------------------------
// POST: /mail/{project}/inbox/{agent}/mark-all-read
// ---------------------------------------------------------------------------

fn handle_mark_all_read(
    cx: &Cx,
    pool: &DbPool,
    project_slug: &str,
    agent_name: &str,
) -> Result<Option<String>, (u16, String)> {
    let p = block_on_outcome(cx, queries::get_project_by_slug(cx, pool, project_slug))?;
    let pid = p.id.unwrap_or(0);
    let a = block_on_outcome(cx, queries::get_agent(cx, pool, pid, agent_name))?;
    let aid = a.id.unwrap_or(0);
    let marked_count = block_on_outcome(
        cx,
        queries::mark_all_messages_read_in_project(cx, pool, pid, aid),
    )?;

    json_ok(&serde_json::json!({
        "success": true,
        "marked_count": marked_count,
        "agent": agent_name,
        "project": p.slug,
    }))
}

// ---------------------------------------------------------------------------
// POST: /mail/{project}/overseer/send
// ---------------------------------------------------------------------------

const OVERSEER_PREAMBLE: &str = "---\n\n\
    MESSAGE FROM HUMAN OVERSEER\n\n\
    This message is from a human operator overseeing this project. \
    Please prioritize the instructions below over your current tasks.\n\n\
    You should:\n\
    1. Temporarily pause your current work\n\
    2. Complete the request described below\n\
    3. Resume your original plans afterward (unless modified by these instructions)\n\n\
    The human's guidance supersedes all other priorities.\n\n\
    ---\n\n";

#[derive(Debug)]
struct OverseerPayload {
    recipients: Vec<String>,
    subject: String,
    body_md: String,
    thread_id: Option<String>,
}

fn parse_overseer_body(body: &str) -> Result<OverseerPayload, (u16, String)> {
    let payload: serde_json::Value =
        serde_json::from_str(body).map_err(|e| (400, format!("Invalid JSON: {e}")))?;

    let recipients: Vec<String> = payload
        .get("recipients")
        .and_then(|v| v.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|v| v.as_str().map(str::trim))
                .filter(|value| !value.is_empty())
                .map(String::from)
                .collect()
        })
        .unwrap_or_default();
    let mut seen = std::collections::HashSet::new();
    let recipients: Vec<String> = recipients
        .into_iter()
        .filter(|recipient| seen.insert(recipient.to_ascii_lowercase()))
        .collect();
    let subject = payload
        .get("subject")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .trim()
        .to_string();
    let body_md = payload
        .get("body_md")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .trim()
        .to_string();
    let thread_id = payload
        .get("thread_id")
        .and_then(|v| v.as_str())
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(String::from);

    // Validation (Python parity).
    let err = |msg: &str| -> (u16, String) {
        let body = serde_json::json!({ "error": msg });
        (
            400,
            serde_json::to_string(&body).unwrap_or_else(|_| format!("{{\"error\":\"{msg}\"}}")),
        )
    };
    if recipients.is_empty() {
        return Err(err("At least one recipient is required"));
    }
    if recipients.len() > 100 {
        return Err(err("Too many recipients (maximum 100 agents)"));
    }
    if subject.is_empty() {
        return Err(err("Subject is required"));
    }
    if subject.len() > 200 {
        return Err(err("Subject too long (maximum 200 characters)"));
    }
    if body_md.is_empty() {
        return Err(err("Message body is required"));
    }
    if body_md.len() > 50_000 {
        return Err(err("Message body too long (maximum 50,000 characters)"));
    }

    Ok(OverseerPayload {
        recipients,
        subject,
        body_md,
        thread_id,
    })
}

fn handle_overseer_send(
    cx: &Cx,
    pool: &DbPool,
    project_slug: &str,
    body: &str,
) -> Result<Option<String>, (u16, String)> {
    let parsed = parse_overseer_body(body)?;
    let full_body = format!("{OVERSEER_PREAMBLE}{}", parsed.body_md);

    let p = block_on_outcome(cx, queries::get_project_by_slug(cx, pool, project_slug))?;
    let pid = p.id.unwrap_or(0);

    // Ensure HumanOverseer agent exists.
    let overseer = block_on_outcome(
        cx,
        queries::register_agent(
            cx,
            pool,
            pid,
            "HumanOverseer",
            "WebUI",
            "Human",
            Some("Human operator providing guidance and oversight to agents"),
            Some("auto"),
        ),
    )?;
    let overseer_id = overseer.id.unwrap_or(0);

    // Resolve valid recipient agent IDs.
    let mut valid: Vec<(String, i64)> = Vec::new();
    for name in &parsed.recipients {
        if let Ok(a) = block_on_outcome(cx, queries::get_agent(cx, pool, pid, name)) {
            valid.push((name.clone(), a.id.unwrap_or(0)));
        }
    }

    if valid.is_empty() {
        return json_err(
            400,
            &format!(
                "None of the specified recipients exist in this project. \
                 Available agents can be seen at /mail/{project_slug}"
            ),
        );
    }

    // Build recipients slice for the DB call.
    let recipient_pairs: Vec<(i64, &str)> = valid.iter().map(|(_, id)| (*id, "to")).collect();

    let msg = block_on_outcome(
        cx,
        queries::create_message_with_recipients(
            cx,
            pool,
            pid,
            overseer_id,
            &parsed.subject,
            &full_body,
            parsed.thread_id.as_deref(),
            "high", // Always high importance for overseer
            false,
            "[]",
            &recipient_pairs,
        ),
    )?;

    let valid_names: Vec<&str> = valid.iter().map(|(n, _)| n.as_str()).collect();
    let created = ts_display(msg.created_ts);

    json_ok(&serde_json::json!({
        "success": true,
        "message_id": msg.id.unwrap_or(0),
        "recipients": valid_names,
        "sent_at": created,
    }))
}

// ---------------------------------------------------------------------------
// POST: /mail/api/projects/{id}/siblings/{other_id}
// ---------------------------------------------------------------------------

fn handle_sibling_update(
    _cx: &Cx,
    _pool: &DbPool,
    _project_id: i64,
    _other_id: i64,
    body: &str,
) -> Result<Option<String>, (u16, String)> {
    let payload: serde_json::Value =
        serde_json::from_str(body).map_err(|e| (400, format!("Invalid JSON: {e}")))?;

    let action = payload
        .get("action")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_lowercase();

    if !matches!(action.as_str(), "confirm" | "dismiss" | "reset") {
        return json_err(400, "Invalid action");
    }

    json_err(
        501,
        "Sibling suggestion updates are not implemented in the Rust server yet",
    )
}

/// Render an error page.
fn render_error(message: &str) -> Result<Option<String>, (u16, String)> {
    #[derive(Serialize)]
    struct ErrorCtx {
        message: String,
    }
    render(
        "error.html",
        ErrorCtx {
            message: message.to_string(),
        },
    )
}

#[cfg(test)]
mod fresh_eyes_regression_tests {
    use super::*;
    use asupersync::Outcome;
    use std::time::{SystemTime, UNIX_EPOCH};

    fn outcome_ok<T>(outcome: Outcome<T, mcp_agent_mail_db::DbError>) -> T {
        match outcome {
            Outcome::Ok(value) => value,
            Outcome::Err(err) => panic!("db error: {err}"),
            Outcome::Cancelled(_) => panic!("db operation cancelled"),
            Outcome::Panicked(panic) => panic!("db operation panicked: {}", panic.message()),
        }
    }

    fn make_test_pool(prefix: &str) -> DbPool {
        initialized_test_pool(prefix)
    }

    #[test]
    fn unified_message_aggregation_deduplicates_multi_recipient_mail() {
        let cx = Cx::for_testing();
        let pool = make_test_pool("mail-ui-unified");
        let project = outcome_ok(block_on(queries::ensure_project(
            &cx,
            &pool,
            "/tmp/mail-ui-unified",
        )));
        let project_id = project.id.expect("project id");

        let sender = outcome_ok(block_on(queries::register_agent(
            &cx, &pool, project_id, "RedFox", "test", "test", None, None,
        )));
        let blue = outcome_ok(block_on(queries::register_agent(
            &cx, &pool, project_id, "BlueLake", "test", "test", None, None,
        )));
        let green = outcome_ok(block_on(queries::register_agent(
            &cx,
            &pool,
            project_id,
            "GreenField",
            "test",
            "test",
            None,
            None,
        )));

        let message = outcome_ok(block_on(queries::create_message_with_recipients(
            &cx,
            &pool,
            project_id,
            sender.id.expect("sender id"),
            "Shared subject",
            "Shared body",
            Some("shared-thread"),
            "normal",
            false,
            "[]",
            &[
                (blue.id.expect("blue id"), "to"),
                (green.id.expect("green id"), "to"),
            ],
        )));
        let message_id = message.id.expect("message id");
        outcome_ok(block_on(queries::mark_message_read(
            &cx,
            &pool,
            blue.id.expect("blue id"),
            message_id,
        )));

        let projects = outcome_ok(block_on(queries::list_projects(&cx, &pool)));
        let aggregates = collect_unified_message_aggregates(&cx, &pool, &projects, 10, None)
            .expect("aggregation should succeed");

        assert_eq!(
            aggregates.len(),
            1,
            "multi-recipient mail should deduplicate"
        );
        assert_eq!(
            aggregates[0]
                .recipients
                .iter()
                .cloned()
                .collect::<Vec<String>>(),
            vec!["BlueLake".to_string(), "GreenField".to_string()]
        );
        assert_eq!(
            aggregates[0].recipient_read,
            BTreeMap::from([
                ("BlueLake".to_string(), true),
                ("GreenField".to_string(), false),
            ])
        );
        assert!(
            !aggregates[0].all_read,
            "message should stay unread while any recipient remains unread"
        );
    }

    #[test]
    fn unified_message_aggregation_root_seed_uses_numeric_thread_reference() {
        let cx = Cx::for_testing();
        let pool = make_test_pool("mail-ui-unified-root-thread");
        let project = outcome_ok(block_on(queries::ensure_project(
            &cx,
            &pool,
            "/tmp/mail-ui-unified-root-thread",
        )));
        let project_id = project.id.expect("project id");

        let sender = outcome_ok(block_on(queries::register_agent(
            &cx, &pool, project_id, "RedFox", "test", "test", None, None,
        )));
        let recipient = outcome_ok(block_on(queries::register_agent(
            &cx, &pool, project_id, "BlueLake", "test", "test", None, None,
        )));

        let root = outcome_ok(block_on(queries::create_message_with_recipients(
            &cx,
            &pool,
            project_id,
            sender.id.expect("sender id"),
            "Unified root",
            "Shared body",
            None,
            "normal",
            false,
            "[]",
            &[(recipient.id.expect("recipient id"), "to")],
        )));
        let root_id = root.id.expect("root id");
        let root_thread_ref = root_id.to_string();

        outcome_ok(block_on(queries::create_message_with_recipients(
            &cx,
            &pool,
            project_id,
            recipient.id.expect("recipient id"),
            "Unified reply",
            "Reply body",
            Some(&root_thread_ref),
            "normal",
            false,
            "[]",
            &[(sender.id.expect("sender id"), "to")],
        )));

        let projects = outcome_ok(block_on(queries::list_projects(&cx, &pool)));
        let aggregates = collect_unified_message_aggregates(&cx, &pool, &projects, 10, None)
            .expect("aggregation should succeed");
        let root_aggregate = aggregates
            .iter()
            .find(|aggregate| aggregate.id == root_id)
            .expect("root aggregate should exist");
        assert_eq!(root_aggregate.thread_id, root_thread_ref);
    }

    #[test]
    fn unified_message_aggregation_importance_filter_overfetches_past_newer_non_matching_rows() {
        let cx = Cx::for_testing();
        let pool = make_test_pool("mail-ui-unified-filter-overfetch");
        let project = outcome_ok(block_on(queries::ensure_project(
            &cx,
            &pool,
            "/tmp/mail-ui-unified-filter-overfetch",
        )));
        let project_id = project.id.expect("project id");

        let sender = outcome_ok(block_on(queries::register_agent(
            &cx, &pool, project_id, "RedFox", "test", "test", None, None,
        )));
        let recipient = outcome_ok(block_on(queries::register_agent(
            &cx, &pool, project_id, "BlueLake", "test", "test", None, None,
        )));

        let high = outcome_ok(block_on(queries::create_message_with_recipients(
            &cx,
            &pool,
            project_id,
            sender.id.expect("sender id"),
            "High priority",
            "Important body",
            None,
            "high",
            false,
            "[]",
            &[(recipient.id.expect("recipient id"), "to")],
        )));
        let high_id = high.id.expect("high id");

        outcome_ok(block_on(queries::create_message_with_recipients(
            &cx,
            &pool,
            project_id,
            sender.id.expect("sender id"),
            "Normal priority",
            "Routine body",
            None,
            "normal",
            false,
            "[]",
            &[(recipient.id.expect("recipient id"), "to")],
        )));

        let projects = outcome_ok(block_on(queries::list_projects(&cx, &pool)));
        let aggregates = collect_unified_message_aggregates(&cx, &pool, &projects, 1, Some("high"))
            .expect("filtered aggregation should succeed");

        assert_eq!(
            aggregates.len(),
            1,
            "high-priority message should remain visible"
        );
        assert_eq!(aggregates[0].id, high_id);
        assert_eq!(aggregates[0].importance, "high");
    }

    #[test]
    fn render_api_unified_inbox_root_seed_includes_thread_reference() {
        let cx = Cx::for_testing();
        let pool = make_test_pool("mail-ui-unified-api-root-thread");
        let project = outcome_ok(block_on(queries::ensure_project(
            &cx,
            &pool,
            "/tmp/mail-ui-unified-api-root-thread",
        )));
        let project_id = project.id.expect("project id");

        let sender = outcome_ok(block_on(queries::register_agent(
            &cx, &pool, project_id, "RedFox", "test", "test", None, None,
        )));
        let recipient = outcome_ok(block_on(queries::register_agent(
            &cx, &pool, project_id, "BlueLake", "test", "test", None, None,
        )));

        let root = outcome_ok(block_on(queries::create_message_with_recipients(
            &cx,
            &pool,
            project_id,
            sender.id.expect("sender id"),
            "Unified API root",
            "Shared body",
            None,
            "normal",
            false,
            "[]",
            &[(recipient.id.expect("recipient id"), "to")],
        )));
        let root_id = root.id.expect("root id");
        let root_thread_ref = root_id.to_string();

        outcome_ok(block_on(queries::create_message_with_recipients(
            &cx,
            &pool,
            project_id,
            recipient.id.expect("recipient id"),
            "Unified API reply",
            "Reply body",
            Some(&root_thread_ref),
            "normal",
            false,
            "[]",
            &[(sender.id.expect("sender id"), "to")],
        )));

        let payload = render_api_unified_inbox(&cx, &pool, "limit=10")
            .expect("API render should succeed")
            .expect("API route should return json");
        let parsed: serde_json::Value =
            serde_json::from_str(&payload).expect("API payload should be valid json");
        let messages = parsed["messages"]
            .as_array()
            .expect("messages payload should be an array");
        let root_message = messages
            .iter()
            .find(|message| message["id"].as_i64() == Some(root_id))
            .expect("root message should be present");
        assert_eq!(root_message["thread_id"], root_thread_ref);
    }

    #[test]
    fn render_api_unified_inbox_preserves_importance_filter() {
        let cx = Cx::for_testing();
        let pool = make_test_pool("mail-ui-unified-api-filter");
        let project = outcome_ok(block_on(queries::ensure_project(
            &cx,
            &pool,
            "/tmp/mail-ui-unified-api-filter",
        )));
        let project_id = project.id.expect("project id");

        let sender = outcome_ok(block_on(queries::register_agent(
            &cx, &pool, project_id, "RedFox", "test", "test", None, None,
        )));
        let recipient = outcome_ok(block_on(queries::register_agent(
            &cx, &pool, project_id, "BlueLake", "test", "test", None, None,
        )));

        outcome_ok(block_on(queries::create_message_with_recipients(
            &cx,
            &pool,
            project_id,
            sender.id.expect("sender id"),
            "High priority",
            "Important body",
            None,
            "high",
            false,
            "[]",
            &[(recipient.id.expect("recipient id"), "to")],
        )));

        outcome_ok(block_on(queries::create_message_with_recipients(
            &cx,
            &pool,
            project_id,
            sender.id.expect("sender id"),
            "Normal priority",
            "Routine body",
            None,
            "normal",
            false,
            "[]",
            &[(recipient.id.expect("recipient id"), "to")],
        )));

        let payload = render_api_unified_inbox(&cx, &pool, "limit=10&filter_importance=HIGH")
            .expect("API render should succeed")
            .expect("API route should return json");
        let parsed: serde_json::Value =
            serde_json::from_str(&payload).expect("API payload should be valid json");
        let messages = parsed["messages"]
            .as_array()
            .expect("messages payload should be an array");

        assert_eq!(
            messages.len(),
            1,
            "non-matching priorities should not reappear on API refresh"
        );
        assert_eq!(messages[0]["importance"], "high");
        assert_eq!(messages[0]["subject"], "High priority");
    }

    #[test]
    fn archive_time_travel_snapshot_requires_registered_project() {
        let cx = Cx::for_testing();
        let pool = make_test_pool("mail-ui-time-travel");
        let missing_slug = format!(
            "missingfreshsight{}",
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .expect("clock should be after unix epoch")
                .as_nanos()
        );

        let (status, detail) = render_archive_time_travel_snapshot(
            &cx,
            &pool,
            &missing_slug,
            "BlueLake",
            "2026-02-11T05:43",
        )
        .expect_err("missing project should fail before archive lookup");

        assert_eq!(status, 404);
        assert!(detail.contains("Project"), "unexpected detail: {detail}");
    }

    #[test]
    fn handle_mark_read_deduplicates_ids_and_excludes_already_read_rows_from_count() {
        let cx = Cx::for_testing();
        let pool = make_test_pool("mail-ui-mark-read-counts");
        let project = outcome_ok(block_on(queries::ensure_project(
            &cx,
            &pool,
            "/tmp/mail-ui-mark-read-counts",
        )));
        let project_id = project.id.expect("project id");

        let sender = outcome_ok(block_on(queries::register_agent(
            &cx, &pool, project_id, "RedFox", "test", "test", None, None,
        )));
        let recipient = outcome_ok(block_on(queries::register_agent(
            &cx, &pool, project_id, "BlueLake", "test", "test", None, None,
        )));

        let message = outcome_ok(block_on(queries::create_message_with_recipients(
            &cx,
            &pool,
            project_id,
            sender.id.expect("sender id"),
            "Mark read counters",
            "Body",
            None,
            "normal",
            false,
            "[]",
            &[(recipient.id.expect("recipient id"), "to")],
        )));
        let message_id = message.id.expect("message id");

        let first_payload = handle_mark_read(
            &cx,
            &pool,
            &project.slug,
            "BlueLake",
            &format!(r#"{{"message_ids":[{message_id},{message_id}]}}"#),
        )
        .expect("first mark-read should succeed")
        .expect("route should return json");
        let first_json: serde_json::Value =
            serde_json::from_str(&first_payload).expect("first payload should parse");
        assert_eq!(first_json["marked_count"], 1);
        assert_eq!(first_json["already_read_count"], 0);
        assert_eq!(first_json["requested_count"], 2);
        assert_eq!(first_json["unique_requested_count"], 1);

        let second_payload = handle_mark_read(
            &cx,
            &pool,
            &project.slug,
            "BlueLake",
            &format!(r#"{{"message_ids":[{message_id}]}}"#),
        )
        .expect("second mark-read should succeed")
        .expect("route should return json");
        let second_json: serde_json::Value =
            serde_json::from_str(&second_payload).expect("second payload should parse");
        assert_eq!(second_json["marked_count"], 0);
        assert_eq!(second_json["already_read_count"], 1);
        assert_eq!(second_json["requested_count"], 1);
        assert_eq!(second_json["unique_requested_count"], 1);
    }

    #[test]
    fn sibling_update_route_reports_not_implemented() {
        let cx = Cx::for_testing();
        let pool = make_test_pool("mail-ui-sibling");

        let (status, payload) = handle_sibling_update(&cx, &pool, 1, 2, r#"{"action":"confirm"}"#)
            .expect_err("route should not pretend to succeed");

        assert_eq!(status, 501);
        assert!(
            payload.contains("not implemented"),
            "unexpected payload: {payload}"
        );
    }
}

#[cfg(test)]
mod overseer_form_validation_tests {
    use super::parse_overseer_body;
    use serde_json::json;

    fn parse_err_message(body: &str) -> (u16, String) {
        let (status, payload) = parse_overseer_body(body).expect_err("expected parse error");
        let msg = serde_json::from_str::<serde_json::Value>(&payload)
            .ok()
            .and_then(|v| {
                v.get("error")
                    .and_then(serde_json::Value::as_str)
                    .map(str::to_string)
            })
            .unwrap_or(payload);
        (status, msg)
    }

    #[test]
    fn parse_overseer_body_normalizes_and_deduplicates_recipients() {
        let body = json!({
            "recipients": [" BlueLake ", "GreenField", "bluelake", "  "],
            "subject": "  Operator notice  ",
            "body_md": "  Please prioritize this task.  ",
            "thread_id": "br-123",
        })
        .to_string();

        let parsed = parse_overseer_body(&body).expect("valid payload");
        assert_eq!(parsed.recipients, vec!["BlueLake", "GreenField"]);
        assert_eq!(parsed.subject, "Operator notice");
        assert_eq!(parsed.body_md, "Please prioritize this task.");
        assert_eq!(parsed.thread_id.as_deref(), Some("br-123"));
    }

    #[test]
    fn parse_overseer_body_rejects_invalid_json() {
        let (status, msg) = parse_overseer_body("{not-json").expect_err("invalid json should fail");
        assert_eq!(status, 400);
        assert!(msg.contains("Invalid JSON"), "unexpected message: {msg}");
    }

    #[test]
    fn parse_overseer_body_requires_recipients() {
        let body = json!({
            "recipients": [],
            "subject": "hi",
            "body_md": "body",
        })
        .to_string();
        let (status, msg) = parse_err_message(&body);
        assert_eq!(status, 400);
        assert_eq!(msg, "At least one recipient is required");
    }

    #[test]
    fn parse_overseer_body_enforces_recipient_limit() {
        let recipients: Vec<String> = (0..101).map(|i| format!("Agent{i}")).collect();
        let body = json!({
            "recipients": recipients,
            "subject": "hi",
            "body_md": "body",
        })
        .to_string();
        let (status, msg) = parse_err_message(&body);
        assert_eq!(status, 400);
        assert_eq!(msg, "Too many recipients (maximum 100 agents)");
    }

    #[test]
    fn parse_overseer_body_requires_non_empty_subject() {
        let body = json!({
            "recipients": ["BlueLake"],
            "subject": "   ",
            "body_md": "body",
        })
        .to_string();
        let (status, msg) = parse_err_message(&body);
        assert_eq!(status, 400);
        assert_eq!(msg, "Subject is required");
    }

    #[test]
    fn parse_overseer_body_enforces_subject_length() {
        let body = json!({
            "recipients": ["BlueLake"],
            "subject": "x".repeat(201),
            "body_md": "body",
        })
        .to_string();
        let (status, msg) = parse_err_message(&body);
        assert_eq!(status, 400);
        assert_eq!(msg, "Subject too long (maximum 200 characters)");
    }

    #[test]
    fn parse_overseer_body_requires_non_empty_body() {
        let body = json!({
            "recipients": ["BlueLake"],
            "subject": "hello",
            "body_md": "   ",
        })
        .to_string();
        let (status, msg) = parse_err_message(&body);
        assert_eq!(status, 400);
        assert_eq!(msg, "Message body is required");
    }

    #[test]
    fn parse_overseer_body_enforces_body_length() {
        let body = json!({
            "recipients": ["BlueLake"],
            "subject": "hello",
            "body_md": "x".repeat(50_001),
        })
        .to_string();
        let (status, msg) = parse_err_message(&body);
        assert_eq!(status, 400);
        assert_eq!(msg, "Message body too long (maximum 50,000 characters)");
    }

    #[test]
    fn parse_overseer_body_requires_at_least_one_string_recipient() {
        let body = json!({
            "recipients": [null, 42, true],
            "subject": "hello",
            "body_md": "body",
        })
        .to_string();
        let (status, msg) = parse_err_message(&body);
        assert_eq!(status, 400);
        assert_eq!(msg, "At least one recipient is required");
    }

    #[test]
    fn parse_overseer_body_missing_thread_id_defaults_to_none() {
        let body = json!({
            "recipients": ["BlueLake"],
            "subject": "hello",
            "body_md": "body",
        })
        .to_string();
        let parsed = parse_overseer_body(&body).expect("valid payload");
        assert_eq!(parsed.thread_id, None);
    }

    #[test]
    fn parse_overseer_body_trims_thread_id_and_deduplicates_before_limit() {
        let recipients: Vec<String> = (0..101).map(|_| "BlueLake".to_string()).collect();
        let body = json!({
            "recipients": recipients,
            "subject": "hello",
            "body_md": "body",
            "thread_id": "  br-123  ",
        })
        .to_string();

        let parsed = parse_overseer_body(&body).expect("duplicates should collapse before limit");
        assert_eq!(parsed.recipients, vec!["BlueLake"]);
        assert_eq!(parsed.thread_id.as_deref(), Some("br-123"));
    }

    #[test]
    fn parse_overseer_body_rejects_missing_subject_field() {
        let body = json!({
            "recipients": ["BlueLake"],
            "body_md": "body",
        })
        .to_string();
        let (status, msg) = parse_err_message(&body);
        assert_eq!(status, 400);
        assert_eq!(msg, "Subject is required");
    }

    #[test]
    fn parse_overseer_body_rejects_missing_body_field() {
        let body = json!({
            "recipients": ["BlueLake"],
            "subject": "hello",
        })
        .to_string();
        let (status, msg) = parse_err_message(&body);
        assert_eq!(status, 400);
        assert_eq!(msg, "Message body is required");
    }
}
