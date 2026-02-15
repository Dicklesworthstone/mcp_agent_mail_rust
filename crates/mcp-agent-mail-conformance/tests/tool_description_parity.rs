// Note: unsafe required for env::set_var in Rust 2024
#![allow(unsafe_code)]

//! Conformance tests verifying Rust tool descriptions match the Python reference
//! fixture character-for-character. The fixture was generated from the Python
//! MCP server and lives at `tests/conformance/fixtures/tool_descriptions.json`.

use fastmcp::{Cx, ListToolsParams, Tool};
use serde::Deserialize;
use serde_json::Value;
use std::collections::{BTreeMap, BTreeSet};
use std::sync::{Mutex, OnceLock};

/// Serialization guard for env mutations across tests.
fn env_lock() -> &'static Mutex<()> {
    static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
    LOCK.get_or_init(|| Mutex::new(()))
}

struct EnvVarGuard {
    previous: Vec<(String, Option<String>)>,
}

impl EnvVarGuard {
    fn set(vars: &[(&str, &str)]) -> Self {
        let mut previous = Vec::new();
        for (key, value) in vars {
            let old = std::env::var(*key).ok();
            previous.push(((*key).to_string(), old));
            unsafe {
                std::env::set_var(key, value);
            }
        }
        mcp_agent_mail_core::Config::reset_cached();
        Self { previous }
    }
}

impl Drop for EnvVarGuard {
    fn drop(&mut self) {
        for (key, value) in self.previous.drain(..) {
            match value {
                Some(v) => unsafe {
                    std::env::set_var(&key, v);
                },
                None => unsafe {
                    std::env::remove_var(&key);
                },
            }
        }
        mcp_agent_mail_core::Config::reset_cached();
    }
}

/// A tool entry from the Python reference fixture.
#[derive(Debug, Deserialize)]
struct FixtureTool {
    name: String,
    description: String,
    #[serde(rename = "inputSchema")]
    input_schema: Value,
}

/// Root structure of tool_descriptions.json fixture.
#[derive(Debug, Deserialize)]
struct ToolDescriptionsFixture {
    tools: Vec<FixtureTool>,
}

/// Python-only tools that exist in the Python server but not in Rust.
/// These are window-management tools not yet ported.
const PYTHON_ONLY_TOOLS: &[&str] = &[
    "expire_window",
    "fetch_summary",
    "fetch_topic",
    "list_window_identities",
    "rename_window",
    "summarize_recent",
];

/// Load the Python reference fixture.
fn load_fixture() -> ToolDescriptionsFixture {
    let fixture_path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../tests/conformance/fixtures/tool_descriptions.json");
    let content = std::fs::read_to_string(&fixture_path)
        .unwrap_or_else(|e| panic!("Failed to read fixture at {}: {e}", fixture_path.display()));
    serde_json::from_str(&content).unwrap_or_else(|e| panic!("Failed to parse fixture JSON: {e}"))
}

/// Build a Rust MCP server with all features enabled and return the tool list.
fn get_rust_tools() -> Vec<Tool> {
    let tmp = tempfile::TempDir::new().expect("tempdir");
    let db_path = tmp.path().join("desc-parity.sqlite3");
    let db_url = format!("sqlite://{}", db_path.display());
    let storage = tmp.path().join("archive");

    let _env_guard = EnvVarGuard::set(&[
        ("DATABASE_URL", &db_url),
        ("STORAGE_ROOT", storage.to_str().unwrap_or_default()),
        ("WORKTREES_ENABLED", "1"),
        ("PRODUCTS_ENABLED", "1"),
        ("TOOLS_FILTER_ENABLED", "0"),
        ("AGENT_NAME_ENFORCEMENT_MODE", "coerce"),
    ]);

    let config = mcp_agent_mail_core::Config::from_env();
    let router = mcp_agent_mail_server::build_server(&config).into_router();
    let cx = Cx::for_testing();

    let tools_result = router
        .handle_tools_list(&cx, ListToolsParams::default(), None)
        .expect("tools/list failed");

    tools_result.tools
}

