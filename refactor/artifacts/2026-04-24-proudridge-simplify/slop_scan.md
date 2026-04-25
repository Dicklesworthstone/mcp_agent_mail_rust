# AI slop scan — 2026-04-24-proudridge-simplify

Generated 2026-04-24T22:21:10Z
Scope: `crates`

(See references/VIBE-CODED-PATHOLOGIES.md for P1-P40 catalog.)


## P1 over-defensive try/catch (Python: ≥3 except Exception per file)

```
9	crates/mcp-agent-mail-guard/src/lib.rs
6	crates/mcp-agent-mail-conformance/tests/conformance/python_reference/generate_fixtures.py
```

## P1 over-defensive try/catch (TS: catch blocks per file)

_none found_

## P2 long nullish/optional chains (three+ `?.`)

_none found_

## P2 double-nullish coalescing

_none found_

## P3 orphaned _v2/_new/_old/_improved/_copy files

```
crates/mcp-agent-mail-db/src/search_v3.rs
```

## P4 utils/helpers/misc/common files > 500 LOC

_none found_

## P5 abstract Base/Abstract class hierarchy

_none found_

## P5 abstract class in Rust (rare idiom; often AI-generated)

_none found_

## P6 feature flags (review each for whether it is still toggling)

```
FEATURE_DISABLED
FEATURE_MAPPING
FEATURE_SCHEMA_VERSION
FEATURE_VERSION
FLAG_9X7Q2K
FLAG_REGISTRY
LEGACY_DATABASE_URL
LEGACY_FIXTURE_REPO_INSTALL_PATH
LEGACY_FIXTURE_REPO_UNINSTALL_PATH
LEGACY_PREDICATE
LEGACY_REQUIRED
LEGACY_VERSION
```

## P7 re-export barrel files (`export * from`)

_none found_

## P8 pass-through wrappers (function whose sole body returns another call)

_none found_

## P9 functions with ≥5 optional parameters

_none found_

## P10 swallowed catch (empty or `return null`)

_none found_

## P10 Python: except ... : pass

```
crates/mcp-agent-mail-conformance/tests/conformance/python_reference/generate_fixtures.py:140:        except Exception:
crates/mcp-agent-mail-conformance/tests/conformance/python_reference/generate_fixtures.py:141:            pass
crates/mcp-agent-mail-conformance/tests/conformance/python_reference/generate_fixtures.py:333:    except Exception:
crates/mcp-agent-mail-conformance/tests/conformance/python_reference/generate_fixtures.py:334:        pass
```

## P11 Step/Phase/TODO comments (per-file counts)

```
crates/mcp-agent-mail-share/src/planner.rs:19
crates/mcp-agent-mail-server/tests/tui_soak_replay.rs:11
crates/mcp-agent-mail-search-core/tests/fault_injection.rs:11
crates/mcp-agent-mail-db/tests/fault_injection.rs:11
crates/mcp-agent-mail-search-core/tests/parser_filter_fusion_rerank.rs:10
crates/mcp-agent-mail-db/tests/parser_filter_fusion_rerank.rs:10
crates/mcp-agent-mail-db/tests/stress.rs:9
crates/mcp-agent-mail-storage/src/lib.rs:8
crates/mcp-agent-mail-db/src/schema.rs:8
crates/mcp-agent-mail-core/src/git_binary.rs:8
crates/mcp-agent-mail-cli/src/lib.rs:7
crates/mcp-agent-mail-server/src/lib.rs:6
crates/mcp-agent-mail-db/src/coalesce.rs:6
crates/mcp-agent-mail-storage/tests/stress_pipeline.rs:5
crates/mcp-agent-mail-server/tests/alien_integration.rs:5
crates/mcp-agent-mail-server/src/startup_checks.rs:5
crates/mcp-agent-mail-server/tests/pty_e2e_search.rs:4
crates/mcp-agent-mail-db/tests/coalesce_stress.rs:4
crates/mcp-agent-mail-db/src/reconstruct.rs:4
crates/mcp-agent-mail-cli/src/ci.rs:4
```

## P12 many-import files (top 20)

_none found_

## P14 mocks (jest.mock, vi.mock, sinon.stub, __mocks__)

_none found_

## P15 TS `any` usage (per-file counts, top 20)

_none found_

## P16 *Error enums in Rust (often duplicate variants)

```
crates/mcp-agent-mail-core/src/agent_detect.rs:61:pub enum AgentDetectError {
crates/mcp-agent-mail-agent-detect/src/lib.rs:56:pub enum AgentDetectError {
crates/mcp-agent-mail-core/src/mcp_config.rs:141:pub enum McpConfigUpdateError {
crates/mcp-agent-mail-conformance/src/lib.rs:183:pub enum FixtureLoadError {
crates/mcp-agent-mail-core/src/toon.rs:664:pub enum EncoderError {
crates/mcp-agent-mail-core/src/setup.rs:19:pub enum SetupError {
crates/mcp-agent-mail-core/src/flags.rs:124:pub enum FlagRegistryError {
crates/mcp-agent-mail-core/src/git_binary.rs:40:pub enum GitBinaryError {
crates/mcp-agent-mail-core/src/experience.rs:815:pub enum FeatureSchemaMigrationError {
crates/mcp-agent-mail-tools/src/llm.rs:224:pub enum LlmError {
crates/mcp-agent-mail-cli/src/bench.rs:134:pub enum BenchValidationError {
crates/mcp-agent-mail-cli/src/bench.rs:190:pub enum BenchTimingError {
crates/mcp-agent-mail-cli/src/bench.rs:374:pub enum BenchSeedError {
crates/mcp-agent-mail-cli/src/bench.rs:1089:pub enum BenchBaselineError {
crates/mcp-agent-mail-search-core/src/error.rs:10:pub enum SearchError {
crates/mcp-agent-mail-cli/src/ci.rs:1391:pub enum GateRunnerError {
crates/mcp-agent-mail-storage/src/lib.rs:44:pub enum StorageError {
crates/mcp-agent-mail-cli/src/golden.rs:97:pub enum GoldenCaptureError {
crates/mcp-agent-mail-cli/src/golden.rs:106:pub enum GoldenChecksumError {
crates/mcp-agent-mail-server/src/tui_macro.rs:249:pub enum MacroError {
crates/mcp-agent-mail-guard/src/lib.rs:8:pub enum GuardError {
crates/mcp-agent-mail-cli/src/lib.rs:49:pub enum CliError {
crates/mcp-agent-mail-share/src/probe.rs:26:pub enum ProbeError {
crates/mcp-agent-mail-server/src/tui_preset.rs:696:pub enum PresetError {
crates/mcp-agent-mail-share/src/wizard.rs:307:pub enum WizardErrorCode {
crates/mcp-agent-mail-share/src/lib.rs:245:pub enum ShareError {
crates/mcp-agent-mail-db/src/search_error.rs:10:pub enum SearchError {
crates/mcp-agent-mail-db/src/error.rs:7:pub enum DbError {
crates/mcp-agent-mail-db/src/migrate.rs:37:pub enum MigrationError {
crates/mcp-agent-mail-db/src/coalesce.rs:129:pub enum CoalesceJoinError {
```

## P17 heavily drilled props (top 10 most-passed via JSX)

_none found_

## P18 everything hook (custom hook file with many useState/useEffect)

_none found_

## P19 N+1 pattern (await inside for loop)

_none found_

## P19 Python N+1 (for ... : await)

```
crates/mcp-agent-mail-conformance/tests/conformance/python_reference/generate_fixtures.py:1016:        for uri, case_name in resource_uris:
crates/mcp-agent-mail-conformance/tests/conformance/python_reference/generate_fixtures.py:1017:            out = await _read_resource_json(mcp, uri)
```

## P20 config files (candidates for unification)

```
./.env
```

## P22 stringly-typed status/state comparisons

_none found_

## P22 Rust stringly-typed status/state comparisons

