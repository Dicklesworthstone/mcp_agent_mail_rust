use asupersync::Cx;
use asupersync::runtime::RuntimeBuilder;
use fastmcp::McpError;
use fastmcp::prelude::McpContext;
use mcp_agent_mail_tools::{ensure_project, register_agent, whois};
use serde_json::Value;
use std::collections::BTreeSet;
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

static TEST_LOCK: Mutex<()> = Mutex::new(());
static TEST_COUNTER: AtomicU64 = AtomicU64::new(0);

const PLACEHOLDER_PATTERNS: &[&str] = &[
    "YOUR_PROJECT",
    "YOUR_PROJECT_PATH",
    "YOUR_PROJECT_KEY",
    "PLACEHOLDER",
    "<PROJECT>",
    "{PROJECT}",
    "$PROJECT",
];

fn unique_suffix() -> u64 {
    let micros = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_micros();
    let time_component = u64::try_from(micros).unwrap_or(u64::MAX);
    time_component.wrapping_add(TEST_COUNTER.fetch_add(1, Ordering::Relaxed))
}

fn run_serial_async<F, Fut, T>(f: F) -> T
where
    F: FnOnce(Cx) -> Fut,
    Fut: std::future::Future<Output = T>,
{
    let _lock = TEST_LOCK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let cx = Cx::for_testing();
    let rt = RuntimeBuilder::current_thread()
        .build()
        .expect("build runtime");
    rt.block_on(f(cx))
}

fn collect_legacy_error_codes_from_source(source: &str, out: &mut BTreeSet<String>) {
    let needle = "legacy_tool_error(";
    let mut start = 0usize;
    while let Some(rel) = source[start..].find(needle) {
        let call_start = start + rel + needle.len();
        let mut cursor = call_start;
        for ch in source[call_start..].chars() {
            if ch.is_whitespace() {
                cursor += ch.len_utf8();
                continue;
            }
            if ch == '"' {
                cursor += 1;
                if let Some(end_quote) = source[cursor..].find('"') {
                    let candidate = &source[cursor..cursor + end_quote];
                    if candidate
                        .chars()
                        .all(|c| c.is_ascii_uppercase() || c.is_ascii_digit() || c == '_')
                    {
                        out.insert(candidate.to_string());
                    }
                }
            }
            break;
        }
        start = call_start;
    }
}

fn collect_declared_error_codes() -> BTreeSet<String> {
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
    let tool_src = root.join("../mcp-agent-mail-tools/src");
    let files = [
        "build_slots.rs",
        "contacts.rs",
        "identity.rs",
        "lib.rs",
        "macros.rs",
        "messaging.rs",
        "products.rs",
        "reservations.rs",
        "search.rs",
    ];

    let mut codes = BTreeSet::new();
    for file in &files {
        let path = tool_src.join(file);
        let content = std::fs::read_to_string(&path)
            .unwrap_or_else(|e| panic!("failed reading {}: {e}", path.display()));
        collect_legacy_error_codes_from_source(&content, &mut codes);
        // CONTACT_BLOCKED is emitted via a dedicated payload helper.
        if content.contains("\"type\": \"CONTACT_BLOCKED\"") {
            codes.insert("CONTACT_BLOCKED".to_string());
        }
    }
    codes
}

fn error_object(err: &McpError, scenario: &str) -> serde_json::Map<String, Value> {
    err.data
        .as_ref()
        .and_then(Value::as_object)
        .and_then(|root| root.get("error"))
        .and_then(Value::as_object)
        .cloned()
        .unwrap_or_else(|| panic!("{scenario}: expected root.error object payload"))
}

fn assert_error_envelope(
    err: &McpError,
    scenario: &str,
    expected_type: &str,
    expected_recoverable: bool,
) -> serde_json::Map<String, Value> {
    let payload = error_object(err, scenario);
    let keys: BTreeSet<String> = payload.keys().cloned().collect();
    let expected_keys: BTreeSet<String> = ["data", "message", "recoverable", "type"]
        .into_iter()
        .map(str::to_string)
        .collect();
    assert_eq!(
        keys, expected_keys,
        "{scenario}: payload keys mismatch; expected {expected_keys:?}, got {keys:?}"
    );
    assert_eq!(
        payload.get("type").and_then(Value::as_str),
        Some(expected_type),
        "{scenario}: error.type mismatch"
    );
    assert_eq!(
        payload.get("recoverable").and_then(Value::as_bool),
        Some(expected_recoverable),
        "{scenario}: error.recoverable mismatch"
    );
    assert_eq!(
        payload.get("message").and_then(Value::as_str),
        Some(err.message.as_str()),
        "{scenario}: top-level message should match payload error.message"
    );
    payload
}

fn extract_slug_from_ensure_project_response(project_json: &str) -> String {
    let value: Value =
        serde_json::from_str(project_json).expect("ensure_project should return valid JSON");
    // ProjectWithIdentityResponse uses #[serde(flatten)] on identity,
    // so slug is at top level, not under "identity".
    value
        .get("slug")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string()
}

