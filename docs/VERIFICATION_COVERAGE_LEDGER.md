# Verification Coverage Ledger

Canonical inventory for [br-aazao.1](br-aazao.1) / `br-aazao.1.1` / `br-aazao.1.2` / `br-aazao.1.3`.

## Scope

This document inventories the current verification surface by workspace crate and major internal cluster. It is meant to stop the repo-wide verification program from repeatedly rediscovering the same test topology.

This pass is intentionally concrete about:

- where inline `#[cfg(test)]` coverage already exists
- which crates have dedicated `tests/*.rs` integration harnesses
- where conformance and parity coverage lives
- how the shell E2E surface is split between direct suites, thin wrappers, and specialized harness lanes
- where the current surface is mostly behavioral vs mostly contract/snapshot oriented
- which obvious low-coverage or realism-risk areas should feed later beads

This pass does **not** try to finish the realism policy work from `br-aazao.2`. It does, however, include the crate-cluster inventory from `br-aazao.1.1`, the shell-suite inventory slice from `br-aazao.1.2`, and the realism/ownership ledger from `br-aazao.1.3`.

## Evidence Sources

- [Cargo.toml](/data/projects/mcp_agent_mail_rust/Cargo.toml)
- [README.md](/data/projects/mcp_agent_mail_rust/README.md)
- [AGENTS.md](/data/projects/mcp_agent_mail_rust/AGENTS.md)
- `rg -n '#[cfg(test)]' crates/*/src`
- `find crates -maxdepth 3 -path '*/tests/*.rs' -type f`
- `find tests/e2e -maxdepth 1 -name 'test_*.sh'`
- `find scripts -maxdepth 1 -name 'e2e*.sh'`
- `rg -n 'source .*e2e_lib\.sh|source .*e2e_search_v3_lib\.sh|exec .*scripts/e2e_.*\.sh|bash .*scripts/e2e_.*\.sh' tests/e2e scripts`
- representative test reads from:
  - [toon_integration.rs](/data/projects/mcp_agent_mail_rust/crates/mcp-agent-mail-core/tests/toon_integration.rs)
  - [scope_policy_property.rs](/data/projects/mcp_agent_mail_rust/crates/mcp-agent-mail-db/tests/scope_policy_property.rs)
  - [fixture_matrix.rs](/data/projects/mcp_agent_mail_rust/crates/mcp-agent-mail-server/tests/fixture_matrix.rs)
  - [semantic_conformance.rs](/data/projects/mcp_agent_mail_rust/crates/mcp-agent-mail-cli/tests/semantic_conformance.rs)
  - [validation_error_parity.rs](/data/projects/mcp_agent_mail_rust/crates/mcp-agent-mail-tools/tests/validation_error_parity.rs)
  - [stress_pipeline.rs](/data/projects/mcp_agent_mail_rust/crates/mcp-agent-mail-storage/tests/stress_pipeline.rs)
  - [guard_env_tests.rs](/data/projects/mcp_agent_mail_rust/crates/mcp-agent-mail-guard/tests/guard_env_tests.rs)
  - [conformance.rs](/data/projects/mcp_agent_mail_rust/crates/mcp-agent-mail-conformance/tests/conformance.rs)

## Current Topology At A Glance

- Cargo workspace crates: `12`
- Additional in-tree, non-workspace crate under `crates/`: `mcp-agent-mail-agent-detect`
- Crates with dedicated `tests/*.rs` integration harnesses: `9`
- Shell E2E suites under [tests/e2e](/data/projects/mcp_agent_mail_rust/tests/e2e): `129`
- In-tree conformance fixture files under [tests/conformance](/data/projects/mcp_agent_mail_rust/tests/conformance): `1`

Inline `#[cfg(test)]` density by crate:

| Crate | Inline test blocks |
|---|---:|
| `mcp-agent-mail-server` | 123 |
| `mcp-agent-mail-core` | 58 |
| `mcp-agent-mail-db` | 52 |
| `mcp-agent-mail-search-core` | 29 |
| `mcp-agent-mail-cli` | 22 |
| `mcp-agent-mail-tools` | 19 |
| `mcp-agent-mail-share` | 19 |
| `mcp-agent-mail` | 3 |
| `mcp-agent-mail-storage` | 2 |
| `mcp-agent-mail-wasm` | 1 |
| `mcp-agent-mail-guard` | 1 |
| `mcp-agent-mail-conformance` | 1 |
| `mcp-agent-mail-agent-detect` | 1 |

Dedicated `tests/*.rs` harness count by crate:

| Crate | Integration/conformance test files |
|---|---:|
| `mcp-agent-mail-db` | 31 |
| `mcp-agent-mail-cli` | 16 |
| `mcp-agent-mail-server` | 15 |
| `mcp-agent-mail-tools` | 6 |
| `mcp-agent-mail-conformance` | 6 |
| `mcp-agent-mail-core` | 5 |
| `mcp-agent-mail-search-core` | 3 |
| `mcp-agent-mail-storage` | 1 |
| `mcp-agent-mail-guard` | 1 |
| `mcp-agent-mail-share` | 0 |
| `mcp-agent-mail` | 0 |
| `mcp-agent-mail-wasm` | 0 |
| `mcp-agent-mail-agent-detect` | 0 |

## Coverage Legend

- `Behavioral`: exercises state transitions, algorithms, or multi-step workflows.
- `Contract`: checks output shapes, parity envelopes, validation payloads, or schema contracts.
- `Snapshot`: golden output, markdown, or UI/text render stability.
- `Stress/Perf`: concurrency, load, or latency envelopes.
- `Substitute lane`: coverage exists, but a stub/fake/mock/fixture stands in for a real dependency or user path.

## Inventory By Crate Cluster

### `mcp-agent-mail-core`

**Cluster: config, identity, models, metrics, setup, diagnostics**

