# Documentation Sweep Audit — 2026-04-18

Audit artifact for `br-o217s.9`.

## Current Truth Used For The Sweep

- MCP tools: `37`
- MCP resources: `25`
- TUI screens: `16`
- Operator CLI entrypoint: `am`
- MCP server entrypoint: `mcp-agent-mail`
- Canonical HTTP path: `/mcp/`
- Canonical web UI path: `/mail/`
- Canonical port: `8765`
- Current search stack naming in live product docs: `frankensearch`

## Method

1. Grep sweep across every `docs/**/*.md` file plus repo-root `*.md` files
   except `CHANGELOG.md`.
2. Pattern review for stale counts, binary names, transport paths, and
   pre-frankensearch search wording.
3. Inline fixes for living docs that should describe the current product.
4. Dated historical-context notes for preserved design/migration artifacts that
   intentionally keep older milestone text.
5. CI guard expansion for current-facing docs so the same drift is caught on PRs.

## Random-Sample Re-Reads

- `docs/SPEC-interface-mode-switch.md`
- `docs/SPEC-meta-command-allowlist.md`
- `docs/TUI_V2_CONTRACT.md`
- `docs/MIGRATION_GUIDE.md`
- `docs/RELEASE_CHECKLIST.md`

## Root/Planning Markdown Audit

| File | Findings | Action | Verification |
|---|---|---|---|
| `AGENTS.md` | none | no action | grep clean |
| `docs/planning/AGENT_MAIL_RUST_VERSION_REPO_TRANSITION_PLAN.md` | none | no action | grep clean |
| `docs/archive/agent-session-notes/CONTEXT_SETUP_LOG.md` | none | no action | grep clean |
| `docs/planning/EXISTING_MCP_AGENT_MAIL_STRUCTURE.md` | none | no action | grep clean |
| `docs/planning/FEATURE_PARITY.md` | `34 of 37 tools with fixtures` is current conformance wording | no action | retained as current wording |
| `docs/archive/agent-session-notes/INTRODUCTION.md` | none | no action | grep clean |
| `docs/planning/PLAN_TO_PORT_MCP_AGENT_MAIL_TO_RUST.md` | historical `20+ resources` / `35 tools` planning counts | added dated historical note | note added; file no longer reads as current surface doc |
| `docs/planning/PROPOSED_ARCHITECTURE.md` | none | no action | grep clean |
| `README.md` | no stale count drift in this sweep | no action | grep clean |
| `docs/RECOVERY_RUNBOOK.md` | none | no action | grep clean |
| `docs/planning/SYNC_STRATEGY.md` | none | no action | grep clean |
| `docs/planning/TODO.md` | none | no action | grep clean |
| `docs/planning/TODO_AGENT_MAIL_RUST_TRANSITION_EXECUTION.md` | none | no action | grep clean |
| `docs/planning/TODO_ATC_ALIEN_UPGRADES.md` | none | no action | grep clean |
| `docs/planning/UPGRADE_LOG.md` | none | no action | grep clean |
| `docs/VISION.md` | stale `11 screens` claim | fixed inline to `16 screens` | old phrase removed |
| `docs/planning/beads_import.md` | none | no action | grep clean |
| `docs/planning/beads_test_coverage.md` | none | no action | grep clean |

## `docs/` Markdown Audit