/// Find the first character position where two strings differ, with context.
fn diff_position(expected: &str, actual: &str) -> Option<(usize, String)> {
    let expected_chars: Vec<char> = expected.chars().collect();
    let actual_chars: Vec<char> = actual.chars().collect();

    for (i, (e, a)) in expected_chars.iter().zip(actual_chars.iter()).enumerate() {
        if e != a {
            let context_start = i.saturating_sub(30);
            let context_end_e = (i + 30).min(expected_chars.len());
            let context_end_a = (i + 30).min(actual_chars.len());
            let expected_ctx: String = expected_chars[context_start..context_end_e]
                .iter()
                .collect();
            let actual_ctx: String = actual_chars[context_start..context_end_a].iter().collect();
            return Some((
                i,
                format!(
                    "char {i}: expected '{e}' got '{a}'\n  expected context: ...{expected_ctx}...\n  actual context:   ...{actual_ctx}..."
                ),
            ));
        }
    }

    if expected_chars.len() != actual_chars.len() {
        return Some((
            expected_chars.len().min(actual_chars.len()),
            format!(
                "length mismatch: expected {} chars, got {} chars",
                expected_chars.len(),
                actual_chars.len()
            ),
        ));
    }

    None
}

/// Compare inputSchema properties: property names, types, and required arrays.
fn compare_input_schemas(tool_name: &str, expected: &Value, actual: &Value) -> Vec<String> {
    let mut errors = Vec::new();

    // Compare required arrays
    let expected_required: BTreeSet<String> = expected
        .get("required")
        .and_then(|r| r.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|v| v.as_str().map(String::from))
                .collect()
        })
        .unwrap_or_default();
    let actual_required: BTreeSet<String> = actual
        .get("required")
        .and_then(|r| r.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|v| v.as_str().map(String::from))
                .collect()
        })
        .unwrap_or_default();

    if expected_required != actual_required {
        let missing: Vec<_> = expected_required.difference(&actual_required).collect();
        let extra: Vec<_> = actual_required.difference(&expected_required).collect();
        errors.push(format!(
            "[{tool_name}] required mismatch: missing={missing:?}, extra={extra:?}"
        ));
    }

    // Compare property names
    let expected_props: BTreeSet<String> = expected
        .get("properties")
        .and_then(|p| p.as_object())
        .map(|obj| obj.keys().cloned().collect())
        .unwrap_or_default();
    let actual_props: BTreeSet<String> = actual
        .get("properties")
        .and_then(|p| p.as_object())
        .map(|obj| obj.keys().cloned().collect())
        .unwrap_or_default();

    let missing_props: Vec<_> = expected_props.difference(&actual_props).collect();
    let extra_props: Vec<_> = actual_props.difference(&expected_props).collect();
    if !missing_props.is_empty() || !extra_props.is_empty() {
        errors.push(format!(
            "[{tool_name}] property mismatch: missing={missing_props:?}, extra={extra_props:?}"
        ));
    }

    // Compare property types for shared properties
    if let (Some(exp_obj), Some(act_obj)) = (
        expected.get("properties").and_then(|p| p.as_object()),
        actual.get("properties").and_then(|p| p.as_object()),
    ) {
        for prop_name in expected_props.intersection(&actual_props) {
            if let (Some(exp_prop), Some(act_prop)) =
                (exp_obj.get(prop_name), act_obj.get(prop_name))
            {
                // Compare type field
                let exp_type = exp_prop.get("type");
                let act_type = act_prop.get("type");
                if exp_type != act_type {
                    // Check for anyOf pattern (Python uses anyOf for Optional types)
                    let exp_any_of = exp_prop.get("anyOf");
                    let act_any_of = act_prop.get("anyOf");
                    if exp_any_of != act_any_of && exp_type != act_type {
                        errors.push(format!(
                            "[{tool_name}].{prop_name} type mismatch: expected={}, actual={}",
                            exp_type.unwrap_or(&Value::Null),
                            act_type.unwrap_or(&Value::Null)
                        ));
                    }
                }
            }
        }
    }

    errors
}