- Strong inline coverage in [config.rs](/data/projects/mcp_agent_mail_rust/crates/mcp-agent-mail-core/src/config.rs), [identity.rs](/data/projects/mcp_agent_mail_rust/crates/mcp-agent-mail-core/src/identity.rs), [models.rs](/data/projects/mcp_agent_mail_rust/crates/mcp-agent-mail-core/src/models.rs), [metrics.rs](/data/projects/mcp_agent_mail_rust/crates/mcp-agent-mail-core/src/metrics.rs), [setup.rs](/data/projects/mcp_agent_mail_rust/crates/mcp-agent-mail-core/src/setup.rs), [evidence_ledger.rs](/data/projects/mcp_agent_mail_rust/crates/mcp-agent-mail-core/src/evidence_ledger.rs), and related support modules.
- Dedicated tests exist in [config_env_otel.rs](/data/projects/mcp_agent_mail_rust/crates/mcp-agent-mail-core/tests/config_env_otel.rs), [agent_detect_integration.rs](/data/projects/mcp_agent_mail_rust/crates/mcp-agent-mail-core/tests/agent_detect_integration.rs), [loom_update_max.rs](/data/projects/mcp_agent_mail_rust/crates/mcp-agent-mail-core/tests/loom_update_max.rs), and [repro_overlap.rs](/data/projects/mcp_agent_mail_rust/crates/mcp-agent-mail-core/tests/repro_overlap.rs).
- Coverage profile: mostly `Behavioral` + invariant testing, with some concurrency/repro-focused harnesses.
- Obvious note: core already contains a lot of serious local correctness testing; the main gap is not zero coverage, it is that many guarantees are module-local rather than exercised through cross-crate workflows.

**Cluster: ATC policy, labeling, fairness, adaptation, contamination, user surfaces**

- ATC coverage is heavily inline across many `atc_*` modules, including [atc_labeling.rs](/data/projects/mcp_agent_mail_rust/crates/mcp-agent-mail-core/src/atc_labeling.rs), [atc_effect_semantics.rs](/data/projects/mcp_agent_mail_rust/crates/mcp-agent-mail-core/src/atc_effect_semantics.rs), [atc_fairness.rs](/data/projects/mcp_agent_mail_rust/crates/mcp-agent-mail-core/src/atc_fairness.rs), [atc_adaptation.rs](/data/projects/mcp_agent_mail_rust/crates/mcp-agent-mail-core/src/atc_adaptation.rs), [atc_user_surfaces.rs](/data/projects/mcp_agent_mail_rust/crates/mcp-agent-mail-core/src/atc_user_surfaces.rs), and nearby files.
- No dedicated `tests/` harness is scoped specifically to ATC end-to-end behavior inside this crate.
- Coverage profile: strong `Behavioral` and invariant coverage at module level.
- Obvious note: this is one of the highest-density logic areas in the repo, but the repo still lacks a single integration-style harness that proves the ATC state machine across core + DB + server boundaries from one place.

**Cluster: TOON / output formatting**

- Inline coverage exists in [toon.rs](/data/projects/mcp_agent_mail_rust/crates/mcp-agent-mail-core/src/toon.rs).
- Dedicated integration harness exists in [toon_integration.rs](/data/projects/mcp_agent_mail_rust/crates/mcp-agent-mail-core/tests/toon_integration.rs).
- Coverage profile: mix of `Behavioral` and `Contract`.
- Realism note: this is explicitly a `Substitute lane`; the integration tests use deterministic stub encoders from `scripts/` rather than a real live encoder install.

### `mcp-agent-mail-db`

**Cluster: schema, migrations, pool, cache, queries, explorer**

- Heavy inline coverage in [queries.rs](/data/projects/mcp_agent_mail_rust/crates/mcp-agent-mail-db/src/queries.rs), [pool.rs](/data/projects/mcp_agent_mail_rust/crates/mcp-agent-mail-db/src/pool.rs), [cache.rs](/data/projects/mcp_agent_mail_rust/crates/mcp-agent-mail-db/src/cache.rs), [migrate.rs](/data/projects/mcp_agent_mail_rust/crates/mcp-agent-mail-db/src/migrate.rs), [schema.rs](/data/projects/mcp_agent_mail_rust/crates/mcp-agent-mail-db/src/schema.rs), [mail_explorer.rs](/data/projects/mcp_agent_mail_rust/crates/mcp-agent-mail-db/src/mail_explorer.rs), and related files.
- Dedicated harnesses include [query_integration.rs](/data/projects/mcp_agent_mail_rust/crates/mcp-agent-mail-db/tests/query_integration.rs), [migration_tests.rs](/data/projects/mcp_agent_mail_rust/crates/mcp-agent-mail-db/tests/migration_tests.rs), [schema_migration.rs](/data/projects/mcp_agent_mail_rust/crates/mcp-agent-mail-db/tests/schema_migration.rs), [pool_exhaustion.rs](/data/projects/mcp_agent_mail_rust/crates/mcp-agent-mail-db/tests/pool_exhaustion.rs), [cache_golden.rs](/data/projects/mcp_agent_mail_rust/crates/mcp-agent-mail-db/tests/cache_golden.rs), [mail_explorer.rs](/data/projects/mcp_agent_mail_rust/crates/mcp-agent-mail-db/tests/mail_explorer.rs), and [atc_experience_lifecycle.rs](/data/projects/mcp_agent_mail_rust/crates/mcp-agent-mail-db/tests/atc_experience_lifecycle.rs).
- Coverage profile: strong mix of `Behavioral`, `Contract`, and some `Stress/Perf`.
- Obvious note: DB coverage is one of the deepest in the workspace and already exercises migrations, lifecycle transitions, and failure handling, not just SQL shape.

**Cluster: search scope, ranking, filter/pagination, planner, quality**

