# Documentation Index

This index is a navigation map for the documentation tree. It is not a
replacement for the documents it links to, and it does not redefine any
contract in those documents.

## Start Here

| Need | Read |
|------|------|
| Operator recipes | [OPERATOR_COOKBOOK.md](OPERATOR_COOKBOOK.md) |
| Incident recovery | [RECOVERY_RUNBOOK.md](RECOVERY_RUNBOOK.md) |
| Deployment and troubleshooting | [OPERATOR_RUNBOOK.md](OPERATOR_RUNBOOK.md) |
| Developer setup | [DEVELOPER_GUIDE.md](DEVELOPER_GUIDE.md) |
| Release work | [RELEASE_CHECKLIST.md](RELEASE_CHECKLIST.md), [ROLLOUT_PLAYBOOK.md](ROLLOUT_PLAYBOOK.md) |
| Security posture | [SPEC-threat-model.md](SPEC-threat-model.md) |
| Testing realism policy | [VERIFICATION_COVERAGE_LEDGER.md](VERIFICATION_COVERAGE_LEDGER.md) |
| Product direction | [VISION.md](VISION.md) |

## Architecture Decisions

These explain why the main surfaces and subsystems are shaped as they are.

- [ADR-001-dual-mode-invariants.md](ADR-001-dual-mode-invariants.md) - MCP default, CLI opt-in, and dual-mode invariants.
- [ADR-002-single-binary-cli-opt-in.md](ADR-002-single-binary-cli-opt-in.md) - single binary behavior and runtime CLI opt-in.
- [ADR-003-search-v3-architecture.md](ADR-003-search-v3-architecture.md) - Search V3 architecture and routing.
- [DESIGN_git_lock.md](DESIGN_git_lock.md) - per-repo Git lock ordering and archive write serialization.
- [CYCLE_SEMANTICS.md](CYCLE_SEMANTICS.md) - cycle semantics for `br` and `bv` style workflows.
- [REGRESSION_BOUNDARIES.md](REGRESSION_BOUNDARIES.md) - what belongs in regression coverage.
- [RESOURCE_COVERAGE_AUDIT.md](RESOURCE_COVERAGE_AUDIT.md) - resource coverage audit.

## Specs And Contracts

Use these when changing behavior, APIs, command surfaces, or operator-visible
output.

- [SPEC-parity-matrix.md](SPEC-parity-matrix.md) - MCP, CLI, web, and TUI parity targets.
- [SPEC-interface-mode-switch.md](SPEC-interface-mode-switch.md) - runtime interface-mode switching.
- [SPEC-denial-ux-contract.md](SPEC-denial-ux-contract.md) - denial errors and exit-code policy.
- [SPEC-meta-command-allowlist.md](SPEC-meta-command-allowlist.md) - allowed MCP-mode meta commands.
- [SPEC-script-migration-matrix.md](SPEC-script-migration-matrix.md) - script-to-native migration map.
- [SPEC-bug-intake-policy.md](SPEC-bug-intake-policy.md) - bug intake, closure, and reopen policy.
- [SPEC-ephemeral-root-policy.md](SPEC-ephemeral-root-policy.md) - ephemeral-root and default-mailbox safety rules.
- [SPEC-mailbox-durability-states.md](SPEC-mailbox-durability-states.md) - mailbox durability state machine.
- [SPEC-doctor-report.md](SPEC-doctor-report.md) - `am doctor` report and capabilities schema.
- [SPEC-doctor-forensic-bundle-schema.md](SPEC-doctor-forensic-bundle-schema.md) - doctor forensic bundle schema.
- [SPEC-verify-live-contract.md](SPEC-verify-live-contract.md) - native verify-live contract.
- [SPEC-artifacts-bundle-schema.md](SPEC-artifacts-bundle-schema.md) - E2E, PTY, and fault-suite artifact bundle schema.
- [SPEC-threat-model.md](SPEC-threat-model.md) - consolidated threat model.
- [SPEC-web-ui-parity-contract.md](SPEC-web-ui-parity-contract.md) - web UI parity contract.
- [SPEC-browser-parity-contract-deferred.md](SPEC-browser-parity-contract-deferred.md) - deferred browser parity reference.
- [SPEC-tui-v2-product-contract.md](SPEC-tui-v2-product-contract.md) - deprecated pointer to [TUI_V2_CONTRACT.md](TUI_V2_CONTRACT.md).