// ──────────────────────────────────────────────────────────────────────────────
// Tests
// ──────────────────────────────────────────────────────────────────────────────

/// Master test: load fixture, build Rust server, compare every tool description.
#[test]
fn tool_descriptions_match_python_fixture() {
    let _lock = env_lock().lock().unwrap_or_else(|e| e.into_inner());

    let fixture = load_fixture();
    let rust_tools = get_rust_tools();

    // Build lookup maps
    let python_by_name: BTreeMap<&str, &FixtureTool> =
        fixture.tools.iter().map(|t| (t.name.as_str(), t)).collect();
    let rust_by_name: BTreeMap<String, &Tool> =
        rust_tools.iter().map(|t| (t.name.clone(), t)).collect();

    let mut passed = 0u32;
    let mut failed = 0u32;
    let mut failures: Vec<String> = Vec::new();

    // Check each Python tool that should exist in Rust
    for py_tool in &fixture.tools {
        if PYTHON_ONLY_TOOLS.contains(&py_tool.name.as_str()) {
            eprintln!("SKIP: {} (Python-only)", py_tool.name);
            continue;
        }

        eprint!("Checking tool: {}... ", py_tool.name);

        let Some(rust_tool) = rust_by_name.get(&py_tool.name) else {
            eprintln!("FAIL: missing in Rust");
            failed += 1;
            failures.push(format!(
                "[{}] MISSING: tool not registered in Rust server",
                py_tool.name
            ));
            continue;
        };

        let rust_desc = rust_tool.description.as_deref().unwrap_or("");
        let py_desc = &py_tool.description;

        if rust_desc == py_desc {
            eprintln!("PASS");
            passed += 1;
        } else {
            if let Some((_pos, detail)) = diff_position(py_desc, rust_desc) {
                eprintln!("FAIL: {detail}");
                failures.push(format!(
                    "[{}] DESCRIPTION MISMATCH:\n  {detail}\n  expected ({} chars): {}\n  actual   ({} chars): {}",
                    py_tool.name,
                    py_desc.len(),
                    &py_desc[..py_desc.len().min(200)],
                    rust_desc.len(),
                    &rust_desc[..rust_desc.len().min(200)]
                ));
            } else {
                eprintln!("FAIL: unknown diff");
                failures.push(format!(
                    "[{}] DESCRIPTION MISMATCH (unknown diff)",
                    py_tool.name
                ));
            }
            failed += 1;
        }
    }

    // Check for extra Rust tools not in Python
    let python_names: BTreeSet<&str> = python_by_name.keys().copied().collect();
    for rust_name in rust_by_name.keys() {
        if !python_names.contains(rust_name.as_str()) {
            eprintln!("EXTRA: {} (Rust-only, not in Python fixture)", rust_name);
            failures.push(format!(
                "[{rust_name}] EXTRA: registered in Rust but not in Python fixture"
            ));
            failed += 1;
        }
    }

    let total = passed + failed;
    eprintln!("\nTool description parity: {passed}/{total} tools passed");

    if !failures.is_empty() {
        let failure_report = failures.join("\n\n");
        panic!("Tool description parity check failed ({failed} failures):\n\n{failure_report}");
    }
}