| File | Findings | Action | Verification |
|---|---|---|---|
| `docs/ADR-001-dual-mode-invariants.md` | superseded ADR still shows `mcp-agent-mail-cli` operator examples | added dated historical note | note added; current docs referenced |
| `docs/ADR-002-single-binary-cli-opt-in.md` | none | no action | grep clean |
| `docs/ADR-003-search-v3-architecture.md` | pre-frankensearch Tantivy design doc | added dated historical note | note added; current docs referenced |
| `docs/CONFORMANCE_AUDIT_2026-04-18.md` | none | no action | grep clean |
| `docs/CYCLE_SEMANTICS.md` | none | no action | grep clean |
| `docs/DEVELOPER_GUIDE.md` | none | no action | grep clean |
| `docs/DUAL_MODE_ROLLOUT_PLAYBOOK.md` | none requiring current-count edit | no action | grep clean |
| `docs/INCIDENT_BR_2K3QX_A4_CLASSIFICATION_MATRIX.md` | historical incident artifact | no action | existing historical context retained |
| `docs/INCIDENT_BR_2K3QX_CLOSURE_REPORT.md` | historical incident artifact | no action | existing historical context retained |
| `docs/INCIDENT_BR_2K3QX_I1_TRIAGE_QUEUE.md` | historical incident artifact | no action | existing historical context retained |
| `docs/INCIDENT_BR_LEGJY_G1_FRANKENSQLITE_PROFILE.md` | historical pre-16-screen phrasing already clearly incident-scoped | no action | retained as incident history |
| `docs/INCIDENT_BR_LEGJY_G2_FRANKENSEARCH_PROFILE.md` | historical pre-16-screen phrasing already clearly incident-scoped | no action | retained as incident history |
| `docs/MIGRATION_GUIDE.md` | early dual-mode migration guide predates `AM_INTERFACE_MODE` and current operator entrypoint wording | added dated historical note | note added; current docs referenced |
| `docs/OLD_REPO_RUST_CUTOVER_PR_BODY.md` | none in this sweep | no action | grep clean for target patterns |
| `docs/OPERATOR_COOKBOOK.md` | none | no action | grep clean |
| `docs/OPERATOR_RUNBOOK.md` | none | no action | grep clean |
| `docs/REGRESSION_BOUNDARIES.md` | no stale count drift found | no action | grep clean |
| `docs/RELEASE_CHECKLIST.md` | stale `all 11 screens load` release gate | fixed inline to `16 screens` | old phrase removed |
| `docs/ROLLOUT_PLAYBOOK.md` | none | no action | grep clean |
| `docs/RUNBOOK-atc-rollback.md` | none | no action | grep clean |
| `docs/RUNBOOK-search-v3-migration.md` | pre-frankensearch/Tantivy migration plan | added dated historical note | note added; current docs referenced |
| `docs/RUNBOOK_LEGACY_PYTHON_TO_RUST_IMPORT.md` | none | no action | grep clean |
| `docs/SPEC-artifacts-bundle-schema.md` | none | no action | grep clean |
| `docs/SPEC-atc-core-contract.md` | none | no action | grep clean |
| `docs/SPEC-atc-data-minimization.md` | none | no action | grep clean |
| `docs/SPEC-atc-privacy-policy.md` | none | no action | grep clean |
| `docs/SPEC-browser-parity-contract-deferred.md` | current deferred-state wording is intentional | no action | retained |
| `docs/SPEC-bug-intake-policy.md` | none | no action | grep clean |
| `docs/SPEC-denial-ux-contract.md` | none | no action | grep clean |
| `docs/SPEC-doctor-forensic-bundle-schema.md` | none | no action | grep clean |
| `docs/SPEC-ephemeral-root-policy.md` | none | no action | grep clean |
| `docs/SPEC-interface-mode-switch.md` | stale CLI-equivalent examples (`mcp-agent-mail serve-http` / `serve-stdio`) | fixed inline to `am serve-http` / `am serve-stdio` and trimmed old binary spelling | old phrases removed |
| `docs/SPEC-mailbox-durability-states.md` | none | no action | grep clean |
| `docs/SPEC-meta-command-allowlist.md` | stale operator command example (`mcp-agent-mail-cli doctor check`) | fixed inline to `am doctor check` | old phrase removed |
| `docs/SPEC-parity-matrix.md` | none | no action | grep clean |
| `docs/SPEC-script-migration-matrix.md` | none requiring current-count edit | no action | grep clean |
| `docs/SPEC-search-v3-quality-gates.md` | pre-frankensearch Tantivy gate doc | added dated historical note | note added; current docs referenced |
| `docs/SPEC-search-v3-query-contract.md` | pre-frankensearch Tantivy contract | added dated historical note | note added; current docs referenced |
| `docs/SPEC-threat-model.md` | none | no action | grep clean |
| `docs/SPEC-tui-v2-product-contract.md` | already deprecated pointer | no action | retained |
| `docs/SPEC-unified-search-corpus.md` | none in this sweep | no action | grep clean |
| `docs/SPEC-verify-live-contract.md` | none | no action | grep clean |
| `docs/SPEC-web-ui-parity-contract.md` | none | no action | grep clean |
| `docs/TEMPLATE_OLD_REPO_RUST_CUTOVER_PR_CHECKLIST.md` | none in this sweep | no action | grep clean for target patterns |
| `docs/TEMPLATE_OLD_REPO_RUST_CUTOVER_RELEASE_NOTES.md` | none | no action | grep clean |
| `docs/TUI_V2_CONTRACT.md` | 12-screen V2 milestone now stale as live count | added dated historical note | note added; current docs referenced |
| `docs/TUI_V2_PARITY_DIFF.md` | none in this sweep | no action | grep clean |
| `docs/VERIFICATION_COVERAGE_LEDGER.md` | none | no action | grep clean |
| `docs/post_cutover_cleanup/decommission_validation_report.md` | none | no action | grep clean |
| `docs/search-v3-component-mapping.md` | pre-frankensearch Tantivy dossier | added dated historical note | note added; current docs referenced |

## CI Guard Added

Expanded the CI docs drift guard from README/AGENTS-only assumptions to a
broader live-doc set:

- `README.md`
- `AGENTS.md`
- `docs/VISION.md`
- `docs/OPERATOR_RUNBOOK.md`
- `docs/OPERATOR_COOKBOOK.md`
- `docs/RELEASE_CHECKLIST.md`
- `docs/SPEC-interface-mode-switch.md`
- `docs/SPEC-meta-command-allowlist.md`

Guarded stale phrases include:

- `34 tools`
- `36 tools`
- `20+ resources`
- `33 resources`
- `15 screens`
- `11 screens`
- `mcp-agent-mail serve-http`
- `mcp-agent-mail serve-stdio`
- `mcp-agent-mail-cli share`
- `mcp-agent-mail-cli doctor`
- `mcp-agent-mail-cli guard`

Historical design docs are intentionally excluded from the CI guard once they
carry an explicit dated note, so the repo preserves design history without
pretending those texts describe the live surface.
