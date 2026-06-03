# Documentation Index

> **Status:** Draft — proposed navigation for the 80+ doc files in this directory.
> **Source of truth:** every entry here links to a real file. This index is the table of contents, not the content.
> **Companion:** [README.md § Table of Contents](../../README.md#table-of-contents) (user-facing entry point).

## Why this index

This `docs/` directory has grown to 80+ files spanning architecture decisions, operational runbooks, SPECs, audits, incident postmortems, and design notes. The README links to a subset; many of the deeper operational / spec / ADR files are reachable only by `ls docs/` and reading the filename.

This index groups the files by topic so a new operator / contributor / auditor can find what they need without guessing from filenames.

> **How to read this:** pick the section that matches your question, then click the file. The "Read this when…" line under each entry tells you what the doc is *for*, not what is in it.

## Architecture & decision records (ADR)

Read these when you want to know **why the system is shaped the way it is** — the trade-offs that produced the current design.

- [ADR-001-dual-mode-invariants](ADR-001-dual-mode-invariants.md) — Read this when you wonder why a single binary has both a Rust core and a Python surface.
- [ADR-002-single-binary-cli-opt-in](ADR-002-single-binary-cli-opt-in.md) — Read this when you wonder why the CLI is bundled into one binary rather than split.
- [ADR-003-search-v3-architecture](ADR-003-search-v3-architecture.md) — Read this when you wonder why search has a v2 and a v3 and which is canonical.

## Specs (SPEC)

Read these when you want to know **what the system is supposed to do** — the contracts that implementations must satisfy.

### Core contracts

- [SPEC-parity-matrix](SPEC-parity-matrix.md) — Read this when you need to know which surface (MCP / CLI / Web / TUI) supports which operation.
- [SPEC-artifacts-bundle-schema](SPEC-artifacts-bundle-schema.md) — Read this when you are writing or consuming an artifact bundle.
- [SPEC-threat-model](SPEC-threat-model.md) — Read this when you are reviewing a security-relevant change.

### Mode / lifecycle

- [SPEC-interface-mode-switch](SPEC-interface-mode-switch.md) — Read this when you are implementing a mode switch or debugging mode drift.
- [SPEC-cycle-semantics](CYCLE_SEMANTICS.md) — Read this when you are tuning a background cycle.
- [SPEC-ephemeral-root-policy](SPEC-ephemeral-root-policy.md) — Read this when you are configuring or migrating an ephemeral root.
- [SPEC-mailbox-durability-states](SPEC-mailbox-durability-states.md) — Read this when you are reasoning about a mailbox's durability state.

### Search v3

- [SPEC-search-v3-query-contract](SPEC-search-v3-query-contract.md) — Read this when you are writing a search query or a new search tool.
- [SPEC-search-v3-quality-gates](SPEC-search-v3-quality-gates.md) — Read this when you wonder what "good enough" means for a v3 search result.
- [SPEC-unified-search-corpus](SPEC-unified-search-corpus.md) — Read this when you wonder which corpora the unified search walks.
- [search-v3-component-mapping](search-v3-component-mapping.md) — Read this when you are tracing a v3 query through the codebase.

### Operational

- [SPEC-atc-core-contract](SPEC-atc-core-contract.md) — Read this when you are adding or changing an ATC alert.
- [SPEC-atc-data-minimization](SPEC-atc-data-minimization.md) — Read this when you are reviewing an ATC change for data exposure.
- [SPEC-atc-privacy-policy](SPEC-atc-privacy-policy.md) — Read this when you are tuning what ATC stores.
- [SPEC-atc-test-data-catalog](SPEC-atc-test-data-catalog.md) — Read this when you are picking test data for an ATC PR.
- [SPEC-doctor-report](SPEC-doctor-report.md) — Read this when you are parsing the output of `am doctor`.
- [SPEC-doctor-forensic-bundle-schema](SPEC-doctor-forensic-bundle-schema.md) — Read this when you are decoding a doctor forensic bundle.
- [SPEC-denial-ux-contract](SPEC-denial-ux-contract.md) — Read this when you are writing or testing a denial error.
- [SPEC-meta-command-allowlist](SPEC-meta-command-allowlist.md) — Read this when you are reviewing which meta-commands are exposed.
- [SPEC-bug-intake-policy](SPEC-bug-intake-policy.md) — Read this when you are triaging an incoming bug.
- [SPEC-verify-live-contract](SPEC-verify-live-contract.md) — Read this when you are running `am verify-live`.
- [SPEC-script-migration-matrix](SPEC-script-migration-matrix.md) — Read this when you are migrating a script from Python to Rust.
- [SPEC-tui-v2-product-contract](SPEC-tui-v2-product-contract.md) — Read this when you are working on the TUI v2.
- [SPEC-web-ui-parity-contract](SPEC-web-ui-parity-contract.md) — Read this when you are working on the web UI.
- [SPEC-browser-parity-contract-deferred](SPEC-browser-parity-contract-deferred.md) — Read this when you are wondering what the browser parity plan *was* and why it was deferred.

## Runbooks

Read these when something is **on fire** and you need to act, not learn.

- [RECOVERY_RUNBOOK](RECOVERY_RUNBOOK.md) — Read this when the system is in a state that `am doctor` flagged as broken.
- [OPERATOR_RUNBOOK](OPERATOR_RUNBOOK.md) — Read this when you are the operator on call and need the playbook of last resort.
- [OPERATOR_COOKBOOK](OPERATOR_COOKBOOK.md) — Read this when you want a recipe for a common operational task.
- [OPERATOR_VERIFICATION_RUNBOOK](OPERATOR_VERIFICATION_RUNBOOK.md) — Read this when you need to verify a fix without a full rollout.
- [RUNBOOK-atc-rollback](RUNBOOK-atc-rollback.md) — Read this when ATC needs to be rolled back.
- [RUNBOOK-search-v3-migration](RUNBOOK-search-v3-migration.md) — Read this when migrating from search v2 to v3 in production.
- [RUNBOOK_LEGACY_PYTHON_TO_RUST_IMPORT](RUNBOOK_LEGACY_PYTHON_TO_RUST_IMPORT.md) — Read this when you are importing a legacy Python archive.
- [DUAL_MODE_ROLLOUT_PLAYBOOK](DUAL_MODE_ROLLOUT_PLAYBOOK.md) — Read this when you are rolling out the dual-mode binary.
- [ROLLOUT_PLAYBOOK](ROLLOUT_PLAYBOOK.md) — Read this when you are rolling out a release in general.
- [RELEASE_CHECKLIST](RELEASE_CHECKLIST.md) — Read this when you are cutting a release.
- [RELEASE_READINESS_TEMPLATE](RELEASE_READINESS_TEMPLATE.md) — Read this when you are filling out a release-readiness form.
- [RELEASE_TRAIN_PLAN](RELEASE_TRAIN_PLAN.md) — Read this when you are planning a release train.

## Design notes (DESIGN / PLAN / PROPOSED)

Read these when you want to know **what a future or past design exploration concluded** — usually pre-ADR or post-ADR.

- [PROPOSED_ARCHITECTURE](../../PROPOSED_ARCHITECTURE.md) — Read this when you want the high-level shape of the system as a whole.
- [VISION](../../VISION.md) — Read this when you want the long-term direction.
- [DESIGN_git_lock](DESIGN_git_lock.md) — Read this when you wonder why git lock contention is shaped the way it is.
- [PERF-archive-batch-fix-design](PERF-archive-batch-fix-design.md) — Read this when you are debugging archive write throughput.
- [OLD_REPO_RUST_CUTOVER_PR_BODY](OLD_REPO_RUST_CUTOVER_PR_BODY.md) — Read this when you want the historical context of the Python→Rust cutover.
- [TEMPLATE_OLD_REPO_RUST_CUTOVER_PR_CHECKLIST](TEMPLATE_OLD_REPO_RUST_CUTOVER_PR_CHECKLIST.md) — Read this when you are cutting a PR that mirrors the original cutover pattern.
- [TEMPLATE_OLD_REPO_RUST_CUTOVER_RELEASE_NOTES](TEMPLATE_OLD_REPO_RUST_CUTOVER_RELEASE_NOTES.md) — Read this when you are writing release notes for a cutover.
- [REGRESSION_BOUNDARIES](REGRESSION_BOUNDARIES.md) — Read this when you are deciding which tests are in scope for a regression suite.
- [RESOURCE_COVERAGE_AUDIT](RESOURCE_COVERAGE_AUDIT.md) — Read this when you are auditing which resources are tested.

## Audits & postmortems

Read these when you want to know **what already went wrong** — concrete incidents and their fixes.

### Audits

- [CONFORMANCE_AUDIT_2026-04-18](CONFORMANCE_AUDIT_2026-04-18.md) — Read this when you want a baseline conformance snapshot.
- [DOC_SWEEP_AUDIT_2026-04-18](DOC_SWEEP_AUDIT_2026-04-18.md) — Read this when you want to know which docs were swept in the last doc pass.
- [AUDIT-mailbox-durability-program](AUDIT-mailbox-durability-program.md) — Read this when you are reviewing mailbox durability.
- [GIT_SHELLOUT_AUDIT](GIT_SHELLOUT_AUDIT.md) — Read this when you are reviewing git shell-out safety.
- [GIT_SHELLOUT_AUDIT_COUNT](GIT_SHELLOUT_AUDIT_COUNT.txt) — Read this when you want the raw count from the shell-out audit.
- [GIT_251_FINDINGS](GIT_251_FINDINGS.md) — Read this when you are reviewing findings from the git-251 audit.
- [OBSERVABILITY_git_251](OBSERVABILITY_git_251.md) — Read this when you are adding observability for a git-251 finding.
- [VERIFICATION_COVERAGE_LEDGER](VERIFICATION_COVERAGE_LEDGER.md) — Read this when you are checking which findings have a verification path.

### Postmortems

- [INCIDENT_BR_2K3QX_A4_CLASSIFICATION_MATRIX](INCIDENT_BR_2K3QX_A4_CLASSIFICATION_MATRIX.md) — Read this when you are reviewing classification decisions for incident BR-2K3QX.
- [INCIDENT_BR_2K3QX_CLOSURE_REPORT](INCIDENT_BR_2K3QX_CLOSURE_REPORT.md) — Read this when you want the closure summary for incident BR-2K3QX.
- [INCIDENT_BR_2K3QX_G1_SURFACE_QUERY_CATALOG](INCIDENT_BR_2K3QX_G1_SURFACE_QUERY_CATALOG.json) — Read this when you are looking up a specific G1 surface query from BR-2K3QX.
- [INCIDENT_BR_2K3QX_I1_TRIAGE_QUEUE](INCIDENT_BR_2K3QX_I1_TRIAGE_QUEUE.md) — Read this when you are triaging I1 items from BR-2K3QX.
- [INCIDENT_BR_LEGJY_G1_FRANKENSQLITE_PROFILE](INCIDENT_BR_LEGJY_G1_FRANKENSQLITE_PROFILE.md) — Read this when you are debugging a Frankenstein-SQLite symptom.
- [INCIDENT_BR_LEGJY_G2_FRANKENSEARCH_PROFILE](INCIDENT_BR_LEGJY_G2_FRANKENSEARCH_PROFILE.md) — Read this when you are debugging a Frankenstein-search symptom.

### Reality checks

- [reality-check_2026-04-18_findings](reality-check_2026-04-18_findings.md) — Read this when you want the full 2026-04-18 reality-check findings.
- [reality-check_2026-04-18_diff](reality-check_2026-04-18_diff.md) — Read this when you want a diff-style summary of the 2026-04-18 findings.

## Reference

Read these when you want to **look something up**, not learn.

- [DEVELOPER_GUIDE](DEVELOPER_GUIDE.md) — Read this when you are setting up a dev environment.
- [MIGRATION_GUIDE](MIGRATION_GUIDE.md) — Read this when you are migrating an existing setup.
- [MIGRATION_v16_to_v17](MIGRATION_v16_to_v17.md) — Read this when you are upgrading from v16 to v17.
- [FLAGS_REGISTRY](FLAGS_REGISTRY.md) — Read this when you are tuning a runtime flag.
- [TUI_HELP](TUI_HELP.md) — Read this when you are using the TUI.
- [TUI_V2_CONTRACT](TUI_V2_CONTRACT.md) — Read this when you are working on the TUI v2 in depth.
- [TUI_V2_PARITY_DIFF](TUI_V2_PARITY_DIFF.md) — Read this when you are tracking the TUI v1 → v2 parity gap.
- [AGENT_HEALTH_SCORING](AGENT_HEALTH_SCORING.md) — Read this when you are computing or interpreting an agent health score.

## ATC (alert / telemetry / control) reference

Read these when you are configuring the **ATC** (alert / telemetry / control) plane.

- [ATC_HOT_PATH_WIRING](ATC_HOT_PATH_WIRING.md) — Read this when you are wiring an ATC hot path.
- [ATC_OBSERVABILITY](ATC_OBSERVABILITY.md) — Read this when you are adding observability to an ATC component.
- [ATC_PERF_BUDGETS](ATC_PERF_BUDGETS.md) — Read this when you are setting or auditing a perf budget.
- [ATC_POLICY_SIMULATION](ATC_POLICY_SIMULATION.md) — Read this when you are simulating an ATC policy change.
- [ATC_ALERTS_EXAMPLE](ATC_ALERTS_EXAMPLE.yaml) — Read this when you are looking up an example alert definition.

## Performance

- [PERF-archive-batch-profile-2026-04-18](PERF-archive-batch-profile-2026-04-18.md) — Read this when you are looking up a recent archive-batch profile.

## How this index was assembled

This index was assembled by walking the `docs/` directory, grouping files by their filename prefix (ADR / SPEC / RUNBOOK / DESIGN / INCIDENT / TEMPLATE / etc.), and writing a one-line "Read this when…" hint for each entry. The hints were drafted from the filenames and the first paragraph of each file; no edits were made to the underlying files.

---

_This is a docs-only contribution. No src/ or docs/* content was changed. CI is unaffected._

_(Posted from an AI agent account — happy to revise grouping, rewording of the "Read this when…" hints, or to drop the index entirely if maintainer prefers a different navigation aid.)_