#[test]
fn error_code_catalog_is_stable() {
    let actual = collect_declared_error_codes();
    let expected: BTreeSet<String> = [
        "ARCHIVE_ERROR",
        "CONFIGURATION_ERROR",
        "CONTACT_BLOCKED",
        "CONTACT_REQUIRED",
        "DATABASE_CORRUPTION",
        "DATABASE_ERROR",
        "DATABASE_POOL_EXHAUSTED",
        "DISK_FULL",
        "EMPTY_MODEL",
        "EMPTY_PATHS",
        "EMPTY_PROGRAM",
        "FEATURE_DISABLED",
        "INVALID_AGENT_NAME",
        "INVALID_ARGUMENT",
        "INVALID_LIMIT",
        "INVALID_THREAD_ID",
        "INVALID_TIMESTAMP",
        "MISSING_FIELD",
        "NOT_FOUND",
        "RECIPIENT_NOT_FOUND",
        "RESERVATION_ACTIVE",
        "RESOURCE_BUSY",
        "TYPE_ERROR",
        "UNHANDLED_EXCEPTION",
    ]
    .into_iter()
    .map(str::to_string)
    .collect();

    eprintln!(
        "Error code parity catalog: expected_count={}, actual_count={}",
        expected.len(),
        actual.len()
    );
    assert_eq!(
        actual, expected,
        "Declared legacy error-code set drifted from parity baseline"
    );
}

#[test]
fn validation_and_lookup_errors_have_expected_envelope_shape() {
    run_serial_async(|cx| async move {
        let ctx = McpContext::new(cx.clone(), 1);
        let project_key = format!("/tmp/error-code-parity-{}", unique_suffix());
        let scenario_prefix = "validation_lookup_envelope";

        eprintln!("[{scenario_prefix}] scenario=relative_human_key_invalid_argument start");
        let relative_project_err = ensure_project(&ctx, "relative/path".to_string(), None)
            .await
            .expect_err("relative path must fail");
        let payload = assert_error_envelope(
            &relative_project_err,
            "relative_human_key_invalid_argument",
            "INVALID_ARGUMENT",
            true,
        );
        assert_eq!(
            payload
                .get("data")
                .and_then(Value::as_object)
                .and_then(|d| d.get("field"))
                .and_then(Value::as_str),
            Some("human_key"),
            "relative_human_key_invalid_argument: expected data.field=human_key"
        );

        ensure_project(&ctx, project_key.clone(), None)
            .await
            .expect("ensure_project should succeed");

        eprintln!("[{scenario_prefix}] scenario=empty_program start");
        let empty_program_err = register_agent(
            &ctx,
            project_key.clone(),
            String::new(),
            "gpt-5".to_string(),
            Some("BlueLake".to_string()),
            Some("error code parity".to_string()),
            None,
        )
        .await
        .expect_err("empty program must fail");
        assert_error_envelope(&empty_program_err, "empty_program", "EMPTY_PROGRAM", true);

        eprintln!("[{scenario_prefix}] scenario=empty_model start");
        let empty_model_err = register_agent(
            &ctx,
            project_key.clone(),
            "codex-cli".to_string(),
            String::new(),
            Some("BlueLake".to_string()),
            Some("error code parity".to_string()),
            None,
        )
        .await
        .expect_err("empty model must fail");
        assert_error_envelope(&empty_model_err, "empty_model", "EMPTY_MODEL", true);

        eprintln!("[{scenario_prefix}] scenario=empty_project_key start");
        let empty_project_key_err =
            whois(&ctx, "   ".to_string(), "BlueLake".to_string(), None, None)
                .await
                .expect_err("empty project key must fail");
        assert_error_envelope(
            &empty_project_key_err,
            "empty_project_key",
            "INVALID_ARGUMENT",
            true,
        );
    });
}

#[test]
fn placeholder_patterns_emit_configuration_error_with_detected_placeholder() {
    run_serial_async(|cx| async move {
        let ctx = McpContext::new(cx.clone(), 1);

        for placeholder in PLACEHOLDER_PATTERNS {
            eprintln!("[placeholder_detection] scenario=placeholder_{placeholder} start");
            let err = whois(
                &ctx,
                (*placeholder).to_string(),
                "BlueLake".to_string(),
                None,
                None,
            )
            .await
            .expect_err("placeholder project key must fail");
            let payload =
                assert_error_envelope(&err, "placeholder_detection", "CONFIGURATION_ERROR", true);
            let data = payload
                .get("data")
                .and_then(Value::as_object)
                .expect("placeholder_detection: expected error.data object");
            assert_eq!(
                data.get("detected_placeholder").and_then(Value::as_str),
                Some(*placeholder),
                "placeholder_detection: detected_placeholder mismatch"
            );
        }
    });
}

