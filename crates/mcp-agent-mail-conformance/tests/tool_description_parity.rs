//! Compatibility checks for the supported tool inventory and input schemas.
//!
//! The historical fixture began as a Python snapshot, but Rust owns current
//! tool prose and may revise it freely. The fixture remains useful for proving
//! that supported tools and their legacy input shapes have not been removed.

use fastmcp::{Cx, ListToolsParams, McpContext, Tool};
use serde::Deserialize;
use serde_json::Value;
use std::collections::{BTreeMap, BTreeSet};
use std::sync::{Mutex, OnceLock};

/// Serialization guard for tests that instantiate temporary server instances.
fn env_lock() -> &'static Mutex<()> {
    static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
    LOCK.get_or_init(|| Mutex::new(()))
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
/// These window-management mutation tools have not been ported yet.
const PYTHON_ONLY_TOOLS: &[&str] = &["expire_window", "rename_window"];

/// Rust-native tools that do not have entries in the shared description fixture.
const TOOLS_WITHOUT_SHARED_DESCRIPTION_FIXTURE: &[&str] = &[
    "check_file_reservation_conflicts",
    "cleanup_pane_identities",
    "fetch_inbox_events",
    "get_message_delivery_receipt",
    "list_agents",
    "resolve_pane_identity",
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

    let mut config = mcp_agent_mail_core::Config::from_env();
    config.database_url = db_url;
    config.storage_root = storage;
    config.worktrees_enabled = true;
    config.tool_filter.enabled = false;
    let router = mcp_agent_mail_server::build_server(&config).into_router();
    let cx = Cx::for_testing();

    let tools_result = router
        .handle_tools_list(
            &McpContext::new(cx.clone(), 1),
            ListToolsParams::default(),
            None,
        )
        .expect("tools/list failed");

    tools_result.tools
}

fn description_matches_fixture(expected: &str, actual: &str) -> bool {
    actual == expected
        || actual
            .strip_prefix(expected)
            .is_some_and(|suffix| suffix.starts_with("\n\n"))
}

fn normalized_required(schema: &Value) -> BTreeSet<String> {
    schema
        .get("required")
        .and_then(Value::as_array)
        .map(|arr| {
            arr.iter()
                .filter_map(Value::as_str)
                .filter(|name| *name != "format")
                .map(ToString::to_string)
                .collect()
        })
        .unwrap_or_default()
}

fn normalized_property_names(schema: &Value) -> BTreeSet<String> {
    schema
        .get("properties")
        .and_then(Value::as_object)
        .map(|obj| {
            obj.keys()
                .filter(|name| name.as_str() != "format")
                .cloned()
                .collect()
        })
        .unwrap_or_default()
}

fn normalized_property_type(prop: &Value) -> Option<String> {
    if let Some(kind) = prop.get("type").and_then(Value::as_str) {
        return Some(kind.to_string());
    }

    let mut non_null: Vec<String> = prop
        .get("type")
        .and_then(Value::as_array)
        .map(|arr| {
            arr.iter()
                .filter_map(Value::as_str)
                .filter(|kind| *kind != "null")
                .map(ToString::to_string)
                .collect()
        })
        .unwrap_or_default();
    non_null.extend(
        prop.get("anyOf")
            .and_then(Value::as_array)
            .map(|arr| {
                arr.iter()
                    .filter_map(|branch| branch.get("type").and_then(Value::as_str))
                    .filter(|kind| *kind != "null")
                    .map(ToString::to_string)
                    .collect::<Vec<String>>()
            })
            .unwrap_or_default(),
    );
    if non_null.is_empty() {
        return None;
    }
    non_null.sort_unstable();
    non_null.dedup();
    Some(non_null.join("|"))
}

#[test]
fn normalized_property_type_handles_nullable_type_arrays() {
    let nullable = serde_json::json!({"type": ["string", "null"]});
    assert_eq!(
        normalized_property_type(&nullable).as_deref(),
        Some("string")
    );
}

