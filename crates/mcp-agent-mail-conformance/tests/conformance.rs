// Note: unsafe required for env::set_var in Rust 2024
#![allow(unsafe_code)]

use fastmcp::{Budget, CallToolParams, Content, Cx, ListToolsParams, ReadResourceParams};
use fastmcp_core::SessionState;
use mcp_agent_mail_conformance::{Case, ExpectedError, Fixtures, Normalize};
use serde::Deserialize;
use serde_json::Value;
use std::collections::BTreeMap;

fn normalize_pair(mut actual: Value, mut expected: Value, norm: &Normalize) -> (Value, Value) {
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
    }
}

fn load_tool_filter_fixtures() -> ToolFilterFixtures {
    let path =
        "tests/conformance/fixtures/tool_filter/cases.json";
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

/// Run all tool fixtures and return the storage root path for archive inspection.
fn run_all_fixtures_and_return_storage_root() -> tempfile::TempDir {
    let tmp = tempfile::TempDir::new().expect("failed to create tempdir");
    let db_path = tmp.path().join("db.sqlite3");
    let db_url = format!("sqlite://{}", db_path.display());
    let storage_root = tmp.path().join("archive");
    unsafe {
        std::env::set_var("DATABASE_URL", db_url);
        std::env::set_var("WORKTREES_ENABLED", "1");
        std::env::set_var(
            "STORAGE_ROOT",
            storage_root
                .to_str()
                .expect("storage_root must be valid UTF-8"),
        );
    }

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
    let cx = Cx::for_testing();
    let budget = Budget::INFINITE;
    let mut req_id: u64 = 1;

    // Run all tool fixtures (creates DB state + archive side effects)
    for (tool_name, tool_fixture) in &fixtures.tools {
        for case in &tool_fixture.cases {
            let params = CallToolParams {
                name: tool_name.clone(),
                arguments: args_from_case(case),
                meta: None,
            };
            let _result = router.handle_tools_call(
                &cx,
                req_id,
                params,
                &budget,
                SessionState::new(),
                None,
                None,
            );
            req_id += 1;
        }
    }

    tmp
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
    let tmp = tempfile::TempDir::new().expect("failed to create tempdir");
    // Use a file-backed SQLite DB for conformance.
    // `sqlite:///:memory:` creates a *separate* database per connection, which breaks our pooled
    // access patterns (updates may be invisible to later reads).
    // SAFETY: This test sets process env vars before any other access to DATABASE_URL.
    let db_path = tmp.path().join("db.sqlite3");
    let db_url = format!("sqlite://{}", db_path.display());
    let storage_root = tmp.path().join("archive");
    unsafe {
        std::env::set_var("DATABASE_URL", db_url);
        std::env::set_var("WORKTREES_ENABLED", "1");
        std::env::set_var(
            "STORAGE_ROOT",
            storage_root
                .to_str()
                .expect("storage_root must be valid UTF-8"),
        );
    }

    // Create fixture directories that guard tools expect (git-init'd repos)
    for repo_name in &["repo_install", "repo_uninstall"] {
        let repo_dir = std::path::Path::new("/tmp/agent-mail-fixtures").join(repo_name);
        std::fs::create_dir_all(&repo_dir).expect("create fixture repo dir");
        // Initialize a bare git repo so git commands work
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
}

#[test]
fn tool_filter_profiles_match_fixtures() {
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
            .handle_resources_read(
                &cx,
                1,
                &params,
                &budget,
                SessionState::new(),
                None,
                None,
            )
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

/// Single combined test for all archive artifact assertions.
/// Must be a single test because all fixtures share process-wide env vars
/// (DATABASE_URL, STORAGE_ROOT) which cannot safely run in parallel.
#[test]
fn archive_artifacts_after_fixtures() {
    let tmp = run_all_fixtures_and_return_storage_root();
    let storage_root = tmp.path().join("archive");
    let files = collect_archive_files(&storage_root);

    // Print all archive files for debugging
    eprintln!(
        "=== Archive files ({}) ===\n{}",
        files.len(),
        files.join("\n")
    );

    // --- .gitattributes ---
    assert!(
        storage_root.join(".gitattributes").exists(),
        "expected .gitattributes at archive root"
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
            "expected agent profile at {profile_rel}, found profiles:\n{}",
            files
                .iter()
                .filter(|f| f.contains("profile.json"))
                .cloned()
                .collect::<Vec<_>>()
                .join("\n")
        );
        let content = std::fs::read_to_string(storage_root.join(profile_rel))
            .unwrap_or_else(|e| panic!("failed to read {profile_rel}: {e}"));
        let parsed: Value = serde_json::from_str(&content)
            .unwrap_or_else(|e| panic!("failed to parse JSON in {profile_rel}: {e}"));
        assert!(
            parsed.get("name").and_then(Value::as_str).is_some(),
            "profile {profile_rel} missing 'name'"
        );
        assert!(
            parsed.get("program").and_then(Value::as_str).is_some(),
            "profile {profile_rel} missing 'program'"
        );
        assert!(
            parsed.get("model").and_then(Value::as_str).is_some(),
            "profile {profile_rel} missing 'model'"
        );
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

    // Verify frontmatter on each canonical
    for msg_rel in &message_files {
        let content = std::fs::read_to_string(storage_root.join(msg_rel))
            .unwrap_or_else(|e| panic!("failed to read {msg_rel}: {e}"));
        let fm = parse_frontmatter(&content).unwrap_or_else(|| {
            panic!("message {msg_rel} has no valid ---json frontmatter:\n{content}")
        });
        assert!(
            fm.get("from").and_then(Value::as_str).is_some(),
            "message {msg_rel} frontmatter missing 'from'"
        );
        assert!(
            fm.get("subject").and_then(Value::as_str).is_some(),
            "message {msg_rel} frontmatter missing 'subject'"
        );
        assert!(
            fm.get("to").and_then(Value::as_array).is_some(),
            "message {msg_rel} frontmatter missing 'to' array"
        );
        assert!(
            fm.get("id").is_some(),
            "message {msg_rel} frontmatter missing 'id'"
        );
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
    // Each canonical should have a matching outbox copy (same filename)
    for msg_rel in &message_files {
        let filename = std::path::Path::new(msg_rel.as_str())
            .file_name()
            .unwrap()
            .to_string_lossy();
        let has_outbox = outbox_files.iter().any(|f| f.ends_with(filename.as_ref()));
        assert!(
            has_outbox,
            "canonical {msg_rel} has no matching outbox copy (filename: {filename})"
        );
    }

    // --- File reservation artifacts ---
    let reservation_files: Vec<&String> = files
        .iter()
        .filter(|f| f.contains("/file_reservations/") && f.ends_with(".json"))
        .collect();
    assert!(
        !reservation_files.is_empty(),
        "expected at least one file reservation JSON artifact"
    );
    for res_rel in &reservation_files {
        let content = std::fs::read_to_string(storage_root.join(res_rel))
            .unwrap_or_else(|e| panic!("failed to read {res_rel}: {e}"));
        let parsed: Value = serde_json::from_str(&content)
            .unwrap_or_else(|e| panic!("failed to parse JSON in {res_rel}: {e}"));
        assert!(
            parsed.get("path_pattern").and_then(Value::as_str).is_some(),
            "reservation {res_rel} missing 'path_pattern'"
        );
        assert!(
            parsed.get("agent").or(parsed.get("agent_name")).is_some(),
            "reservation {res_rel} missing 'agent'/'agent_name'"
        );
        assert!(
            parsed.get("exclusive").is_some(),
            "reservation {res_rel} missing 'exclusive'"
        );
    }
    assert!(
        reservation_files.iter().any(|f| f.contains("/id-")),
        "expected at least one id-<N>.json reservation file"
    );

    // --- Thread digest ---
    let thread_files: Vec<&String> = files
        .iter()
        .filter(|f| f.contains("/messages/threads/") && f.ends_with(".md"))
        .collect();
    assert!(
        !thread_files.is_empty(),
        "expected at least one thread digest file"
    );
    for thread_rel in &thread_files {
        let content = std::fs::read_to_string(storage_root.join(thread_rel))
            .unwrap_or_else(|e| panic!("failed to read {thread_rel}: {e}"));
        assert!(
            content.starts_with("# Thread"),
            "thread digest {thread_rel} should start with '# Thread', got: {}",
            &content[..content.len().min(100)]
        );
        assert!(
            content.contains("[View canonical]"),
            "thread digest {thread_rel} should contain '[View canonical]' link"
        );
    }

    // --- Frontmatter parity with Python for "Hello" message ---
    let hello_candidates: Vec<&String> = files
        .iter()
        .filter(|f| f.contains("/messages/") && !f.contains("/threads/") && f.contains("__hello__"))
        .collect();
    assert!(
        !hello_candidates.is_empty(),
        "expected a 'hello' message canonical, found message files: {:?}",
        message_files
    );
    let content = std::fs::read_to_string(storage_root.join(hello_candidates[0]))
        .expect("failed to read hello message");
    let fm = parse_frontmatter(&content).expect("hello message has no frontmatter");
    assert_eq!(fm.get("from").and_then(Value::as_str), Some("BlueLake"));
    assert_eq!(fm.get("subject").and_then(Value::as_str), Some("Hello"));
    assert_eq!(
        fm.get("to")
            .and_then(Value::as_array)
            .map(|a| a.iter().filter_map(Value::as_str).collect::<Vec<_>>()),
        Some(vec!["GreenCastle"])
    );
    assert_eq!(fm.get("importance").and_then(Value::as_str), Some("urgent"));
    assert_eq!(fm.get("ack_required").and_then(Value::as_bool), Some(true));
    // Verify body
    let body_start = content.find("\n---\n").expect("no --- end marker");
    let body = content[body_start + 5..].trim();
    assert_eq!(body, "Test", "hello message body should be 'Test'");
}