#[test]
fn project_not_found_with_suggestions_has_expected_shape() {
    run_serial_async(|cx| async move {
        let ctx = McpContext::new(cx.clone(), 1);
        let base = format!("/tmp/error-parity-project-alpha-{}", unique_suffix());

        let project_json = ensure_project(&ctx, base.clone(), None)
            .await
            .expect("ensure_project should succeed");
        let slug = extract_slug_from_ensure_project_response(&project_json);
        let typo = format!("{slug}-x");

        eprintln!("[not_found_suggestions] scenario=project_typo key={typo}");
        let err = whois(&ctx, typo, "BlueLake".to_string(), None, None)
            .await
            .expect_err("project typo should fail with NOT_FOUND");
        let payload = assert_error_envelope(
            &err,
            "project_not_found_with_suggestions",
            "NOT_FOUND",
            true,
        );
        assert!(
            err.message.contains("Did you mean:"),
            "project_not_found_with_suggestions: message should include Did you mean"
        );

        let data = payload
            .get("data")
            .and_then(Value::as_object)
            .expect("project_not_found_with_suggestions: expected error.data object");
        let suggestions = data
            .get("suggestions")
            .and_then(Value::as_array)
            .expect("project_not_found_with_suggestions: expected suggestions array");
        assert!(
            !suggestions.is_empty(),
            "project_not_found_with_suggestions: expected at least one suggestion"
        );
        for item in suggestions {
            let suggestion = item
                .as_object()
                .expect("project_not_found_with_suggestions: suggestion must be object");
            let score = suggestion
                .get("score")
                .and_then(Value::as_f64)
                .expect("project_not_found_with_suggestions: suggestion.score should be numeric");
            let two_decimal_steps = score * 100.0;
            assert!(
                (two_decimal_steps.round() - two_decimal_steps).abs() < f64::EPSILON,
                "project_not_found_with_suggestions: expected score rounded to 2 decimals, got {score}"
            );
            assert!(
                suggestion.get("slug").and_then(Value::as_str).is_some(),
                "project_not_found_with_suggestions: suggestion.slug missing"
            );
            assert!(
                suggestion
                    .get("human_key")
                    .and_then(Value::as_str)
                    .is_some(),
                "project_not_found_with_suggestions: suggestion.human_key missing"
            );
        }
    });
}

#[test]
fn not_found_without_suggestions_and_missing_agent_have_expected_payload_fields() {
    run_serial_async(|cx| async move {
        let ctx = McpContext::new(cx.clone(), 1);
        let project_key = format!("/tmp/error-parity-agent-{}", unique_suffix());
        let random_key = format!("qzxw-unrelated-project-{}", unique_suffix());

        ensure_project(&ctx, project_key.clone(), None)
            .await
            .expect("ensure_project should succeed");
        register_agent(
            &ctx,
            project_key.clone(),
            "codex-cli".to_string(),
            "gpt-5".to_string(),
            Some("BlueLake".to_string()),
            Some("error code parity".to_string()),
            None,
        )
        .await
        .expect("register_agent should succeed");

        eprintln!("[not_found_without_suggestions] scenario=unrelated_project");
        let unrelated_project_err =
            whois(&ctx, random_key.clone(), "BlueLake".to_string(), None, None)
                .await
                .expect_err("unrelated project should fail");
        let payload = assert_error_envelope(
            &unrelated_project_err,
            "project_without_suggestions",
            "NOT_FOUND",
            true,
        );
        // Message may contain "no similar projects exist" OR "Did you mean"
        // depending on whether the shared test DB has fuzzy matches.
        let msg = &unrelated_project_err.message;
        assert!(
            msg.contains("not found"),
            "project_without_suggestions: message should include 'not found', got: {msg}"
        );
        let data = payload
            .get("data")
            .and_then(Value::as_object)
            .expect("project_without_suggestions: expected error.data object");
        assert_eq!(
            data.get("identifier").and_then(Value::as_str),
            Some(random_key.as_str()),
            "project_without_suggestions: expected data.identifier to match missing identifier"
        );

        eprintln!("[not_found_without_suggestions] scenario=missing_agent");
        let missing_agent_err = whois(&ctx, project_key, "UnknownAgent".to_string(), None, None)
            .await
            .expect_err("missing agent should fail");
        let payload = assert_error_envelope(&missing_agent_err, "missing_agent", "NOT_FOUND", true);
        let data = payload
            .get("data")
            .and_then(Value::as_object)
            .expect("missing_agent: expected error.data object");
        assert_eq!(
            data.get("entity").and_then(Value::as_str),
            Some("Agent"),
            "missing_agent: expected data.entity=Agent"
        );
        let ident = data
            .get("identifier")
            .and_then(Value::as_str)
            .expect("missing_agent: expected data.identifier");
        // DB layer formats as "{project_id}:{agent_name}"
        assert!(
            ident.ends_with("UnknownAgent"),
            "missing_agent: expected data.identifier to end with UnknownAgent, got: {ident}"
        );
    });
}
