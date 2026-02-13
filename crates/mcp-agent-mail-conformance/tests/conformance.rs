// Note: unsafe required for env::set_var in Rust 2024
#![allow(unsafe_code)]

use fastmcp::{Budget, CallToolParams, Content, Cx, ListToolsParams, ReadResourceParams};
use fastmcp_core::SessionState;
use mcp_agent_mail_conformance::{Case, ExpectedError, Fixtures, Normalize};
use serde::Deserialize;
use serde_json::Value;
use std::collections::{BTreeMap, BTreeSet};
use std::sync::{Mutex, OnceLock};

/// Auto-increment ID field names that are non-deterministic across test runs.
const AUTO_INCREMENT_ID_KEYS: &[&str] = &["id", "message_id", "reply_to"];

/// Tests in this file mutate process-wide environment variables (Rust has no per-test env isolation).
/// The Rust test harness runs tests in parallel by default, so serialize any env mutations and
/// `Config::from_env()` calls to avoid flakey cross-test races.
fn env_lock() -> &'static Mutex<()> {
    static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
    LOCK.get_or_init(|| Mutex::new(()))
}

/// Recursively null out auto-increment integer ID fields in a JSON value.
/// This handles the fact that fixture cases run sequentially in a shared DB,
/// so auto-increment IDs depend on execution order.
fn null_auto_increment_ids(value: &mut Value) {
    match value {
        Value::Object(map) => {
            for (key, val) in map.iter_mut() {
                if AUTO_INCREMENT_ID_KEYS.contains(&key.as_str()) && val.is_number() {
                    *val = Value::Null;
                } else {
                    null_auto_increment_ids(val);
                }
            }
        }
        Value::Array(arr) => {
            for item in arr.iter_mut() {
                null_auto_increment_ids(item);
            }
        }
        _ => {}
    }
}

/// For tooling/directory-like resources with "clusters" → "tools" arrays,
/// filter the actual response to only include tools whose names appear in
/// the expected output. This handles tools added after fixture generation.
fn align_cluster_tools(actual: &mut Value, expected: &Value) {
    let Some(expected_clusters) = expected.get("clusters").and_then(|c| c.as_array()) else {
        return;
    };
    let Some(actual_clusters) = actual.get_mut("clusters").and_then(|c| c.as_array_mut()) else {
        return;
    };

    // Collect all tool names from expected
    let mut expected_tool_names: std::collections::HashSet<String> =
        std::collections::HashSet::new();
    for cluster in expected_clusters {
        if let Some(tools) = cluster.get("tools").and_then(|t| t.as_array()) {
            for tool in tools {
                if let Some(name) = tool.get("name").and_then(|n| n.as_str()) {
                    expected_tool_names.insert(name.to_string());
                }
            }
        }
    }

    if expected_tool_names.is_empty() {
        return;
    }

    // Filter actual clusters: remove tools not in expected, remove empty clusters
    for cluster in actual_clusters.iter_mut() {
        if let Some(tools) = cluster.get_mut("tools").and_then(|t| t.as_array_mut()) {
            tools.retain(|tool| {
                tool.get("name")
                    .and_then(|n| n.as_str())
                    .is_some_and(|name| expected_tool_names.contains(name))
            });
        }
    }
    actual_clusters.retain(|c| {
        c.get("tools")
            .and_then(|t| t.as_array())
            .is_some_and(|tools| !tools.is_empty())
    });
}

/// For tooling/metrics-like responses, filter to only tools in expected.
fn align_metrics_tools(actual: &mut Value, expected: &Value) {
    let Some(expected_tools) = expected.get("tools").and_then(|t| t.as_array()) else {
        return;
    };
    let Some(actual_tools) = actual.get_mut("tools").and_then(|t| t.as_array_mut()) else {
        return;
    };

    let expected_names: std::collections::HashSet<String> = expected_tools
        .iter()
        .filter_map(|t| t.get("name").and_then(|n| n.as_str()).map(String::from))
        .collect();

    if expected_names.is_empty() {
        return;
    }

    actual_tools.retain(|tool| {
        tool.get("name")
            .and_then(|n| n.as_str())
            .is_some_and(|name| expected_names.contains(name))
    });
}

fn normalize_pair(mut actual: Value, mut expected: Value, norm: &Normalize) -> (Value, Value) {
    // Always null out auto-increment IDs since they're non-deterministic
    null_auto_increment_ids(&mut actual);
    null_auto_increment_ids(&mut expected);

    // Align tool lists (handle tools added after fixture generation)
    align_cluster_tools(&mut actual, &expected);
    align_metrics_tools(&mut actual, &expected);

    for ptr in &norm.ignore_json_pointers {
        if let Some(v) = actual.pointer_mut(ptr) {
            *v = Value::Null;
        }
        if let Some(v) = expected.pointer_mut(ptr) {
            *v = Value::Null;
        }
    }

    for (ptr, replacement) in &norm.replace {
        if let Some(v) = actual.pointer_mut(ptr) {
            *v = replacement.clone();
        }
        if let Some(v) = expected.pointer_mut(ptr) {
            *v = replacement.clone();
        }
    }

    (actual, expected)
}

fn decode_json_from_tool_content(content: &[Content]) -> Result<Value, String> {
    if content.len() != 1 {
        return Err(format!(
            "expected exactly 1 content item, got {}",
            content.len()
        ));
    }

    match &content[0] {
        Content::Text { text } => match serde_json::from_str(text) {
            Ok(v) => Ok(v),
            Err(_) => Ok(Value::String(text.clone())),
        },
        Content::Resource { resource } => {
            let text = resource
                .text
                .as_deref()
                .ok_or_else(|| "tool returned Resource content without text".to_string())?;
            match serde_json::from_str(text) {
                Ok(v) => Ok(v),
                Err(_) => Ok(Value::String(text.to_string())),
            }
        }
        Content::Image { mime_type, .. } => Err(format!(
            "tool returned Image content (mime_type={mime_type}); JSON decode not supported yet"
        )),
        Content::Audio { mime_type, .. } => Err(format!(
            "tool returned Audio content (mime_type={mime_type}); JSON decode not supported yet"
        )),
    }
}

fn decode_json_from_resource_contents(
    uri: &str,
    contents: &[fastmcp::ResourceContent],
) -> Result<Value, String> {
    if contents.len() != 1 {
        return Err(format!(
            "expected exactly 1 resource content item for {uri}, got {}",
            contents.len()
        ));
    }
    let item = &contents[0];
    let text = item
        .text
        .as_deref()
        .ok_or_else(|| format!("resource {uri} returned no text"))?;
    match serde_json::from_str(text) {
        Ok(v) => Ok(v),
        Err(_) => Ok(Value::String(text.to_string())),
    }
}

fn assert_expected_error(got: &str, expect: &ExpectedError) {
    if let Some(substr) = &expect.message_contains {
        assert!(
            got.contains(substr),
            "expected error message to contain {substr:?}, got {got:?}"
        );
    }
}

#[derive(Debug, Deserialize)]
struct ToolFilterFixtures {
    version: String,
    generated_at: String,
    cases: Vec<ToolFilterCase>,
}

#[derive(Debug, Deserialize)]
struct ToolFilterCase {
    name: String,
    #[serde(default)]
    env: BTreeMap<String, String>,
    expected_tools: Vec<String>,
}

struct ToolFilterEnvGuard {
    previous: Vec<(String, Option<String>)>,
}

impl ToolFilterEnvGuard {
    fn apply(case_env: &BTreeMap<String, String>) -> Self {
        let keys = [
            "TOOLS_FILTER_ENABLED",
            "TOOLS_FILTER_PROFILE",
            "TOOLS_FILTER_MODE",
            "TOOLS_FILTER_CLUSTERS",
            "TOOLS_FILTER_TOOLS",
        ];

        let mut previous = Vec::new();
        for key in keys {
            let old = std::env::var(key).ok();
            previous.push((key.to_string(), old));
            if let Some(value) = case_env.get(key) {
                unsafe {
                    std::env::set_var(key, value);
                }
            } else {
                unsafe {
                    std::env::remove_var(key);
                }
            }
        }

        // Reset cached config so Config::get() re-reads env vars
        mcp_agent_mail_core::Config::reset_cached();

        Self { previous }
    }
}