/// Test that all shared tools have matching inputSchema property names and required arrays.
#[test]
fn tool_input_schemas_match_python_fixture() {
    let _lock = env_lock().lock().unwrap_or_else(|e| e.into_inner());

    let fixture = load_fixture();
    let rust_tools = get_rust_tools();

    let rust_by_name: BTreeMap<String, &Tool> =
        rust_tools.iter().map(|t| (t.name.clone(), t)).collect();

    let mut all_errors: Vec<String> = Vec::new();
    let mut passed = 0u32;
    let mut checked = 0u32;

    for py_tool in &fixture.tools {
        if PYTHON_ONLY_TOOLS.contains(&py_tool.name.as_str()) {
            continue;
        }

        let Some(rust_tool) = rust_by_name.get(&py_tool.name) else {
            continue; // Missing tools are caught by the description test
        };

        checked += 1;
        eprint!("Checking schema: {}... ", py_tool.name);

        let errors = compare_input_schemas(
            &py_tool.name,
            &py_tool.input_schema,
            &rust_tool.input_schema,
        );

        if errors.is_empty() {
            eprintln!("PASS");
            passed += 1;
        } else {
            eprintln!("FAIL ({} issues)", errors.len());
            all_errors.extend(errors);
        }
    }

    eprintln!("\nSchema parity: {passed}/{checked} tools passed");

    if !all_errors.is_empty() {
        let report = all_errors.join("\n");
        panic!(
            "Input schema parity check failed ({} issues):\n\n{report}",
            all_errors.len()
        );
    }
}

/// Verify the fixture itself is well-formed and non-empty.
#[test]
fn fixture_is_valid() {
    let fixture = load_fixture();
    assert!(
        fixture.tools.len() >= 34,
        "Fixture should have at least 34 tools, got {}",
        fixture.tools.len()
    );

    // Every tool should have a name
    for tool in &fixture.tools {
        assert!(!tool.name.is_empty(), "Tool name must not be empty");
        assert!(
            tool.input_schema.get("properties").is_some()
                || tool.input_schema.get("type").is_some(),
            "Tool {} must have inputSchema with properties or type",
            tool.name
        );
    }

    // No duplicate tool names
    let names: BTreeSet<&str> = fixture.tools.iter().map(|t| t.name.as_str()).collect();
    assert_eq!(
        names.len(),
        fixture.tools.len(),
        "Fixture contains duplicate tool names"
    );
}

/// Verify the Rust tool count matches expected shared tool count.
#[test]
fn rust_tool_count_matches_expected() {
    let _lock = env_lock().lock().unwrap_or_else(|e| e.into_inner());

    let rust_tools = get_rust_tools();
    let fixture = load_fixture();

    let shared_python_count = fixture
        .tools
        .iter()
        .filter(|t| !PYTHON_ONLY_TOOLS.contains(&t.name.as_str()))
        .count();

    // Rust should have exactly the shared tools (no more, no less)
    assert_eq!(
        rust_tools.len(),
        shared_python_count,
        "Rust has {} tools, Python has {} shared tools (excluding {} Python-only). \
         Rust tools: {:?}",
        rust_tools.len(),
        shared_python_count,
        PYTHON_ONLY_TOOLS.len(),
        rust_tools.iter().map(|t| &t.name).collect::<Vec<_>>()
    );
}

// ──────────────────────────────────────────────────────────────────────────────
// Per-cluster tests for granular reporting
// ──────────────────────────────────────────────────────────────────────────────

/// Infrastructure cluster: health_check, ensure_project, install_precommit_guard, uninstall_precommit_guard
#[test]
fn cluster_infrastructure_descriptions() {
    let _lock = env_lock().lock().unwrap_or_else(|e| e.into_inner());
    check_cluster_descriptions(&[
        "health_check",
        "ensure_project",
        "install_precommit_guard",
        "uninstall_precommit_guard",
    ]);
}

/// Identity cluster: register_agent, create_agent_identity, whois
#[test]
fn cluster_identity_descriptions() {
    let _lock = env_lock().lock().unwrap_or_else(|e| e.into_inner());
    check_cluster_descriptions(&["register_agent", "create_agent_identity", "whois"]);
}

/// Messaging cluster: send_message, reply_message, fetch_inbox, mark_message_read, acknowledge_message
#[test]
fn cluster_messaging_descriptions() {
    let _lock = env_lock().lock().unwrap_or_else(|e| e.into_inner());
    check_cluster_descriptions(&[
        "send_message",
        "reply_message",
        "fetch_inbox",
        "mark_message_read",
        "acknowledge_message",
    ]);
}