## Search

- [ADR-003-search-v3-architecture.md](ADR-003-search-v3-architecture.md) - high-level Search V3 architecture.
- [SPEC-search-v3-query-contract.md](SPEC-search-v3-query-contract.md) - query, filter, and explain contract.
- [SPEC-search-v3-quality-gates.md](SPEC-search-v3-quality-gates.md) - relevance and performance gates.
- [SPEC-unified-search-corpus.md](SPEC-unified-search-corpus.md) - unified corpus schema and FTS migration strategy.
- [search-v3-component-mapping.md](search-v3-component-mapping.md) - component-level mapping dossier.

## Operations And Recovery

- [OPERATOR_COOKBOOK.md](OPERATOR_COOKBOOK.md) - copy-paste recipes for normal operator work.
- [OPERATOR_RUNBOOK.md](OPERATOR_RUNBOOK.md) - deployment, troubleshooting, and diagnostics.
- [OPERATOR_VERIFICATION_RUNBOOK.md](OPERATOR_VERIFICATION_RUNBOOK.md) - targeted operator verification.
- [RECOVERY_RUNBOOK.md](RECOVERY_RUNBOOK.md) - recovery playbook for broken mailbox state.
- [DUAL_MODE_ROLLOUT_PLAYBOOK.md](DUAL_MODE_ROLLOUT_PLAYBOOK.md) - dual-mode rollout and kill-switch workflow.
- [ROLLOUT_PLAYBOOK.md](ROLLOUT_PLAYBOOK.md) - staged rollout strategy.
- [RUNBOOK_LEGACY_PYTHON_TO_RUST_IMPORT.md](RUNBOOK_LEGACY_PYTHON_TO_RUST_IMPORT.md) - legacy Python to Rust import.
- [RUNBOOK-search-v3-migration.md](RUNBOOK-search-v3-migration.md) - Search V3 migration.
- [RUNBOOK-atc-rollback.md](RUNBOOK-atc-rollback.md) - ATC rollback.
- [MIGRATION_GUIDE.md](MIGRATION_GUIDE.md) - dual-mode migration guide.
- [MIGRATION_v16_to_v17.md](MIGRATION_v16_to_v17.md) - ATC v16 to v17 migration.

## Release And Cutover

- [RELEASE_CHECKLIST.md](RELEASE_CHECKLIST.md) - pre-release checklist.
- [RELEASE_READINESS_TEMPLATE.md](RELEASE_READINESS_TEMPLATE.md) - release-readiness template.
- [RELEASE_TRAIN_PLAN.md](RELEASE_TRAIN_PLAN.md) - release-train planning.
- [OLD_REPO_RUST_CUTOVER_PR_BODY.md](OLD_REPO_RUST_CUTOVER_PR_BODY.md) - historical Python-to-Rust cutover body.
- [TEMPLATE_OLD_REPO_RUST_CUTOVER_PR_CHECKLIST.md](TEMPLATE_OLD_REPO_RUST_CUTOVER_PR_CHECKLIST.md) - old-repo cutover checklist template.
- [TEMPLATE_OLD_REPO_RUST_CUTOVER_RELEASE_NOTES.md](TEMPLATE_OLD_REPO_RUST_CUTOVER_RELEASE_NOTES.md) - old-repo cutover release-notes template.

## TUI, Web, And ATC

- [TUI_HELP.md](TUI_HELP.md) - TUI help reference.
- [TUI_V2_CONTRACT.md](TUI_V2_CONTRACT.md) - TUI V2 product contract.
- [TUI_V2_PARITY_DIFF.md](TUI_V2_PARITY_DIFF.md) - TUI parity and differentiation matrix.
- [AGENT_HEALTH_SCORING.md](AGENT_HEALTH_SCORING.md) - agent health scoring.
- [SPEC-atc-core-contract.md](SPEC-atc-core-contract.md) - ATC core contract.
- [SPEC-atc-data-minimization.md](SPEC-atc-data-minimization.md) - ATC field-level data minimization.
- [SPEC-atc-privacy-policy.md](SPEC-atc-privacy-policy.md) - ATC privacy policy.
- [SPEC-atc-test-data-catalog.md](SPEC-atc-test-data-catalog.md) - ATC test-data catalog.
- [ATC_HOT_PATH_WIRING.md](ATC_HOT_PATH_WIRING.md) - ATC hot-path wiring.
- [ATC_OBSERVABILITY.md](ATC_OBSERVABILITY.md) - ATC observability contract.
- [ATC_PERF_BUDGETS.md](ATC_PERF_BUDGETS.md) - ATC performance budgets.
- [ATC_POLICY_SIMULATION.md](ATC_POLICY_SIMULATION.md) - ATC policy simulation.
- [ATC_ALERTS_EXAMPLE.yaml](ATC_ALERTS_EXAMPLE.yaml) - example ATC alert configuration.