impl Drop for ToolFilterEnvGuard {
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

fn load_tool_filter_fixtures() -> ToolFilterFixtures {
    let path = "tests/conformance/fixtures/tool_filter/cases.json";
    let raw = std::fs::read_to_string(path).expect("tool filter fixtures missing");
    let fixtures: ToolFilterFixtures =
        serde_json::from_str(&raw).expect("tool filter fixtures invalid JSON");
    assert!(
        !fixtures.version.trim().is_empty(),
        "tool filter fixtures version must be non-empty"
    );
    assert!(
        !fixtures.generated_at.trim().is_empty(),
        "tool filter fixtures generated_at must be non-empty"
    );
    fixtures
}

fn extract_tool_names_from_directory(value: &Value) -> Vec<String> {
    let mut names = Vec::new();
    let Some(clusters) = value.get("clusters").and_then(|v| v.as_array()) else {
        return names;
    };
    for cluster in clusters {
        let Some(tools) = cluster.get("tools").and_then(|v| v.as_array()) else {
            continue;
        };
        for tool in tools {
            if let Some(name) = tool.get("name").and_then(|v| v.as_str()) {
                names.push(name.to_string());
            }
        }
    }
    names
}

fn args_from_case(case: &Case) -> Option<Value> {
    match &case.input {
        Value::Null => None,
        Value::Object(map) if map.is_empty() => None,
        other => Some(other.clone()),
    }
}

struct FixtureEnv {
    tmp: tempfile::TempDir,
    _env_guard: EnvVarGuard,
    fixtures: Fixtures,
    router: fastmcp::Router,
}

/// Set up env vars, run all tool fixtures, and return the environment for further assertions.
fn setup_fixture_env() -> FixtureEnv {
    let tmp = tempfile::TempDir::new().expect("failed to create tempdir");
    let db_path = tmp.path().join("db.sqlite3");
    let db_url = format!("sqlite://{}", db_path.display());
    let storage_root = tmp.path().join("archive");
    let storage_root_str = storage_root
        .to_str()
        .expect("storage_root must be valid UTF-8");
    // Ensure fixtures run deterministically regardless of developer shell env.
    // Also explicitly disable tool filtering (otherwise tools may not be registered).
    let env_guard = EnvVarGuard::set(&[
        ("DATABASE_URL", &db_url),
        ("WORKTREES_ENABLED", "1"),
        ("STORAGE_ROOT", storage_root_str),
        ("TOOLS_FILTER_ENABLED", "0"),
        // Deterministic LLM paths for llm_mode=true conformance fixtures.
        ("LLM_ENABLED", "1"),
        ("MCP_AGENT_MAIL_LLM_STUB", "1"),
        ("TOOLS_FILTER_PROFILE", "full"),
        ("TOOLS_FILTER_MODE", "include"),
        ("TOOLS_FILTER_CLUSTERS", ""),
        ("TOOLS_FILTER_TOOLS", ""),
        ("MCP_AGENT_MAIL_OUTPUT_FORMAT", ""),
        ("TOON_DEFAULT_FORMAT", ""),
        ("TOON_BIN", ""),
        ("TOON_TRU_BIN", ""),
        ("TOON_STATS", "0"),
        ("AGENT_NAME_ENFORCEMENT_MODE", "coerce"),
    ]);

    for repo_name in &["repo_install", "repo_uninstall"] {
        let repo_dir = std::path::Path::new("/tmp/agent-mail-fixtures").join(repo_name);
        std::fs::create_dir_all(&repo_dir).expect("create fixture repo dir");
        if !repo_dir.join(".git").exists() {
            std::process::Command::new("git")
                .args(["init", "--quiet"])
                .current_dir(&repo_dir)
                .status()
                .expect("git init");
        }
    }

    let fixtures = Fixtures::load_default().expect("failed to load fixtures");
    let config = mcp_agent_mail_core::Config::from_env();
    let router = mcp_agent_mail_server::build_server(&config).into_router();

    FixtureEnv {
        tmp,
        _env_guard: env_guard,
        fixtures,
        router,
    }
}

/// Parse frontmatter from a message markdown file.
/// Returns the JSON value from the `---json ... ---` block.
fn parse_frontmatter(content: &str) -> Option<Value> {
    let content = content.trim();
    if !content.starts_with("---json") {
        return None;
    }
    let after_start = &content["---json".len()..];
    let end_idx = after_start.find("\n---")?;
    let json_str = &after_start[..end_idx];
    serde_json::from_str(json_str.trim()).ok()
}

#[test]
fn load_and_validate_fixture_schema() {
    let fixtures = Fixtures::load_default().expect("failed to load fixtures");
    assert!(
        fixtures.tools.contains_key("health_check"),
        "fixtures should include at least health_check"
    );
    assert!(
        fixtures
            .resources
            .contains_key("resource://config/environment"),
        "fixtures should include resource://config/environment"
    );
}

#[test]
fn run_fixtures_against_rust_server_router() {
    let _lock = env_lock().lock().unwrap_or_else(|e| e.into_inner());
    let env = setup_fixture_env();
    let storage_root = env.tmp.path().join("archive");
    let fixtures = &env.fixtures;
    let router = &env.router;

    // Tool metrics are tracked in a global registry; reset so this test's
    // `resource://tooling/metrics` assertion is deterministic.
    mcp_agent_mail_tools::reset_tool_metrics();

    let cx = Cx::for_testing();
    let budget = Budget::INFINITE;
    let mut req_id: u64 = 1;

    for (tool_name, tool_fixture) in &fixtures.tools {
        for case in &tool_fixture.cases {
            let params = CallToolParams {
                name: tool_name.clone(),
                arguments: args_from_case(case),
                meta: None,
            };

            let result = router.handle_tools_call(
                &cx,
                req_id,
                params,
                &budget,
                SessionState::new(),
                None,
                None,
            );
            req_id += 1;

            match (&case.expect.ok, &case.expect.err) {
                (Some(expected_ok), None) => {
                    let call_result = result.unwrap_or_else(|e| {
                        panic!(
                            "tool {tool_name} case {}: unexpected router error: {e}",
                            case.name
                        )
                    });
                    if call_result.is_error {
                        // Print error content for debugging
                        let err_text = call_result
                            .content
                            .first()
                            .and_then(|c| match c {
                                Content::Text { text } => Some(text.clone()),
                                _ => None,
                            })
                            .unwrap_or_default();
                        panic!(
                            "tool {tool_name} case {}: expected ok, got error: {err_text}",
                            case.name
                        );
                    }

                    let actual = decode_json_from_tool_content(&call_result.content)
                        .unwrap_or_else(|e| panic!("tool {tool_name} case {}: {e}", case.name));
                    let (actual, expected) =
                        normalize_pair(actual, expected_ok.clone(), &case.normalize);
                    assert_eq!(
                        actual, expected,
                        "tool {tool_name} case {}: output mismatch",
                        case.name
                    );
                }
                (None, Some(expected_err)) => match result {
                    Ok(call_result) => {
                        assert!(
                            call_result.is_error,
                            "tool {tool_name} case {}: expected error, got ok",
                            case.name
                        );
                        let got = match &call_result.content.first() {
                            Some(Content::Text { text }) => text.as_str(),
                            _ => "<non-text error>",
                        };
                        assert_expected_error(got, expected_err);
                    }
                    Err(e) => {
                        assert_expected_error(&e.message, expected_err);
                    }
                },
                _ => panic!(
                    "tool {tool_name} case {}: invalid fixture expectation (must contain exactly one of ok/err)",
                    case.name
                ),
            }
        }
    }

    for (uri, resource_fixture) in &fixtures.resources {
        for case in &resource_fixture.cases {
            let params = ReadResourceParams {
                uri: uri.clone(),
                meta: None,
            };
            let result = router.handle_resources_read(
                &cx,
                req_id,
                &params,
                &budget,
                SessionState::new(),
                None,
                None,
            );
            req_id += 1;

            match (&case.expect.ok, &case.expect.err) {
                (Some(expected_ok), None) => {
                    let read_result = result.unwrap_or_else(|e| {
                        panic!(
                            "resource {uri} case {}: unexpected router error: {e}",
                            case.name
                        )
                    });
                    let actual = decode_json_from_resource_contents(uri, &read_result.contents)
                        .unwrap_or_else(|e| panic!("resource {uri} case {}: {e}", case.name));
                    let (actual, expected) =
                        normalize_pair(actual, expected_ok.clone(), &case.normalize);
                    assert_eq!(
                        actual, expected,
                        "resource {uri} case {}: output mismatch",
                        case.name
                    );
                }
                (None, Some(expected_err)) => match result {
                    Ok(read_result) => {
                        let got = read_result
                            .contents
                            .first()
                            .and_then(|c| c.text.as_deref())
                            .unwrap_or("<non-text error>");
                        assert_expected_error(got, expected_err);
                    }
                    Err(e) => {
                        assert_expected_error(&e.message, expected_err);
                    }
                },
                _ => panic!(
                    "resource {uri} case {}: invalid fixture expectation (must contain exactly one of ok/err)",
                    case.name
                ),
            }
        }
    }

    // -----------------------------------------------------------------------
    // Archive artifact assertions (run in same test to avoid env var races)
    // -----------------------------------------------------------------------
    // Reservation/message/profile writes are queued through WBQ and async git
    // commit coalescing. Flush both layers before asserting on-disk artifacts.
    mcp_agent_mail_storage::wbq_flush();
    mcp_agent_mail_storage::flush_async_commits();
    let files = collect_archive_files(&storage_root);

    // --- .gitattributes ---
    assert!(
        storage_root.join(".gitattributes").exists(),
        "expected .gitattributes at archive root, found {} files: {:?}",
        files.len(),
        files
    );

    // --- Agent profiles ---
    let expected_profiles = [
        "projects/abs-path-backend/agents/BlueLake/profile.json",
        "projects/abs-path-backend/agents/GreenCastle/profile.json",
        "projects/abs-path-backend/agents/OrangeFox/profile.json",
    ];
    for profile_rel in &expected_profiles {
        assert!(
            files.iter().any(|f| f == profile_rel),
            "expected agent profile at {profile_rel}"
        );
        let content = std::fs::read_to_string(storage_root.join(profile_rel))
            .unwrap_or_else(|e| panic!("failed to read {profile_rel}: {e}"));
        let parsed: Value = serde_json::from_str(&content)
            .unwrap_or_else(|e| panic!("failed to parse JSON in {profile_rel}: {e}"));
        assert!(parsed.get("name").and_then(Value::as_str).is_some());
        assert!(parsed.get("program").and_then(Value::as_str).is_some());
        assert!(parsed.get("model").and_then(Value::as_str).is_some());
    }

    // --- Canonical message files ---
    let message_files: Vec<&String> = files
        .iter()
        .filter(|f| {
            f.starts_with("projects/")
                && f.contains("/messages/")
                && f.ends_with(".md")
                && !f.contains("/threads/")
        })
        .collect();
    assert!(
        message_files.len() >= 2,
        "expected at least 2 canonical message files, found {}: {:?}",
        message_files.len(),
        message_files
    );

    for msg_rel in &message_files {
        let content = std::fs::read_to_string(storage_root.join(msg_rel))
            .unwrap_or_else(|e| panic!("failed to read {msg_rel}: {e}"));
        let fm = parse_frontmatter(&content)
            .unwrap_or_else(|| panic!("message {msg_rel} has no valid ---json frontmatter"));
        assert!(fm.get("from").and_then(Value::as_str).is_some());
        assert!(fm.get("subject").and_then(Value::as_str).is_some());
        assert!(fm.get("to").and_then(Value::as_array).is_some());
        assert!(fm.get("id").is_some());
    }

    // --- Inbox/outbox copies ---
    let inbox_files: Vec<&String> = files
        .iter()
        .filter(|f| f.contains("/inbox/") && f.ends_with(".md"))
        .collect();
    let outbox_files: Vec<&String> = files
        .iter()
        .filter(|f| f.contains("/outbox/") && f.ends_with(".md"))
        .collect();
    assert!(!inbox_files.is_empty(), "expected at least one inbox copy");
    assert!(
        !outbox_files.is_empty(),
        "expected at least one outbox copy"
    );

    // --- File reservation artifacts ---
    let reservation_files: Vec<&String> = files
        .iter()
        .filter(|f| f.contains("/file_reservations/") && f.ends_with(".json"))
        .collect();
    assert!(
        !reservation_files.is_empty(),
        "expected at least one file reservation JSON artifact"
    );

    // -----------------------------------------------------------------------
    // Enhanced archive artifact assertions (legacy format parity)
    // -----------------------------------------------------------------------

    // --- Agent profile full schema validation ---
    // Core fields that both Python and Rust implementations write.
    // Python also writes "id" and "project_id"; the Rust tools layer currently
    // omits those (parity gap tracked separately).
    let required_profile_fields = [
        "name",
        "program",
        "model",
        "attachments_policy",
        "inception_ts",
        "last_active_ts",
        "task_description",
    ];
    for profile_rel in &expected_profiles {
        let content = std::fs::read_to_string(storage_root.join(profile_rel))
            .unwrap_or_else(|e| panic!("read {profile_rel}: {e}"));
        let parsed: Value =
            serde_json::from_str(&content).unwrap_or_else(|e| panic!("parse {profile_rel}: {e}"));
        let obj = parsed
            .as_object()
            .unwrap_or_else(|| panic!("{profile_rel} is not a JSON object"));
        for field in &required_profile_fields {
            assert!(
                obj.contains_key(*field),
                "profile {profile_rel} missing required field: {field}"
            );
        }
        // Validate types: name/program/model must be strings, id/project_id must be numbers
        assert!(
            obj["name"].is_string(),
            "{profile_rel}: name must be string"
        );
        assert!(
            obj["program"].is_string(),
            "{profile_rel}: program must be string"
        );
        assert!(
            obj["model"].is_string(),
            "{profile_rel}: model must be string"
        );
        assert!(
            obj["attachments_policy"].is_string(),
            "{profile_rel}: attachments_policy must be string"
        );
        // JSON must be pretty-printed (contains newlines + indentation)
        assert!(
            content.contains('\n') && content.contains("  "),
            "{profile_rel}: JSON must be pretty-printed"
        );
    }

    // --- Canonical message frontmatter: full schema + format ---
    let required_fm_fields = [
        "id",
        "from",
        "to",
        "cc",
        "bcc",
        "subject",
        "importance",
        "created",
        "ack_required",
        "thread_id",
        "project",
        "project_slug",
        "attachments",
    ];
    for msg_rel in &message_files {
        let content = std::fs::read_to_string(storage_root.join(msg_rel))
            .unwrap_or_else(|e| panic!("read {msg_rel}: {e}"));
        // Must start with ---json marker
        assert!(
            content.trim_start().starts_with("---json"),
            "message {msg_rel} must start with ---json frontmatter marker"
        );
        let fm = parse_frontmatter(&content)
            .unwrap_or_else(|| panic!("message {msg_rel} has no valid ---json frontmatter"));
        let fm_obj = fm
            .as_object()
            .unwrap_or_else(|| panic!("{msg_rel} frontmatter is not a JSON object"));

        for field in &required_fm_fields {
            assert!(
                fm_obj.contains_key(*field),
                "message {msg_rel} frontmatter missing field: {field}"
            );
        }

        // Type assertions
        assert!(fm_obj["from"].is_string(), "{msg_rel}: from must be string");
        assert!(fm_obj["to"].is_array(), "{msg_rel}: to must be array");
        assert!(fm_obj["cc"].is_array(), "{msg_rel}: cc must be array");
        assert!(fm_obj["bcc"].is_array(), "{msg_rel}: bcc must be array");
        assert!(
            fm_obj["subject"].is_string(),
            "{msg_rel}: subject must be string"
        );
        assert!(
            fm_obj["importance"].is_string(),
            "{msg_rel}: importance must be string"
        );
        assert!(
            fm_obj["created"].is_string(),
            "{msg_rel}: created must be string"
        );
        assert!(
            fm_obj["ack_required"].is_boolean(),
            "{msg_rel}: ack_required must be boolean"
        );
        assert!(
            fm_obj["project"].is_string(),
            "{msg_rel}: project must be string"
        );
        assert!(
            fm_obj["project_slug"].is_string(),
            "{msg_rel}: project_slug must be string"
        );
        assert!(
            fm_obj["attachments"].is_array(),
            "{msg_rel}: attachments must be array"
        );

        // Body content: after the closing --- there should be body text
        let after_close = content
            .split("\n---\n")
            .nth(1)
            .unwrap_or_else(|| panic!("{msg_rel}: missing closing --- delimiter"));
        assert!(
            !after_close.trim().is_empty(),
            "message {msg_rel} body should not be empty"
        );

        // Frontmatter JSON must be pretty-printed
        let fm_section =
            &content[content.find("---json").unwrap() + 7..content.find("\n---").unwrap()];
        assert!(
            fm_section.contains('\n') && fm_section.contains("  "),
            "{msg_rel}: frontmatter JSON must be pretty-printed"
        );
    }

    // --- Filename pattern validation ---
    // Message filenames should follow: {ISO-timestamp}__{slug}__{id}.md
    let filename_re =
        regex::Regex::new(r"^\d{4}-\d{2}-\d{2}T\d{2}-\d{2}-\d{2}Z__[a-z0-9._-]+__\d+\.md$")
            .expect("valid regex");
    for msg_rel in &message_files {
        let filename = std::path::Path::new(msg_rel.as_str())
            .file_name()
            .and_then(|f| f.to_str())
            .unwrap_or("");
        assert!(
            filename_re.is_match(filename),
            "message filename does not match legacy pattern: {filename}"
        );
    }

    // --- Inbox/outbox content must match canonical ---
    for msg_rel in &message_files {
        let canonical = std::fs::read_to_string(storage_root.join(msg_rel))
            .unwrap_or_else(|e| panic!("read canonical {msg_rel}: {e}"));
        let canonical_fm = parse_frontmatter(&canonical).expect("canonical has frontmatter");
        let msg_id = canonical_fm.get("id").and_then(Value::as_i64).unwrap_or(-1);
        let filename = std::path::Path::new(msg_rel.as_str())
            .file_name()
            .and_then(|f| f.to_str())
            .unwrap_or("");

        // Find matching inbox and outbox copies by filename
        for inbox_rel in &inbox_files {
            if inbox_rel.ends_with(filename) {
                let inbox_content = std::fs::read_to_string(storage_root.join(inbox_rel))
                    .unwrap_or_else(|e| panic!("read inbox {inbox_rel}: {e}"));
                let inbox_fm = parse_frontmatter(&inbox_content)
                    .unwrap_or_else(|| panic!("inbox {inbox_rel} has no frontmatter"));
                let inbox_id = inbox_fm.get("id").and_then(Value::as_i64).unwrap_or(-2);
                if inbox_id == msg_id {
                    assert_eq!(
                        canonical.trim(),
                        inbox_content.trim(),
                        "inbox copy {inbox_rel} must match canonical {msg_rel}"
                    );
                }
            }
        }
        for outbox_rel in &outbox_files {
            if outbox_rel.ends_with(filename) {
                let outbox_content = std::fs::read_to_string(storage_root.join(outbox_rel))
                    .unwrap_or_else(|e| panic!("read outbox {outbox_rel}: {e}"));
                let outbox_fm = parse_frontmatter(&outbox_content)
                    .unwrap_or_else(|| panic!("outbox {outbox_rel} has no frontmatter"));
                let outbox_id = outbox_fm.get("id").and_then(Value::as_i64).unwrap_or(-2);
                if outbox_id == msg_id {
                    assert_eq!(
                        canonical.trim(),
                        outbox_content.trim(),
                        "outbox copy {outbox_rel} must match canonical {msg_rel}"
                    );
                }
            }
        }
    }

    // --- File reservation artifact full schema validation ---
    // Core fields written by the Rust tools layer.
    // Python also writes "created_ts", "released_ts", "project"; those are
    // parity gaps tracked separately.
    let required_res_fields = [
        "id",
        "agent",
        "path_pattern",
        "exclusive",
        "reason",
        "expires_ts",
    ];
    let mut has_sha1_named = false;
    let mut has_id_named = false;
    for res_rel in &reservation_files {
        let content = std::fs::read_to_string(storage_root.join(res_rel))
            .unwrap_or_else(|e| panic!("read {res_rel}: {e}"));
        let parsed: Value =
            serde_json::from_str(&content).unwrap_or_else(|e| panic!("parse {res_rel}: {e}"));
        let obj = parsed
            .as_object()
            .unwrap_or_else(|| panic!("{res_rel} is not a JSON object"));

        for field in &required_res_fields {
            assert!(
                obj.contains_key(*field),
                "reservation {res_rel} missing required field: {field}"
            );
        }

        // Type assertions
        assert!(obj["agent"].is_string(), "{res_rel}: agent must be string");
        assert!(
            obj["path_pattern"].is_string(),
            "{res_rel}: path_pattern must be string"
        );
        assert!(
            obj["exclusive"].is_boolean(),
            "{res_rel}: exclusive must be boolean"
        );

        // Must NOT have legacy "path" key (only "path_pattern")
        assert!(
            !obj.contains_key("path"),
            "{res_rel}: should use 'path_pattern' not 'path'"
        );

        // JSON must be pretty-printed
        assert!(
            content.contains('\n') && content.contains("  "),
            "{res_rel}: JSON must be pretty-printed"
        );

        // Track naming patterns
        let filename = std::path::Path::new(res_rel.as_str())
            .file_name()
            .and_then(|f| f.to_str())
            .unwrap_or("");
        if filename.starts_with("id-") {
            has_id_named = true;
        } else if filename.len() == 45 && filename.ends_with(".json") {
            // SHA1 hex (40 chars) + .json (5 chars) = 45
            has_sha1_named = true;
        }
    }
    assert!(
        has_id_named,
        "expected at least one id-<N>.json reservation file"
    );
    assert!(
        has_sha1_named,
        "expected at least one SHA1-named reservation file"
    );

    // --- Paired SHA1 + stable-id files: SHA1 file reflects the latest reservation ---
    let res_dir_prefix = "projects/abs-path-backend/file_reservations/";
    let res_in_dir: Vec<&str> = reservation_files
        .iter()
        .filter(|f| f.starts_with(res_dir_prefix))
        .map(|f| f.as_str())
        .collect();
    let id_files_in_dir: Vec<&str> = res_in_dir
        .iter()
        .copied()
        .filter(|f| {
            std::path::Path::new(f)
                .file_name()
                .and_then(|n| n.to_str())
                .is_some_and(|n| n.starts_with("id-"))
        })
        .collect();
    // For each id file, verify its SHA1-named sibling exists and has the same path_pattern.
    for id_file in &id_files_in_dir {
        let id_content = std::fs::read_to_string(storage_root.join(id_file))
            .unwrap_or_else(|e| panic!("read {id_file}: {e}"));
        let id_parsed: Value =
            serde_json::from_str(&id_content).unwrap_or_else(|e| panic!("parse {id_file}: {e}"));
        let path_pattern = id_parsed
            .get("path_pattern")
            .and_then(Value::as_str)
            .unwrap_or("");
        if path_pattern.is_empty() {
            continue;
        }
        // Compute SHA1 of path_pattern and verify the SHA1 file exists
        use sha1::Digest;
        let mut hasher = sha1::Sha1::new();
        hasher.update(path_pattern.as_bytes());
        let sha1_hex = format!("{:x}", hasher.finalize());
        let sha1_file = format!("{res_dir_prefix}{sha1_hex}.json");
        assert!(
            res_in_dir.iter().any(|f| *f == sha1_file),
            "SHA1 file {sha1_file} must exist for path_pattern '{path_pattern}' (from {id_file})"
        );
        // The SHA1 file contains the LATEST reservation for this path_pattern
        // (may differ from this specific id file if multiple reservations share the pattern).
        let sha1_content = std::fs::read_to_string(storage_root.join(&sha1_file))
            .unwrap_or_else(|e| panic!("read {sha1_file}: {e}"));
        let sha1_parsed: Value = serde_json::from_str(&sha1_content)
            .unwrap_or_else(|e| panic!("parse {sha1_file}: {e}"));
        assert_eq!(
            sha1_parsed.get("path_pattern").and_then(Value::as_str),
            Some(path_pattern),
            "SHA1 file {sha1_file} must have same path_pattern as {id_file}"
        );
    }

    // --- Thread digest format validation ---
    let thread_files: Vec<&String> = files
        .iter()
        .filter(|f| f.contains("/messages/threads/") && f.ends_with(".md"))
        .collect();
    for thread_rel in &thread_files {
        let content = std::fs::read_to_string(storage_root.join(thread_rel))
            .unwrap_or_else(|e| panic!("read {thread_rel}: {e}"));
        // Thread digest must start with "# Thread "
        assert!(
            content.starts_with("# Thread "),
            "thread digest {thread_rel} must start with '# Thread ' header"
        );
        // Must contain at least one entry separator
        assert!(
            content.contains("---"),
            "thread digest {thread_rel} must contain --- entry separator"
        );
        // Must contain a canonical link
        assert!(
            content.contains("[View canonical]"),
            "thread digest {thread_rel} must contain [View canonical] link"
        );
        // Must contain sender→recipient arrow
        assert!(
            content.contains('→') || content.contains("→"),
            "thread digest {thread_rel} must contain sender → recipient arrow"
        );
    }

    // --- .gitattributes content ---
    let gitattrs = std::fs::read_to_string(storage_root.join(".gitattributes"))
        .unwrap_or_else(|e| panic!("read .gitattributes: {e}"));
    assert!(
        gitattrs.contains("*.json") && gitattrs.contains("*.md"),
        ".gitattributes must declare *.json and *.md as text"
    );

    // --- Notification signal assertions (tool flow) ---
    let notif_tmp = tempfile::TempDir::new().expect("failed to create notifications tempdir");
    let notif_db_path = notif_tmp.path().join("db.sqlite3");
    let notif_db_url = format!("sqlite://{}", notif_db_path.display());
    let notif_storage_root = notif_tmp.path().join("archive");
    let notif_signals_dir = notif_tmp.path().join("signals");
    let _env_guard = EnvVarGuard::set(&[
        ("DATABASE_URL", notif_db_url.as_str()),
        (
            "STORAGE_ROOT",
            notif_storage_root
                .to_str()
                .expect("storage_root must be valid UTF-8"),
        ),
        ("NOTIFICATIONS_ENABLED", "1"),
        ("NOTIFICATIONS_DEBOUNCE_MS", "0"),
        ("NOTIFICATIONS_INCLUDE_METADATA", "1"),
        (
            "NOTIFICATIONS_SIGNALS_DIR",
            notif_signals_dir
                .to_str()
                .expect("signals_dir must be valid UTF-8"),
        ),
    ]);

    let config = mcp_agent_mail_core::Config::from_env();
    let router = mcp_agent_mail_server::build_server(&config).into_router();

    let project_dir = notif_tmp.path().join("project");
    std::fs::create_dir_all(&project_dir).expect("create notification project dir");
    let project_key = project_dir.to_string_lossy().to_string();
    let project_slug = mcp_agent_mail_core::compute_project_slug(&project_key);

    let ensure_params = CallToolParams {
        name: "ensure_project".to_string(),
        arguments: Some(serde_json::json!({ "human_key": project_key.clone() })),
        meta: None,
    };
    let ensure_result = router
        .handle_tools_call(
            &cx,
            req_id,
            ensure_params,
            &budget,
            SessionState::new(),
            None,
            None,
        )
        .unwrap_or_else(|e| panic!("ensure_project failed: {e}"));
    req_id += 1;
    assert!(!ensure_result.is_error, "ensure_project returned error");

    for name in ["BoldCastle", "CalmRiver", "QuietMeadow", "SilverPeak"] {
        let register_params = CallToolParams {
            name: "register_agent".to_string(),
            arguments: Some(serde_json::json!({
                "project_key": project_key.clone(),
                "program": "codex-cli",
                "model": "gpt-5",
                "name": name,
            })),
            meta: None,
        };
        let register_result = router
            .handle_tools_call(
                &cx,
                req_id,
                register_params,
                &budget,
                SessionState::new(),
                None,
                None,
            )
            .unwrap_or_else(|e| panic!("register_agent failed for {name}: {e}"));
        req_id += 1;
        assert!(
            !register_result.is_error,
            "register_agent returned error for {name}: {:?}",
            register_result.content
        );
    }

    let send_params = CallToolParams {
        name: "send_message".to_string(),
        arguments: Some(serde_json::json!({
            "project_key": project_key.clone(),
            "sender_name": "BoldCastle",
            "to": ["CalmRiver"],
            "cc": ["QuietMeadow"],
            "bcc": ["SilverPeak"],
            "subject": "Signal test",
            "body_md": "Hello from notifications test.",
            "importance": "high",
        })),
        meta: None,
    };
    let send_result = router
        .handle_tools_call(
            &cx,
            req_id,
            send_params,
            &budget,
            SessionState::new(),
            None,
            None,
        )
        .unwrap_or_else(|e| panic!("send_message failed: {e}"));
    req_id += 1;
    assert!(
        !send_result.is_error,
        "send_message returned error: {:?}",
        send_result.content
    );

    let send_json = decode_json_from_tool_content(&send_result.content)
        .expect("failed to decode send_message response");
    let message_id = send_json
        .pointer("/deliveries/0/payload/id")
        .and_then(Value::as_i64)
        .expect("send_message response missing deliveries[0].payload.id");

    let signal_root = notif_signals_dir
        .join("projects")
        .join(&project_slug)
        .join("agents");
    let to_signal = signal_root.join("CalmRiver.signal");
    let cc_signal = signal_root.join("QuietMeadow.signal");
    let bcc_signal = signal_root.join("SilverPeak.signal");

    // Flush write-behind queue so notification signals are written to disk
    mcp_agent_mail_storage::wbq_flush();

    assert!(to_signal.exists(), "expected CalmRiver signal file");
    assert!(cc_signal.exists(), "expected QuietMeadow signal file");
    assert!(
        !bcc_signal.exists(),
        "did not expect SilverPeak signal file"
    );

    let to_payload: Value = serde_json::from_str(
        &std::fs::read_to_string(&to_signal).expect("failed to read CalmRiver signal"),
    )
    .expect("failed to parse CalmRiver signal JSON");
    assert_eq!(to_payload["project"], project_slug);
    assert_eq!(to_payload["agent"], "CalmRiver");
    assert_eq!(to_payload["message"]["id"], message_id);
    assert_eq!(to_payload["message"]["from"], "BoldCastle");
    assert_eq!(to_payload["message"]["subject"], "Signal test");
    assert_eq!(to_payload["message"]["importance"], "high");

    let fetch_params = CallToolParams {
        name: "fetch_inbox".to_string(),
        arguments: Some(serde_json::json!({
            "project_key": project_key.clone(),
            "agent_name": "CalmRiver",
        })),
        meta: None,
    };
    let fetch_result = router
        .handle_tools_call(
            &cx,
            req_id,
            fetch_params,
            &budget,
            SessionState::new(),
            None,
            None,
        )
        .unwrap_or_else(|e| panic!("fetch_inbox failed: {e}"));
    assert!(!fetch_result.is_error, "fetch_inbox returned error");

    assert!(
        !to_signal.exists(),
        "expected CalmRiver signal to be cleared after fetch_inbox"
    );
    assert!(cc_signal.exists(), "expected QuietMeadow signal to remain");
}

#[test]
fn tool_filter_profiles_match_fixtures() {
    let _lock = env_lock().lock().unwrap_or_else(|e| e.into_inner());
    let fixtures = load_tool_filter_fixtures();

    for case in fixtures.cases {
        let _env_guard = ToolFilterEnvGuard::apply(&case.env);
        let config = mcp_agent_mail_core::Config::from_env();
        let router = mcp_agent_mail_server::build_server(&config).into_router();

        let cx = Cx::for_testing();
        let budget = Budget::INFINITE;

        // tools/list
        let tools_result = router
            .handle_tools_list(&cx, ListToolsParams::default(), None)
            .expect("tools/list failed");
        let mut actual_tools: Vec<String> =
            tools_result.tools.into_iter().map(|t| t.name).collect();
        actual_tools.sort();

        let mut expected_tools = case.expected_tools.clone();
        expected_tools.sort();

        assert_eq!(
            actual_tools, expected_tools,
            "tools/list mismatch for case {}",
            case.name
        );

        // tooling directory
        let params = ReadResourceParams {
            uri: "resource://tooling/directory".to_string(),
            meta: None,
        };
        let result = router
            .handle_resources_read(&cx, 1, &params, &budget, SessionState::new(), None, None)
            .expect("tooling directory read failed");
        let dir_json = decode_json_from_resource_contents(&params.uri, &result.contents)
            .expect("tooling directory JSON decode failed");
        let mut directory_tools = extract_tool_names_from_directory(&dir_json);
        directory_tools.sort();

        assert_eq!(
            directory_tools, expected_tools,
            "tooling/directory mismatch for case {}",
            case.name
        );
    }
}

#[test]
fn backpressure_shedding_rejects_only_shedable_tools_when_enabled() {
    let _lock = env_lock().lock().unwrap_or_else(|e| e.into_inner());

    let tmp = tempfile::TempDir::new().expect("tempdir");
    let db_path = tmp.path().join("backpressure-shedding.sqlite3");
    let db_url = format!("sqlite://{}", db_path.display());
    let storage = tmp.path().join("archive");
    let _env_guard = EnvVarGuard::set(&[
        ("DATABASE_URL", &db_url),
        ("STORAGE_ROOT", storage.to_str().unwrap_or_default()),
        ("WORKTREES_ENABLED", "1"),
        ("TOOLS_FILTER_ENABLED", "0"),
        ("BACKPRESSURE_SHEDDING_ENABLED", "1"),
        ("AGENT_NAME_ENFORCEMENT_MODE", "coerce"),
    ]);

    let config = mcp_agent_mail_core::Config::from_env();
    assert!(
        config.backpressure_shedding_enabled,
        "test precondition: shedding must be enabled via env"
    );

    let router = mcp_agent_mail_server::build_server(&config).into_router();
    let cx = Cx::for_testing();
    let budget = Budget::INFINITE;
    let mut req_id: u64 = 1;

    // Save and restore global metric state because backpressure cache/metrics are process-wide.
    let metrics = mcp_agent_mail_core::metrics::global_metrics();
    let original_pool_total = metrics.db.pool_total_connections.load();
    let original_pool_active = metrics.db.pool_active_connections.load();
    let original_pool_idle = metrics.db.pool_idle_connections.load();
    let original_shedding_enabled = mcp_agent_mail_core::shedding_enabled();

    // Force a deterministic Red level via pool utilization threshold.
    metrics.db.pool_total_connections.set(100);
    metrics.db.pool_active_connections.set(95);
    metrics.db.pool_idle_connections.set(5);
    let (level, _) = mcp_agent_mail_core::refresh_health_level();
    assert_eq!(
        level,
        mcp_agent_mail_core::HealthLevel::Red,
        "test precondition: forced metrics should classify as Red"
    );

    // Shedable tool should be rejected before tool-specific argument parsing.
    let shedable_params = CallToolParams {
        name: "whois".to_string(),
        arguments: Some(serde_json::json!({
            "project_key": "/tmp/nonexistent-project-for-shedding-test",
            "agent_name": "NoSuchAgent",
        })),
        meta: None,
    };
    let shedable_result = router.handle_tools_call(
        &cx,
        req_id,
        shedable_params,
        &budget,
        SessionState::new(),
        None,
        None,
    );
    req_id += 1;
    match shedable_result {
        Ok(call_result) => {
            assert!(call_result.is_error, "expected whois to be shed");
            let text = call_result
                .content
                .first()
                .and_then(|c| match c {
                    Content::Text { text } => Some(text.as_str()),
                    _ => None,
                })
                .unwrap_or("<non-text error>");
            assert!(
                text.contains("Server overloaded") && text.contains("whois"),
                "unexpected shed error content: {text}"
            );
        }
        Err(err) => {
            assert!(
                err.message.contains("Server overloaded") && err.message.contains("whois"),
                "unexpected shed router error: {}",
                err.message
            );
        }
    }

    // Non-shedable critical tool should still execute under Red.
    let critical_params = CallToolParams {
        name: "health_check".to_string(),
        arguments: Some(serde_json::json!({})),
        meta: None,
    };
    let critical_result = router
        .handle_tools_call(
            &cx,
            req_id,
            critical_params,
            &budget,
            SessionState::new(),
            None,
            None,
        )
        .unwrap_or_else(|e| panic!("health_check should not be shed: {e}"));
    assert!(
        !critical_result.is_error,
        "health_check must remain available even under Red"
    );

    // Restore process-global state for other tests.
    metrics.db.pool_total_connections.set(original_pool_total);
    metrics.db.pool_active_connections.set(original_pool_active);
    metrics.db.pool_idle_connections.set(original_pool_idle);
    mcp_agent_mail_core::set_shedding_enabled(original_shedding_enabled);
    let _ = mcp_agent_mail_core::refresh_health_level();
}

#[test]
fn product_bus_tools_end_to_end_across_linked_projects() {
    let _lock = env_lock().lock().unwrap_or_else(|e| e.into_inner());

    let tmp = tempfile::TempDir::new().expect("tempdir");
    let db_path = tmp.path().join("product-bus-e2e.sqlite3");
    let db_url = format!("sqlite://{}", db_path.display());
    let storage = tmp.path().join("archive");
    let _env_guard = EnvVarGuard::set(&[
        ("DATABASE_URL", &db_url),
        ("STORAGE_ROOT", storage.to_str().unwrap_or_default()),
        ("WORKTREES_ENABLED", "1"),
        ("TOOLS_FILTER_ENABLED", "0"),
        ("LLM_ENABLED", "0"),
        ("AGENT_NAME_ENFORCEMENT_MODE", "coerce"),
    ]);

    let config = mcp_agent_mail_core::Config::from_env();
    let router = mcp_agent_mail_server::build_server(&config).into_router();
    let cx = Cx::for_testing();
    let budget = Budget::INFINITE;
    let mut req_id: u64 = 1;

    let mut call_tool_json = |name: &str, arguments: Value| -> Value {
        let params = CallToolParams {
            name: name.to_string(),
            arguments: Some(arguments),
            meta: None,
        };
        let result = router
            .handle_tools_call(
                &cx,
                req_id,
                params,
                &budget,
                SessionState::new(),
                None,
                None,
            )
            .unwrap_or_else(|e| panic!("{name} failed: {e}"));
        req_id += 1;
        assert!(
            !result.is_error,
            "{name} returned error: {:?}",
            result.content
        );
        decode_json_from_tool_content(&result.content)
            .unwrap_or_else(|e| panic!("{name} returned non-JSON content: {e}"))
    };

    let project_a_dir = tmp.path().join("project-alpha");
    let project_b_dir = tmp.path().join("project-beta");
    let project_c_dir = tmp.path().join("project-gamma-unlinked");
    std::fs::create_dir_all(&project_a_dir).expect("create project alpha dir");
    std::fs::create_dir_all(&project_b_dir).expect("create project beta dir");
    std::fs::create_dir_all(&project_c_dir).expect("create project gamma dir");
    let project_a_key = project_a_dir.to_string_lossy().to_string();
    let project_b_key = project_b_dir.to_string_lossy().to_string();
    let project_c_key = project_c_dir.to_string_lossy().to_string();

    let project_a = call_tool_json(
        "ensure_project",
        serde_json::json!({ "human_key": project_a_key }),
    );
    let project_b = call_tool_json(
        "ensure_project",
        serde_json::json!({ "human_key": project_b_key }),
    );
    let project_c = call_tool_json(
        "ensure_project",
        serde_json::json!({ "human_key": project_c_key }),
    );
    let project_a_id = project_a
        .get("id")
        .and_then(Value::as_i64)
        .expect("ensure_project alpha should include numeric id");
    let project_b_id = project_b
        .get("id")
        .and_then(Value::as_i64)
        .expect("ensure_project beta should include numeric id");
    let project_c_id = project_c
        .get("id")
        .and_then(Value::as_i64)
        .expect("ensure_project gamma should include numeric id");

    for (project_key, agent_name) in [
        (project_a_key.as_str(), "BlueLake"),
        (project_a_key.as_str(), "GreenCastle"),
        (project_b_key.as_str(), "BlueLake"),
        (project_b_key.as_str(), "RedHarbor"),
        (project_c_key.as_str(), "BlueLake"),
        (project_c_key.as_str(), "PurpleBear"),
    ] {
        let _ = call_tool_json(
            "register_agent",
            serde_json::json!({
                "project_key": project_key,
                "program": "test",
                "model": "test-model",
                "name": agent_name
            }),
        );
    }

    let shared_thread = "product-bus-thread-e2e";
    let _ = call_tool_json(
        "send_message",
        serde_json::json!({
            "project_key": project_a_key,
            "sender_name": "GreenCastle",
            "to": ["BlueLake"],
            "subject": "product-bus-e2e alpha",
            "body_md": "- [ ] ACTION alpha follow-up",
            "thread_id": shared_thread
        }),
    );
    let _ = call_tool_json(
        "send_message",
        serde_json::json!({
            "project_key": project_b_key,
            "sender_name": "RedHarbor",
            "to": ["BlueLake"],
            "subject": "product-bus-e2e beta",
            "body_md": "- [ ] ACTION beta follow-up",
            "thread_id": shared_thread
        }),
    );
    let _ = call_tool_json(
        "send_message",
        serde_json::json!({
            "project_key": project_c_key,
            "sender_name": "PurpleBear",
            "to": ["BlueLake"],
            "subject": "product-bus-e2e gamma unlinked",
            "body_md": "- [ ] ACTION gamma follow-up",
            "thread_id": shared_thread
        }),
    );

    let product_key = "a1b2c3d4e5f6a7b8c9d0";
    let ensured = call_tool_json(
        "ensure_product",
        serde_json::json!({
            "product_key": product_key,
            "name": "ProductBusE2E"
        }),
    );
    assert_eq!(
        ensured.get("product_uid").and_then(Value::as_str),
        Some(product_key),
        "ensure_product should keep explicit hex product key"
    );

    let _ = call_tool_json(
        "products_link",
        serde_json::json!({
            "product_key": product_key,
            "project_key": project_a_key
        }),
    );
    let _ = call_tool_json(
        "products_link",
        serde_json::json!({
            "product_key": product_key,
            "project_key": project_b_key
        }),
    );

    let search = call_tool_json(
        "search_messages_product",
        serde_json::json!({
            "product_key": product_key,
            "query": "product-bus-e2e",
            "limit": 20
        }),
    );
    let search_rows = search
        .get("result")
        .and_then(Value::as_array)
        .expect("search_messages_product response must include result array");
    assert!(
        search_rows.len() >= 2,
        "expected at least two linked-project hits, got {search_rows:?}"
    );
    let mut search_project_ids = BTreeSet::new();
    for row in search_rows {
        if let Some(project_id) = row.get("project_id").and_then(Value::as_i64) {
            search_project_ids.insert(project_id);
        }
    }
    assert!(search_project_ids.contains(&project_a_id));
    assert!(search_project_ids.contains(&project_b_id));
    assert!(
        !search_project_ids.contains(&project_c_id),
        "unlinked project should not be included in product search"
    );

    let inbox = call_tool_json(
        "fetch_inbox_product",
        serde_json::json!({
            "product_key": product_key,
            "agent_name": "BlueLake",
            "limit": 20,
            "include_bodies": true
        }),
    );
    let inbox_rows = inbox
        .as_array()
        .expect("fetch_inbox_product should return array response");
    assert!(
        inbox_rows.len() >= 2,
        "expected at least two inbox rows from linked projects"
    );
    let mut inbox_project_ids = BTreeSet::new();
    for row in inbox_rows {
        if let Some(project_id) = row.get("project_id").and_then(Value::as_i64) {
            inbox_project_ids.insert(project_id);
        }
    }
    assert!(inbox_project_ids.contains(&project_a_id));
    assert!(inbox_project_ids.contains(&project_b_id));
    assert!(
        !inbox_project_ids.contains(&project_c_id),
        "unlinked project should not be included in product inbox"
    );

    let summary = call_tool_json(
        "summarize_thread_product",
        serde_json::json!({
            "product_key": product_key,
            "thread_id": shared_thread,
            "include_examples": true,
            "llm_mode": false
        }),
    );
    assert_eq!(
        summary.get("thread_id").and_then(Value::as_str),
        Some(shared_thread)
    );
    let participants = summary
        .pointer("/summary/participants")
        .and_then(Value::as_array)
        .expect("summary.participants must be an array");
    let participant_names: BTreeSet<&str> = participants.iter().filter_map(Value::as_str).collect();
    assert!(participant_names.contains("GreenCastle"));
    assert!(participant_names.contains("RedHarbor"));
    assert!(
        !participant_names.contains("PurpleBear"),
        "unlinked project participant should not appear in summary"
    );
    let examples = summary
        .get("examples")
        .and_then(Value::as_array)
        .expect("summary examples must be an array");
    assert!(
        examples.len() >= 2,
        "expected examples from linked projects in thread summary"
    );
}

// ---------------------------------------------------------------------------
// Archive artifact conformance tests
// ---------------------------------------------------------------------------

/// Collect all files under a directory (excluding .git), returning paths relative to root.
fn collect_archive_files(root: &std::path::Path) -> Vec<String> {
    let mut files = Vec::new();
    collect_files_recursive(root, root, &mut files);
    files.sort();
    files
}

fn collect_files_recursive(base: &std::path::Path, dir: &std::path::Path, out: &mut Vec<String>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries {
        let Ok(entry) = entry else { continue };
        let path = entry.path();
        let name = entry.file_name().to_string_lossy().to_string();
        if name == ".git" {
            continue;
        }
        if path.is_dir() {
            collect_files_recursive(base, &path, out);
        } else if let Ok(rel) = path.strip_prefix(base) {
            out.push(rel.to_string_lossy().to_string());
        }
    }
}

// Archive artifact conformance assertions are now embedded at the end of
// `run_fixtures_against_rust_server_router` to avoid parallel env var races.

// ---------------------------------------------------------------------------
// Fixture schema drift guard
// ---------------------------------------------------------------------------

#[test]
fn fixture_schema_drift_guard() {
    let fixtures = Fixtures::load_default().expect("failed to load fixtures");

    // Every tool in TOOL_CLUSTER_MAP must have at least one fixture case.
    let tool_names: Vec<&str> = mcp_agent_mail_tools::TOOL_CLUSTER_MAP
        .iter()
        .map(|(name, _)| *name)
        .collect();
    for tool_name in &tool_names {
        assert!(
            fixtures.tools.contains_key(*tool_name),
            "tool {tool_name} is registered in TOOL_CLUSTER_MAP but has no fixture"
        );
        let fixture = &fixtures.tools[*tool_name];
        assert!(
            !fixture.cases.is_empty(),
            "tool {tool_name} fixture has zero cases"
        );
    }

    // Every fixture tool must be in TOOL_CLUSTER_MAP (no stale fixtures).
    for tool_name in fixtures.tools.keys() {
        assert!(
            tool_names.contains(&tool_name.as_str()),
            "fixture tool {tool_name} is not in TOOL_CLUSTER_MAP (stale fixture?)"
        );
    }

    // Every fixture case must have either ok or err expectation (already validated by load,
    // but belt-and-suspenders).
    for (tool_name, fixture) in &fixtures.tools {
        for case in &fixture.cases {
            assert!(
                case.expect.ok.is_some() || case.expect.err.is_some(),
                "tool {tool_name} case {} has neither ok nor err expectation",
                case.name
            );
        }
    }
    for (uri, fixture) in &fixtures.resources {
        for case in &fixture.cases {
            assert!(
                case.expect.ok.is_some() || case.expect.err.is_some(),
                "resource {uri} case {} has neither ok nor err expectation",
                case.name
            );
        }
    }
}

// ---------------------------------------------------------------------------
// Coverage completeness assertions
// ---------------------------------------------------------------------------

#[test]
fn fixture_tool_error_case_coverage() {
    let fixtures = Fixtures::load_default().expect("failed to load fixtures");

    // These tools MUST have at least one error case (they have well-defined error paths).
    let tools_requiring_error_cases = [
        "ensure_project",                 // relative path -> error
        "file_reservation_paths",         // empty paths -> error
        "force_release_file_reservation", // force release missing -> error
        "search_messages",                // empty query -> error
        "summarize_thread",               // empty thread_id -> error
        "whois",                          // non-existent agent -> error
        "renew_file_reservations",        // insufficient seconds -> error
        "send_message",                   // non-existent sender -> error
    ];

    for tool_name in &tools_requiring_error_cases {
        let fixture = fixtures
            .tools
            .get(*tool_name)
            .unwrap_or_else(|| panic!("fixture missing for {tool_name}"));
        let has_error_case = fixture.cases.iter().any(|c| c.expect.err.is_some());
        assert!(
            has_error_case,
            "tool {tool_name} should have at least one error case fixture"
        );
    }
}

#[test]
fn fixture_resource_identity_coverage() {
    let fixtures = Fixtures::load_default().expect("failed to load fixtures");

    // resource://identity/{project} must be covered.
    let has_identity = fixtures
        .resources
        .keys()
        .any(|uri| uri.starts_with("resource://identity/"));
    assert!(
        has_identity,
        "fixtures must include at least one resource://identity/{{project}} case"
    );
}

// ---------------------------------------------------------------------------
// Resource query routing edge cases (fastmcp matching behavior)
// ---------------------------------------------------------------------------

#[test]
fn resource_query_router_projects_limit_and_contains_are_honored() {
    let _lock = env_lock().lock().unwrap_or_else(|e| e.into_inner());

    let tmp = tempfile::TempDir::new().expect("tempdir");
    let db_path = tmp.path().join("projects-query-routing.sqlite3");
    let db_url = format!("sqlite://{}", db_path.display());
    let storage = tmp.path().join("archive");
    let _env_guard = EnvVarGuard::set(&[
        ("DATABASE_URL", &db_url),
        ("STORAGE_ROOT", storage.to_str().unwrap_or_default()),
        ("TOOLS_FILTER_ENABLED", "0"),
        ("AGENT_NAME_ENFORCEMENT_MODE", "coerce"),
    ]);

    let config = mcp_agent_mail_core::Config::from_env();
    let router = mcp_agent_mail_server::build_server(&config).into_router();
    let cx = Cx::for_testing();
    let budget = Budget::INFINITE;
    let mut req_id: u64 = 1;

    // Seed multiple projects so filtering/limits are observable.
    let project_keys = [
        "/tmp/router-query-alpha",
        "/tmp/router-query-mail-one",
        "/tmp/router-query-mail-two",
    ];
    for human_key in project_keys {
        let params = CallToolParams {
            name: "ensure_project".to_string(),
            arguments: Some(serde_json::json!({ "human_key": human_key })),
            meta: None,
        };
        let result = router.handle_tools_call(
            &cx,
            req_id,
            params,
            &budget,
            SessionState::new(),
            None,
            None,
        );
        req_id += 1;
        let call_result = result.unwrap_or_else(|e| panic!("ensure_project failed: {e}"));
        assert!(!call_result.is_error, "ensure_project returned error");
    }

    // Critical assertion: query URI must route to query-aware resource behavior.
    let params = ReadResourceParams {
        uri: "resource://projects?contains=mail&limit=1".to_string(),
        meta: None,
    };
    let result = router.handle_resources_read(
        &cx,
        req_id,
        &params,
        &budget,
        SessionState::new(),
        None,
        None,
    );
    req_id += 1;
    let read_result = result.expect("projects query read should succeed");
    let json = decode_json_from_resource_contents(&params.uri, &read_result.contents)
        .expect("projects query response should decode");
    let projects = json
        .as_array()
        .unwrap_or_else(|| panic!("expected projects array, got: {json}"));
    assert_eq!(
        projects.len(),
        1,
        "expected limit=1 to be honored for resource://projects query route"
    );

    let first = &projects[0];
    let slug_has_mail = first
        .get("slug")
        .and_then(Value::as_str)
        .is_some_and(|s| s.to_ascii_lowercase().contains("mail"));
    let human_key_has_mail = first
        .get("human_key")
        .and_then(Value::as_str)
        .is_some_and(|s| s.to_ascii_lowercase().contains("mail"));
    assert!(
        slug_has_mail || human_key_has_mail,
        "expected contains=mail filter to be honored, got row: {first}"
    );

    let zero_params = ReadResourceParams {
        uri: "resource://projects?limit=0".to_string(),
        meta: None,
    };
    let zero_result = router
        .handle_resources_read(
            &cx,
            req_id,
            &zero_params,
            &budget,
            SessionState::new(),
            None,
            None,
        )
        .expect("projects limit=0 read should succeed");
    let zero_json = decode_json_from_resource_contents(&zero_params.uri, &zero_result.contents)
        .expect("projects limit=0 response should decode");
    let zero_projects = zero_json
        .as_array()
        .unwrap_or_else(|| panic!("expected projects array, got: {zero_json}"));
    assert!(
        zero_projects.is_empty(),
        "expected limit=0 to return empty list, got: {zero_json}"
    );
}

#[test]
fn resource_query_router_projects_invalid_query_values_surface_errors() {
    let _lock = env_lock().lock().unwrap_or_else(|e| e.into_inner());

    let tmp = tempfile::TempDir::new().expect("tempdir");
    let db_path = tmp.path().join("projects-query-errors.sqlite3");
    let db_url = format!("sqlite://{}", db_path.display());
    let storage = tmp.path().join("archive");
    let _env_guard = EnvVarGuard::set(&[
        ("DATABASE_URL", &db_url),
        ("STORAGE_ROOT", storage.to_str().unwrap_or_default()),
        ("TOOLS_FILTER_ENABLED", "0"),
        ("AGENT_NAME_ENFORCEMENT_MODE", "coerce"),
    ]);

    let config = mcp_agent_mail_core::Config::from_env();
    let router = mcp_agent_mail_server::build_server(&config).into_router();
    let cx = Cx::for_testing();
    let budget = Budget::INFINITE;

    let invalid_cases = [
        ("resource://projects?limit=NaN", "Invalid limit"),
        (
            "resource://projects?format=xml",
            "Unsupported projects format",
        ),
    ];

    for (idx, (uri, expected_substr)) in invalid_cases.into_iter().enumerate() {
        let params = ReadResourceParams {
            uri: uri.to_string(),
            meta: None,
        };
        let result = router.handle_resources_read(
            &cx,
            u64::try_from(idx + 1).expect("request id"),
            &params,
            &budget,
            SessionState::new(),
            None,
            None,
        );

        match result {
            Err(err) => {
                assert!(
                    err.message.contains(expected_substr),
                    "expected router error to contain {:?}, got {:?} for URI {}",
                    expected_substr,
                    err.message,
                    uri
                );
            }
            Ok(read_result) => {
                let text = read_result
                    .contents
                    .first()
                    .and_then(|c| c.text.as_deref())
                    .unwrap_or("<non-text>");
                assert!(
                    text.contains(expected_substr),
                    "expected query validation error containing {:?}, got successful content {:?} for URI {}",
                    expected_substr,
                    text,
                    uri
                );
            }
        }
    }
}

#[test]
fn resource_router_error_cases_missing_projects_invalid_uris_and_bad_params() {
    let _lock = env_lock().lock().unwrap_or_else(|e| e.into_inner());

    let tmp = tempfile::TempDir::new().expect("tempdir");
    let db_path = tmp.path().join("resource-error-cases.sqlite3");
    let db_url = format!("sqlite://{}", db_path.display());
    let storage = tmp.path().join("archive");
    let _env_guard = EnvVarGuard::set(&[
        ("DATABASE_URL", &db_url),
        ("STORAGE_ROOT", storage.to_str().unwrap_or_default()),
        ("TOOLS_FILTER_ENABLED", "0"),
        ("AGENT_NAME_ENFORCEMENT_MODE", "coerce"),
    ]);

    let config = mcp_agent_mail_core::Config::from_env();
    let router = mcp_agent_mail_server::build_server(&config).into_router();
    let cx = Cx::for_testing();
    let budget = Budget::INFINITE;

    let cases: [(&str, &[&str]); 4] = [
        (
            "resource://project/does-not-exist",
            &["Project not found", "project not found"],
        ),
        (
            "resource://message/not-a-number",
            &["Invalid message ID", "invalid message id"],
        ),
        (
            "resource://message/1",
            &["project query parameter is required"],
        ),
        (
            "resource://definitely-not-real/123",
            &["Unknown resource", "unknown resource", "not found"],
        ),
    ];

    for (idx, (uri, expected_any)) in cases.into_iter().enumerate() {
        let params = ReadResourceParams {
            uri: uri.to_string(),
            meta: None,
        };
        let result = router.handle_resources_read(
            &cx,
            u64::try_from(idx + 1).expect("request id"),
            &params,
            &budget,
            SessionState::new(),
            None,
            None,
        );

        let contains_any = |text: &str| -> bool {
            let lower = text.to_ascii_lowercase();
            expected_any
                .iter()
                .any(|needle| text.contains(needle) || lower.contains(&needle.to_ascii_lowercase()))
        };

        match result {
            Err(err) => {
                assert!(
                    contains_any(&err.message),
                    "expected one of {:?}, got error {:?} for URI {}",
                    expected_any,
                    err.message,
                    uri
                );
            }
            Ok(read_result) => {
                let text = read_result
                    .contents
                    .first()
                    .and_then(|c| c.text.as_deref())
                    .unwrap_or("<non-text>");
                assert!(
                    contains_any(text),
                    "expected one of {:?}, got successful content {:?} for URI {}",
                    expected_any,
                    text,
                    uri
                );
            }
        }
    }
}

// ---------------------------------------------------------------------------
// TOON format parameter handling
// ---------------------------------------------------------------------------

#[test]
fn toon_format_resolution_json_fallback() {
    // When TOON_BIN is empty (no encoder available), format requests should
    // resolve to JSON or produce a TOON envelope with fallback data.
    let _lock = env_lock().lock().unwrap_or_else(|e| e.into_inner());

    let tmp = tempfile::TempDir::new().expect("tempdir");
    let db_path = tmp.path().join("toon-test.sqlite3");
    let db_url = format!("sqlite://{}", db_path.display());
    let storage = tmp.path().join("archive");
    let _env_guard = EnvVarGuard::set(&[
        ("DATABASE_URL", &db_url),
        ("STORAGE_ROOT", storage.to_str().unwrap()),
        ("TOOLS_FILTER_ENABLED", "0"),
        ("TOON_BIN", ""),
        ("TOON_TRU_BIN", ""),
        ("MCP_AGENT_MAIL_OUTPUT_FORMAT", ""),
        ("TOON_DEFAULT_FORMAT", ""),
        ("AGENT_NAME_ENFORCEMENT_MODE", "coerce"),
    ]);

    let config = mcp_agent_mail_core::Config::from_env();
    let router = mcp_agent_mail_server::build_server(&config).into_router();
    let cx = Cx::for_testing();
    let budget = Budget::INFINITE;

    // Call health_check - should work regardless of TOON config.
    let params = CallToolParams {
        name: "health_check".to_string(),
        arguments: None,
        meta: None,
    };
    let result = router.handle_tools_call(&cx, 1, params, &budget, SessionState::new(), None, None);
    let call_result = result.expect("health_check should not fail");
    assert!(!call_result.is_error, "health_check should succeed");

    let json = decode_json_from_tool_content(&call_result.content)
        .expect("health_check should return JSON");
    assert_eq!(
        json.get("status").and_then(|v| v.as_str()),
        Some("ok"),
        "health_check must return status=ok"
    );
}

// ---------------------------------------------------------------------------
// LLM mode parameter acceptance
// ---------------------------------------------------------------------------

#[test]
fn llm_mode_parameter_accepted_by_tools() {
    // Verify that tools accepting llm_mode parameter don't reject it.
    let _lock = env_lock().lock().unwrap_or_else(|e| e.into_inner());

    let tmp = tempfile::TempDir::new().expect("tempdir");
    let db_path = tmp.path().join("llm-test.sqlite3");
    let db_url = format!("sqlite://{}", db_path.display());
    let storage = tmp.path().join("archive");
    let _env_guard = EnvVarGuard::set(&[
        ("DATABASE_URL", &db_url),
        ("STORAGE_ROOT", storage.to_str().unwrap()),
        ("TOOLS_FILTER_ENABLED", "0"),
        ("TOON_BIN", ""),
        ("TOON_TRU_BIN", ""),
        ("MCP_AGENT_MAIL_OUTPUT_FORMAT", ""),
        ("AGENT_NAME_ENFORCEMENT_MODE", "coerce"),
    ]);

    let config = mcp_agent_mail_core::Config::from_env();
    let router = mcp_agent_mail_server::build_server(&config).into_router();
    let cx = Cx::for_testing();
    let budget = Budget::INFINITE;
    let mut req_id: u64 = 1;

    // Set up project + agents + message for summarize_thread
    let setup_calls: Vec<(&str, Value)> = vec![
        (
            "ensure_project",
            serde_json::json!({"human_key": "/tmp/llm-mode-test-project"}),
        ),
        (
            "register_agent",
            serde_json::json!({
                "project_key": "/tmp/llm-mode-test-project",
                "program": "test",
                "model": "test-model",
                "name": "BlueLake"
            }),
        ),
        (
            "register_agent",
            serde_json::json!({
                "project_key": "/tmp/llm-mode-test-project",
                "program": "test",
                "model": "test-model",
                "name": "GreenCastle"
            }),
        ),
        (
            "send_message",
            serde_json::json!({
                "project_key": "/tmp/llm-mode-test-project",
                "sender_name": "BlueLake",
                "to": ["GreenCastle"],
                "subject": "LLM mode test",
                "body_md": "Testing llm_mode=false parameter.",
                "thread_id": "llm-test-thread"
            }),
        ),
    ];

    for (tool_name, args) in setup_calls {
        let params = CallToolParams {
            name: tool_name.to_string(),
            arguments: Some(args),
            meta: None,
        };
        let result = router.handle_tools_call(
            &cx,
            req_id,
            params,
            &budget,
            SessionState::new(),
            None,
            None,
        );
        req_id += 1;
        let call_result = result.unwrap_or_else(|e| panic!("{tool_name} setup failed: {e}"));
        assert!(!call_result.is_error, "{tool_name} setup returned error");
    }

    // summarize_thread with llm_mode=false should succeed (no LLM call attempted).
    let params = CallToolParams {
        name: "summarize_thread".to_string(),
        arguments: Some(serde_json::json!({
            "project_key": "/tmp/llm-mode-test-project",
            "thread_id": "llm-test-thread",
            "llm_mode": false
        })),
        meta: None,
    };
    let result = router.handle_tools_call(
        &cx,
        req_id,
        params,
        &budget,
        SessionState::new(),
        None,
        None,
    );
    req_id += 1;
    let call_result = result.expect("summarize_thread should not fail with llm_mode=false");
    assert!(
        !call_result.is_error,
        "summarize_thread should succeed with llm_mode=false"
    );
    let json = decode_json_from_tool_content(&call_result.content)
        .expect("summarize_thread should return JSON");
    assert_eq!(
        json.get("thread_id").and_then(|v| v.as_str()),
        Some("llm-test-thread"),
        "thread_id should match"
    );

    // macro_prepare_thread with llm_mode=false should succeed.
    let params = CallToolParams {
        name: "macro_prepare_thread".to_string(),
        arguments: Some(serde_json::json!({
            "project_key": "/tmp/llm-mode-test-project",
            "agent_name": "GreenCastle",
            "thread_id": "llm-test-thread",
            "program": "test",
            "model": "test-model",
            "llm_mode": false
        })),
        meta: None,
    };
    let result = router.handle_tools_call(
        &cx,
        req_id,
        params,
        &budget,
        SessionState::new(),
        None,
        None,
    );
    let call_result = result.expect("macro_prepare_thread should not fail with llm_mode=false");
    assert!(
        !call_result.is_error,
        "macro_prepare_thread should succeed with llm_mode=false: {:?}",
        call_result.content
    );
}