/// Contacts cluster: request_contact, respond_contact, list_contacts, set_contact_policy
#[test]
fn cluster_contacts_descriptions() {
    let _lock = env_lock().lock().unwrap_or_else(|e| e.into_inner());
    check_cluster_descriptions(&[
        "request_contact",
        "respond_contact",
        "list_contacts",
        "set_contact_policy",
    ]);
}

/// File reservations cluster: file_reservation_paths, release_file_reservations, renew_file_reservations, force_release_file_reservation
#[test]
fn cluster_file_reservations_descriptions() {
    let _lock = env_lock().lock().unwrap_or_else(|e| e.into_inner());
    check_cluster_descriptions(&[
        "file_reservation_paths",
        "release_file_reservations",
        "renew_file_reservations",
        "force_release_file_reservation",
    ]);
}

/// Search cluster: search_messages, summarize_thread
#[test]
fn cluster_search_descriptions() {
    let _lock = env_lock().lock().unwrap_or_else(|e| e.into_inner());
    check_cluster_descriptions(&["search_messages", "summarize_thread"]);
}

/// Macros cluster: macro_start_session, macro_prepare_thread, macro_file_reservation_cycle, macro_contact_handshake
#[test]
fn cluster_macros_descriptions() {
    let _lock = env_lock().lock().unwrap_or_else(|e| e.into_inner());
    check_cluster_descriptions(&[
        "macro_start_session",
        "macro_prepare_thread",
        "macro_file_reservation_cycle",
        "macro_contact_handshake",
    ]);
}

/// Product bus cluster: ensure_product, products_link, search_messages_product, fetch_inbox_product, summarize_thread_product
#[test]
fn cluster_product_bus_descriptions() {
    let _lock = env_lock().lock().unwrap_or_else(|e| e.into_inner());
    check_cluster_descriptions(&[
        "ensure_product",
        "products_link",
        "search_messages_product",
        "fetch_inbox_product",
        "summarize_thread_product",
    ]);
}

/// Build slots cluster: acquire_build_slot, renew_build_slot, release_build_slot
#[test]
fn cluster_build_slots_descriptions() {
    let _lock = env_lock().lock().unwrap_or_else(|e| e.into_inner());
    check_cluster_descriptions(&[
        "acquire_build_slot",
        "renew_build_slot",
        "release_build_slot",
    ]);
}

/// Helper: check tool descriptions for a set of tools in one cluster.
fn check_cluster_descriptions(tool_names: &[&str]) {
    let fixture = load_fixture();
    let rust_tools = get_rust_tools();

    let python_by_name: BTreeMap<&str, &FixtureTool> =
        fixture.tools.iter().map(|t| (t.name.as_str(), t)).collect();
    let rust_by_name: BTreeMap<String, &Tool> =
        rust_tools.iter().map(|t| (t.name.clone(), t)).collect();

    let mut failures: Vec<String> = Vec::new();

    for &name in tool_names {
        let Some(py_tool) = python_by_name.get(name) else {
            failures.push(format!("[{name}] not found in Python fixture"));
            continue;
        };
        let Some(rust_tool) = rust_by_name.get(name) else {
            failures.push(format!("[{name}] not registered in Rust server"));
            continue;
        };

        let rust_desc = rust_tool.description.as_deref().unwrap_or("");
        if rust_desc != py_tool.description {
            if let Some((_pos, detail)) = diff_position(&py_tool.description, rust_desc) {
                failures.push(format!(
                    "[{name}] {detail}\n  Python ({} chars): {}\n  Rust   ({} chars): {}",
                    py_tool.description.len(),
                    &py_tool.description[..py_tool.description.len().min(150)],
                    rust_desc.len(),
                    &rust_desc[..rust_desc.len().min(150)]
                ));
            }
        }
    }

    if !failures.is_empty() {
        panic!(
            "Cluster description parity failures:\n\n{}",
            failures.join("\n\n")
        );
    }
}