/// Compare the supported input-schema contract.
fn compare_input_schemas(tool_name: &str, expected: &Value, actual: &Value) -> Vec<String> {
    let mut errors = Vec::new();

    // Compare required arrays
    let expected_required = normalized_required(expected);
    let actual_required = normalized_required(actual);

    let relaxed: Vec<_> = expected_required.difference(&actual_required).collect();
    let newly_required: Vec<_> = actual_required.difference(&expected_required).collect();
    if !newly_required.is_empty() {
        errors.push(format!(
            "[{tool_name}] new required inputs break existing clients: {newly_required:?}"
        ));
    } else if !relaxed.is_empty() {
        eprintln!(
            "[{tool_name}] note: formerly required inputs are now optional (ok): {relaxed:?}"
        );
    }

    // Compare property names
    let expected_props = normalized_property_names(expected);
    let actual_props = normalized_property_names(actual);

    let missing_props: Vec<_> = expected_props.difference(&actual_props).collect();
    let extra_props: Vec<_> = actual_props.difference(&expected_props).collect();
    // Only flag missing properties as errors (backwards compatibility).
    // Extra properties are allowed — the Rust implementation may extend
    // beyond the Python baseline (e.g. Search V3 filter parameters).
    if !missing_props.is_empty() {
        errors.push(format!(
            "[{tool_name}] property mismatch: missing={missing_props:?}, extra={extra_props:?}"
        ));
    } else if !extra_props.is_empty() {
        eprintln!("[{tool_name}] note: extra properties (ok): {extra_props:?}");
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
                let exp_type = normalized_property_type(exp_prop);
                let act_type = normalized_property_type(act_prop);
                if exp_type != act_type {
                    errors.push(format!(
                        "[{tool_name}].{prop_name} type mismatch: expected={:?}, actual={:?}",
                        exp_type, act_type
                    ));
                }
            }
        }
    }

    errors
}

// ──────────────────────────────────────────────────────────────────────────────
// Tests
// ──────────────────────────────────────────────────────────────────────────────

/// Every supported fixture tool remains registered with a useful Rust-owned
/// description. Historical wording is diagnostic only, not product authority.
#[test]
fn supported_tools_have_rust_owned_descriptions() {
    let _lock = env_lock().lock().unwrap_or_else(|e| e.into_inner());

    let fixture = load_fixture();
    let rust_tools = get_rust_tools();

    // Build lookup maps
    let fixture_by_name: BTreeMap<&str, &FixtureTool> =
        fixture.tools.iter().map(|t| (t.name.as_str(), t)).collect();
    let rust_by_name: BTreeMap<String, &Tool> =
        rust_tools.iter().map(|t| (t.name.clone(), t)).collect();

    let mut passed = 0u32;
    let mut failed = 0u32;
    let mut failures: Vec<String> = Vec::new();

    // Check each Python tool that should exist in Rust
    for fixture_tool in &fixture.tools {
        if PYTHON_ONLY_TOOLS.contains(&fixture_tool.name.as_str()) {
            eprintln!("SKIP: {} (Python-only)", fixture_tool.name);
            continue;
        }

        eprint!("Checking tool: {}... ", fixture_tool.name);

        let Some(rust_tool) = rust_by_name.get(&fixture_tool.name) else {
            eprintln!("FAIL: missing in Rust");
            failed += 1;
            failures.push(format!(
                "[{}] MISSING: tool not registered in Rust server",
                fixture_tool.name
            ));
            continue;
        };

        let rust_desc = rust_tool.description.as_deref().unwrap_or("");
        let historical_desc = &fixture_tool.description;

        if rust_desc.trim().is_empty() {
            eprintln!("FAIL: empty Rust description");
            failures.push(format!("[{}] EMPTY DESCRIPTION", fixture_tool.name));
            failed += 1;
        } else {
            if description_matches_fixture(historical_desc, rust_desc) {
                eprintln!("PASS");
            } else {
                eprintln!("PASS (Rust wording revised)");
            }
            passed += 1;
        }
    }

    // Check for extra Rust tools not in Python
    let fixture_names: BTreeSet<&str> = fixture_by_name.keys().copied().collect();
    for rust_name in rust_by_name.keys() {
        if !fixture_names.contains(rust_name.as_str()) {
            if TOOLS_WITHOUT_SHARED_DESCRIPTION_FIXTURE.contains(&rust_name.as_str()) {
                eprintln!("RUST-NATIVE: {} (not in Python fixture)", rust_name);
                passed += 1;
                continue;
            }
            eprintln!("EXTRA: {} (unexpected Rust-only tool)", rust_name);
            failures.push(format!(
                "[{rust_name}] EXTRA: registered in Rust but not declared Rust-native"
            ));
            failed += 1;
        }
    }

    let total = passed + failed;
    eprintln!("\nSupported tool descriptions: {passed}/{total} tools passed");

    if !failures.is_empty() {
        let failure_report = failures.join("\n\n");
        panic!("Supported tool description check failed ({failed} failures):\n\n{failure_report}");
    }
}

