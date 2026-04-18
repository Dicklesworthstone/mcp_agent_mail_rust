use mcp_agent_mail_conformance::Fixtures;
use std::collections::BTreeSet;
use std::fs;
use std::path::{Path, PathBuf};

const RESOURCE_AUDIT_RELATIVE: &str = "docs/RESOURCE_COVERAGE_AUDIT.md";
const PYTHON_FIXTURE_RELATIVE: &str = "tests/conformance/fixtures/python_reference.json";

#[derive(Debug, Clone, PartialEq, Eq)]
struct ResourceCoverageRow {
    name: String,
    fixture_status: String,
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

fn parse_resource_coverage_table(doc: &str) -> Vec<ResourceCoverageRow> {
    let heading_line = "## Coverage Matrix";
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
                "expected 6 cells in resource coverage row, got {cells:?}"
            );
            ResourceCoverageRow {
                name: cells[0].trim_matches('`').to_string(),
                fixture_status: cells[3].trim_matches('`').to_string(),
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
fn resource_coverage_audit_matches_live_inventory() {
    let audit_doc = read_file(workspace_root().join(RESOURCE_AUDIT_RELATIVE));
    for needle in [
        "# Resource Coverage Audit",
        "25 logical resource templates",
        "23 of the 25 templates",
        "resource://tooling/metrics_core",
        "resource://tooling/diagnostics",
        "br-a2k3h.6",
    ] {
        assert!(
            audit_doc.contains(needle),
            "resource audit doc should contain coverage summary needle {needle}"
        );
    }

    let rows = parse_resource_coverage_table(&audit_doc);
    let runtime_resources = collect_runtime_resources();
    assert_eq!(
        runtime_resources.len(),
        25,
        "logical resource template count drifted from resource audit baseline"
    );

    let actual_resources: BTreeSet<String> = rows.iter().map(|row| row.name.clone()).collect();
    assert_eq!(
        actual_resources, runtime_resources,
        "resource coverage audit matrix drifted from the live resource registry"
    );

    let python_resources: BTreeSet<String> = load_python_fixture()
        .resources
        .keys()
        .map(|uri| normalize_fixture_resource_uri(uri))
        .collect();
    let covered_resources: BTreeSet<String> = rows
        .iter()
        .filter(|row| row.fixture_status == "covered")
        .map(|row| row.name.clone())
        .collect();
    assert_eq!(
        covered_resources, python_resources,
        "covered resource set drifted from the Python-parity fixture inventory"
    );

    let gap_resources: BTreeSet<String> = rows
        .iter()
        .filter(|row| row.fixture_status == "gap")
        .map(|row| row.name.clone())
        .collect();
    let expected_gaps: BTreeSet<String> = runtime_resources
        .difference(&python_resources)
        .cloned()
        .collect();
    assert_eq!(
        gap_resources, expected_gaps,
        "resource gap set drifted from the live resource minus fixture inventory"
    );

    let expected_gap_baseline: BTreeSet<String> = [
        "resource://tooling/diagnostics",
        "resource://tooling/metrics_core",
    ]
    .into_iter()
    .map(str::to_string)
    .collect();
    assert_eq!(
        gap_resources, expected_gap_baseline,
        "resource audit baseline changed; add or remove fixture coverage intentionally and update the audit"
    );
}
