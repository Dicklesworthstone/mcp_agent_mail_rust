use arbitrary::{Arbitrary, Unstructured};
use fastmcp::{Budget, CallToolParams, Content, Cx};
use fastmcp_core::SessionState;
use mcp_agent_mail_core::{Config, config::with_process_env_overrides_for_test};
use mcp_agent_mail_tools::{FileReservationPaths, SearchMessages, SendMessage};
use proptest::prelude::*;
use serde_json::{Map, Number, Value, json};
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

static ENV_LOCK: Mutex<()> = Mutex::new(());
static CASE_COUNTER: AtomicU64 = AtomicU64::new(0);

fn proptest_config() -> proptest::test_runner::Config {
    proptest::test_runner::Config {
        cases: 96,
        max_shrink_iters: 128,
        ..proptest::test_runner::Config::default()
    }
}

fn unique_suffix() -> u64 {
    let micros = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_micros();
    let time_component = u64::try_from(micros).unwrap_or(u64::MAX);
    time_component.wrapping_add(CASE_COUNTER.fetch_add(1, Ordering::Relaxed))
}

fn router_with_fuzzed_tools() -> fastmcp::Router {
    fastmcp_server::ServerBuilder::new("mcp-agent-mail-tools-proptest", "0")
        .tool(SendMessage)
        .tool(FileReservationPaths)
        .tool(SearchMessages)
        .build()
        .into_router()
}

fn panic_payload_to_string(payload: Box<dyn std::any::Any + Send>) -> String {
    match payload.downcast::<String>() {
        Ok(message) => *message,
        Err(payload) => payload.downcast::<&'static str>().map_or_else(
            |_| "non-string panic payload".to_string(),
            |message| (*message).to_string(),
        ),
    }
}

fn bounded_string(u: &mut Unstructured<'_>) -> arbitrary::Result<String> {
    let len = u.int_in_range::<usize>(0..=64)?;
    let bytes = u.bytes(len)?;
    Ok(String::from_utf8_lossy(bytes).into_owned())
}

fn bounded_string_array(u: &mut Unstructured<'_>) -> Value {
    let len = u.int_in_range::<usize>(0..=5).unwrap_or(0);
    let items = (0..len)
        .map(|_| Value::String(bounded_string(u).unwrap_or_default()))
        .collect();
    Value::Array(items)
}

fn arbitrary_json_value(u: &mut Unstructured<'_>, depth: u8) -> arbitrary::Result<Value> {
    let max_variant = if depth >= 3 { 4 } else { 6 };
    match u.int_in_range::<u8>(0..=max_variant)? {
        0 => Ok(Value::Null),
        1 => Ok(Value::Bool(bool::arbitrary(u)?)),
        2 => Ok(Value::Number(Number::from(
            u.int_in_range::<i64>(-1_000_000..=1_000_000)?,
        ))),
        3 => Ok(Value::String(bounded_string(u)?)),
        4 => Ok(Value::Array(
            (0..u.int_in_range::<usize>(0..=4)?)
                .map(|_| arbitrary_json_value(u, depth.saturating_add(1)).unwrap_or(Value::Null))
                .collect(),
        )),
        _ => {
            let mut object = Map::new();
            for idx in 0..u.int_in_range::<usize>(0..=4)? {
                let key = bounded_string(u).unwrap_or_else(|_| format!("key_{idx}"));
                let value = arbitrary_json_value(u, depth.saturating_add(1)).unwrap_or(Value::Null);
                object.insert(key, value);
            }
            Ok(Value::Object(object))
        }
    }
}

fn plausible_field_value(field: &str, u: &mut Unstructured<'_>) -> Value {
    match field {
        "project_key" => Value::String(format!(
            "/tmp/tools-input-proptest-{}",
            bounded_string(u).unwrap_or_default()
        )),
        "sender_name" | "agent_name" => Value::String("BlueLake".to_string()),
        "to" => json!(["GreenField"]),
        "paths" => json!(["src/lib.rs"]),
        "subject" => Value::String(bounded_string(u).unwrap_or_else(|_| "subject".to_string())),
        "body_md" => Value::String(bounded_string(u).unwrap_or_else(|_| "body".to_string())),
        "query" => Value::String(bounded_string(u).unwrap_or_else(|_| "needle".to_string())),
        "cc" | "bcc" | "attachment_paths" => bounded_string_array(u),
        "ttl_seconds" | "limit" | "offset" => Value::Number(Number::from(
            u.int_in_range::<i64>(-10_000..=31_536_999)
                .unwrap_or_default(),
        )),
        "exclusive"
        | "convert_images"
        | "ack_required"
        | "broadcast"
        | "auto_contact_if_blocked"
        | "explain" => Value::Bool(bool::arbitrary(u).unwrap_or_default()),
        _ => Value::String(bounded_string(u).unwrap_or_default()),
    }
}

fn field_value(field: &str, u: &mut Unstructured<'_>) -> Value {
    if bool::arbitrary(u).unwrap_or_default() {
        plausible_field_value(field, u)
    } else {
        arbitrary_json_value(u, 0).unwrap_or(Value::Null)
    }
}

