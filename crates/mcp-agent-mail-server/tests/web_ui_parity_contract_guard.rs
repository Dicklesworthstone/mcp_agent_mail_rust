#![forbid(unsafe_code)]

use std::collections::HashSet;
use std::fs;
use std::path::PathBuf;

use serde::Deserialize;

#[derive(Debug, Deserialize)]
struct WebUiParityContract {
    schema_version: u32,
    rows: Vec<WebUiParityRow>,
}

#[derive(Debug, Deserialize)]
struct WebUiParityRow {
    id: String,
    category: String,
    method: String,
    python_path: String,
    rust_path: Option<String>,
    policy: String,
    status: String,
    owner_beads: Vec<String>,
    evidence: Vec<String>,
    notes: String,
}

const START_MARKER: &str = "<!-- WEB_UI_PARITY_CONTRACT_JSON_START -->";
const END_MARKER: &str = "<!-- WEB_UI_PARITY_CONTRACT_JSON_END -->";

fn repo_root_from_manifest_dir() -> PathBuf {
    // crates/mcp-agent-mail-server -> crates -> repo root
    let manifest = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    manifest
        .parent()
        .and_then(|p| p.parent())
        .expect("CARGO_MANIFEST_DIR should be <repo>/crates/mcp-agent-mail-server")
        .to_path_buf()
}

fn extract_json_block(markdown: &str) -> String {
    let start = markdown
        .find(START_MARKER)
        .expect("missing START_MARKER for web UI parity contract JSON block");
    let end = markdown
        .find(END_MARKER)
        .expect("missing END_MARKER for web UI parity contract JSON block");
    assert!(
        start < end,
        "web UI parity contract markers are out of order"
    );

    let slice = &markdown[start..end];

    let fence_open = slice
        .find("```json")
        .expect("missing ```json fence inside parity contract markers");
    let after_open = &slice[fence_open + "```json".len()..];
    let after_open = after_open
        .strip_prefix('\n')
        .or_else(|| after_open.strip_prefix("\r\n"))
        .unwrap_or(after_open);

    let fence_close = after_open
        .find("```")
        .expect("missing closing ``` fence for parity contract JSON block");
    after_open[..fence_close].trim().to_string()
}

#[test]
fn web_ui_parity_contract_has_owners_and_known_status() {
    let repo_root = repo_root_from_manifest_dir();
    let contract_path = repo_root.join("docs/SPEC-web-ui-parity-contract.md");
    let markdown = fs::read_to_string(&contract_path).expect("read parity contract markdown");

    let json_str = extract_json_block(&markdown);
    let contract: WebUiParityContract =
        serde_json::from_str(&json_str).expect("parse parity contract JSON");

    assert_eq!(
        contract.schema_version, 1,
        "unexpected schema_version for web parity contract"
    );
    assert!(
        !contract.rows.is_empty(),
        "web parity contract must contain at least 1 row"
    );

    let allowed_policies: HashSet<&'static str> =
        HashSet::from(["must_match", "approved_difference"]);
    let allowed_statuses: HashSet<&'static str> =
        HashSet::from(["implemented", "partial", "gap", "waived", "pending_review"]);

    let mut seen_ids: HashSet<String> = HashSet::new();
    for row in &contract.rows {
        assert!(
            seen_ids.insert(row.id.clone()),
            "duplicate row id in parity contract: {}",
            row.id
        );

        assert!(
            !row.category.trim().is_empty(),
            "row {}: category must be non-empty",
            row.id
        );
        assert!(
            !row.method.trim().is_empty(),
            "row {}: method must be non-empty",
            row.id
        );
        assert!(
            !row.python_path.trim().is_empty(),
            "row {}: python_path must be non-empty",
            row.id
        );

        assert!(
            allowed_policies.contains(row.policy.as_str()),
            "row {}: unknown policy: {}",
            row.id,
            row.policy
        );

        assert_ne!(
            row.status.as_str(),
            "unknown",
            "row {}: status must not be 'unknown'",
            row.id
        );
        assert!(
            allowed_statuses.contains(row.status.as_str()),
            "row {}: unknown status: {}",
            row.id,
            row.status
        );

        if row.status == "waived" {
            assert_eq!(
                row.policy, "approved_difference",
                "row {}: status=waived requires policy=approved_difference",
                row.id
            );
        }

        assert!(
            !row.owner_beads.is_empty(),
            "row {}: owner_beads must be non-empty",
            row.id
        );

        // Evidence and notes are required to keep the contract actionable.
        assert!(
            !row.evidence.is_empty(),
            "row {}: evidence must be non-empty",
            row.id
        );
        assert!(
            !row.notes.trim().is_empty(),
            "row {}: notes must be non-empty",
            row.id
        );

        // rust_path may be null for gaps, but if present it should not be empty.
        if let Some(rust_path) = row.rust_path.as_deref() {
            assert!(
                !rust_path.trim().is_empty(),
                "row {}: rust_path must not be empty when present",
                row.id
            );
        }
    }
}