```
crates/mcp-agent-mail-tools/src/identity.rs:809:    let pool = if semantic_readiness.status == "fail" {
crates/mcp-agent-mail-tools/src/identity.rs:850:        status: if semantic_readiness.status == "fail" {
crates/mcp-agent-mail-conformance/tests/resource_coverage_guard.rs:204:        .filter(|row| row.fixture_status == "covered")
crates/mcp-agent-mail-conformance/tests/resource_coverage_guard.rs:214:        .filter(|row| row.fixture_status == "gap")
crates/mcp-agent-mail-cli/tests/tui_accessibility_harness.rs:376:            adapter_status == "pass",
crates/mcp-agent-mail-core/src/diagnostics.rs:711:            next_action: if status == "ok" {
crates/mcp-agent-mail-cli/src/ci.rs:786:            if statuses.iter().any(|status| status == "fail") {
crates/mcp-agent-mail-cli/src/ci.rs:788:            } else if statuses.iter().all(|status| status == "pass") {
crates/mcp-agent-mail-cli/src/ci.rs:790:            } else if statuses.iter().all(|status| status == "skip") {
crates/mcp-agent-mail-cli/src/ci.rs:792:            } else if statuses.iter().any(|status| status == "missing") {
crates/mcp-agent-mail-server/tests/web_ui_parity_contract_guard.rs:139:        if row.status == "waived" {
crates/mcp-agent-mail-cli/src/robot.rs:4386:                unhealthy: status == "fail",
crates/mcp-agent-mail-cli/src/robot.rs:4387:                degraded: status == "warn",
crates/mcp-agent-mail-db/src/atc_queries.rs:1764:                        let resolved = state == "resolved";
crates/mcp-agent-mail-db/src/schema.rs:2627:            let statement_result = if migration.id == "v15_add_recipients_json_to_messages" {
crates/mcp-agent-mail-cli/src/lib.rs:8213:        let present = rows.iter().filter(|row| row.status == "present").count();
crates/mcp-agent-mail-cli/src/lib.rs:8214:        let missing = rows.iter().filter(|row| row.status == "missing").count();
crates/mcp-agent-mail-cli/src/lib.rs:8215:        let stale = rows.iter().filter(|row| row.status == "stale").count();
crates/mcp-agent-mail-cli/src/lib.rs:16237:        } else if status == "fail" {
crates/mcp-agent-mail-cli/src/lib.rs:16239:        } else if status == "warn" {
crates/mcp-agent-mail-cli/src/lib.rs:16356:    let primary_fail_count = if primary_status == "fail" { 1 } else { 0 };
crates/mcp-agent-mail-cli/src/lib.rs:16357:    let primary_warn_count = if primary_status == "warn" { 1 } else { 0 };
crates/mcp-agent-mail-cli/src/lib.rs:16452:        let icon = if overall_status == "ok" { "OK" } else { "WARN" };
crates/mcp-agent-mail-cli/src/lib.rs:16453:        let detail = if overall_status == "ok" {
crates/mcp-agent-mail-cli/src/lib.rs:18132:                "status": if source == "env" { "fail" } else { "warn" },
crates/mcp-agent-mail-cli/src/lib.rs:19656:    let fail_count = checks.iter().filter(|c| c["status"] == "fail").count();
crates/mcp-agent-mail-cli/src/lib.rs:19657:    let warn_count = checks.iter().filter(|c| c["status"] == "warn").count();
crates/mcp-agent-mail-cli/src/lib.rs:20796:                            if post.http_check.status == "ok" && post.rpc_check.status == "ok" =>
crates/mcp-agent-mail-cli/src/lib.rs:20817:                    ) && post.http_check.status == "ok"
crates/mcp-agent-mail-cli/src/lib.rs:20818:                        && post.rpc_check.status == "ok")
crates/mcp-agent-mail-server/src/tui_action_menu.rs:892:    if status == "pending" {
crates/mcp-agent-mail-server/src/tui_screens/system_health.rs:2180:        if execution.status == "failed" {
crates/mcp-agent-mail-server/src/tui_screens/system_health.rs:2209:        let level = if agent.state == "dead" {
crates/mcp-agent-mail-server/src/tui_screens/contacts.rs:131:            Self::Pending => status == "pending",
crates/mcp-agent-mail-server/src/tui_screens/contacts.rs:132:            Self::Approved => status == "approved",
crates/mcp-agent-mail-server/src/tui_screens/contacts.rs:133:            Self::Blocked => status == "blocked",
crates/mcp-agent-mail-server/src/tui_screens/contacts.rs:1008:            .filter(|c| c.status == "approved")
crates/mcp-agent-mail-server/src/tui_screens/contacts.rs:1013:            .filter(|c| c.status == "pending")
crates/mcp-agent-mail-server/src/tui_screens/contacts.rs:1018:            .filter(|c| c.status == "blocked")
crates/mcp-agent-mail-server/src/tui_web_dashboard.rs:1963:    if (!resp.ok || payload.status === "inactive") {
```

## P23 reflex trim/lower/upper normalization

```
crates/mcp-agent-mail-test-helpers/src/parity.rs:59:    String::from_utf8_lossy(&out.stdout).trim().to_string()
crates/mcp-agent-mail-test-helpers/src/parity.rs:79:    let version = stdout.trim();
crates/mcp-agent-mail-agent-detect/src/lib.rs:92:    let slug = raw.trim().to_ascii_lowercase();
crates/mcp-agent-mail-cli/src/bench.rs:229:        if self.name.trim().is_empty() {
crates/mcp-agent-mail-cli/src/bench.rs:1268:            .filter(|v| !v.trim().is_empty())
crates/mcp-agent-mail-cli/src/bench.rs:1277:            .map(|out| out.trim().to_string())
crates/mcp-agent-mail-cli/src/ci.rs:438:    } else if !stderr.trim().is_empty() {
crates/mcp-agent-mail-cli/src/e2e_artifacts.rs:60:        match s.to_lowercase().as_str() {
crates/mcp-agent-mail-cli/src/e2e_artifacts.rs:215:                        .map(|s| s.trim().to_string())
crates/mcp-agent-mail-cli/src/e2e_artifacts.rs:230:                        .map(|s| s.trim().to_string())
crates/mcp-agent-mail-search-core/tests/query_assistance_explain.rs:239:        assert!(qa.query_text.is_empty() || qa.query_text.trim().is_empty());
crates/mcp-agent-mail-cli/src/output.rs:552:        let parsed: serde_json::Value = serde_json::from_str(output.trim()).unwrap();
crates/mcp-agent-mail-cli/src/output.rs:572:        assert_eq!(output.trim(), "[]");
crates/mcp-agent-mail-cli/src/output.rs:580:        assert_eq!(output.trim(), "No items found.");
crates/mcp-agent-mail-cli/src/output.rs:658:        let parsed: serde_json::Value = serde_json::from_str(output.trim()).unwrap();
crates/mcp-agent-mail-cli/src/output.rs:907:            serde_json::from_str(output.trim()).expect("JSON output must be valid JSON");
crates/mcp-agent-mail-cli/src/output.rs:990:            serde_json::from_str(output.trim()).expect("must be valid JSON array");
crates/mcp-agent-mail-cli/src/output.rs:1133:        let parsed: serde_json::Value = serde_json::from_str(output.trim()).unwrap();
crates/mcp-agent-mail-cli/src/output.rs:1215:        let parsed: serde_json::Value = serde_json::from_str(output.trim()).unwrap();
crates/mcp-agent-mail-cli/src/output.rs:1264:            serde_json::from_str(output.trim()).expect("error output should be valid json");
crates/mcp-agent-mail-cli/src/output.rs:1287:            serde_json::from_str(output.trim()).expect("error output should be valid json");
crates/mcp-agent-mail-cli/src/output.rs:1296:        assert_eq!(output.trim(), "[]");
crates/mcp-agent-mail-cli/src/output.rs:1304:        assert_eq!(output.trim(), "[]");
crates/mcp-agent-mail-cli/src/output.rs:1312:        assert_eq!(output.trim(), "No results found.");
crates/mcp-agent-mail-cli/src/flags.rs:439:        let trimmed = output.trim();
crates/mcp-agent-mail-cli/src/doctor_orphan_refs.rs:509:            String::from_utf8_lossy(&out.stderr).trim(),
crates/mcp-agent-mail-cli/src/golden.rs:285:        let line = raw_line.trim();
crates/mcp-agent-mail-cli/src/golden.rs:295:        let hash = hash_raw.trim();
crates/mcp-agent-mail-cli/src/golden.rs:296:        let filename = filename_raw.trim();
crates/mcp-agent-mail-cli/src/context.rs:100:    let trimmed = host.trim();
crates/mcp-agent-mail-cli/src/context.rs:309:    let key = key.trim();
crates/mcp-agent-mail-cli/src/robot.rs:43:    let table_ref = table_ref.trim().trim_end_matches('.');
crates/mcp-agent-mail-cli/src/robot.rs:100:    let table_ref = table_ref.trim().trim_end_matches('.');
crates/mcp-agent-mail-cli/src/robot.rs:168:            let trimmed = text.trim();
crates/mcp-agent-mail-cli/src/robot.rs:1878:                .map(|value| value.trim().to_string())
crates/mcp-agent-mail-cli/src/robot.rs:1961:            raw.trim().to_ascii_lowercase().as_str(),
crates/mcp-agent-mail-cli/src/robot.rs:1971:        .and_then(|raw| raw.trim().parse::<i64>().ok())
crates/mcp-agent-mail-cli/src/robot.rs:1985:                .map(|value| value.trim().to_string())
crates/mcp-agent-mail-cli/src/robot.rs:1991:                .map(|value| value.trim().to_string())
crates/mcp-agent-mail-cli/src/robot.rs:2212:    let project_key = project_key.trim();
crates/mcp-agent-mail-cli/src/robot.rs:2226:            let file_name = file_name.trim();
crates/mcp-agent-mail-cli/src/robot.rs:2279:    let agent_name = agent_name.trim();
crates/mcp-agent-mail-cli/src/robot.rs:4018:    let raw_query = query.trim();
crates/mcp-agent-mail-cli/src/robot.rs:5508:        let logical_name = name.to_lowercase();
crates/mcp-agent-mail-cli/src/robot.rs:6271:                .map(|value| value.trim().to_ascii_lowercase())
crates/mcp-agent-mail-cli/src/robot.rs:6288:                let format = format.trim();
crates/mcp-agent-mail-cli/src/robot.rs:6298:                .map(|value| value.trim())
crates/mcp-agent-mail-cli/src/robot.rs:6418:                .filter(|value| !value.trim().is_empty())
crates/mcp-agent-mail-cli/src/robot.rs:6596:        .map(|value| value.trim().to_string())
crates/mcp-agent-mail-cli/src/robot.rs:6608:        let trimmed = raw_url.trim();
crates/mcp-agent-mail-cli/src/robot.rs:7155:        let needle = needle.trim().to_ascii_lowercase();
crates/mcp-agent-mail-cli/src/robot.rs:7845:        .filter(|value| !value.trim().is_empty())
crates/mcp-agent-mail-cli/src/robot.rs:8487:            let health_str = format!("{health_level:?}").to_lowercase();
crates/mcp-agent-mail-cli/src/legacy.rs:1257:            let path_str = String::from_utf8_lossy(&output.stdout).trim().to_string();
crates/mcp-agent-mail-cli/src/legacy.rs:1580:        let trimmed = line.trim();
crates/mcp-agent-mail-cli/src/legacy.rs:1592:        let key = k.trim().to_string();
crates/mcp-agent-mail-cli/src/legacy.rs:1596:        let mut val = v.trim().to_string();
crates/mcp-agent-mail-cli/src/legacy.rs:1828:    let input = input.trim().to_ascii_lowercase();
crates/mcp-agent-mail-cli/tests/integration_runs.rs:720:        !marker_body.trim().is_empty(),
crates/mcp-agent-mail-cli/tests/integration_runs.rs:771:        subject.trim(),
crates/mcp-agent-mail-cli/tests/integration_runs.rs:1241:    let trimmed = stdout.trim();
crates/mcp-agent-mail-cli/tests/integration_runs.rs:1845:    if !stdout.trim().is_empty() {
crates/mcp-agent-mail-cli/tests/integration_runs.rs:1846:        let value: serde_json::Value = serde_json::from_str(stdout.trim()).expect("valid JSON");
crates/mcp-agent-mail-cli/tests/integration_runs.rs:1889:    let value: serde_json::Value = serde_json::from_str(stdout.trim()).expect("valid JSON");
crates/mcp-agent-mail-cli/src/e2e_runner.rs:156:                let line = line.trim();
crates/mcp-agent-mail-cli/src/e2e_runner.rs:171:                        .map(|t| t.trim().to_lowercase())
crates/mcp-agent-mail-cli/src/e2e_runner.rs:1443:                            stdout.trim().is_empty(),
crates/mcp-agent-mail-cli/src/e2e_runner.rs:1859:            let line_lower = clean_line.to_lowercase();
crates/mcp-agent-mail-cli/src/e2e_runner.rs:1866:                    let word_lower = word.to_lowercase();
crates/mcp-agent-mail/src/main.rs:27:    let trimmed = raw.trim();
crates/mcp-agent-mail/src/main.rs:134:            value.trim().to_ascii_lowercase().as_str(),
crates/mcp-agent-mail/src/main.rs:277:    let trimmed = raw.trim();
crates/mcp-agent-mail/src/main.rs:321:    if let Some(path) = env_http_path.filter(|v| !v.trim().is_empty()) {
crates/mcp-agent-mail/src/main.rs:338:    let normalized = raw.trim().to_ascii_lowercase();
crates/mcp-agent-mail/src/main.rs:357:        let trimmed = line.trim();
crates/mcp-agent-mail/src/main.rs:365:        if lhs.trim() != key {
crates/mcp-agent-mail/src/main.rs:368:        let value = unquote_env_value(rhs.trim()).trim().to_string();
crates/mcp-agent-mail/src/main.rs:382:    if current_token.is_some_and(|token| !token.trim().is_empty()) {
crates/mcp-agent-mail/src/main.rs:586:                    if !description.trim().is_empty() {
crates/mcp-agent-mail-cli/tests/semantic_conformance.rs:200:    let value: serde_json::Value = serde_json::from_str(stdout.trim()).expect("valid JSON");
```

