# Share/Export Conformance Fixtures

Deterministic test data for the share/export pipeline.

## Fixture DBs

| File | Description |
|------|-------------|
| `minimal.sqlite3` | 1 project, 1 agent, 1 message (ack_required=1, read+acked) |
| `with_attachments.sqlite3` | 1 project, 1 agent, 1 message with 2 file-type attachments (one has secret keys in metadata) |
| `needs_scrub.sqlite3` | 2 projects, 2 agents, 3 messages with secrets in subject/body/attachments, file reservations, agent links, sibling suggestions |

## Expected Outputs

| File | Tests |
|------|-------|
| `expected_standard.json` | ScrubSummary + post-scrub DB state for `standard` preset |
| `expected_strict.json` | ScrubSummary + post-scrub DB state for `strict` preset |
| `expected_archive.json` | ScrubSummary + post-scrub DB state for `archive` preset |
| `expected_scoped.json` | ProjectScopeResult for selecting only `proj-alpha` from multi-project DB |
| `expected_fts_ddl.sql` | Exact FTS5 DDL for the search index |
| `expected_views_ddl.sql` | Exact DDL for materialized views |
| `source_db_hashes.json` | SHA256 hashes of source fixture DBs |

## How to Regenerate

From the repo root (requires legacy Python environment):

```bash
cd legacy_python_mcp_agent_mail_code/mcp_agent_mail
uv run python ../../crates/mcp-agent-mail-conformance/tests/conformance/fixtures/share/generate_share_fixtures.py
```

The generator creates fixture DBs with deterministic data (fixed timestamps, predictable IDs) and runs the legacy Python `scrub_snapshot()` and `apply_project_scope()` functions to produce expected outputs.

## Using in Rust Tests

```rust
#[test]
fn test_scrub_standard_preset() {
    let fixture_db = fixture_path("share/needs_scrub.sqlite3");
    let expected: Value = load_json("share/expected_standard.json");

    // Copy fixture to temp, run Rust scrub, compare ScrubSummary fields
    let copy = temp_copy(&fixture_db);
    let summary = scrub_snapshot(&copy, "standard", None);
    assert_eq!(summary.secrets_replaced, expected["summary"]["secrets_replaced"]);
    // ... compare all fields
}
```

## Secrets Embedded in `needs_scrub.sqlite3`

These are intentionally embedded for scrub testing:
- Message 1 subject: `ghp_aBcDeFgHiJkLmNoPqRsTuVwXyZ0123456789` (GitHub PAT)
- Message 1 body: `sk-abcdef0123456789012345` (API key) + `bearer MyToken1234567890123456` (Bearer token)
- Message 2 body: JWT token (`eyJ...`)
- Message 3 body: `xoxb-1234567890-abcdefghij` (Slack) + `github_pat_...` (fine-grained PAT)
- Message 3 attachments: `download_url`, `signed_url`, `authorization`, `bearer_token` keys (should be stripped)