fn structured_arguments_from_bytes(
    bytes: &[u8],
    required_fields: &[&str],
    optional_fields: &[&str],
) -> Value {
    let mut u = Unstructured::new(bytes);
    if u.int_in_range::<u8>(0..=9).unwrap_or_default() == 0 {
        return arbitrary_json_value(&mut u, 0).unwrap_or(Value::Null);
    }

    let mut object = Map::new();
    for field in required_fields {
        if bool::arbitrary(&mut u).unwrap_or(true) {
            object.insert((*field).to_string(), field_value(field, &mut u));
        }
    }
    for field in optional_fields {
        if bool::arbitrary(&mut u).unwrap_or_default() {
            object.insert((*field).to_string(), field_value(field, &mut u));
        }
    }
    for idx in 0..u.int_in_range::<usize>(0..=4).unwrap_or_default() {
        let key = bounded_string(&mut u).unwrap_or_else(|_| format!("extra_{idx}"));
        let value = arbitrary_json_value(&mut u, 0).unwrap_or(Value::Null);
        object.insert(key, value);
    }
    Value::Object(object)
}

fn send_message_arguments(bytes: &[u8]) -> Value {
    structured_arguments_from_bytes(
        bytes,
        &["project_key", "sender_name", "to", "subject", "body_md"],
        &[
            "cc",
            "bcc",
            "attachment_paths",
            "convert_images",
            "importance",
            "ack_required",
            "thread_id",
            "topic",
            "broadcast",
            "auto_contact_if_blocked",
            "sender_token",
        ],
    )
}

fn file_reservation_paths_arguments(bytes: &[u8]) -> Value {
    structured_arguments_from_bytes(
        bytes,
        &["project_key", "agent_name", "paths"],
        &["ttl_seconds", "exclusive", "reason"],
    )
}

fn search_messages_arguments(bytes: &[u8]) -> Value {
    structured_arguments_from_bytes(
        bytes,
        &["project_key", "query"],
        &[
            "limit",
            "offset",
            "cursor",
            "ranking",
            "sender",
            "from_agent",
            "sender_name",
            "importance",
            "thread_id",
            "date_start",
            "date_end",
            "date_from",
            "date_to",
            "after",
            "before",
            "since",
            "until",
            "explain",
            "include_body_md",
        ],
    )
}

fn assert_no_internal_panic_marker(text: &str) -> Result<(), TestCaseError> {
    for marker in [
        "panicked at",
        "stack backtrace",
        "called `Option::unwrap()`",
        "called `Result::unwrap()`",
    ] {
        prop_assert!(
            !text.contains(marker),
            "tool response leaked panic marker {marker:?}: {text}"
        );
    }
    Ok(())
}

fn assert_bounded_error(err: &fastmcp::McpError) -> Result<(), TestCaseError> {
    prop_assert!(
        err.message.len() <= 16_384,
        "error message exceeded bound: {} bytes",
        err.message.len()
    );
    assert_no_internal_panic_marker(&err.message)?;
    if let Some(data) = &err.data {
        let data_text = data.to_string();
        prop_assert!(
            data_text.len() <= 65_536,
            "error data exceeded bound: {} bytes",
            data_text.len()
        );
        assert_no_internal_panic_marker(&data_text)?;
    }
    Ok(())
}

fn assert_bounded_content(content: &[Content]) -> Result<(), TestCaseError> {
    let mut total_text_bytes = 0usize;
    for item in content {
        if let Content::Text { text } = item {
            total_text_bytes = total_text_bytes.saturating_add(text.len());
            assert_no_internal_panic_marker(text)?;
        }
    }
    prop_assert!(
        total_text_bytes <= 65_536,
        "tool content exceeded bound: {total_text_bytes} bytes"
    );
    Ok(())
}

fn assert_tool_input_decode_no_panic(
    tool_name: &str,
    arguments: Value,
) -> Result<(), TestCaseError> {
    let _lock = ENV_LOCK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let suffix = unique_suffix();
    let db_path = format!("/tmp/tools-input-proptest-{suffix}.sqlite3");
    let database_url = format!("sqlite://{db_path}");
    let storage_root = format!("/tmp/tools-input-proptest-storage-{suffix}");

    with_process_env_overrides_for_test(
        &[
            ("DATABASE_URL", database_url.as_str()),
            ("STORAGE_ROOT", storage_root.as_str()),
            ("AGENT_NAME_ENFORCEMENT_MODE", "coerce"),
        ],
        || {
            Config::reset_cached();
            let router = router_with_fuzzed_tools();
            let params = CallToolParams {
                name: tool_name.to_string(),
                arguments: Some(arguments),
                meta: None,
            };
            let result = catch_unwind(AssertUnwindSafe(|| {
                router.handle_tools_call(
                    &Cx::for_testing(),
                    1,
                    params,
                    &Budget::INFINITE,
                    SessionState::new(),
                    None,
                    None,
                    None,
                )
            }));

            let result = result.map_err(|payload| {
                TestCaseError::fail(format!(
                    "{tool_name} panicked while decoding input: {}",
                    panic_payload_to_string(payload)
                ))
            })?;

            match result {
                Ok(call_result) => assert_bounded_content(&call_result.content),
                Err(err) => assert_bounded_error(&err),
            }
        },
    )
}

proptest! {
    #![proptest_config(proptest_config())]

    #[test]
    fn send_message_input_deserialization_proptest_no_panic(
        bytes in prop::collection::vec(any::<u8>(), 0..512)
    ) {
        assert_tool_input_decode_no_panic("send_message", send_message_arguments(&bytes))?;
    }

    #[test]
    fn file_reservation_paths_input_deserialization_proptest_no_panic(
        bytes in prop::collection::vec(any::<u8>(), 0..512)
    ) {
        assert_tool_input_decode_no_panic(
            "file_reservation_paths",
            file_reservation_paths_arguments(&bytes),
        )?;
    }

    #[test]
    fn search_messages_input_deserialization_proptest_no_panic(
        bytes in prop::collection::vec(any::<u8>(), 0..512)
    ) {
        assert_tool_input_decode_no_panic("search_messages", search_messages_arguments(&bytes))?;
    }
}