## P24 testability wrappers / mutable deps seams

_none found_

## P25 docstrings/comments that may contradict implementation

```
crates/mcp-agent-mail-conformance/tests/conformance/python_reference/generate_fixtures.py:650:                "body_md": "- ACK: will deploy\n- [ ] TODO: update docs\n`api/v2/users`\n@Carol\n",
crates/mcp-agent-mail-conformance/tests/conformance/fixtures/python_reference.json:750:            "body_md": "- ACK: will deploy\n- [ ] TODO: update docs\n`api/v2/users`\n@Carol\n"
crates/mcp-agent-mail-conformance/tests/conformance/fixtures/python_reference.json:763:              "body_md": "- ACK: will deploy\n- [ ] TODO: update docs\n`api/v2/users`\n@Carol\n",
crates/mcp-agent-mail-conformance/tests/conformance/fixtures/python_reference.json:784:                    "body_md": "- ACK: will deploy\n- [ ] TODO: update docs\n`api/v2/users`\n@Carol\n",
crates/mcp-agent-mail-conformance/tests/conformance/fixtures/python_reference.json:1177:                  "TODO: update docs"
crates/mcp-agent-mail-conformance/tests/conformance/fixtures/python_reference.json:1486:                  "TODO: update docs"
crates/mcp-agent-mail-conformance/tests/conformance/fixtures/python_reference.json:2008:                    "TODO: update docs"
crates/mcp-agent-mail-share/src/static_render.rs:820:  <footer>Generated by MCP Agent Mail static export pipeline</footer>
crates/mcp-agent-mail-guard/src/lib.rs:290:# Auto-generated by mcp-agent-mail install_guard
crates/mcp-agent-mail-tools/src/llm.rs:1364:            action_items: vec!["- [ ] TODO: update docs".into()],
crates/mcp-agent-mail-tools/src/llm.rs:1384:        assert!(merged.key_points.contains(&"TODO: update docs".to_string()));
crates/mcp-agent-mail-cli/src/lib.rs:1739:        /// Agent name (adjective+noun, e.g. "BlueLake"). Auto-generated if omitted.
crates/mcp-agent-mail-cli/src/lib.rs:1766:        /// Name hint (adjective+noun). Auto-generated if omitted.
crates/mcp-agent-mail-cli/src/lib.rs:1839:        /// Agent name (adjective+noun). Auto-generated if omitted.
crates/mcp-agent-mail-cli/src/lib.rs:1879:        /// Agent name. Auto-generated if omitted.
crates/mcp-agent-mail-cli/src/robot.rs:9673:                "# {}\n\n*Generated by {} at {}*",
crates/mcp-agent-mail-core/src/evidence_ledger.rs:123:/// Returns:
```

## P26 TypeScript type assertions

_none found_

## P27 addEventListener sites (audit for cleanup)

_none found_

## P28 timers (audit for clearTimeout/clearInterval cleanup)

_none found_

## P29 regex construction in functions/loops