## Audits, Incidents, And Performance

- [AUDIT-mailbox-durability-program.md](AUDIT-mailbox-durability-program.md) - mailbox durability audit.
- [CONFORMANCE_AUDIT_2026-04-18.md](CONFORMANCE_AUDIT_2026-04-18.md) - conformance audit snapshot.
- [DOC_SWEEP_AUDIT_2026-04-18.md](DOC_SWEEP_AUDIT_2026-04-18.md) - documentation sweep audit.
- [GIT_SHELLOUT_AUDIT.md](GIT_SHELLOUT_AUDIT.md) - Git shell-out audit.
- [GIT_SHELLOUT_AUDIT_COUNT.txt](GIT_SHELLOUT_AUDIT_COUNT.txt) - Git shell-out audit count.
- [GIT_251_FINDINGS.md](GIT_251_FINDINGS.md) - Git 2.51 findings.
- [OBSERVABILITY_git_251.md](OBSERVABILITY_git_251.md) - Git 2.51 observability contract.
- [INCIDENT_BR_2K3QX_A4_CLASSIFICATION_MATRIX.md](INCIDENT_BR_2K3QX_A4_CLASSIFICATION_MATRIX.md) - incident classification matrix.
- [INCIDENT_BR_2K3QX_CLOSURE_REPORT.md](INCIDENT_BR_2K3QX_CLOSURE_REPORT.md) - incident closure report.
- [INCIDENT_BR_2K3QX_G1_SURFACE_QUERY_CATALOG.json](INCIDENT_BR_2K3QX_G1_SURFACE_QUERY_CATALOG.json) - incident query catalog.
- [INCIDENT_BR_2K3QX_I1_TRIAGE_QUEUE.md](INCIDENT_BR_2K3QX_I1_TRIAGE_QUEUE.md) - incident triage queue.
- [INCIDENT_BR_LEGJY_G1_FRANKENSQLITE_PROFILE.md](INCIDENT_BR_LEGJY_G1_FRANKENSQLITE_PROFILE.md) - frankensqlite profile.
- [INCIDENT_BR_LEGJY_G2_FRANKENSEARCH_PROFILE.md](INCIDENT_BR_LEGJY_G2_FRANKENSEARCH_PROFILE.md) - frankensearch profile.
- [PERF-archive-batch-fix-design.md](PERF-archive-batch-fix-design.md) - archive batch write perf fix design.
- [PERF-archive-batch-profile-2026-04-18.md](PERF-archive-batch-profile-2026-04-18.md) - archive batch profile.
- [reality-check_2026-04-18_diff.md](reality-check_2026-04-18_diff.md) - reality-check diff.
- [reality-check_2026-04-18_findings.md](reality-check_2026-04-18_findings.md) - reality-check findings.
- [VERIFICATION_COVERAGE_LEDGER.md](VERIFICATION_COVERAGE_LEDGER.md) - verification coverage and closure policy.

## Reference

- [DEVELOPER_GUIDE.md](DEVELOPER_GUIDE.md) - developer setup and extension notes.
- [FLAGS_REGISTRY.md](FLAGS_REGISTRY.md) - runtime flags registry.
- [VISION.md](VISION.md) - product vision.
- [docs/examples/mcp/](examples/mcp/) - MCP configuration examples.
- [docs/schemas/git_251/](schemas/git_251/) - JSON schemas for Git 2.51 investigation artifacts.
- [docs/assets/](assets/) - README and social-preview assets.
- [docs/perf/flamegraphs/](perf/flamegraphs/) - checked-in performance flamegraphs.

## Planning And Historical Material

The [planning/](planning/) directory contains early Rust-port planning,
feature-parity notes, transition TODOs, and upgrade logs. Treat these as
historical context unless a current spec or runbook explicitly points back to
them.

The [archive/](archive/) and [post_cutover_cleanup/](post_cutover_cleanup/)
directories hold historical session notes and cutover cleanup records.