- Inline search-related test blocks are distributed across many `search_*` modules.
- Dedicated harnesses include [scope_policy_property.rs](/data/projects/mcp_agent_mail_rust/crates/mcp-agent-mail-db/tests/scope_policy_property.rs), [filter_pagination.rs](/data/projects/mcp_agent_mail_rust/crates/mcp-agent-mail-db/tests/filter_pagination.rs), [diversity_dedup.rs](/data/projects/mcp_agent_mail_rust/crates/mcp-agent-mail-db/tests/diversity_dedup.rs), [query_assistance_explain.rs](/data/projects/mcp_agent_mail_rust/crates/mcp-agent-mail-db/tests/query_assistance_explain.rs), [golden_ranking.rs](/data/projects/mcp_agent_mail_rust/crates/mcp-agent-mail-db/tests/golden_ranking.rs), [search_quality.rs](/data/projects/mcp_agent_mail_rust/crates/mcp-agent-mail-db/tests/search_quality.rs), [search_v3_conformance.rs](/data/projects/mcp_agent_mail_rust/crates/mcp-agent-mail-db/tests/search_v3_conformance.rs), and [search_conformance_fuzz.rs](/data/projects/mcp_agent_mail_rust/crates/mcp-agent-mail-db/tests/search_conformance_fuzz.rs).
- Coverage profile: broad mix of `Behavioral`, `Contract`, property testing, and `Stress/Perf`.
- Realism note: the broader verification epic already calls out stub embedder/search lanes elsewhere; those realism exceptions should be catalogued precisely in `br-aazao.1.3`.

**Cluster: fault injection, load, soak**

- Dedicated harnesses include [fault_injection.rs](/data/projects/mcp_agent_mail_rust/crates/mcp-agent-mail-db/tests/fault_injection.rs), [load_bench.rs](/data/projects/mcp_agent_mail_rust/crates/mcp-agent-mail-db/tests/load_bench.rs), [load_concurrency.rs](/data/projects/mcp_agent_mail_rust/crates/mcp-agent-mail-db/tests/load_concurrency.rs), [stress.rs](/data/projects/mcp_agent_mail_rust/crates/mcp-agent-mail-db/tests/stress.rs), and [sustained_load.rs](/data/projects/mcp_agent_mail_rust/crates/mcp-agent-mail-db/tests/sustained_load.rs).
- Coverage profile: meaningful `Stress/Perf` presence instead of pure happy-path testing.
- Obvious note: the DB crate already owns much of the repo’s serious operational verification burden.

### `mcp-agent-mail-server`

**Cluster: server dispatch, MCP routing, resources, HTTP endpoints**

- Heavy inline coverage in [lib.rs](/data/projects/mcp_agent_mail_rust/crates/mcp-agent-mail-server/src/lib.rs), [startup_checks.rs](/data/projects/mcp_agent_mail_rust/crates/mcp-agent-mail-server/src/startup_checks.rs), [integrity_guard.rs](/data/projects/mcp_agent_mail_rust/crates/mcp-agent-mail-server/src/integrity_guard.rs), [cleanup.rs](/data/projects/mcp_agent_mail_rust/crates/mcp-agent-mail-server/src/cleanup.rs), and adjacent runtime files.
- Dedicated harnesses include [health_endpoints.rs](/data/projects/mcp_agent_mail_rust/crates/mcp-agent-mail-server/tests/health_endpoints.rs), [workers.rs](/data/projects/mcp_agent_mail_rust/crates/mcp-agent-mail-server/tests/workers.rs), [startup_compat.rs](/data/projects/mcp_agent_mail_rust/crates/mcp-agent-mail-server/tests/startup_compat.rs), and [http_logging.rs](/data/projects/mcp_agent_mail_rust/crates/mcp-agent-mail-server/tests/http_logging.rs).
- Coverage profile: mostly `Behavioral`, with some operational-contract checks.

**Cluster: ATC orchestration and execution capture**

- Major inline coverage in [atc.rs](/data/projects/mcp_agent_mail_rust/crates/mcp-agent-mail-server/src/atc.rs) and ATC handling inside [lib.rs](/data/projects/mcp_agent_mail_rust/crates/mcp-agent-mail-server/src/lib.rs).
- No dedicated `tests/` file focused only on ATC orchestration; coverage is mainly inline or indirect through broader server suites.
- Coverage profile: substantial `Behavioral` coverage, but integration-style proof remains fragmented.
- Obvious note: this is a high-risk boundary because it connects core policy, DB persistence, and outward operator surfaces.

**Cluster: TUI, mail UI, markdown, web/dashboard parity**