/// Test that all shared tools have matching inputSchema property names and required arrays.
#[test]
fn tool_input_schemas_preserve_supported_compatibility() {
    let _lock = env_lock().lock().unwrap_or_else(|e| e.into_inner());

    let fixture = load_fixture();
    let rust_tools = get_rust_tools();

    let rust_by_name: BTreeMap<String, &Tool> =
        rust_tools.iter().map(|t| (t.name.clone(), t)).collect();

    let mut all_errors: Vec<String> = Vec::new();
    let mut passed = 0u32;
    let mut checked = 0u32;

    for fixture_tool in &fixture.tools {
        if PYTHON_ONLY_TOOLS.contains(&fixture_tool.name.as_str()) {
            continue;
        }

        let Some(rust_tool) = rust_by_name.get(&fixture_tool.name) else {
            continue; // Missing tools are caught by the description test
        };

        checked += 1;
        eprint!("Checking schema: {}... ", fixture_tool.name);

        let errors = compare_input_schemas(
            &fixture_tool.name,
            &fixture_tool.input_schema,
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
        fixture.tools.len() >= 37,
        "Fixture should have at least 37 tools, got {}",
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

#[test]
fn description_change_classifier_distinguishes_append_only_from_revised_wording() {
    let fixture = "alpha beta gamma";

    assert!(description_matches_fixture(fixture, fixture));
    assert!(description_matches_fixture(
        fixture,
        "alpha beta gamma\n\nRust-only extension"
    ));
    assert!(!description_matches_fixture(fixture, "alpha beta delta"));
    assert!(!description_matches_fixture(
        fixture,
        "alpha beta gamma Rust-only extension"
    ));
}

#[test]
fn schema_compatibility_allows_relaxed_requirements_but_rejects_new_ones() {
    let expected = serde_json::json!({
        "type": "object",
        "required": ["project_key"],
        "properties": {
            "project_key": {"type": "string", "description": "legacy wording"}
        }
    });
    let relaxed = serde_json::json!({
        "type": "object",
        "required": [],
        "properties": {
            "project_key": {"type": "string", "description": "clearer Rust wording"},
            "include_health": {"type": "boolean"}
        }
    });
    assert!(
        compare_input_schemas("example", &expected, &relaxed).is_empty(),
        "optionalizing an input, adding an optional input, and improving prose are compatible"
    );

    let newly_required = serde_json::json!({
        "type": "object",
        "required": ["project_key", "include_health"],
        "properties": {
            "project_key": {"type": "string"},
            "include_health": {"type": "boolean"}
        }
    });
    assert!(
        compare_input_schemas("example", &expected, &newly_required)
            .iter()
            .any(|error| error.contains("new required inputs")),
        "a new mandatory input would break existing clients"
    );
}

/// Verify the Rust tool count matches expected shared tool count.
#[test]
fn rust_tool_count_matches_expected() {
    let _lock = env_lock().lock().unwrap_or_else(|e| e.into_inner());

    let rust_tools = get_rust_tools();
    let fixture = load_fixture();

    let supported_fixture_count = fixture
        .tools
        .iter()
        .filter(|t| !PYTHON_ONLY_TOOLS.contains(&t.name.as_str()))
        .count();

    let expected_rust_count =
        supported_fixture_count + TOOLS_WITHOUT_SHARED_DESCRIPTION_FIXTURE.len();

    // Rust should have the supported shared-contract tools plus declared
    // Rust-native tools. This is an inventory check, not a design-authority
    // handoff to the legacy implementation.
    assert_eq!(
        rust_tools.len(),
        expected_rust_count,
        "Rust has {} tools, expected {} ({} supported fixture tools + {} Rust-native tools without shared descriptions, excluding {} Python-only). \
         Rust tools: {:?}",
        rust_tools.len(),
        expected_rust_count,
        supported_fixture_count,
        TOOLS_WITHOUT_SHARED_DESCRIPTION_FIXTURE.len(),
        PYTHON_ONLY_TOOLS.len(),
        rust_tools.iter().map(|t| &t.name).collect::<Vec<_>>()
    );

    let rust_names: BTreeSet<&str> = rust_tools.iter().map(|tool| tool.name.as_str()).collect();
    for rust_native_tool in TOOLS_WITHOUT_SHARED_DESCRIPTION_FIXTURE {
        assert!(
            rust_names.contains(rust_native_tool),
            "declared Rust-native tool is not registered: {rust_native_tool}"
        );
    }
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

/// Identity cluster: registration, lifecycle, lookup, and roster tools.
#[test]
fn cluster_identity_descriptions() {
    let _lock = env_lock().lock().unwrap_or_else(|e| e.into_inner());
    check_cluster_descriptions(&[
        "register_agent",
        "create_agent_identity",
        "list_window_identities",
        "sweep_stale_agents",
        "retire_agent",
        "unretire_agent",
        "deregister_agent",
        "whois",
    ]);
}

/// Messaging cluster: send_message, reply_message, fetch_inbox, fetch_inbox_events,
/// get_message_delivery_receipt, mark_message_read, acknowledge_message
#[test]
fn cluster_messaging_descriptions() {
    let _lock = env_lock().lock().unwrap_or_else(|e| e.into_inner());
    check_cluster_descriptions(&[
        "send_message",
        "reply_message",
        "fetch_inbox",
        "fetch_topic",
        "fetch_inbox_events",
        "get_message_delivery_receipt",
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

/// Search cluster descriptions.
#[test]
fn cluster_search_descriptions() {
    let _lock = env_lock().lock().unwrap_or_else(|e| e.into_inner());
    check_cluster_descriptions(&[
        "search_messages",
        "summarize_thread",
        "summarize_recent",
        "fetch_summary",
    ]);
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

    let fixture_by_name: BTreeMap<&str, &FixtureTool> =
        fixture.tools.iter().map(|t| (t.name.as_str(), t)).collect();
    let rust_by_name: BTreeMap<String, &Tool> =
        rust_tools.iter().map(|t| (t.name.clone(), t)).collect();

    let mut failures: Vec<String> = Vec::new();

    for &name in tool_names {
        // Rust-native tools intentionally have no Python description baseline.
        // Registration is asserted by the inventory test above instead.
        if TOOLS_WITHOUT_SHARED_DESCRIPTION_FIXTURE.contains(&name) {
            continue;
        }

        if !fixture_by_name.contains_key(name) {
            failures.push(format!("[{name}] not found in supported fixture"));
            continue;
        }
        let Some(rust_tool) = rust_by_name.get(name) else {
            failures.push(format!("[{name}] not registered in Rust server"));
            continue;
        };

        if rust_tool
            .description
            .as_deref()
            .is_none_or(|description| description.trim().is_empty())
        {
            failures.push(format!("[{name}] Rust description is empty"));
        }
    }

    if !failures.is_empty() {
        panic!(
            "Cluster description coverage failures:\n\n{}",
            failures.join("\n\n")
        );
    }
}
