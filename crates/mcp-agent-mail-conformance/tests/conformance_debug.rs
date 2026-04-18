use mcp_agent_mail_conformance::Fixtures;
use serde::Deserialize;
use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Path, PathBuf};

const AUDIT_DOC_RELATIVE: &str = "docs/CONFORMANCE_AUDIT_2026-04-18.md";
const README_RELATIVE: &str = "README.md";
const PYTHON_FIXTURE_RELATIVE: &str = "tests/conformance/fixtures/python_reference.json";
const TOOL_FILTER_FIXTURE_RELATIVE: &str = "tests/conformance/fixtures/tool_filter/cases.json";

#[derive(Debug, Deserialize)]
struct ToolFilterFixtures {
    cases: Vec<ToolFilterCase>,
}

#[derive(Debug, Deserialize)]
struct ToolFilterCase {
    #[serde(default)]
    expected_tools: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct AuditRow {
    name: String,
    has_fixture: String,
    classification: String,
    fixture_file: String,
}

fn crate_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

fn workspace_root() -> PathBuf {
    crate_root()
        .parent()
        .and_then(Path::parent)
        .expect("crate should have a workspace root")
        .to_path_buf()
}

fn read_file(path: impl AsRef<Path>) -> String {
    let path = path.as_ref();
    fs::read_to_string(path).unwrap_or_else(|e| panic!("failed to read {}: {e}", path.display()))
}

fn load_python_fixture() -> Fixtures {
    Fixtures::load(crate_root().join(PYTHON_FIXTURE_RELATIVE)).expect("python fixture should load")
}

fn load_tool_filter_fixture() -> ToolFilterFixtures {
    let path = crate_root().join(TOOL_FILTER_FIXTURE_RELATIVE);
    serde_json::from_str(&read_file(&path))
        .unwrap_or_else(|e| panic!("failed to parse {}: {e}", path.display()))
}

fn parse_table(doc: &str, heading: &str) -> Vec<AuditRow> {
    let heading_line = format!("## {heading}");
    let mut in_section = false;
    let mut table_lines: Vec<&str> = Vec::new();

    for line in doc.lines() {
        if line.trim() == heading_line {
            in_section = true;
            continue;
        }
        if !in_section {
            continue;
        }
        if line.starts_with("## ") {
            break;
        }
        if line.trim_start().starts_with('|') {
            table_lines.push(line);
        }
    }

    assert!(
        table_lines.len() >= 3,
        "expected markdown table under heading {heading_line}"
    );

    table_lines
        .into_iter()
        .skip(2)
        .map(|line| {
            let cells: Vec<String> = line
                .trim()
                .trim_matches('|')
                .split('|')
                .map(|cell| cell.trim().to_string())
                .collect();
            assert_eq!(
                cells.len(),
                6,
                "expected 6 cells in audit table row under {heading_line}, got {cells:?}"
            );
            AuditRow {
                name: cells[0].clone(),
                has_fixture: cells[1].clone(),
                classification: cells[3].clone(),
                fixture_file: cells[4].clone(),
            }
        })
        .collect()
}

fn normalize_runtime_resource_uri(uri: &str) -> String {
    uri.strip_suffix("?{query}").unwrap_or(uri).to_string()
}

fn normalize_fixture_resource_uri(uri: &str) -> String {
    if uri.starts_with("resource://config/environment") {
        return "resource://config/environment".to_string();
    }
    if uri.starts_with("resource://projects") {
        return "resource://projects".to_string();
    }
    if uri.starts_with("resource://project/") {
        return "resource://project/{slug}".to_string();
    }
    if uri.starts_with("resource://identity/") {
        return "resource://identity/{project}".to_string();
    }
    if uri.starts_with("resource://agents/") {
        return "resource://agents/{project_key}".to_string();
    }
    if uri.starts_with("resource://product/") {
        return "resource://product/{key}".to_string();
    }
    if uri.starts_with("resource://inbox/") {
        return "resource://inbox/{agent}".to_string();
    }
    if uri.starts_with("resource://message/") {
        return "resource://message/{message_id}".to_string();
    }
    if uri.starts_with("resource://thread/") {
        return "resource://thread/{thread_id}".to_string();
    }
    if uri.starts_with("resource://file_reservations/") {
        return "resource://file_reservations/{slug}".to_string();
    }
    if uri.starts_with("resource://tooling/capabilities/") {
        return "resource://tooling/capabilities/{agent}".to_string();
    }
    if uri.starts_with("resource://tooling/recent/") {
        return "resource://tooling/recent/{window_seconds}".to_string();
    }
    if uri.starts_with("resource://views/urgent-unread/") {
        return "resource://views/urgent-unread/{agent}".to_string();
    }
    if uri.starts_with("resource://views/ack-required/") {
        return "resource://views/ack-required/{agent}".to_string();
    }
    if uri.starts_with("resource://views/acks-stale/") {
        return "resource://views/acks-stale/{agent}".to_string();
    }
    if uri.starts_with("resource://views/ack-overdue/") {
        return "resource://views/ack-overdue/{agent}".to_string();
    }
    if uri.starts_with("resource://mailbox-with-commits/") {
        return "resource://mailbox-with-commits/{agent}".to_string();
    }
    if uri.starts_with("resource://mailbox/") {
        return "resource://mailbox/{agent}".to_string();
    }
    if uri.starts_with("resource://outbox/") {
        return "resource://outbox/{agent}".to_string();
    }
    uri.split('?').next().unwrap_or(uri).to_string()
}

fn collect_runtime_resources() -> BTreeSet<String> {
    let mut config = mcp_agent_mail_core::Config::from_env();
    config.tool_filter.enabled = false;
    config.worktrees_enabled = true;

    let router = mcp_agent_mail_server::build_server(&config).into_router();
    let mut resources = BTreeSet::new();
    for resource in router.resources() {
        resources.insert(normalize_runtime_resource_uri(&resource.uri));
    }
    for template in router.resource_templates() {
        resources.insert(normalize_runtime_resource_uri(&template.uri_template));
    }
    resources
}

#[test]
fn audit_doc_matches_live_inventory() {
    let audit_doc = read_file(workspace_root().join(AUDIT_DOC_RELATIVE));
    for heading in [
        "Tool coverage table",
        "Resource coverage table",
        "Skip/ignore inventory",
        "Test-run output",
        "Mystery states",
    ] {
        assert!(
            audit_doc.contains(&format!("## {heading}")),
            "audit doc should contain section {heading}"
        );
    }

    let tool_rows = parse_table(&audit_doc, "Tool coverage table");
    let resource_rows = parse_table(&audit_doc, "Resource coverage table");

    let python_fixture = load_python_fixture();
    let python_tools: BTreeSet<String> = python_fixture.tools.keys().cloned().collect();

    let mut rust_native_tools = BTreeSet::new();
    for case in load_tool_filter_fixture().cases {
        for tool in case.expected_tools {
            if !python_tools.contains(&tool) {
                rust_native_tools.insert(tool);
            }
        }
    }

    let runtime_tools: BTreeSet<String> = mcp_agent_mail_tools::TOOL_CLUSTER_MAP
        .iter()
        .map(|(name, _cluster)| (*name).to_string())
        .collect();
    assert_eq!(
        runtime_tools.len(),
        37,
        "tool count drifted from audit baseline"
    );

    let mut expected_tool_rows = BTreeMap::new();
    for tool in &runtime_tools {
        let expected = if python_tools.contains(tool) {
            AuditRow {
                name: tool.clone(),
                has_fixture: "yes".to_string(),
                classification: "python-parity".to_string(),
                fixture_file: "crates/mcp-agent-mail-conformance/tests/conformance/fixtures/python_reference.json".to_string(),
            }
        } else if rust_native_tools.contains(tool) {
            AuditRow {
                name: tool.clone(),
                has_fixture: "yes".to_string(),
                classification: "rust-native".to_string(),
                fixture_file: "crates/mcp-agent-mail-conformance/tests/conformance/fixtures/tool_filter/cases.json".to_string(),
            }
        } else {
            AuditRow {
                name: tool.clone(),
                has_fixture: "no".to_string(),
                classification: "unknown".to_string(),
                fixture_file: "none".to_string(),
            }
        };

        eprintln!(
            "conformance.audit.tool_classified {{ tool: {}, classification: {}, source_evidence: {} }}",
            expected.name, expected.classification, expected.fixture_file
        );
        if expected.classification == "unknown" {
            eprintln!(
                "conformance.audit.mystery_flagged {{ kind: tool, details: {}, followup_bead_proposed: br-a2k3h.3/br-a2k3h.6 }}",
                expected.name
            );
        }
        expected_tool_rows.insert(tool.clone(), expected);
    }

    assert_eq!(
        tool_rows.len(),
        expected_tool_rows.len(),
        "tool table row count should match live tool inventory"
    );
    for row in &tool_rows {
        let expected = expected_tool_rows
            .get(&row.name)
            .unwrap_or_else(|| panic!("unexpected tool row {}", row.name));
        assert_eq!(row, expected, "tool audit row drifted for {}", row.name);
    }

    let python_resources: BTreeSet<String> = python_fixture
        .resources
        .keys()
        .map(|uri| normalize_fixture_resource_uri(uri))
        .collect();
    let runtime_resources = collect_runtime_resources();
    assert_eq!(
        runtime_resources.len(),
        25,
        "resource template count drifted from audit baseline"
    );

    let mut expected_resource_rows = BTreeMap::new();
    for resource in &runtime_resources {
        let expected = if python_resources.contains(resource) {
            AuditRow {
                name: resource.clone(),
                has_fixture: "yes".to_string(),
                classification: "python-parity".to_string(),
                fixture_file: "crates/mcp-agent-mail-conformance/tests/conformance/fixtures/python_reference.json".to_string(),
            }
        } else {
            AuditRow {
                name: resource.clone(),
                has_fixture: "no".to_string(),
                classification: "rust-native".to_string(),
                fixture_file: "none".to_string(),
            }
        };

        eprintln!(
            "conformance.audit.resource_classified {{ template: {}, classification: {}, source_evidence: {} }}",
            expected.name, expected.classification, expected.fixture_file
        );
        if expected.classification != "python-parity" {
            eprintln!(
                "conformance.audit.mystery_flagged {{ kind: resource, details: {}, followup_bead_proposed: br-a2k3h.4/br-a2k3h.6 }}",
                expected.name
            );
        }
        expected_resource_rows.insert(resource.clone(), expected);
    }

    assert_eq!(
        resource_rows.len(),
        expected_resource_rows.len(),
        "resource table row count should match live resource inventory"
    );
    for row in &resource_rows {
        let expected = expected_resource_rows
            .get(&row.name)
            .unwrap_or_else(|| panic!("unexpected resource row {}", row.name));
        assert_eq!(row, expected, "resource audit row drifted for {}", row.name);
    }

    for needle in [
        "list_agents",
        "resource://tooling/metrics_core",
        "resource://tooling/diagnostics",
        "br-a2k3h.3",
        "br-a2k3h.4",
        "br-a2k3h.6",
        "br-0ijq8",
    ] {
        assert!(
            audit_doc.contains(needle),
            "audit doc should mention mystery or blocker marker {needle}"
        );
    }

    eprintln!(
        "conformance.audit.complete {{ total_tools: {}, total_resources: {}, mysteries: 3, followups_proposed: br-a2k3h.3/br-a2k3h.4/br-a2k3h.6 }}",
        expected_tool_rows.len(),
        expected_resource_rows.len()
    );
}

fn crate_readme_current_coverage_matches_audit_summary() {
    let readme = read_file(crate_root().join(README_RELATIVE));
    for needle in [
        "# mcp-agent-mail-conformance",
        "## Current coverage (as of 2026-04-18)",
        "37 tools",
        "34 tools have Python behavior fixtures",
        "resolve_pane_identity",
        "cleanup_pane_identities",
        "list_agents",
        "25 logical resource templates",
        "23 resource templates have Python behavior fixtures",
        "resource://tooling/metrics_core",
        "resource://tooling/diagnostics",
        "br-a2k3h.3",
        "br-a2k3h.4",
        "br-a2k3h.6",
        "br-0ijq8",
        "CONFORMANCE_AUDIT_2026-04-18.md",
    ] {
        assert!(
            readme.contains(needle),
            "crate README should contain coverage summary needle {needle}"
        );
    }
}