- Extremely dense inline test surface across `tui_*`, [mail_ui.rs](/data/projects/mcp_agent_mail_rust/crates/mcp-agent-mail-server/src/mail_ui.rs), and [tui_screens/*](/data/projects/mcp_agent_mail_rust/crates/mcp-agent-mail-server/src/tui_screens).
- Dedicated harnesses include [golden_markdown_snapshots.rs](/data/projects/mcp_agent_mail_rust/crates/mcp-agent-mail-server/tests/golden_markdown_snapshots.rs), [golden_snapshots.rs](/data/projects/mcp_agent_mail_rust/crates/mcp-agent-mail-server/tests/golden_snapshots.rs), [ui_markdown_templates.rs](/data/projects/mcp_agent_mail_rust/crates/mcp-agent-mail-server/tests/ui_markdown_templates.rs), [web_ui_parity_contract_guard.rs](/data/projects/mcp_agent_mail_rust/crates/mcp-agent-mail-server/tests/web_ui_parity_contract_guard.rs), [tui_perf_baselines.rs](/data/projects/mcp_agent_mail_rust/crates/mcp-agent-mail-server/tests/tui_perf_baselines.rs), [tui_soak_replay.rs](/data/projects/mcp_agent_mail_rust/crates/mcp-agent-mail-server/tests/tui_soak_replay.rs), and [pty_e2e_search.rs](/data/projects/mcp_agent_mail_rust/crates/mcp-agent-mail-server/tests/pty_e2e_search.rs).
- Coverage profile: a lot of `Snapshot` and parity coverage plus some `Behavioral`/PTY interaction testing.
- Obvious note: the UI surface is not untested; the remaining question is whether the current mix overweights snapshot contracts relative to real operator workflows.

**Cluster: fixture-rich scenario seeding**

- [fixture_matrix.rs](/data/projects/mcp_agent_mail_rust/crates/mcp-agent-mail-server/tests/fixture_matrix.rs) seeds realistic multi-project datasets and emits artifact-rich reports.
- Coverage profile: `Behavioral` plus forensic artifact generation.
- Obvious note: this is a good building block for later realism and E2E closure work because it creates reusable non-trivial state.

### `mcp-agent-mail-cli`

**Cluster: command dispatch, mode gating, context, output, robot**

- Inline coverage is spread through [lib.rs](/data/projects/mcp_agent_mail_rust/crates/mcp-agent-mail-cli/src/lib.rs), [robot.rs](/data/projects/mcp_agent_mail_rust/crates/mcp-agent-mail-cli/src/robot.rs), [context.rs](/data/projects/mcp_agent_mail_rust/crates/mcp-agent-mail-cli/src/context.rs), [ci.rs](/data/projects/mcp_agent_mail_rust/crates/mcp-agent-mail-cli/src/ci.rs), [output.rs](/data/projects/mcp_agent_mail_rust/crates/mcp-agent-mail-cli/src/output.rs), [legacy.rs](/data/projects/mcp_agent_mail_rust/crates/mcp-agent-mail-cli/src/legacy.rs), and related files.
- Dedicated harnesses include [semantic_conformance.rs](/data/projects/mcp_agent_mail_rust/crates/mcp-agent-mail-cli/tests/semantic_conformance.rs), [mode_matrix_harness.rs](/data/projects/mcp_agent_mail_rust/crates/mcp-agent-mail-cli/tests/mode_matrix_harness.rs), [http_transport_harness.rs](/data/projects/mcp_agent_mail_rust/crates/mcp-agent-mail-cli/tests/http_transport_harness.rs), [integration_runs.rs](/data/projects/mcp_agent_mail_rust/crates/mcp-agent-mail-cli/tests/integration_runs.rs), [ci_integration.rs](/data/projects/mcp_agent_mail_rust/crates/mcp-agent-mail-cli/tests/ci_integration.rs), [security_privacy_harness.rs](/data/projects/mcp_agent_mail_rust/crates/mcp-agent-mail-cli/tests/security_privacy_harness.rs), and [tui_transport_harness.rs](/data/projects/mcp_agent_mail_rust/crates/mcp-agent-mail-cli/tests/tui_transport_harness.rs).
- Coverage profile: healthy `Behavioral` and parity coverage with explicit CLI-vs-MCP state comparisons.

**Cluster: JSON/help/golden/output stability**

- Dedicated harnesses include [cli_json_snapshots.rs](/data/projects/mcp_agent_mail_rust/crates/mcp-agent-mail-cli/tests/cli_json_snapshots.rs), [help_snapshots.rs](/data/projects/mcp_agent_mail_rust/crates/mcp-agent-mail-cli/tests/help_snapshots.rs), and [golden_integration.rs](/data/projects/mcp_agent_mail_rust/crates/mcp-agent-mail-cli/tests/golden_integration.rs).
- Coverage profile: mostly `Contract` and `Snapshot`.
- Obvious note: these tests are valuable, but they should not be mistaken for proof that the underlying workflow semantics are correct.

**Cluster: performance/security/share-specific harnesses**

- Dedicated harnesses include [perf_guardrails.rs](/data/projects/mcp_agent_mail_rust/crates/mcp-agent-mail-cli/tests/perf_guardrails.rs), [perf_security_regressions.rs](/data/projects/mcp_agent_mail_rust/crates/mcp-agent-mail-cli/tests/perf_security_regressions.rs), [share_archive_harness.rs](/data/projects/mcp_agent_mail_rust/crates/mcp-agent-mail-cli/tests/share_archive_harness.rs), [share_verify_decrypt.rs](/data/projects/mcp_agent_mail_rust/crates/mcp-agent-mail-cli/tests/share_verify_decrypt.rs), and [tui_accessibility_harness.rs](/data/projects/mcp_agent_mail_rust/crates/mcp-agent-mail-cli/tests/tui_accessibility_harness.rs).
- Coverage profile: mixed `Behavioral`, `Contract`, and some `Stress/Perf`.

### `mcp-agent-mail-tools`

**Cluster: identity, messaging, contacts, reservations, products, search, macros**

- Inline coverage exists in [identity.rs](/data/projects/mcp_agent_mail_rust/crates/mcp-agent-mail-tools/src/identity.rs), [messaging.rs](/data/projects/mcp_agent_mail_rust/crates/mcp-agent-mail-tools/src/messaging.rs), [contacts.rs](/data/projects/mcp_agent_mail_rust/crates/mcp-agent-mail-tools/src/contacts.rs), [reservations.rs](/data/projects/mcp_agent_mail_rust/crates/mcp-agent-mail-tools/src/reservations.rs), [products.rs](/data/projects/mcp_agent_mail_rust/crates/mcp-agent-mail-tools/src/products.rs), [search.rs](/data/projects/mcp_agent_mail_rust/crates/mcp-agent-mail-tools/src/search.rs), [resources.rs](/data/projects/mcp_agent_mail_rust/crates/mcp-agent-mail-tools/src/resources.rs), and [macros.rs](/data/projects/mcp_agent_mail_rust/crates/mcp-agent-mail-tools/src/macros.rs).
- Dedicated harnesses include [agent_name_parity.rs](/data/projects/mcp_agent_mail_rust/crates/mcp-agent-mail-tools/tests/agent_name_parity.rs), [contact_policy_parity.rs](/data/projects/mcp_agent_mail_rust/crates/mcp-agent-mail-tools/tests/contact_policy_parity.rs), [messaging_error_parity.rs](/data/projects/mcp_agent_mail_rust/crates/mcp-agent-mail-tools/tests/messaging_error_parity.rs), [reservation_error_parity.rs](/data/projects/mcp_agent_mail_rust/crates/mcp-agent-mail-tools/tests/reservation_error_parity.rs), [system_error_parity.rs](/data/projects/mcp_agent_mail_rust/crates/mcp-agent-mail-tools/tests/system_error_parity.rs), and [validation_error_parity.rs](/data/projects/mcp_agent_mail_rust/crates/mcp-agent-mail-tools/tests/validation_error_parity.rs).
- Coverage profile: strong on `Contract` and Python-parity validation behavior; weaker on full multi-tool behavioral workflows inside the crate itself.
- Obvious note: higher-level behavior for tool composition appears to be validated mostly through server/CLI/E2E layers rather than rich direct tool integration tests.

### `mcp-agent-mail-share`

**Cluster: snapshot, scrub, bundle, crypto, deploy, wizard, probe, executor**

- Broad inline coverage exists in [snapshot.rs](/data/projects/mcp_agent_mail_rust/crates/mcp-agent-mail-share/src/snapshot.rs), [scrub.rs](/data/projects/mcp_agent_mail_rust/crates/mcp-agent-mail-share/src/scrub.rs), [bundle.rs](/data/projects/mcp_agent_mail_rust/crates/mcp-agent-mail-share/src/bundle.rs), [crypto.rs](/data/projects/mcp_agent_mail_rust/crates/mcp-agent-mail-share/src/crypto.rs), [deploy.rs](/data/projects/mcp_agent_mail_rust/crates/mcp-agent-mail-share/src/deploy.rs), [wizard.rs](/data/projects/mcp_agent_mail_rust/crates/mcp-agent-mail-share/src/wizard.rs), and neighboring modules.
- The crate has a `tests/` directory but no dedicated `tests/*.rs` harness files right now.
- Coverage profile: primarily module-local `Behavioral` testing.
- Obvious note: share is an obvious candidate for later cross-module integration coverage because the feature surface is broad but the dedicated integration harness count is currently zero.

### `mcp-agent-mail-search-core`

**Cluster: parsing, filtering, ranking, fusion, diversity, updater, cache, embedding jobs**

- Inline coverage is broad across [query.rs](/data/projects/mcp_agent_mail_rust/crates/mcp-agent-mail-search-core/src/query.rs), [filter_compiler.rs](/data/projects/mcp_agent_mail_rust/crates/mcp-agent-mail-search-core/src/filter_compiler.rs), [fusion.rs](/data/projects/mcp_agent_mail_rust/crates/mcp-agent-mail-search-core/src/fusion.rs), [diversity.rs](/data/projects/mcp_agent_mail_rust/crates/mcp-agent-mail-search-core/src/diversity.rs), [engine.rs](/data/projects/mcp_agent_mail_rust/crates/mcp-agent-mail-search-core/src/engine.rs), [embedder.rs](/data/projects/mcp_agent_mail_rust/crates/mcp-agent-mail-search-core/src/embedder.rs), and other search modules.
- Dedicated harnesses include [fault_injection.rs](/data/projects/mcp_agent_mail_rust/crates/mcp-agent-mail-search-core/tests/fault_injection.rs), [parser_filter_fusion_rerank.rs](/data/projects/mcp_agent_mail_rust/crates/mcp-agent-mail-search-core/tests/parser_filter_fusion_rerank.rs), and [query_assistance_explain.rs](/data/projects/mcp_agent_mail_rust/crates/mcp-agent-mail-search-core/tests/query_assistance_explain.rs).
- Coverage profile: mixed `Behavioral` and fault-injection testing.
- Realism note: this cluster is one of the most likely places to rely on sanctioned substitutes for embedders or local model behavior; that needs explicit grading in `br-aazao.1.3`.

### `mcp-agent-mail-storage`

**Cluster: archive writing, commit coalescer, WBQ, archive root setup**

- Inline coverage lives in [lib.rs](/data/projects/mcp_agent_mail_rust/crates/mcp-agent-mail-storage/src/lib.rs).
- Dedicated integration harness exists in [stress_pipeline.rs](/data/projects/mcp_agent_mail_rust/crates/mcp-agent-mail-storage/tests/stress_pipeline.rs).
- Coverage profile: meaningful `Behavioral` and `Stress/Perf`.
- Obvious note: storage has fewer files, but its single dedicated harness is high-value because it targets the combined DB + Git pipeline under concurrency rather than just isolated helpers.

### `mcp-agent-mail-guard`

**Cluster: reservation enforcement, guard mode, env-driven gating**

- Inline coverage exists in [lib.rs](/data/projects/mcp_agent_mail_rust/crates/mcp-agent-mail-guard/src/lib.rs).
- Dedicated harness exists in [guard_env_tests.rs](/data/projects/mcp_agent_mail_rust/crates/mcp-agent-mail-guard/tests/guard_env_tests.rs).
- Coverage profile: `Behavioral`, with environment mutation serialized under test.
- Realism note: some scenarios necessarily fabricate archive/reservation files, which is appropriate, but should still be tagged as local-fixture rather than full real-path.

### `mcp-agent-mail-conformance`

**Cluster: Python parity for tools/resources/descriptions/error codes**

- Inline coverage in [main.rs](/data/projects/mcp_agent_mail_rust/crates/mcp-agent-mail-conformance/src/main.rs) is minimal.
- Dedicated harnesses include [conformance.rs](/data/projects/mcp_agent_mail_rust/crates/mcp-agent-mail-conformance/tests/conformance.rs), [conformance_debug.rs](/data/projects/mcp_agent_mail_rust/crates/mcp-agent-mail-conformance/tests/conformance_debug.rs), [contact_enforcement_outage.rs](/data/projects/mcp_agent_mail_rust/crates/mcp-agent-mail-conformance/tests/contact_enforcement_outage.rs), [error_code_parity.rs](/data/projects/mcp_agent_mail_rust/crates/mcp-agent-mail-conformance/tests/error_code_parity.rs), [resource_description_parity.rs](/data/projects/mcp_agent_mail_rust/crates/mcp-agent-mail-conformance/tests/resource_description_parity.rs), and [tool_description_parity.rs](/data/projects/mcp_agent_mail_rust/crates/mcp-agent-mail-conformance/tests/tool_description_parity.rs).
- Coverage profile: overwhelmingly `Contract`.
- Obvious note: the conformance layer is important but should not be confused with behavioral realism; it proves parity of envelopes and error surfaces more than end-user workflow truth.
- Audit flag: the visible in-tree conformance fixture corpus is surprisingly small at the filesystem level and deserves a follow-up sanity check.

### `mcp-agent-mail`

**Cluster: server binary entrypoint / mode plumbing**

- Inline coverage exists in [main.rs](/data/projects/mcp_agent_mail_rust/crates/mcp-agent-mail/src/main.rs).
- No dedicated `tests/*.rs` harnesses.
- Coverage profile: light entrypoint coverage.
- Obvious note: this binary relies heavily on lower-layer verification; if CLI/server launch semantics drift, there is little crate-local integration protection.

### `mcp-agent-mail-wasm`

**Cluster: WASM surface**

- Minimal inline coverage exists in [lib.rs](/data/projects/mcp_agent_mail_rust/crates/mcp-agent-mail-wasm/src/lib.rs).
- No dedicated `tests/*.rs` harnesses.
- Coverage profile: thin.
- Obvious note: this is an explicit under-covered surface and already has a follow-on bead in the verification tree.

### `mcp-agent-mail-agent-detect`

**Cluster: agent detection helpers**

- Minimal inline coverage exists in [lib.rs](/data/projects/mcp_agent_mail_rust/crates/mcp-agent-mail-agent-detect/src/lib.rs).
- No dedicated `tests/*.rs` harnesses.
- Coverage profile: thin.
- Obvious note: coverage exists, but this surface is still substantially lighter than the mainline crates.

## Cross-Crate Observations

- The repo is **not** short on tests in the aggregate. The real issue is uneven confidence quality.
- `db`, `server`, `cli`, and `core` already have substantial real verification mass.
- `tools` and `conformance` are strong on parity/validation contracts but lighter on deep behavioral flows at their own layer.
- `share`, `wasm`, `agent-detect`, and the `mcp-agent-mail` binary are the clearest coverage-depth laggards by crate.
- ATC logic has a lot of local correctness testing, but its cross-crate orchestration proof is still fragmented.
- Snapshot/golden/parity coverage is abundant in server/CLI surfaces; later beads should be careful not to mistake output stability for true workflow realism.

## Shell E2E Suite Inventory (`br-aazao.1.2`)

### Entry Surface

- The authoritative runner is `am e2e run --project <repo> [suite...]`, with suite discovery documented in [AGENTS.md](/data/projects/mcp_agent_mail_rust/AGENTS.md) and implemented via the CLI.
- [scripts/e2e_test.sh](/data/projects/mcp_agent_mail_rust/scripts/e2e_test.sh) is now a compatibility shim. By default it delegates to `am e2e run`; only `AM_E2E_FORCE_LEGACY=1` keeps the old in-script execution path alive.
- The common shell artifact contract is rooted in [scripts/e2e_lib.sh](/data/projects/mcp_agent_mail_rust/scripts/e2e_lib.sh): `e2e_init_artifacts()` creates `diagnostics/`, `trace/`, `transcript/`, `logs/`, and `screenshots/`, while `e2e_summary()` writes `summary.json`, `meta.json`, `metrics.json`, repro files, forensic indexes, and a validated `bundle.json`.

### Suite Topology

| Lane | Count | Shape | Representative files | Artifact richness |
|---|---:|---|---|---|
| Thin wrapper suites | 22 | `tests/e2e/test_*.sh` delegates directly to a sibling `scripts/e2e_*.sh` implementation | [test_http.sh](/data/projects/mcp_agent_mail_rust/tests/e2e/test_http.sh), [test_cli.sh](/data/projects/mcp_agent_mail_rust/tests/e2e/test_cli.sh), [test_tui_a11y.sh](/data/projects/mcp_agent_mail_rust/tests/e2e/test_tui_a11y.sh), [test_share.sh](/data/projects/mcp_agent_mail_rust/tests/e2e/test_share.sh) | Wrapper itself is thin; all useful logging comes from the delegated script |
| Direct common-harness suites | 102 | Suite sources [e2e_lib.sh](/data/projects/mcp_agent_mail_rust/scripts/e2e_lib.sh) and owns its assertions inline | [test_stdio.sh](/data/projects/mcp_agent_mail_rust/tests/e2e/test_stdio.sh), [test_macros.sh](/data/projects/mcp_agent_mail_rust/tests/e2e/test_macros.sh), [test_workflow_happy_path.sh](/data/projects/mcp_agent_mail_rust/tests/e2e/test_workflow_happy_path.sh), [test_robot.sh](/data/projects/mcp_agent_mail_rust/tests/e2e/test_robot.sh) | Rich default artifact set: trace, transcript, diagnostics, repro breadcrumbs, summary/meta/metrics, validated bundle manifest |
| Search V3 specialized harness suites | 4 | Suite sources [e2e_search_v3_lib.sh](/data/projects/mcp_agent_mail_rust/scripts/e2e_search_v3_lib.sh), which extends the base harness | [test_search_v3_http.sh](/data/projects/mcp_agent_mail_rust/tests/e2e/test_search_v3_http.sh), [test_search_v3_stdio.sh](/data/projects/mcp_agent_mail_rust/tests/e2e/test_search_v3_stdio.sh), [test_search_v3_shadow_parity.sh](/data/projects/mcp_agent_mail_rust/tests/e2e/test_search_v3_shadow_parity.sh) | Richer-than-base: adds per-case request/response capture, ranking diffs, index metadata, and Search V3 run manifests |
| Optional-harness soak/stress wrapper | 1 | Suite can source `e2e_lib.sh` when present but still carries a standalone fallback | [test_soak_harness.sh](/data/projects/mcp_agent_mail_rust/tests/e2e/test_soak_harness.sh) | Variable; richer when `e2e_lib.sh` is present, less uniform than the main harness lanes |

### Wrapper vs Direct Notes

- The wrapper lane is concentrated in legacy/compatibility-oriented suites where the canonical logic already lives under `scripts/`: archive, CLI, CI, console, dual-mode, HTTP, share, serve, and the heavier TUI flows.
- The direct lane is now the dominant path. Most suites under [tests/e2e](/data/projects/mcp_agent_mail_rust/tests/e2e) are not thin wrappers; they are full suites that source the common harness and own their assertions inline.
- The Search V3 lane is intentionally separate. It still inherits the base harness, but it layers additional ranking/index diagnostics under `test_logs/search_v3/...`, so it should not be treated as just another generic `e2e_lib.sh` consumer.
- The soak harness is the clearest outlier: it tries to reuse the common harness, but it is still designed to degrade gracefully if that harness is unavailable. That makes it useful operationally, but less uniform as a verification artifact producer.

### Harness Richness and Artifact Gaps

- The common harness is already materially richer than a “stdout + exit code” shell runner. It captures structured traces, environment snapshots with redaction, transcript summaries, repro breadcrumbs, fixture IDs, and a validated `bundle.json`.
- The biggest artifact-contract gap is that the common harness does **not** emit the per-suite `manifest.json` requested by [br-aazao.8.1](br-aazao.8.1). Current artifacts are rich, but case-level manifest structure is still implicit across `summary.json`, `trace/events.jsonl`, `bundle.json`, and suite-specific sidecars.
- Thin wrappers inherit richness from their delegated scripts, but the wrapper files themselves add almost no observability beyond the delegation comment and fallback invocation hints. That is acceptable for compatibility, but not a substitute for auditing the underlying script.
- Several suites explicitly weaken artifact enforcement by tolerating summary failure on some paths with `e2e_summary || true`. Representative examples live in [scripts/e2e_http.sh](/data/projects/mcp_agent_mail_rust/scripts/e2e_http.sh), [scripts/e2e_console.sh](/data/projects/mcp_agent_mail_rust/scripts/e2e_console.sh), [scripts/e2e_share.sh](/data/projects/mcp_agent_mail_rust/scripts/e2e_share.sh), [scripts/e2e_tui_full_traversal.sh](/data/projects/mcp_agent_mail_rust/scripts/e2e_tui_full_traversal.sh), and [tests/e2e/test_http_streamable.sh](/data/projects/mcp_agent_mail_rust/tests/e2e/test_http_streamable.sh).
- One suite calls out a known harness weakness directly: [test_check_inbox.sh](/data/projects/mcp_agent_mail_rust/tests/e2e/test_check_inbox.sh) documents that `e2e_summary` may fail because of a pre-existing bug in `e2e_write_server_log_stats`, then suppresses the failure. That is a real artifact-confidence gap, not just a stylistic nit.
- Compatibility fallback breadcrumbs remain widespread in suite headers. That is helpful operationally, but it means the docs/comments still normalize the deprecated shim path almost everywhere even though `am e2e run` is the authoritative interface now.

## Realism Clues Already Visible

- [toon_integration.rs](/data/projects/mcp_agent_mail_rust/crates/mcp-agent-mail-core/tests/toon_integration.rs) explicitly uses deterministic stub encoders.
- The verification epic already calls out stub embedder / stub engine lanes in search-related coverage.
- The verification epic also calls out stubbed LLM completions, mock release artifacts, and fake repo/owner-state scenarios that still need explicit realism grading.
- [guard_env_tests.rs](/data/projects/mcp_agent_mail_rust/crates/mcp-agent-mail-guard/tests/guard_env_tests.rs) and similar harnesses correctly use local synthetic fixtures for isolated policy checks; these should be treated as local-fixture confidence, not full real-path workflow proof.

## Realism Grade Rubric (`br-aazao.1.3`)

- `R0 Real-path`: production code paths exercised with the real runtime and durable state machinery. Local tempdirs and ephemeral DB files are fine if the test is still using the real engine rather than a substitute.
- `R1 Deterministic local fixture`: real code over synthetic local files, fixture payloads, or controlled repo/archive layouts. Good for logic and migration confidence, weaker for external integration claims.
- `R2 Sanctioned substitute`: an intentional offline stand-in that preserves a meaningful contract boundary, but is still not the real dependency. Good for repeatability, not enough for sign-off on the replaced dependency.
- `R3 Mock / stub / fake lane`: explicit fake behavior or mocked external surface. Useful for branch coverage and control flow, but should be called out as realism debt when it sits on a critical path.
- `R4 Thin / fragmented / unowned`: little direct coverage, or coverage only exists indirectly through lower layers. These surfaces need explicit ownership before confidence claims mean much.

## Prioritized Realism And Ownership Ledger (`br-aazao.1.3`)

| Surface | Current grade | Evidence | Priority | Expected owner / next bead |
|---|---|---|---|---|
| Real DB lifecycle and query orchestration | `R0 Real-path` | [atc_experience_lifecycle.rs](/data/projects/mcp_agent_mail_rust/crates/mcp-agent-mail-db/tests/atc_experience_lifecycle.rs) and [query_integration.rs](/data/projects/mcp_agent_mail_rust/crates/mcp-agent-mail-db/tests/query_integration.rs) explicitly exercise real SQLite, real migrations, real queries, and real FTS with no mocks | Critical-path confidence already present; preserve it | `br-aazao.5` should extend this style when closing remaining core/DB/storage/guard gaps |
| Direct transport/workflow E2E suites using the common harness | `R0 Real-path` with observability debt | Most `tests/e2e/test_*.sh` suites invoke the real binaries and real local HTTP/stdio paths via [e2e_lib.sh](/data/projects/mcp_agent_mail_rust/scripts/e2e_lib.sh), but artifact structure is still uneven and wrapper suites hide delegated logic | High | `br-aazao.8.1`, `br-aazao.9`, `br-aazao.10`, and `br-aazao.11` own the remaining artifact and matrix closure work |
| TOON encoder integration and golden capture | `R2 Sanctioned substitute` | [toon_integration.rs](/data/projects/mcp_agent_mail_rust/crates/mcp-agent-mail-core/tests/toon_integration.rs) and [test_toon.sh](/data/projects/mcp_agent_mail_rust/tests/e2e/test_toon.sh) intentionally rely on [toon_stub_encoder.sh](/data/projects/mcp_agent_mail_rust/scripts/toon_stub_encoder.sh) and [toon_stub_encoder_fail.sh](/data/projects/mcp_agent_mail_rust/scripts/toon_stub_encoder_fail.sh) instead of a real encoder install | High: user-visible formatting surface, but current proof is offline-contract only | `br-aazao.3` should replace or explicitly isolate this lane with real-path inputs |
| Search semantic/embedder engine adapter tests | `R3 Mock / stub / fake lane` | Search-core and DB search modules use `StubEngine`, `StubLifecycle`, and `StubEmbedder` in files like [engine.rs](/data/projects/mcp_agent_mail_rust/crates/mcp-agent-mail-search-core/src/engine.rs), [fs_bridge.rs](/data/projects/mcp_agent_mail_rust/crates/mcp-agent-mail-search-core/src/fs_bridge.rs), [two_tier.rs](/data/projects/mcp_agent_mail_rust/crates/mcp-agent-mail-search-core/src/two_tier.rs), and [updater.rs](/data/projects/mcp_agent_mail_rust/crates/mcp-agent-mail-search-core/src/updater.rs) | High: search quality claims are realism-sensitive | `br-aazao.4` owns stub embedder / stub engine isolation or replacement |
| LLM-assisted thread summarization E2E | `R3 Mock / stub / fake lane` | [test_llm.sh](/data/projects/mcp_agent_mail_rust/tests/e2e/test_llm.sh) declares itself “stubbed, offline” and forces `MCP_AGENT_MAIL_LLM_STUB=1` for deterministic output | High: product surface exists, but current evidence is explicitly synthetic | `br-aazao.4` should separate sanctioned offline smoke from any real-model confidence claims |
| Self-update / installer release-flow E2E | `R3 Mock / stub / fake lane` | [test_self_update.sh](/data/projects/mcp_agent_mail_rust/tests/e2e/test_self_update.sh) uses mocked release endpoints, local HTTP, and tiny synthetic payloads; related install/fresh-install suites also build fake destinations and fake tool installations | High: install/update failures are operator-facing and recent regressions proved this path is fragile | `br-aazao.3` should own real-path release/install inputs and demote the mock lane to explicit substitute coverage |
| Share / reconstruct / archive salvage tests with synthetic repo data | `R1 Deterministic local fixture` | Share, deploy, storage, and reconstruct coverage often uses fake encrypted blobs, fake archive trees, or fake lock ownership to drive recovery logic, which is appropriate for fault isolation but not equivalent to full live archive round-trips | Medium | `br-aazao.5` and `br-aazao.7` should preserve the fixture lanes but add more end-to-end archive/recovery proof where the workflow matters |
| Binary entrypoints, WASM, and agent-detect | `R4 Thin / fragmented / unowned` | [crates/mcp-agent-mail/src/main.rs](/data/projects/mcp_agent_mail_rust/crates/mcp-agent-mail/src/main.rs), [crates/mcp-agent-mail-wasm/src/lib.rs](/data/projects/mcp_agent_mail_rust/crates/mcp-agent-mail-wasm/src/lib.rs), and [crates/mcp-agent-mail-agent-detect/src/lib.rs](/data/projects/mcp_agent_mail_rust/crates/mcp-agent-mail-agent-detect/src/lib.rs) have notably lighter direct harness coverage than the mainline crates | Medium, but easy to forget | `br-aazao.7` is the obvious home for WASM/agent-detect closure; entrypoint/runtime surface follow-up should also feed `br-aazao.6` where CLI/server behavior is shared |

### Critical-Path Gaps To Treat As Realism Debt, Not “More Tests”

- `TOON`: current coverage proves the envelope contract, not a real encoder deployment. This is a release-confidence gap, not a missing-assertion nit.
- `Search + embedder + LLM`: these lanes already have good algorithm and contract coverage, but the substitutes are sitting on product-critical ranking and summarization paths. They need explicit realism boundaries.
- `Installer + self-update`: the mocked release/test-install lanes are useful, but recent live failures show they cannot be mistaken for production-proof coverage.
- `Shell E2E artifacts`: the harness is rich, but until `manifest.json` and summary-failure enforcement are uniform, downstream forensic automation still has blind spots.

### Secondary Gaps Where Ownership Matters More Than Raw Test Count

- `share`, `wasm`, `agent-detect`, and the binary entrypoint surfaces are the clearest places where coverage depth is still thin enough to warrant dedicated closure beads.
- Local-fixture tests in guard/storage/share are doing the right job for narrow policy/fault logic. The missing piece is not deleting those tests; it is pairing them with a few higher-level real-path proofs where they currently stand alone.

## Obvious Follow-On Targets

- `br-aazao.8.1`: add the missing per-suite `manifest.json` contract so the current rich artifact set becomes uniformly machine-readable at the case level.
- `br-aazao.5` / `6` / `7`: use the per-crate laggards above to drive concrete closure work instead of broad “add more tests” efforts.

## Completion Bar For `br-aazao.1`

This ledger satisfies the `br-aazao.1` audit parent if future beads can now point to one file for:

- which crates already have inline test density
- which crates already have dedicated integration/conformance harnesses
- where coverage is primarily behavioral vs contract/snapshot
- how the shell E2E surface is actually partitioned between wrappers, direct suites, and specialized harnesses
- which major surfaces are presently `R0` / `R1` / `R2` / `R3` / `R4`
- which downstream beads own the most important realism and coverage gaps