```
crates/mcp-agent-mail-conformance/tests/doc_consistency.rs:88:    Regex::new(pattern).unwrap_or_else(|e| panic!("invalid regex {pattern:?}: {e}"))
crates/mcp-agent-mail-conformance/tests/conformance.rs:1720:        regex::Regex::new(r"^\d{4}-\d{2}-\d{2}T\d{2}-\d{2}-\d{2}Z__[a-z0-9._-]+__\d+\.md$")
crates/mcp-agent-mail-search-core/src/lexical_parser.rs:35:    LazyLock::new(|| Regex::new(r"[\[\]{}^~\\]").unwrap_or_else(|_| unreachable!()));
crates/mcp-agent-mail-search-core/src/lexical_parser.rs:39:    LazyLock::new(|| Regex::new(r"^[\*\.\?!()]+$").unwrap_or_else(|_| unreachable!()));
crates/mcp-agent-mail-search-core/src/lexical_parser.rs:45:    Regex::new(r"[a-zA-Z0-9]+(?:-[a-zA-Z0-9]+)+").unwrap_or_else(|_| unreachable!())
crates/mcp-agent-mail-search-core/src/lexical_parser.rs:50:    LazyLock::new(|| Regex::new(r" {2,}").unwrap_or_else(|_| unreachable!()));
crates/mcp-agent-mail-search-core/src/canonical.rs:47:        regex::Regex::new(r"(?i)ghp_[A-Za-z0-9]{36,}").unwrap_or_else(|_| unreachable!()),
crates/mcp-agent-mail-search-core/src/canonical.rs:48:        regex::Regex::new(r"(?i)github_pat_[A-Za-z0-9_]{20,}").unwrap_or_else(|_| unreachable!()),
crates/mcp-agent-mail-search-core/src/canonical.rs:50:        regex::Regex::new(r"(?i)xox[baprs]-[A-Za-z0-9\-]{10,}").unwrap_or_else(|_| unreachable!()),
crates/mcp-agent-mail-search-core/src/canonical.rs:52:        regex::Regex::new(r"(?i)sk-[A-Za-z0-9]{20,}").unwrap_or_else(|_| unreachable!()),
crates/mcp-agent-mail-search-core/src/canonical.rs:54:        regex::Regex::new(r"(?i)bearer\s+[A-Za-z0-9_\-\.]{16,}").unwrap_or_else(|_| unreachable!()),
crates/mcp-agent-mail-search-core/src/canonical.rs:56:        regex::Regex::new(r"eyJ[0-9A-Za-z_-]+\.[0-9A-Za-z_-]+\.[0-9A-Za-z_-]+")
crates/mcp-agent-mail-search-core/src/canonical.rs:59:        regex::Regex::new(r"AKIA[0-9A-Z]{16}").unwrap_or_else(|_| unreachable!()),
crates/mcp-agent-mail-search-core/src/canonical.rs:61:        regex::Regex::new(r"-----BEGIN[A-Z ]* PRIVATE KEY-----").unwrap_or_else(|_| unreachable!()),
crates/mcp-agent-mail-search-core/src/canonical.rs:63:        regex::Regex::new(r"(?i)sk-ant-[A-Za-z0-9\-]{20,}").unwrap_or_else(|_| unreachable!()),
crates/mcp-agent-mail-search-core/src/canonical.rs:65:        regex::Regex::new(r"glpat-[A-Za-z0-9\-_]{20,}").unwrap_or_else(|_| unreachable!()),
crates/mcp-agent-mail-search-core/src/canonical.rs:67:        regex::Regex::new(r"(?i)(?:AGENT_MAIL_TOKEN|API_KEY|SECRET_KEY|PASSWORD)\s*=\s*\S+")
crates/mcp-agent-mail-search-core/src/canonical.rs:83:        regex::Regex::new(r"(?ms)^```[^\n]*\n.*?^```").unwrap_or_else(|_| unreachable!())
crates/mcp-agent-mail-search-core/src/canonical.rs:86:        LazyLock::new(|| regex::Regex::new(r"`[^`]+`").unwrap_or_else(|_| unreachable!()));
crates/mcp-agent-mail-search-core/src/canonical.rs:88:        regex::Regex::new(r"!\[([^\]]*)\]\([^)]+\)").unwrap_or_else(|_| unreachable!())
crates/mcp-agent-mail-search-core/src/canonical.rs:91:        regex::Regex::new(r"\[([^\]]*)\]\([^)]+\)").unwrap_or_else(|_| unreachable!())
crates/mcp-agent-mail-search-core/src/canonical.rs:94:        LazyLock::new(|| regex::Regex::new(r"(?m)^#{1,6}\s+").unwrap_or_else(|_| unreachable!()));
crates/mcp-agent-mail-search-core/src/canonical.rs:96:        regex::Regex::new(r"\*{1,3}([^*]+)\*{1,3}").unwrap_or_else(|_| unreachable!())
crates/mcp-agent-mail-search-core/src/canonical.rs:99:        regex::Regex::new(r"_{1,3}([^_]+)_{1,3}").unwrap_or_else(|_| unreachable!())
crates/mcp-agent-mail-search-core/src/canonical.rs:102:        LazyLock::new(|| regex::Regex::new(r"~~([^~]+)~~").unwrap_or_else(|_| unreachable!()));
crates/mcp-agent-mail-search-core/src/canonical.rs:104:        LazyLock::new(|| regex::Regex::new(r"(?m)^>\s*").unwrap_or_else(|_| unreachable!()));
crates/mcp-agent-mail-search-core/src/canonical.rs:106:        regex::Regex::new(r"(?m)^[-*_]{3,}\s*$").unwrap_or_else(|_| unreachable!())
crates/mcp-agent-mail-search-core/src/canonical.rs:109:        regex::Regex::new(r"(?m)^(\s*)[-*+]\s+").unwrap_or_else(|_| unreachable!())
crates/mcp-agent-mail-search-core/src/canonical.rs:112:        regex::Regex::new(r"(?m)^(\s*)\d+\.\s+").unwrap_or_else(|_| unreachable!())
crates/mcp-agent-mail-search-core/src/canonical.rs:115:        LazyLock::new(|| regex::Regex::new(r"<[^>]+>").unwrap_or_else(|_| unreachable!()));
crates/mcp-agent-mail-search-core/src/canonical.rs:117:        regex::Regex::new(r"(?m)^\|?[\s-]+\|[\s\-|]+$").unwrap_or_else(|_| unreachable!())
crates/mcp-agent-mail-core/src/git_binary.rs:427:    let re = Regex::new(r"git version (\d+)\.(\d+)\.(\d+)").ok()?;
crates/mcp-agent-mail-core/src/experience.rs:703:    LazyLock::new(|| Regex::new(r"-----BEGIN [A-Z ]+-----").expect("PEM regex"));
crates/mcp-agent-mail-core/src/experience.rs:706:    Regex::new(r"eyJ[A-Za-z0-9_-]{10,}\.[A-Za-z0-9_-]{10,}\.").expect("JWT regex")
crates/mcp-agent-mail-storage/src/lib.rs:4596:    RE.get_or_init(|| Regex::new(r"[^a-zA-Z0-9._-]+").unwrap_or_else(|_| unreachable!()))
crates/mcp-agent-mail-storage/src/lib.rs:5740:        Regex::new(r"!\[(?P<alt>[^\]]*)\]\((?P<path>[^)]+)\)").unwrap_or_else(|_| unreachable!())
crates/mcp-agent-mail-share/src/scrub.rs:32:        Regex::new(r"(?i)ghp_[A-Za-z0-9]{36,}").unwrap_or_else(|_| unreachable!()),
crates/mcp-agent-mail-share/src/scrub.rs:33:        Regex::new(r"(?i)github_pat_[A-Za-z0-9_]{20,}").unwrap_or_else(|_| unreachable!()),
crates/mcp-agent-mail-share/src/scrub.rs:35:        Regex::new(r"(?i)xox[baprs]-[A-Za-z0-9\-]{10,}").unwrap_or_else(|_| unreachable!()),
crates/mcp-agent-mail-share/src/scrub.rs:37:        Regex::new(r"(?i)sk-ant-[A-Za-z0-9\-]{20,}").unwrap_or_else(|_| unreachable!()),
crates/mcp-agent-mail-share/src/scrub.rs:39:        Regex::new(r"(?i)(?:sk|pk|rk)_(?:live|test)_[A-Za-z0-9]{10,}")
crates/mcp-agent-mail-share/src/scrub.rs:42:        Regex::new(r"(?i)sk-[A-Za-z0-9]{20,}").unwrap_or_else(|_| unreachable!()),
crates/mcp-agent-mail-share/src/scrub.rs:44:        Regex::new(r"(?i)bearer\s+[A-Za-z0-9_\-\./+=]{16,}").unwrap_or_else(|_| unreachable!()),
crates/mcp-agent-mail-share/src/scrub.rs:46:        Regex::new(r"(?i)[a-z][a-z0-9+.-]*://[^/\s@]+:[^@\s/]+@")
crates/mcp-agent-mail-share/src/scrub.rs:49:        Regex::new(r"(?i)\$[A-Z_][A-Z0-9_]*(?:SECRET|TOKEN|KEY|PASSWORD)[A-Z0-9_]*")
crates/mcp-agent-mail-share/src/scrub.rs:52:        Regex::new(r"eyJ[0-9A-Za-z_-]+\.[0-9A-Za-z_-]+\.[0-9A-Za-z_-]+")
crates/mcp-agent-mail-share/src/scrub.rs:55:        Regex::new(r"AKIA[0-9A-Z]{16}").unwrap_or_else(|_| unreachable!()),
crates/mcp-agent-mail-share/src/scrub.rs:57:        Regex::new(r"(?i)(?:AccountKey|SharedAccessKey)=[A-Za-z0-9+/=]{20,}")
crates/mcp-agent-mail-share/src/scrub.rs:60:        Regex::new(r#""private_key_id"\s*:\s*"[a-f0-9]{40}""#).unwrap_or_else(|_| unreachable!()),
crates/mcp-agent-mail-share/src/scrub.rs:62:        Regex::new(r"AIza[0-9A-Za-z\-_]{35}").unwrap_or_else(|_| unreachable!()),
crates/mcp-agent-mail-share/src/scrub.rs:64:        Regex::new(r"(?i)npm_[A-Za-z0-9]{36,}").unwrap_or_else(|_| unreachable!()),
crates/mcp-agent-mail-share/src/scrub.rs:66:        Regex::new(r"(?s)-----BEGIN[A-Z ]* PRIVATE KEY-----.*?-----END[A-Z ]* PRIVATE KEY-----")
crates/mcp-agent-mail-share/src/scrub.rs:69:        Regex::new(r"glpat-[A-Za-z0-9\-_]{20,}").unwrap_or_else(|_| unreachable!()),
crates/mcp-agent-mail-db/src/search_canonical.rs:47:        regex::Regex::new(r"(?i)ghp_[A-Za-z0-9]{36,}").unwrap_or_else(|_| unreachable!()),
crates/mcp-agent-mail-db/src/search_canonical.rs:48:        regex::Regex::new(r"(?i)github_pat_[A-Za-z0-9_]{20,}").unwrap_or_else(|_| unreachable!()),
crates/mcp-agent-mail-db/src/search_canonical.rs:50:        regex::Regex::new(r"(?i)xox[baprs]-[A-Za-z0-9\-]{10,}").unwrap_or_else(|_| unreachable!()),
crates/mcp-agent-mail-db/src/search_canonical.rs:52:        regex::Regex::new(r"(?i)sk-[A-Za-z0-9]{20,}").unwrap_or_else(|_| unreachable!()),
crates/mcp-agent-mail-db/src/search_canonical.rs:54:        regex::Regex::new(r"(?i)bearer\s+[A-Za-z0-9_\-\.]{16,}").unwrap_or_else(|_| unreachable!()),
crates/mcp-agent-mail-db/src/search_canonical.rs:56:        regex::Regex::new(r"eyJ[0-9A-Za-z_-]+\.[0-9A-Za-z_-]+\.[0-9A-Za-z_-]+")
crates/mcp-agent-mail-db/src/search_canonical.rs:59:        regex::Regex::new(r"AKIA[0-9A-Z]{16}").unwrap_or_else(|_| unreachable!()),
crates/mcp-agent-mail-db/src/search_canonical.rs:61:        regex::Regex::new(r"-----BEGIN[A-Z ]* PRIVATE KEY-----").unwrap_or_else(|_| unreachable!()),
crates/mcp-agent-mail-db/src/search_canonical.rs:63:        regex::Regex::new(r"(?i)sk-ant-[A-Za-z0-9\-]{20,}").unwrap_or_else(|_| unreachable!()),
crates/mcp-agent-mail-db/src/search_canonical.rs:65:        regex::Regex::new(r"glpat-[A-Za-z0-9\-_]{20,}").unwrap_or_else(|_| unreachable!()),
crates/mcp-agent-mail-db/src/search_canonical.rs:67:        regex::Regex::new(r"(?i)(?:AGENT_MAIL_TOKEN|API_KEY|SECRET_KEY|PASSWORD)\s*=\s*\S+")
crates/mcp-agent-mail-db/src/search_canonical.rs:83:        regex::Regex::new(r"(?ms)^```[^\n]*\n.*?^```").unwrap_or_else(|_| unreachable!())
crates/mcp-agent-mail-db/src/search_canonical.rs:86:        LazyLock::new(|| regex::Regex::new(r"`[^`]+`").unwrap_or_else(|_| unreachable!()));
crates/mcp-agent-mail-db/src/search_canonical.rs:88:        regex::Regex::new(r"!\[([^\]]*)\]\([^)]+\)").unwrap_or_else(|_| unreachable!())
crates/mcp-agent-mail-db/src/search_canonical.rs:91:        regex::Regex::new(r"\[([^\]]*)\]\([^)]+\)").unwrap_or_else(|_| unreachable!())
crates/mcp-agent-mail-db/src/search_canonical.rs:94:        LazyLock::new(|| regex::Regex::new(r"(?m)^#{1,6}\s+").unwrap_or_else(|_| unreachable!()));
crates/mcp-agent-mail-db/src/search_canonical.rs:96:        regex::Regex::new(r"\*{1,3}([^*]+)\*{1,3}").unwrap_or_else(|_| unreachable!())
crates/mcp-agent-mail-db/src/search_canonical.rs:99:        regex::Regex::new(r"_{1,3}([^_]+)_{1,3}").unwrap_or_else(|_| unreachable!())
crates/mcp-agent-mail-db/src/search_canonical.rs:102:        LazyLock::new(|| regex::Regex::new(r"~~([^~]+)~~").unwrap_or_else(|_| unreachable!()));
crates/mcp-agent-mail-db/src/search_canonical.rs:104:        LazyLock::new(|| regex::Regex::new(r"(?m)^>\s*").unwrap_or_else(|_| unreachable!()));
crates/mcp-agent-mail-db/src/search_canonical.rs:106:        regex::Regex::new(r"(?m)^[-*_]{3,}\s*$").unwrap_or_else(|_| unreachable!())
crates/mcp-agent-mail-db/src/search_canonical.rs:109:        regex::Regex::new(r"(?m)^(\s*)[-*+]\s+").unwrap_or_else(|_| unreachable!())
crates/mcp-agent-mail-db/src/search_canonical.rs:112:        regex::Regex::new(r"(?m)^(\s*)\d+\.\s+").unwrap_or_else(|_| unreachable!())
crates/mcp-agent-mail-db/src/search_canonical.rs:115:        LazyLock::new(|| regex::Regex::new(r"<[^>]+>").unwrap_or_else(|_| unreachable!()));
crates/mcp-agent-mail-db/src/search_canonical.rs:117:        regex::Regex::new(r"(?m)^\|?[\s-]+\|[\s\-|]+$").unwrap_or_else(|_| unreachable!())
crates/mcp-agent-mail-db/src/query_assistance.rs:35:    LazyLock::new(|| Regex::new(r"[\[\]{}^~\\]").unwrap_or_else(|_| unreachable!()));
crates/mcp-agent-mail-db/src/query_assistance.rs:39:    LazyLock::new(|| Regex::new(r"^[\*\.\?!()]+$").unwrap_or_else(|_| unreachable!()));
```

## P30 debug print/log leftovers

```
crates/mcp-agent-mail-test-helpers/src/parity.rs:117:                eprintln!(
crates/mcp-agent-mail-test-helpers/src/parity.rs:123:            eprintln!(
crates/mcp-agent-mail/src/main.rs:455:            eprintln!("Error: {msg}");
crates/mcp-agent-mail/src/main.rs:456:            eprintln!("Usage: AM_INTERFACE_MODE={{mcp|cli}} mcp-agent-mail ...");
crates/mcp-agent-mail/src/main.rs:548:                    eprintln!("Error: {msg}");
crates/mcp-agent-mail/src/main.rs:565:                    eprintln!(
crates/mcp-agent-mail/src/main.rs:572:                    eprintln!(
crates/mcp-agent-mail/src/main.rs:576:                    eprintln!(
crates/mcp-agent-mail/src/main.rs:582:                    eprintln!(
crates/mcp-agent-mail/src/main.rs:587:                        eprintln!("am: {description}");
crates/mcp-agent-mail/src/main.rs:589:                    eprintln!("am: free the port or choose a different one with --port.");
crates/mcp-agent-mail/src/main.rs:619:            eprintln!("{}", summary.format(mode));
crates/mcp-agent-mail/src/main.rs:633:            ftui_runtime::ftui_println!("{:#?}", config);
crates/mcp-agent-mail/src/main.rs:649:    eprintln!(
crates/mcp-agent-mail/src/main.rs:660:        eprintln!("\nTip: Run `am --help` for the full command list.");
crates/mcp-agent-mail/src/main.rs:668:    eprintln!(
crates/mcp-agent-mail-tools/tests/validation_error_parity.rs:103:        eprintln!("Testing validation: input=invalid since_ts...");
crates/mcp-agent-mail-tools/tests/validation_error_parity.rs:175:        eprintln!("Testing validation: input=invalid thread_id...");
crates/mcp-agent-mail-tools/tests/validation_error_parity.rs:303:        eprintln!("Testing validation: input=empty program...");
crates/mcp-agent-mail-tools/tests/validation_error_parity.rs:348:        eprintln!("Testing validation: input=empty model...");
crates/mcp-agent-mail-tools/tests/validation_error_parity.rs:393:        eprintln!("Testing validation: input=invalid limit...");
crates/mcp-agent-mail-tools/tests/validation_error_parity.rs:443:        eprintln!("Testing validation: input=negative limit...");
crates/mcp-agent-mail-tools/tests/validation_error_parity.rs:481:        eprintln!("Testing validation: input=limit > 1000 caps...");
crates/mcp-agent-mail-tools/tests/validation_error_parity.rs:517:        eprintln!("Testing validation: input=subject > 200 chars...");
crates/mcp-agent-mail-tools/tests/validation_error_parity.rs:566:        eprintln!("Testing validation: input=subject exactly 200 chars...");
crates/mcp-agent-mail-tools/tests/validation_error_parity.rs:614:        eprintln!("Testing validation: input=empty paths array...");
crates/mcp-agent-mail-tools/tests/validation_error_parity.rs:656:        eprintln!("Testing validation: input=invalid reservation glob...");
crates/mcp-agent-mail/benches/benchmarks.rs:3148:                            println!(
crates/mcp-agent-mail-tools/tests/reservation_error_parity.rs:74:        eprintln!("Testing reservation error: scenario={scenario}...");
crates/mcp-agent-mail-tools/tests/reservation_error_parity.rs:128:        eprintln!("Testing reservation error: scenario={scenario}...");
crates/mcp-agent-mail-tools/tests/reservation_error_parity.rs:240:        eprintln!("Testing reservation error: scenario={scenario}...");
crates/mcp-agent-mail-tools/tests/reservation_error_parity.rs:300:        eprintln!("Testing reservation error: scenario={scenario}...");
crates/mcp-agent-mail-tools/tests/reservation_error_parity.rs:345:        eprintln!("Testing reservation error: scenario={scenario}...");
crates/mcp-agent-mail-db/tests/atc_rollup_snapshot.rs:29:                eprintln!("[SETUP] Applied {} migrations for {name}", applied.len());
crates/mcp-agent-mail-tools/tests/contact_policy_parity.rs:149:        eprintln!("Testing contact violation: scenario={scenario}...");
crates/mcp-agent-mail-tools/tests/contact_policy_parity.rs:218:        eprintln!("Testing contact violation: scenario={scenario}...");
crates/mcp-agent-mail-tools/tests/contact_policy_parity.rs:315:        eprintln!("Testing contact violation: scenario={scenario}...");
crates/mcp-agent-mail-db/tests/atc_leader_lease.rs:30:                eprintln!("[SETUP] Applied {} migrations for {name}", applied.len());
crates/mcp-agent-mail-tools/tests/agent_name_parity.rs:55:    eprintln!("Testing agent name \"{name}\"...");
crates/mcp-agent-mail-tools/tests/agent_name_parity.rs:59:    eprintln!("Detected as {actual_category}: message=\"{actual_msg}\"");
crates/mcp-agent-mail-tools/tests/agent_name_parity.rs:196:        eprintln!("Testing agent name \"{name}\"...");
crates/mcp-agent-mail-db/tests/search_v3_conformance.rs:77:    eprintln!(
crates/mcp-agent-mail-db/tests/atc_experience_lifecycle.rs:58:                eprintln!("[SETUP] Applied {} migrations for {name}", applied.len());
crates/mcp-agent-mail-db/tests/atc_experience_lifecycle.rs:60:                    eprintln!("[SETUP]   - {m}");
crates/mcp-agent-mail-db/tests/atc_experience_lifecycle.rs:140:        eprintln!("\n[1/6] APPEND experience: decision=100, effect=200, subject=GreenCastle");
crates/mcp-agent-mail-db/tests/atc_experience_lifecycle.rs:151:        eprintln!("[1/6] OK: experience_id={exp_id}, state=planned");
crates/mcp-agent-mail-db/tests/atc_experience_lifecycle.rs:154:        eprintln!("[2/6] TRANSITION: planned → dispatched");
crates/mcp-agent-mail-db/tests/atc_experience_lifecycle.rs:167:        eprintln!("[2/6] OK: dispatched");
crates/mcp-agent-mail-db/tests/atc_experience_lifecycle.rs:170:        eprintln!("[3/6] TRANSITION: dispatched → executed");
crates/mcp-agent-mail-db/tests/atc_experience_lifecycle.rs:183:        eprintln!("[3/6] OK: executed");
crates/mcp-agent-mail-db/tests/atc_experience_lifecycle.rs:186:        eprintln!("[4/6] TRANSITION: executed → open");
crates/mcp-agent-mail-db/tests/atc_experience_lifecycle.rs:207:        eprintln!("[4/6] OK: open (verified via fetch_open_atc_experiences)");
crates/mcp-agent-mail-db/tests/atc_experience_lifecycle.rs:218:        eprintln!("[5/6] RESOLVE: correct=true, loss=0.5, regret=0.0");
crates/mcp-agent-mail-db/tests/atc_experience_lifecycle.rs:266:        eprintln!("[5/6] OK: resolved (verified state=resolved + outcome in DB)");
crates/mcp-agent-mail-db/tests/atc_experience_lifecycle.rs:269:        eprintln!("[6/6] ROLLUP: stratum=liveness:probe:tier0");
crates/mcp-agent-mail-db/tests/atc_experience_lifecycle.rs:287:        eprintln!("[6/6] OK: rollup updated");
crates/mcp-agent-mail-db/tests/atc_experience_lifecycle.rs:289:        eprintln!("\n === FULL LIFECYCLE PASSED ===");
crates/mcp-agent-mail-db/tests/atc_experience_lifecycle.rs:290:        eprintln!("   Planned → Dispatched → Executed → Open → Resolved → Rollup");
crates/mcp-agent-mail-db/tests/atc_experience_lifecycle.rs:291:        eprintln!("   Real SQLite. Real schema. No mocks.");
crates/mcp-agent-mail-db/tests/atc_experience_lifecycle.rs:303:        eprintln!("\n[THROTTLE] Testing non-execution path...");
crates/mcp-agent-mail-db/tests/atc_experience_lifecycle.rs:351:        eprintln!("[THROTTLE] OK: Planned → Dispatched → Throttled (terminal)");
crates/mcp-agent-mail-db/tests/atc_experience_lifecycle.rs:368:        eprintln!("\n[INVALID] Attempting Planned → Resolved (should fail)...");
crates/mcp-agent-mail-db/tests/atc_experience_lifecycle.rs:384:        eprintln!("[INVALID] OK: correctly rejected invalid transition");
crates/mcp-agent-mail-db/tests/atc_experience_lifecycle.rs:395:        eprintln!("\n[FANOUT] One decision, three effects...");
crates/mcp-agent-mail-db/tests/atc_experience_lifecycle.rs:423:        eprintln!(
crates/mcp-agent-mail-db/tests/atc_experience_lifecycle.rs:452:        eprintln!(
crates/mcp-agent-mail-db/tests/cache_golden.rs:153:        eprintln!(
crates/mcp-agent-mail-db/tests/cache_golden.rs:202:    eprintln!(
crates/mcp-agent-mail-db/tests/cache_golden.rs:259:    eprintln!("S3-FIFO hit rate: {s3_rate:.4}, LRU hit rate: {lru_rate:.4}, ratio: {ratio:.4}");
crates/mcp-agent-mail-db/tests/timeout_backpressure.rs:395:            eprintln!("FAIL {label}: expected {expected:?}, got {actual:?}");
crates/mcp-agent-mail-db/tests/timeout_backpressure.rs:399:    eprintln!(
crates/mcp-agent-mail-db/tests/timeout_backpressure.rs:459:    eprintln!("tool_shedding_matrix: {assertions} assertions passed");
crates/mcp-agent-mail-db/tests/timeout_backpressure.rs:605:    eprintln!("diagnostics_extraction_matrix: 11 cases, all passed");
crates/mcp-agent-mail-db/tests/timeout_backpressure.rs:654:    eprintln!("health_signals_from_snapshot: 5 scenarios passed");
crates/mcp-agent-mail-db/tests/timeout_backpressure.rs:709:    eprintln!("search_with_cost_quota_budget: 3 scenarios passed");
crates/mcp-agent-mail-db/tests/timeout_backpressure.rs:765:    eprintln!("execute_search_with_budget_options: 2 scenarios passed");
crates/mcp-agent-mail-db/tests/timeout_backpressure.rs:795:    eprintln!("health_level_transitions_are_deterministic: 200 assertions passed");
crates/mcp-agent-mail-db/tests/timeout_backpressure.rs:845:    eprintln!("all_degraded_diagnostics_have_remediation_hints: {assertions} assertions");
crates/mcp-agent-mail-db/tests/timeout_backpressure.rs:872:    eprintln!("health_level_ordering_and_serialization: 11 assertions passed");
crates/mcp-agent-mail-db/tests/timeout_backpressure.rs:925:    eprintln!("worst_signal_wins_classification: 9 scenarios passed");
```

## P31 JSON.stringify used as key/hash/memo identity

_none found_

## P32 money-like arithmetic (audit integer cents/decimal)

```
crates/mcp-agent-mail-db/benches/search_v3_bench.rs:309:                (count as f64 * build_ops as f64) / (build_total_us as f64 / 1_000_000.0)
crates/mcp-agent-mail/benches/benchmarks.rs:732:                    total_elements_f64 / (total_us_f64 / 1_000_000.0)
crates/mcp-agent-mail/benches/benchmarks.rs:1758:                        (total_elements / (total_us / 1_000_000.0) * 100.0).round() / 100.0
crates/mcp-agent-mail/benches/benchmarks.rs:3550:                total_reads_f64 / (total_us_f64 / 1_000_000.0)
crates/mcp-agent-mail-search-core/src/rollout.rs:228:            (overlap_sum as f64 / total as f64) / 10000.0
crates/mcp-agent-mail-search-core/src/rollout.rs:246:                equivalent as f64 / total as f64 * 100.0
crates/mcp-agent-mail-search-core/src/rollout.rs:252:                errors as f64 / total as f64 * 100.0
crates/mcp-agent-mail-search-core/src/cache.rs:207:            (self.hits as f64 / total as f64) * 100.0
crates/mcp-agent-mail-cli/src/robot.rs:8283:                (total_errors as f64 / total_calls as f64) * 100.0
crates/mcp-agent-mail-core/src/metrics.rs:1088:            shadow_equiv as f64 / shadow_total as f64 * 100.0
crates/mcp-agent-mail-cli/src/lib.rs:12467:                            let pct = (received as f64 / total as f64 * 100.0).min(100.0);
crates/mcp-agent-mail-cli/src/lib.rs:53915:                    format!("{:.1}", entry.total_wait_ns as f64 / 1_000_000.0),
crates/mcp-agent-mail-cli/src/lib.rs:54066:                        entry.total_wait_ns as f64 / 1_000_000.0
crates/mcp-agent-mail-core/src/diagnostics.rs:1739:        snap.tools.tool_errors_total = 5; // 2.5%
crates/mcp-agent-mail-server/src/lib.rs:514:    let query_time_ms = (after.total_time_ms - before.total_time_ms).max(0.0);
crates/mcp-agent-mail-db/src/atc_queries.rs:1841:        assert!((entry.total_regret - 1.5).abs() < f64::EPSILON);
crates/mcp-agent-mail-db/src/atc_queries.rs:1842:        assert!((entry.total_loss - 2.0).abs() < f64::EPSILON);
crates/mcp-agent-mail-db/src/atc_queries.rs:2151:        assert!((probe.total_loss - 1.0).abs() < f64::EPSILON);
crates/mcp-agent-mail-db/src/atc_queries.rs:2152:        assert!((probe.total_regret - 0.25).abs() < f64::EPSILON);
crates/mcp-agent-mail-db/src/atc_queries.rs:2258:        assert!((row.total_loss - 0.25).abs() < f64::EPSILON);
crates/mcp-agent-mail-db/src/atc_queries.rs:2259:        assert!((row.total_regret - 0.05).abs() < f64::EPSILON);
crates/mcp-agent-mail-db/src/search_cache.rs:206:            (self.hits as f64 / total as f64) * 100.0
crates/mcp-agent-mail-server/src/tui_widgets.rs:7721:        assert!((tool_end_total - 1.0).abs() < f64::EPSILON);
crates/mcp-agent-mail-server/src/tui_widgets.rs:7722:        assert!((msg_sent_total - 1.0).abs() < f64::EPSILON);
crates/mcp-agent-mail-server/src/tui_widgets.rs:7740:            (total - 2.0).abs() < f64::EPSILON,
crates/mcp-agent-mail-server/src/atc.rs:944:            (total - 1.0).abs() < 1e-10,
crates/mcp-agent-mail-server/src/atc.rs:1217:            (total - 1.0).abs() < 1e-6,
crates/mcp-agent-mail-server/src/atc.rs:1296:            (total - 1.0).abs() < 1e-10,
crates/mcp-agent-mail-server/src/atc.rs:8218:            (total - 1.0).abs() < 1e-10,
crates/mcp-agent-mail-db/src/archive_anomaly.rs:1://! Archive anomaly taxonomy and safe remediation classes (br-97gc6.5.2.4.1).
crates/mcp-agent-mail-db/src/tracking.rs:1258:                (snap.total_time_ms - expected_time).abs() < 0.02,
crates/mcp-agent-mail-db/src/tracking.rs:1471:        assert!((snap.total_time_ms - 6.0).abs() < 0.01);
crates/mcp-agent-mail-db/src/tracking.rs:1563:        assert!((snap.total_time_ms - 12.34).abs() < 0.001);
crates/mcp-agent-mail-db/src/search_rollout.rs:225:            (overlap_sum as f64 / total as f64) / 10000.0
crates/mcp-agent-mail-db/src/search_rollout.rs:241:                equivalent as f64 / total as f64 * 100.0
crates/mcp-agent-mail-db/src/search_rollout.rs:247:                errors as f64 / total as f64 * 100.0
crates/mcp-agent-mail-server/src/tui_screens/analytics.rs:678:        (total_errors as f64 / total_calls as f64) * 100.0
crates/mcp-agent-mail-server/src/tui_screens/analytics.rs:926:        (snapshot.total_errors as f64 / snapshot.total_calls as f64) * 100.0
crates/mcp-agent-mail-server/src/tui_screens/analytics.rs:2000:                    (snapshot.total_errors as f64 / snapshot.total_calls as f64) * 100.0
crates/mcp-agent-mail-server/src/tui_screens/analytics.rs:2033:            (snapshot.total_errors as f64 / snapshot.total_calls as f64) * 100.0
crates/mcp-agent-mail-storage/tests/stress_pipeline.rs:3415:                errs as f64 / total_i as f64 * 100.0,
crates/mcp-agent-mail-storage/tests/stress_pipeline.rs:3416:                lat_sum as f64 / total_i as f64 / 1000.0,
crates/mcp-agent-mail-server/src/tui_screens/tool_metrics.rs:1017:            format!("{:.1}%", (total_errors as f64 / total_calls as f64) * 100.0)
crates/mcp-agent-mail-server/src/tui_screens/tool_metrics.rs:1046:        let trend_errors = if total_errors as f64 / (total_calls.max(1) as f64) > 0.05 {
crates/mcp-agent-mail-server/src/tui_screens/atc.rs:604:            st.total_micros as f64 / 1000.0,
```

## P33 local time / UTC drift candidates

```
crates/mcp-agent-mail-server/templates/mail_unified_inbox.html:745:      const diffSeconds = Math.floor((Date.now() - this.lastRefreshTime.getTime()) / 1000);
crates/mcp-agent-mail-server/templates/mail_unified_inbox.html:768:        this.lastRefreshTime = new Date();
crates/mcp-agent-mail-server/templates/mail_unified_inbox.html:1111:        this.lastRefreshTime = new Date();
crates/mcp-agent-mail-server/templates/mail_search.html:522:      const seconds = Math.max(0, Math.floor((Date.now() - ts) / 1000));
crates/mcp-agent-mail-guard/src/lib.rs:760:    now = datetime.datetime.now(datetime.timezone.utc)
crates/mcp-agent-mail-server/templates/base.html:2768:              const startTime = Date.now();
crates/mcp-agent-mail-server/templates/base.html:2775:                const elapsed = Date.now() - startTime;
crates/mcp-agent-mail-server/templates/base.html:2820:              toast.pausedAt = Date.now();
crates/mcp-agent-mail-storage/tests/fixtures/notifications/README.md:8:- timestamps are placeholders; tests should accept any valid ISO-8601 UTC string from datetime.now(timezone.utc).isoformat().
crates/mcp-agent-mail-server/src/tui_web_dashboard.rs:1143:  return `dash${Math.random().toString(16).slice(2, 10)}${Date.now().toString(16).slice(-8)}`;
crates/mcp-agent-mail-server/src/tui_web_dashboard.rs:1557:  lastFrameAgeMs = Math.max(0, Math.round((Date.now() * 1000 - lastTimestampUs) / 1000));
crates/mcp-agent-mail-server/src/tui_web_dashboard.rs:2014:    lastInputFlushAt = Date.now();
```

## P34 detailed internal errors exposed

```
crates/mcp-agent-mail/benches/benchmarks.rs:2220:        database_url: format!("sqlite+aiosqlite:///{}", db_path.display()),
crates/mcp-agent-mail-tools/tests/tool_input_proptest.rs:268:    let db_path = format!("/tmp/tools-input-proptest-{suffix}.sqlite3");
crates/mcp-agent-mail-tools/tests/tool_input_proptest.rs:269:    let database_url = format!("sqlite://{db_path}");
crates/mcp-agent-mail-tools/tests/validation_error_parity.rs:40:    let db_path = format!("/tmp/validation-error-parity-{env_suffix}.sqlite3");
crates/mcp-agent-mail-tools/tests/validation_error_parity.rs:41:    let database_url = format!("sqlite://{db_path}");
crates/mcp-agent-mail-tools/tests/system_error_parity.rs:41:    let db_path = format!("/tmp/system-error-parity-{env_suffix}.sqlite3");
crates/mcp-agent-mail-tools/tests/system_error_parity.rs:42:    let database_url = format!("sqlite://{db_path}");
crates/mcp-agent-mail-search-core/tests/parser_filter_fusion_rerank.rs:50:        let input = format!("test{s}query");
crates/mcp-agent-mail-tools/tests/messaging_error_parity.rs:37:    let db_path = format!("/tmp/messaging-error-parity-{env_suffix}.sqlite3");
crates/mcp-agent-mail-tools/tests/messaging_error_parity.rs:38:    let database_url = format!("sqlite://{db_path}");
crates/mcp-agent-mail-tools/tests/contact_policy_parity.rs:35:    let db_path = format!("/tmp/contact-policy-parity-{env_suffix}.sqlite3");
crates/mcp-agent-mail-tools/tests/contact_policy_parity.rs:36:    let database_url = format!("sqlite://{db_path}");
crates/mcp-agent-mail-db/src/pool.rs:2465:            .map_err(|e| DbError::Sqlite(format!("consistency probe query: {e}")))?;
crates/mcp-agent-mail-db/src/pool.rs:4790:    PathBuf::from(format!("{}.activity.lock", sqlite_path.display()))
crates/mcp-agent-mail-db/src/pool.rs:5208:        &format!("storage.sqlite3{suffix}.{label}-{timestamp}"),
crates/mcp-agent-mail-db/src/pool.rs:5219:        &format!("storage.sqlite3.reconstructing-{timestamp}"),
crates/mcp-agent-mail-db/src/pool.rs:5226:            &format!("storage.sqlite3.reconstructing-{timestamp}-{suffix:02}"),
crates/mcp-agent-mail-db/src/pool.rs:5246:        &format!("storage.sqlite3.restoring-{timestamp}"),
crates/mcp-agent-mail-db/src/pool.rs:5253:            &format!("storage.sqlite3.restoring-{timestamp}-{suffix:02}"),
crates/mcp-agent-mail-db/src/pool.rs:5282:        &format!("storage.sqlite3.{reason}-{timestamp}"),
crates/mcp-agent-mail-db/src/pool.rs:5510:        &format!("storage.sqlite3.{reason}-{timestamp}"),
crates/mcp-agent-mail-db/src/pool.rs:5641:        &format!("storage.sqlite3.archive-reconcile-{timestamp}"),
crates/mcp-agent-mail-db/src/pool.rs:5798:        &format!("storage.sqlite3.corrupt-{timestamp}"),
crates/mcp-agent-mail-db/src/pool.rs:5849:        &format!("storage.sqlite3.corrupt-{timestamp}"),
crates/mcp-agent-mail-db/src/pool.rs:7027:            database_url: format!("sqlite:///{}", db_path.display()),
crates/mcp-agent-mail-db/src/pool.rs:7059:            database_url: format!("sqlite:///{}", db_path.display()),
crates/mcp-agent-mail-db/src/pool.rs:7161:            database_url: format!("sqlite:///{}", db_path.display()),
crates/mcp-agent-mail-db/src/pool.rs:7199:                        .map_err(|e| format!("query via second pooled connection failed: {e}")),
crates/mcp-agent-mail-db/src/pool.rs:7240:            database_url: format!("sqlite:///{}", db_path.display()),
crates/mcp-agent-mail-db/src/pool.rs:7653:            database_url: format!("sqlite:///{}", primary.display()),
crates/mcp-agent-mail-db/src/pool.rs:7701:            database_url: format!("sqlite:///{}", db_path.display()),
crates/mcp-agent-mail-db/src/pool.rs:7762:        let db_url = format!("sqlite:///{}", db_path.display());
crates/mcp-agent-mail-db/src/pool.rs:7810:            database_url: format!("sqlite:///{}", db_path.display()),
crates/mcp-agent-mail-db/src/pool.rs:7856:            database_url: format!("sqlite:///{}", db_path.display()),
crates/mcp-agent-mail-db/src/pool.rs:7875:            database_url: format!("sqlite:///{}", db_path.display()),
crates/mcp-agent-mail-db/src/pool.rs:7934:            database_url: format!("sqlite:///{}", db_path.display()),
crates/mcp-agent-mail-db/src/pool.rs:7952:            database_url: format!("sqlite:///{}", db_path.display()),
crates/mcp-agent-mail-db/src/pool.rs:8009:            database_url: format!("sqlite:///{}", db_path.display()),
crates/mcp-agent-mail-db/src/pool.rs:8056:            database_url: format!("sqlite:///{}", db_path.display()),
crates/mcp-agent-mail-db/src/pool.rs:8130:            database_url: format!("sqlite:///{}", db_path.display()),
crates/mcp-agent-mail-db/src/pool.rs:8149:        let database_url = format!("sqlite:///{}", db_path.display());
crates/mcp-agent-mail-db/src/pool.rs:8183:            database_url: format!("sqlite:///{}", db_path.display()),
crates/mcp-agent-mail-db/src/pool.rs:8188:            database_url: format!("sqlite:///{}", db_path.display()),
crates/mcp-agent-mail-db/src/pool.rs:8240:            database_url: format!("sqlite:///{}", db_path.display()),
crates/mcp-agent-mail-db/src/pool.rs:8265:            database_url: format!("sqlite:///{}", db_path.display()),
crates/mcp-agent-mail-db/src/pool.rs:8374:            database_url: format!("sqlite:///{}", primary.display()),
crates/mcp-agent-mail-db/src/pool.rs:8417:            database_url: format!("sqlite:///{}", db_path.display()),
crates/mcp-agent-mail-db/src/pool.rs:8432:            database_url: format!("sqlite:///{}", db_path.display()),
crates/mcp-agent-mail-db/src/pool.rs:8465:        let db_url = format!("sqlite:///{}", db_path.display());
crates/mcp-agent-mail-db/src/pool.rs:8518:            database_url: format!("sqlite:///{}", db_path.display()),
crates/mcp-agent-mail-db/src/pool.rs:8568:                    database_url: format!("sqlite:///{}", db_path.display()),
crates/mcp-agent-mail-db/src/pool.rs:10461:            database_url: format!("sqlite:///{}", db_path.display()),
crates/mcp-agent-mail-db/src/pool.rs:10495:            database_url: format!("sqlite:///{}", db_path.display()),
crates/mcp-agent-mail-db/src/pool.rs:11198:        let db_url = format!("sqlite://{}", db_path.display());
crates/mcp-agent-mail-db/src/reconstruct.rs:2900:                    DbError::Sqlite(format!("reconstruct salvage: query projects: {e}"))
crates/mcp-agent-mail-db/src/reconstruct.rs:3019:                .map_err(|e| DbError::Sqlite(format!("reconstruct salvage: query agents: {e}")))?;
crates/mcp-agent-mail-db/src/reconstruct.rs:3174:                            DbError::Sqlite(format!("reconstruct salvage: query agent_links: {e}"))
crates/mcp-agent-mail-db/src/reconstruct.rs:3276:                        DbError::Sqlite(format!("reconstruct salvage: query products: {e}"))
crates/mcp-agent-mail-db/src/reconstruct.rs:3448:                        DbError::Sqlite(format!("reconstruct salvage: query messages: {e}"))
crates/mcp-agent-mail-db/src/reconstruct.rs:3594:                .map_err(|e| DbError::Sqlite(format!("reconstruct salvage: query recipients: {e}")))?;
```

## P35 suspicious ambiguous imports

```
crates/mcp-agent-mail-test-helpers/src/parity.rs:46:use std::path::Path;
crates/mcp-agent-mail-test-helpers/src/repo.rs:12:use std::path::{Path, PathBuf};
crates/mcp-agent-mail-test-helpers/src/shim_git.rs:23:use std::path::{Path, PathBuf};
crates/mcp-agent-mail/tests/archive_perf_reporting.rs:8:use std::path::Path;
crates/mcp-agent-mail-conformance/tests/conformance_debug.rs:5:use std::path::{Path, PathBuf};
crates/mcp-agent-mail/src/main.rs:10:use std::path::Path;
crates/mcp-agent-mail-conformance/tests/resource_coverage_guard.rs:4:use std::path::{Path, PathBuf};
crates/mcp-agent-mail-conformance/tests/doc_consistency.rs:4:use std::path::{Path, PathBuf};
crates/mcp-agent-mail-conformance/tests/conformance.rs:15:use std::path::{Path, PathBuf};
crates/mcp-agent-mail/benches/benchmarks.rs:21:use std::path::{Path, PathBuf};
crates/mcp-agent-mail-cli/tests/flake_triage_integration.rs:7:use std::path::PathBuf;
crates/mcp-agent-mail-cli/src/context.rs:16:use std::path::Path;
crates/mcp-agent-mail-cli/tests/robot_golden_snapshots.rs:3:use std::path::{Path, PathBuf};
crates/mcp-agent-mail-cli/tests/cli_json_snapshots.rs:4:use std::path::{Path, PathBuf};
crates/mcp-agent-mail-cli/tests/perf_security_regressions.rs:10:use std::path::{Path, PathBuf};
crates/mcp-agent-mail-cli/tests/integration_runs.rs:3:use std::path::{Path, PathBuf};
crates/mcp-agent-mail-cli/tests/perf_guardrails.rs:12:use std::path::{Path, PathBuf};
crates/mcp-agent-mail-cli/tests/tui_transport_harness.rs:13:use std::path::{Path, PathBuf};
crates/mcp-agent-mail-cli/tests/ci_integration.rs:8:use std::path::PathBuf;
crates/mcp-agent-mail-cli/tests/share_archive_harness.rs:12:use std::path::{Path, PathBuf};
crates/mcp-agent-mail-cli/tests/tui_accessibility_harness.rs:11:use std::path::{Path, PathBuf};
crates/mcp-agent-mail-cli/src/bin/test_db.rs:2:use std::fmt::Write as _;
crates/mcp-agent-mail-cli/src/bin/test_db.rs:4:use std::path::{Path, PathBuf};
crates/mcp-agent-mail-cli/src/bin/test_db.rs:408:    use std::path::PathBuf;
crates/mcp-agent-mail-cli/tests/mode_matrix_harness.rs:8:use std::path::{Path, PathBuf};
crates/mcp-agent-mail-cli/tests/share_verify_decrypt.rs:3:use std::path::{Path, PathBuf};
crates/mcp-agent-mail-cli/tests/semantic_conformance.rs:12:use std::path::{Path, PathBuf};
crates/mcp-agent-mail-cli/tests/http_transport_harness.rs:12:use std::path::{Path, PathBuf};
crates/mcp-agent-mail-cli/tests/help_snapshots.rs:3:use std::path::{Path, PathBuf};
crates/mcp-agent-mail-cli/tests/security_privacy_harness.rs:10:use std::path::{Path, PathBuf};
crates/mcp-agent-mail-cli/tests/golden_integration.rs:3:use std::path::PathBuf;
crates/mcp-agent-mail-conformance/src/main.rs:4:use std::path::{Path, PathBuf};
crates/mcp-agent-mail-conformance/src/lib.rs:9:use std::path::Path;
crates/mcp-agent-mail-agent-detect/src/lib.rs:10:use std::path::PathBuf;
crates/mcp-agent-mail-cli/src/e2e_runner.rs:34:use std::path::{Path, PathBuf};
crates/mcp-agent-mail-cli/src/e2e_runner.rs:1971:    use std::path::Path;
crates/mcp-agent-mail-cli/src/bench.rs:10:use std::path::Path;
crates/mcp-agent-mail-server/tests/truthfulness_integration.rs:20:use std::path::PathBuf;
crates/mcp-agent-mail-server/tests/e2e_atc_learning_loop.rs:3:use std::fmt::Write as _;
crates/mcp-agent-mail-server/tests/e2e_atc_learning_loop.rs:6:use std::path::Path;
crates/mcp-agent-mail-cli/src/golden.rs:12:use std::path::Path;
crates/mcp-agent-mail-cli/src/golden.rs:343:    use std::fmt::Write;
crates/mcp-agent-mail-server/tests/tui_soak_replay.rs:26:use std::path::{Path, PathBuf};
crates/mcp-agent-mail-server/tests/tui_perf_baselines.rs:21:use std::path::{Path, PathBuf};
crates/mcp-agent-mail-server/tests/pty_e2e_search.rs:29:use std::path::{Path, PathBuf};
crates/mcp-agent-mail-server/tests/web_ui_parity_contract_guard.rs:5:use std::path::PathBuf;
crates/mcp-agent-mail-server/tests/fixture_matrix.rs:31:use std::path::{Path, PathBuf};
crates/mcp-agent-mail-db/benches/search_v3_bench.rs:39:use std::path::{Path, PathBuf};
crates/mcp-agent-mail-server/benches/frame_bench.rs:13:use std::path::Path;
crates/mcp-agent-mail-cli/src/robot.rs:20:use std::path::{Path, PathBuf};
crates/mcp-agent-mail-cli/src/robot.rs:9047:    use std::path::{Path, PathBuf};
crates/mcp-agent-mail-search-core/src/index_layout.rs:11:use std::path::{Path, PathBuf};
crates/mcp-agent-mail-cli/src/legacy.rs:24:use std::path::{Component, Path, PathBuf};
crates/mcp-agent-mail-cli/src/e2e_artifacts.rs:34:use std::path::{Path, PathBuf};
crates/mcp-agent-mail-cli/src/doctor_orphan_refs.rs:29:use std::path::{Path, PathBuf};
crates/mcp-agent-mail-cli/src/lib.rs:30:use std::path::{Path, PathBuf};
crates/mcp-agent-mail-cli/src/lib.rs:9075:        use std::fmt::Write as _;
crates/mcp-agent-mail-cli/src/lib.rs:11732:        use std::path::Component;
crates/mcp-agent-mail-cli/src/lib.rs:46146:        use std::path::Component;
crates/mcp-agent-mail-search-core/src/model2vec.rs:12:use std::path::{Path, PathBuf};
crates/mcp-agent-mail-tools/src/identity.rs:17:use std::path::{Path, PathBuf};
crates/mcp-agent-mail-tools/src/identity.rs:1859:    use std::path::PathBuf;
crates/mcp-agent-mail-server/src/tui_preset.rs:16:use std::path::{Component, Path, PathBuf};
crates/mcp-agent-mail-server/src/integrity_guard.rs:18:use std::path::{Path, PathBuf};
crates/mcp-agent-mail-core/tests/toon_integration.rs:10:use std::path::PathBuf;
crates/mcp-agent-mail-core/tests/agent_detect_integration.rs:21:    use std::path::PathBuf;
crates/mcp-agent-mail-server/src/tui_persist.rs:13:use std::path::{Component, Path, PathBuf};
crates/mcp-agent-mail-guard/tests/guard_env_tests.rs:8:use std::path::Path;
crates/mcp-agent-mail-server/src/tui_macro.rs:14:use std::path::{Component, Path, PathBuf};
crates/mcp-agent-mail-tools/src/resources.rs:19:use std::path::{Path, PathBuf};
crates/mcp-agent-mail-tools/src/resources.rs:4747:    use std::path::PathBuf;
crates/mcp-agent-mail-guard/src/lib.rs:4:use std::path::{Path, PathBuf};
crates/mcp-agent-mail-tools/src/reservations.rs:20:use std::fmt::Write;
crates/mcp-agent-mail-tools/src/reservations.rs:21:use std::path::PathBuf;
crates/mcp-agent-mail-tools/src/reservations.rs:1622:    use std::path::PathBuf;
crates/mcp-agent-mail-core/src/toon.rs:9:use std::path::Path;
crates/mcp-agent-mail-tools/src/lib.rs:62:    use std::path::{Path, PathBuf};
crates/mcp-agent-mail-server/src/console.rs:1736:        use std::fmt::Write as _;
crates/mcp-agent-mail-core/src/setup.rs:11:use std::path::{Path, PathBuf};
crates/mcp-agent-mail-core/src/setup.rs:297:        use std::fmt::Write;
```

## P36 infra/config surfaces that should not ride with refactor commits

```
./.github/workflows/publish.yml
./.github/workflows/search-v3-weekly.yml
./.github/workflows/notify-acfs.yml
./.github/workflows/ci.yml
./.github/workflows/supply-chain-audit.yml
./.github/workflows/conformance-fixture-regen.yml
./.github/workflows/archive-perf-gate.yml
./.github/workflows/archive-fsync-matrix.yml
./.github/workflows/atc-perf-gate.yml
./.github/workflows/dist.yml
./crates/mcp-agent-mail-agent-detect/Cargo.toml
./crates/mcp-agent-mail-cli/Cargo.toml
./crates/mcp-agent-mail-conformance/Cargo.toml
./crates/mcp-agent-mail-core/Cargo.toml
./crates/mcp-agent-mail-db/Cargo.toml
./crates/mcp-agent-mail-guard/Cargo.toml
./crates/mcp-agent-mail-search-core/Cargo.toml
./crates/mcp-agent-mail-server/Cargo.toml
./crates/mcp-agent-mail-share/Cargo.toml
./crates/mcp-agent-mail-storage/Cargo.toml
./crates/mcp-agent-mail-tools/Cargo.toml
./crates/mcp-agent-mail/Cargo.toml
./crates/mcp-agent-mail-test-helpers/Cargo.toml
./vendor/uring-fs/Cargo.toml
./experimental/mcp-agent-mail-wasm/Cargo.toml
./Cargo.toml
./Cargo.lock
```

## P37 unpinned dependency snippets

_none found_

## P38 wildcard/glob imports

```
crates/mcp-agent-mail/benches/benchmarks.rs:30:use tracing_subscriber::prelude::*;
crates/mcp-agent-mail-tools/tests/tool_input_proptest.rs:6:use proptest::prelude::*;
crates/mcp-agent-mail-tools/src/identity.rs:12:use fastmcp::prelude::*;
crates/mcp-agent-mail-tools/src/resources.rs:13:use fastmcp::prelude::*;
crates/mcp-agent-mail-conformance/tests/conformance.rs:7:use proptest::prelude::*;
crates/mcp-agent-mail-tools/src/reservations.rs:12:use fastmcp::prelude::*;
crates/mcp-agent-mail-tools/src/contacts.rs:9:use fastmcp::prelude::*;
crates/mcp-agent-mail-tools/src/macros.rs:20:use fastmcp::prelude::*;
crates/mcp-agent-mail-tools/src/products.rs:10:use fastmcp::prelude::*;
crates/mcp-agent-mail-db/src/queries.rs:27:use sqlmodel::prelude::*;
crates/mcp-agent-mail-tools/src/build_slots.rs:9:use fastmcp::prelude::*;
crates/mcp-agent-mail-tools/src/messaging.rs:12:use fastmcp::prelude::*;
crates/mcp-agent-mail-tools/src/search.rs:8:use fastmcp::prelude::*;
crates/mcp-agent-mail-core/src/proptest_generators.rs:7:use proptest::prelude::*;
crates/mcp-agent-mail-server/src/lib.rs:138:use fastmcp::prelude::*;
```

## P39 async functions returning Promise (audit for real await)

_none found_

## P40 await/then in nearby non-async contexts (manual audit)

_none found_

---

## Next steps

1. Review each section; confirm which hits are real vs. false positives.
2. File beads for accepted patterns (one per pathology class).
3. Proceed to `./scripts/dup_scan.sh` for structural duplication.
4. Score candidates via `./scripts/score_candidates.py`.
5. For each accepted candidate: fill isomorphism card, edit, verify, ledger.

Full P1-P40 pathology catalog: `references/VIBE-CODED-PATHOLOGIES.md`.
Attack order (cheap wins first): the "AI-slop refactor playbook" in that file.
