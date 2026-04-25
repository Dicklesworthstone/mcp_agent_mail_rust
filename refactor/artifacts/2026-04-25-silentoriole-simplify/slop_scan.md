# AI slop scan — 2026-04-25-silentoriole-simplify

Generated 2026-04-25T01:09:09Z
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
crates/mcp-agent-mail-core/src/toon.rs:664:pub enum EncoderError {
crates/mcp-agent-mail-core/src/setup.rs:19:pub enum SetupError {
crates/mcp-agent-mail-tools/src/llm.rs:224:pub enum LlmError {
crates/mcp-agent-mail-cli/src/golden.rs:97:pub enum GoldenCaptureError {
crates/mcp-agent-mail-cli/src/golden.rs:106:pub enum GoldenChecksumError {
crates/mcp-agent-mail-core/src/git_binary.rs:40:pub enum GitBinaryError {
crates/mcp-agent-mail-storage/src/lib.rs:44:pub enum StorageError {
crates/mcp-agent-mail-cli/src/bench.rs:134:pub enum BenchValidationError {
crates/mcp-agent-mail-cli/src/bench.rs:190:pub enum BenchTimingError {
crates/mcp-agent-mail-cli/src/bench.rs:374:pub enum BenchSeedError {
crates/mcp-agent-mail-cli/src/bench.rs:1089:pub enum BenchBaselineError {
crates/mcp-agent-mail-cli/src/ci.rs:1391:pub enum GateRunnerError {
crates/mcp-agent-mail-core/src/experience.rs:815:pub enum FeatureSchemaMigrationError {
crates/mcp-agent-mail-core/src/flags.rs:124:pub enum FlagRegistryError {
crates/mcp-agent-mail-agent-detect/src/lib.rs:56:pub enum AgentDetectError {
crates/mcp-agent-mail-conformance/src/lib.rs:183:pub enum FixtureLoadError {
crates/mcp-agent-mail-core/src/mcp_config.rs:141:pub enum McpConfigUpdateError {
crates/mcp-agent-mail-share/src/probe.rs:26:pub enum ProbeError {
crates/mcp-agent-mail-core/src/agent_detect.rs:61:pub enum AgentDetectError {
crates/mcp-agent-mail-guard/src/lib.rs:8:pub enum GuardError {
crates/mcp-agent-mail-server/src/tui_preset.rs:696:pub enum PresetError {
crates/mcp-agent-mail-share/src/wizard.rs:307:pub enum WizardErrorCode {
crates/mcp-agent-mail-server/src/tui_macro.rs:249:pub enum MacroError {
crates/mcp-agent-mail-share/src/lib.rs:245:pub enum ShareError {
crates/mcp-agent-mail-cli/src/lib.rs:49:pub enum CliError {
crates/mcp-agent-mail-db/src/search_error.rs:10:pub enum SearchError {
crates/mcp-agent-mail-db/src/error.rs:7:pub enum DbError {
crates/mcp-agent-mail-search-core/src/error.rs:10:pub enum SearchError {
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
crates/mcp-agent-mail-cli/src/ci.rs:786:            if statuses.iter().any(|status| status == "fail") {
crates/mcp-agent-mail-cli/src/ci.rs:788:            } else if statuses.iter().all(|status| status == "pass") {
crates/mcp-agent-mail-cli/src/ci.rs:790:            } else if statuses.iter().all(|status| status == "skip") {
crates/mcp-agent-mail-cli/src/ci.rs:792:            } else if statuses.iter().any(|status| status == "missing") {
crates/mcp-agent-mail-cli/src/robot.rs:4386:                unhealthy: status == "fail",
crates/mcp-agent-mail-cli/src/robot.rs:4387:                degraded: status == "warn",
crates/mcp-agent-mail-cli/tests/tui_accessibility_harness.rs:376:            adapter_status == "pass",
crates/mcp-agent-mail-cli/src/lib.rs:8213:        let present = rows.iter().filter(|row| row.status == "present").count();
crates/mcp-agent-mail-cli/src/lib.rs:8214:        let missing = rows.iter().filter(|row| row.status == "missing").count();
crates/mcp-agent-mail-cli/src/lib.rs:8215:        let stale = rows.iter().filter(|row| row.status == "stale").count();
crates/mcp-agent-mail-cli/src/lib.rs:16240:        } else if status == "fail" {
crates/mcp-agent-mail-cli/src/lib.rs:16242:        } else if status == "warn" {
crates/mcp-agent-mail-cli/src/lib.rs:16359:    let primary_fail_count = if primary_status == "fail" { 1 } else { 0 };
crates/mcp-agent-mail-cli/src/lib.rs:16360:    let primary_warn_count = if primary_status == "warn" { 1 } else { 0 };
crates/mcp-agent-mail-cli/src/lib.rs:16455:        let icon = if overall_status == "ok" { "OK" } else { "WARN" };
crates/mcp-agent-mail-cli/src/lib.rs:16456:        let detail = if overall_status == "ok" {
crates/mcp-agent-mail-cli/src/lib.rs:18135:                "status": if source == "env" { "fail" } else { "warn" },
crates/mcp-agent-mail-cli/src/lib.rs:19659:    let fail_count = checks.iter().filter(|c| c["status"] == "fail").count();
crates/mcp-agent-mail-cli/src/lib.rs:19660:    let warn_count = checks.iter().filter(|c| c["status"] == "warn").count();
crates/mcp-agent-mail-cli/src/lib.rs:20799:                            if post.http_check.status == "ok" && post.rpc_check.status == "ok" =>
crates/mcp-agent-mail-cli/src/lib.rs:20820:                    ) && post.http_check.status == "ok"
crates/mcp-agent-mail-cli/src/lib.rs:20821:                        && post.rpc_check.status == "ok")
crates/mcp-agent-mail-tools/src/identity.rs:809:    let pool = if semantic_readiness.status == "fail" {
crates/mcp-agent-mail-tools/src/identity.rs:850:        status: if semantic_readiness.status == "fail" {
crates/mcp-agent-mail-conformance/tests/resource_coverage_guard.rs:204:        .filter(|row| row.fixture_status == "covered")
crates/mcp-agent-mail-conformance/tests/resource_coverage_guard.rs:214:        .filter(|row| row.fixture_status == "gap")
crates/mcp-agent-mail-server/tests/web_ui_parity_contract_guard.rs:139:        if row.status == "waived" {
crates/mcp-agent-mail-core/src/diagnostics.rs:711:            next_action: if status == "ok" {
crates/mcp-agent-mail-server/src/lib.rs:4549:    if execution.status == "executed" {
crates/mcp-agent-mail-server/src/lib.rs:5028:    if raw_status == "executed" {
crates/mcp-agent-mail-server/src/lib.rs:5097:    if raw_status == "dry_run" || raw_status == "shadowed" || raw_status.starts_with("shadowed_") {
crates/mcp-agent-mail-server/src/lib.rs:5117:    if raw_status == "executor_unavailable" || raw_status.starts_with("missing_") {
crates/mcp-agent-mail-server/src/tui_web_dashboard.rs:1963:    if (!resp.ok || payload.status === "inactive") {
crates/mcp-agent-mail-db/src/atc_queries.rs:1764:                        let resolved = state == "resolved";
crates/mcp-agent-mail-db/src/schema.rs:2627:            let statement_result = if migration.id == "v15_add_recipients_json_to_messages" {
crates/mcp-agent-mail-server/src/tui_action_menu.rs:892:    if status == "pending" {
crates/mcp-agent-mail-server/src/tui_screens/contacts.rs:131:            Self::Pending => status == "pending",
crates/mcp-agent-mail-server/src/tui_screens/contacts.rs:132:            Self::Approved => status == "approved",
crates/mcp-agent-mail-server/src/tui_screens/contacts.rs:133:            Self::Blocked => status == "blocked",
crates/mcp-agent-mail-server/src/tui_screens/contacts.rs:1008:            .filter(|c| c.status == "approved")
```

## P23 reflex trim/lower/upper normalization

```
crates/mcp-agent-mail-test-helpers/src/parity.rs:59:    String::from_utf8_lossy(&out.stdout).trim().to_string()
crates/mcp-agent-mail-test-helpers/src/parity.rs:79:    let version = stdout.trim();
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
crates/mcp-agent-mail-conformance/tests/conformance_debug.rs:99:        if line.trim() == heading_line {
crates/mcp-agent-mail-conformance/tests/conformance_debug.rs:124:                .trim()
crates/mcp-agent-mail-conformance/tests/conformance_debug.rs:127:                .map(|cell| cell.trim().to_string())
crates/mcp-agent-mail-search-core/tests/query_assistance_explain.rs:239:        assert!(qa.query_text.is_empty() || qa.query_text.trim().is_empty());
crates/mcp-agent-mail-conformance/tests/protocol_compliance.rs:169:        .filter(|line| !line.trim().is_empty())
crates/mcp-agent-mail-conformance/tests/protocol_compliance.rs:197:                headers.insert(name.trim().to_ascii_lowercase(), value.trim().to_string());
crates/mcp-agent-mail-conformance/tests/resource_coverage_guard.rs:42:        if line.trim() == heading_line {
crates/mcp-agent-mail-conformance/tests/resource_coverage_guard.rs:67:                .trim()
crates/mcp-agent-mail-conformance/tests/resource_coverage_guard.rs:70:                .map(|cell| cell.trim().to_string())
crates/mcp-agent-mail/benches/benchmarks.rs:1186:    let trimmed = text.trim();
crates/mcp-agent-mail-conformance/tests/doc_consistency.rs:108:            line_text: line.trim().to_string(),
crates/mcp-agent-mail-conformance/tests/doc_consistency.rs:160:                        line.trim()
crates/mcp-agent-mail-conformance/tests/conformance.rs:278:            !self.version.trim().is_empty(),
crates/mcp-agent-mail-conformance/tests/conformance.rs:283:            !self.generated_at.trim().is_empty(),
crates/mcp-agent-mail-conformance/tests/conformance.rs:318:            !self.name.trim().is_empty(),
crates/mcp-agent-mail-conformance/tests/conformance.rs:361:                !path.trim().is_empty(),
crates/mcp-agent-mail-conformance/tests/conformance.rs:517:        !fixtures.version.trim().is_empty(),
crates/mcp-agent-mail-conformance/tests/conformance.rs:521:        !fixtures.generated_at.trim().is_empty(),
crates/mcp-agent-mail-conformance/tests/conformance.rs:1266:    let content = content.trim();
crates/mcp-agent-mail-conformance/tests/conformance.rs:1273:    serde_json::from_str(json_str.trim()).ok()
crates/mcp-agent-mail-conformance/tests/conformance.rs:1704:            !after_close.trim().is_empty(),
crates/mcp-agent-mail-conformance/tests/conformance.rs:1754:                        canonical.trim(),
crates/mcp-agent-mail-conformance/tests/conformance.rs:1755:                        inbox_content.trim(),
crates/mcp-agent-mail-conformance/tests/conformance.rs:1770:                        canonical.trim(),
crates/mcp-agent-mail-conformance/tests/conformance.rs:1771:                        outbox_content.trim(),
crates/mcp-agent-mail-share/viewer_assets/index.html:1004:                    <span x-text="(msg.sender || '?').substring(0, 1).toUpperCase()"></span>
crates/mcp-agent-mail-conformance/tests/conformance/python_reference/generate_fixtures.py:126:    scratch_root_env = os.environ.get("MCP_AGENT_MAIL_CONFORMANCE_SCRATCH_ROOT", "").strip()
crates/mcp-agent-mail-conformance/tests/conformance/python_reference/generate_fixtures.py:127:    allow_repo_scratch = os.environ.get("MCP_AGENT_MAIL_CONFORMANCE_ALLOW_REPO_SCRATCH", "").strip().lower() in {
crates/mcp-agent-mail-conformance/tests/conformance/python_reference/generate_fixtures.py:212:            enabled = os.environ.get("MCP_AGENT_MAIL_LLM_STUB", "").strip().lower() in {
crates/mcp-agent-mail-conformance/tests/conformance/python_reference/generate_fixtures.py:227:            sys_lower = (system or "").lower()
crates/mcp-agent-mail-tools/src/identity.rs:1104:    let program = program.trim().to_string();
crates/mcp-agent-mail-tools/src/identity.rs:1115:    let model = model.trim().to_string();
crates/mcp-agent-mail-tools/src/identity.rs:1134:            let n = n.trim();
crates/mcp-agent-mail-tools/src/identity.rs:1161:    let policy = raw_policy.trim().to_ascii_lowercase();
crates/mcp-agent-mail-tools/src/identity.rs:1358:    let program = program.trim().to_string();
crates/mcp-agent-mail-tools/src/identity.rs:1369:    let model = model.trim().to_string();
crates/mcp-agent-mail-tools/src/identity.rs:1388:            let hint = hint.trim();
crates/mcp-agent-mail-tools/src/identity.rs:1416:    let policy = raw_policy.trim().to_ascii_lowercase();
crates/mcp-agent-mail-tools/src/identity.rs:1719:        Some(p) if !p.trim().is_empty() => p.trim().to_string(),
crates/mcp-agent-mail-tools/src/identity.rs:2174:        assert!("".trim().is_empty());
crates/mcp-agent-mail-tools/src/identity.rs:2175:        assert!("  ".trim().is_empty());
crates/mcp-agent-mail-tools/src/identity.rs:2176:        assert!("\t".trim().is_empty());
crates/mcp-agent-mail-tools/src/identity.rs:2177:        assert!(!"claude-code".trim().is_empty());
crates/mcp-agent-mail-tools/src/identity.rs:2182:        assert!("".trim().is_empty());
crates/mcp-agent-mail-tools/src/identity.rs:2183:        assert!("  ".trim().is_empty());
crates/mcp-agent-mail-tools/src/identity.rs:2184:        assert!(!"opus-4.5".trim().is_empty());
crates/mcp-agent-mail-guard/src/lib.rs:38:            .trim()
crates/mcp-agent-mail-guard/src/lib.rs:129:        let rel = rel.trim();
crates/mcp-agent-mail-guard/src/lib.rs:163:        let raw = raw.trim();
crates/mcp-agent-mail-guard/src/lib.rs:241:        "    if os.name != 'posix' and path.suffix.lower() == '.py':".to_string(),
crates/mcp-agent-mail-guard/src/lib.rs:302:AGENT_NAME = os.environ.get("AGENT_NAME", "").strip()
crates/mcp-agent-mail-guard/src/lib.rs:470:                detail = (res.stderr or "").strip()
crates/mcp-agent-mail-guard/src/lib.rs:479:            commits = [c.strip() for c in res.stdout.splitlines() if c.strip()]
crates/mcp-agent-mail-guard/src/lib.rs:488:                    detail = diff_res.stderr.decode("utf-8", "ignore").strip()
crates/mcp-agent-mail-guard/src/lib.rs:500:                    status = parts[i].decode('utf-8', 'ignore').strip()
crates/mcp-agent-mail-guard/src/lib.rs:540:    for ch in value.strip().lower():
crates/mcp-agent-mail-guard/src/lib.rs:553:        value = os.environ.get(key, "").strip()
crates/mcp-agent-mail-guard/src/lib.rs:583:        value = (result.stdout or "").strip()
crates/mcp-agent-mail-guard/src/lib.rs:597:    value = (value or "").strip()
crates/mcp-agent-mail-guard/src/lib.rs:612:    slug = str(metadata.get("slug", "")).strip()
crates/mcp-agent-mail-guard/src/lib.rs:621:    human_key = str(metadata.get("human_key", "")).strip()
crates/mcp-agent-mail-guard/src/lib.rs:638:    project_value = PROJECT.strip()
crates/mcp-agent-mail-guard/src/lib.rs:711:        trimmed = value.strip()
crates/mcp-agent-mail-guard/src/lib.rs:712:        lowered = trimmed.lower()
crates/mcp-agent-mail-guard/src/lib.rs:729:        trimmed = value.strip()
crates/mcp-agent-mail-guard/src/lib.rs:789:        pattern = str(record.get("path_pattern") or record.get("path") or "").strip()
crates/mcp-agent-mail-guard/src/lib.rs:790:        holder = str(record.get("agent_name") or record.get("agent") or "").strip()
crates/mcp-agent-mail-guard/src/lib.rs:813:            value = (res.stdout or "").strip().lower()
```

## P24 testability wrappers / mutable deps seams

_none found_

## P25 docstrings/comments that may contradict implementation

```
crates/mcp-agent-mail-guard/src/lib.rs:290:# Auto-generated by mcp-agent-mail install_guard
crates/mcp-agent-mail-conformance/tests/conformance/python_reference/generate_fixtures.py:650:                "body_md": "- ACK: will deploy\n- [ ] TODO: update docs\n`api/v2/users`\n@Carol\n",
crates/mcp-agent-mail-tools/src/llm.rs:1364:            action_items: vec!["- [ ] TODO: update docs".into()],
crates/mcp-agent-mail-tools/src/llm.rs:1384:        assert!(merged.key_points.contains(&"TODO: update docs".to_string()));
crates/mcp-agent-mail-core/src/evidence_ledger.rs:123:/// Returns:
crates/mcp-agent-mail-conformance/tests/conformance/fixtures/python_reference.json:750:            "body_md": "- ACK: will deploy\n- [ ] TODO: update docs\n`api/v2/users`\n@Carol\n"
crates/mcp-agent-mail-conformance/tests/conformance/fixtures/python_reference.json:763:              "body_md": "- ACK: will deploy\n- [ ] TODO: update docs\n`api/v2/users`\n@Carol\n",
crates/mcp-agent-mail-conformance/tests/conformance/fixtures/python_reference.json:784:                    "body_md": "- ACK: will deploy\n- [ ] TODO: update docs\n`api/v2/users`\n@Carol\n",
crates/mcp-agent-mail-conformance/tests/conformance/fixtures/python_reference.json:1177:                  "TODO: update docs"
crates/mcp-agent-mail-conformance/tests/conformance/fixtures/python_reference.json:1486:                  "TODO: update docs"
crates/mcp-agent-mail-conformance/tests/conformance/fixtures/python_reference.json:2008:                    "TODO: update docs"
crates/mcp-agent-mail-share/src/static_render.rs:820:  <footer>Generated by MCP Agent Mail static export pipeline</footer>
crates/mcp-agent-mail-cli/src/lib.rs:1739:        /// Agent name (adjective+noun, e.g. "BlueLake"). Auto-generated if omitted.
crates/mcp-agent-mail-cli/src/lib.rs:1766:        /// Name hint (adjective+noun). Auto-generated if omitted.
crates/mcp-agent-mail-cli/src/lib.rs:1839:        /// Agent name (adjective+noun). Auto-generated if omitted.
crates/mcp-agent-mail-cli/src/lib.rs:1879:        /// Agent name. Auto-generated if omitted.
crates/mcp-agent-mail-cli/src/robot.rs:9673:                "# {}\n\n*Generated by {} at {}*",
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
crates/mcp-agent-mail-search-core/src/lexical_parser.rs:35:    LazyLock::new(|| Regex::new(r"[\[\]{}^~\\]").unwrap_or_else(|_| unreachable!()));
crates/mcp-agent-mail-search-core/src/lexical_parser.rs:39:    LazyLock::new(|| Regex::new(r"^[\*\.\?!()]+$").unwrap_or_else(|_| unreachable!()));
crates/mcp-agent-mail-search-core/src/lexical_parser.rs:45:    Regex::new(r"[a-zA-Z0-9]+(?:-[a-zA-Z0-9]+)+").unwrap_or_else(|_| unreachable!())
crates/mcp-agent-mail-search-core/src/lexical_parser.rs:50:    LazyLock::new(|| Regex::new(r" {2,}").unwrap_or_else(|_| unreachable!()));
crates/mcp-agent-mail-cli/src/golden.rs:142:                regex: Regex::new(pattern).unwrap_or_else(|e| {
crates/mcp-agent-mail-cli/src/e2e_runner.rs:1854:            std::sync::LazyLock::new(|| regex::Regex::new(r"\x1b\[[0-9;]*m").expect("valid regex"));
crates/mcp-agent-mail-cli/src/ci.rs:328:        LazyLock::new(|| regex::Regex::new(r"^error\[E\d+\]: (.+)$").expect("valid regex"));
crates/mcp-agent-mail-cli/src/ci.rs:330:        LazyLock::new(|| regex::Regex::new(r"thread '(.+)' panicked").expect("valid regex"));
crates/mcp-agent-mail-cli/src/ci.rs:332:        LazyLock::new(|| regex::Regex::new(r"^---- (.+) ----$").expect("valid regex"));
crates/mcp-agent-mail-cli/src/ci.rs:334:        LazyLock::new(|| regex::Regex::new(r"^warning: (.+)$").expect("valid regex"));
crates/mcp-agent-mail-cli/src/ci.rs:336:        LazyLock::new(|| regex::Regex::new(r"^Diff in (.+):$").expect("valid regex"));
crates/mcp-agent-mail-cli/src/ci.rs:456:        regex::Regex::new(
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
crates/mcp-agent-mail-db/src/query_assistance.rs:45:    Regex::new(r"[a-zA-Z0-9]+(?:-[a-zA-Z0-9]+)+").unwrap_or_else(|_| unreachable!())
crates/mcp-agent-mail-db/src/query_assistance.rs:50:    LazyLock::new(|| Regex::new(r" {2,}").unwrap_or_else(|_| unreachable!()));
crates/mcp-agent-mail-storage/src/lib.rs:4596:    RE.get_or_init(|| Regex::new(r"[^a-zA-Z0-9._-]+").unwrap_or_else(|_| unreachable!()))
crates/mcp-agent-mail-storage/src/lib.rs:5740:        Regex::new(r"!\[(?P<alt>[^\]]*)\]\((?P<path>[^)]+)\)").unwrap_or_else(|_| unreachable!())
crates/mcp-agent-mail-core/src/git_binary.rs:427:    let re = Regex::new(r"git version (\d+)\.(\d+)\.(\d+)").ok()?;
crates/mcp-agent-mail-db/src/tracking.rs:32:        Regex::new(r#"(?i)\binsert\s+(?:or\s+\w+\s+)?into\s+([\w.`"\[\]]+)"#)
crates/mcp-agent-mail-db/src/tracking.rs:34:        Regex::new(r#"(?i)\bupdate\s+([\w.`"\[\]]+)"#).unwrap_or_else(|_| unreachable!()),
crates/mcp-agent-mail-db/src/tracking.rs:35:        Regex::new(r#"(?i)\bfrom\s+([\w.`"\[\]]+)"#).unwrap_or_else(|_| unreachable!()),
crates/mcp-agent-mail-db/src/tracking.rs:630:        LazyLock::new(|| Regex::new(r#"[`"\[\]]*\.[`"\[\]]*"#).unwrap_or_else(|_| unreachable!()));
crates/mcp-agent-mail-core/src/experience.rs:703:    LazyLock::new(|| Regex::new(r"-----BEGIN [A-Z ]+-----").expect("PEM regex"));
crates/mcp-agent-mail-core/src/experience.rs:706:    Regex::new(r"eyJ[A-Za-z0-9_-]{10,}\.[A-Za-z0-9_-]{10,}\.").expect("JWT regex")
crates/mcp-agent-mail-share/src/scrub.rs:32:        Regex::new(r"(?i)ghp_[A-Za-z0-9]{36,}").unwrap_or_else(|_| unreachable!()),
crates/mcp-agent-mail-share/src/scrub.rs:33:        Regex::new(r"(?i)github_pat_[A-Za-z0-9_]{20,}").unwrap_or_else(|_| unreachable!()),
crates/mcp-agent-mail-share/src/scrub.rs:35:        Regex::new(r"(?i)xox[baprs]-[A-Za-z0-9\-]{10,}").unwrap_or_else(|_| unreachable!()),
```

## P30 debug print/log leftovers

```
crates/mcp-agent-mail-test-helpers/src/parity.rs:117:                eprintln!(
crates/mcp-agent-mail-test-helpers/src/parity.rs:123:            eprintln!(
crates/mcp-agent-mail-conformance/tests/conformance_debug.rs:295:        eprintln!(
crates/mcp-agent-mail-conformance/tests/conformance_debug.rs:300:            eprintln!(
crates/mcp-agent-mail-conformance/tests/conformance_debug.rs:350:        eprintln!(
crates/mcp-agent-mail-conformance/tests/conformance_debug.rs:355:            eprintln!(
crates/mcp-agent-mail-conformance/tests/conformance_debug.rs:401:    eprintln!(
crates/mcp-agent-mail-conformance/tests/resource_description_parity.rs:181:    eprintln!(
crates/mcp-agent-mail-conformance/tests/resource_description_parity.rs:229:    eprintln!(
crates/mcp-agent-mail-conformance/tests/error_code_parity.rs:279:    eprintln!(
crates/mcp-agent-mail-conformance/tests/error_code_parity.rs:297:        eprintln!("[{scenario_prefix}] scenario=relative_human_key_invalid_argument start");
crates/mcp-agent-mail-conformance/tests/error_code_parity.rs:321:        eprintln!("[{scenario_prefix}] scenario=empty_program start");
crates/mcp-agent-mail-conformance/tests/error_code_parity.rs:336:        eprintln!("[{scenario_prefix}] scenario=empty_model start");
crates/mcp-agent-mail-conformance/tests/error_code_parity.rs:351:        eprintln!("[{scenario_prefix}] scenario=empty_project_key start");
crates/mcp-agent-mail-conformance/tests/error_code_parity.rs:371:            eprintln!("[placeholder_detection] scenario=placeholder_{placeholder} start");
crates/mcp-agent-mail-conformance/tests/error_code_parity.rs:408:        eprintln!("[not_found_suggestions] scenario=project_typo key={typo}");
crates/mcp-agent-mail-conformance/tests/error_code_parity.rs:487:        eprintln!("[not_found_without_suggestions] scenario=unrelated_project");
crates/mcp-agent-mail-conformance/tests/error_code_parity.rs:515:        eprintln!("[not_found_without_suggestions] scenario=missing_agent");
crates/mcp-agent-mail-conformance/tests/tool_description_parity.rs:198:        eprintln!("[{tool_name}] note: extra properties (ok): {extra_props:?}");
crates/mcp-agent-mail-conformance/tests/tool_description_parity.rs:271:            eprintln!("SKIP: {} (Python-only)", py_tool.name);
crates/mcp-agent-mail-conformance/tests/tool_description_parity.rs:278:            eprintln!("FAIL: missing in Rust");
crates/mcp-agent-mail-conformance/tests/tool_description_parity.rs:296:            eprintln!("PASS");
crates/mcp-agent-mail-conformance/tests/tool_description_parity.rs:299:            eprintln!("PASS (extended)");
crates/mcp-agent-mail-conformance/tests/tool_description_parity.rs:303:                eprintln!("FAIL: {detail}");
crates/mcp-agent-mail-conformance/tests/tool_description_parity.rs:313:                eprintln!("FAIL: unknown diff");
crates/mcp-agent-mail-conformance/tests/tool_description_parity.rs:328:                eprintln!("RUST-NATIVE: {} (not in Python fixture)", rust_name);
crates/mcp-agent-mail-conformance/tests/tool_description_parity.rs:332:            eprintln!("EXTRA: {} (unexpected Rust-only tool)", rust_name);
crates/mcp-agent-mail-conformance/tests/tool_description_parity.rs:341:    eprintln!("\nTool description parity: {passed}/{total} tools passed");
crates/mcp-agent-mail-conformance/tests/tool_description_parity.rs:383:            eprintln!("PASS");
crates/mcp-agent-mail-conformance/tests/tool_description_parity.rs:386:            eprintln!("FAIL ({} issues)", errors.len());
crates/mcp-agent-mail-conformance/tests/tool_description_parity.rs:391:    eprintln!("\nSchema parity: {passed}/{checked} tools passed");
crates/mcp-agent-mail-conformance/tests/contact_enforcement_outage.rs:435:    eprintln!(
crates/mcp-agent-mail-conformance/tests/contact_enforcement_outage.rs:483:    eprintln!(
crates/mcp-agent-mail-conformance/tests/contact_enforcement_outage.rs:656:    eprintln!("[br-1i11.2.6] Atomicity PASS: {n} messages sent, counter delta={delta} (>= {n})");
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
crates/mcp-agent-mail-guard/src/lib.rs:423:        print(
crates/mcp-agent-mail-guard/src/lib.rs:429:        print(
crates/mcp-agent-mail-guard/src/lib.rs:473:                print(
crates/mcp-agent-mail-guard/src/lib.rs:491:                    print(
crates/mcp-agent-mail-guard/src/lib.rs:518:        print(
crates/mcp-agent-mail-guard/src/lib.rs:750:        print(
crates/mcp-agent-mail-guard/src/lib.rs:765:        print("mcp-agent-mail: guard failed to read reservations: " + str(exc), file=sys.stderr)
crates/mcp-agent-mail-guard/src/lib.rs:924:        print("mcp-agent-mail: AGENT_NAME environment variable is required.", file=sys.stderr)
crates/mcp-agent-mail-guard/src/lib.rs:955:        print(f"WARNING: {msg}", file=sys.stderr)
crates/mcp-agent-mail-guard/src/lib.rs:958:        print(f"ERROR: {msg}", file=sys.stderr)
crates/mcp-agent-mail-guard/src/lib.rs:959:        print("Set AGENT_MAIL_GUARD_MODE=warn to allow commit anyway.", file=sys.stderr)
crates/mcp-agent-mail-guard/src/lib.rs:1304:                    eprintln!(
crates/mcp-agent-mail/benches/benchmarks.rs:3148:                            println!(
crates/mcp-agent-mail-conformance/tests/conformance/python_reference/generate_fixtures.py:1044:        print(f"[fixture-gen] ERROR: {exc}", file=sys.stderr)
crates/mcp-agent-mail-conformance/tests/conformance/python_reference/generate_fixtures.py:1049:    print(f"Wrote fixtures to {FIXTURES}")
crates/mcp-agent-mail-cli/tests/robot_golden_snapshots.rs:29:        eprintln!("updated golden fixture: {}", path.display());
crates/mcp-agent-mail-cli/tests/integration_runs.rs:43:    eprintln!(
crates/mcp-agent-mail-cli/tests/tui_transport_harness.rs:204:    eprintln!(
crates/mcp-agent-mail-cli/tests/tui_transport_harness.rs:314:        eprintln!(
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
crates/mcp-agent-mail-db/tests/atc_rollup_snapshot.rs:29:                eprintln!("[SETUP] Applied {} migrations for {name}", applied.len());
crates/mcp-agent-mail-cli/tests/tui_accessibility_harness.rs:220:    eprintln!("tui_a11y artifact root: {}", run_root.display());
```

## P31 JSON.stringify used as key/hash/memo identity

_none found_

## P32 money-like arithmetic (audit integer cents/decimal)

```
crates/mcp-agent-mail/benches/benchmarks.rs:732:                    total_elements_f64 / (total_us_f64 / 1_000_000.0)
crates/mcp-agent-mail/benches/benchmarks.rs:1758:                        (total_elements / (total_us / 1_000_000.0) * 100.0).round() / 100.0
crates/mcp-agent-mail/benches/benchmarks.rs:3550:                total_reads_f64 / (total_us_f64 / 1_000_000.0)
crates/mcp-agent-mail-search-core/src/rollout.rs:228:            (overlap_sum as f64 / total as f64) / 10000.0
crates/mcp-agent-mail-search-core/src/rollout.rs:246:                equivalent as f64 / total as f64 * 100.0
crates/mcp-agent-mail-search-core/src/rollout.rs:252:                errors as f64 / total as f64 * 100.0
crates/mcp-agent-mail-search-core/src/cache.rs:207:            (self.hits as f64 / total as f64) * 100.0
crates/mcp-agent-mail-core/src/metrics.rs:1088:            shadow_equiv as f64 / shadow_total as f64 * 100.0
crates/mcp-agent-mail-core/src/diagnostics.rs:1739:        snap.tools.tool_errors_total = 5; // 2.5%
crates/mcp-agent-mail-cli/src/lib.rs:12467:                            let pct = (received as f64 / total as f64 * 100.0).min(100.0);
crates/mcp-agent-mail-cli/src/lib.rs:53918:                    format!("{:.1}", entry.total_wait_ns as f64 / 1_000_000.0),
crates/mcp-agent-mail-cli/src/lib.rs:54069:                        entry.total_wait_ns as f64 / 1_000_000.0
crates/mcp-agent-mail-cli/src/robot.rs:8283:                (total_errors as f64 / total_calls as f64) * 100.0
crates/mcp-agent-mail-server/src/lib.rs:514:    let query_time_ms = (after.total_time_ms - before.total_time_ms).max(0.0);
crates/mcp-agent-mail-storage/tests/stress_pipeline.rs:3415:                errs as f64 / total_i as f64 * 100.0,
crates/mcp-agent-mail-storage/tests/stress_pipeline.rs:3416:                lat_sum as f64 / total_i as f64 / 1000.0,
crates/mcp-agent-mail-db/src/search_rollout.rs:225:            (overlap_sum as f64 / total as f64) / 10000.0
crates/mcp-agent-mail-db/src/search_rollout.rs:241:                equivalent as f64 / total as f64 * 100.0
crates/mcp-agent-mail-db/src/search_rollout.rs:247:                errors as f64 / total as f64 * 100.0
crates/mcp-agent-mail-db/benches/search_v3_bench.rs:309:                (count as f64 * build_ops as f64) / (build_total_us as f64 / 1_000_000.0)
crates/mcp-agent-mail-db/src/tracking.rs:1258:                (snap.total_time_ms - expected_time).abs() < 0.02,
crates/mcp-agent-mail-db/src/tracking.rs:1471:        assert!((snap.total_time_ms - 6.0).abs() < 0.01);
crates/mcp-agent-mail-db/src/tracking.rs:1563:        assert!((snap.total_time_ms - 12.34).abs() < 0.001);
crates/mcp-agent-mail-db/src/search_cache.rs:206:            (self.hits as f64 / total as f64) * 100.0
crates/mcp-agent-mail-db/src/archive_anomaly.rs:1://! Archive anomaly taxonomy and safe remediation classes (br-97gc6.5.2.4.1).
crates/mcp-agent-mail-db/src/atc_queries.rs:1841:        assert!((entry.total_regret - 1.5).abs() < f64::EPSILON);
crates/mcp-agent-mail-db/src/atc_queries.rs:1842:        assert!((entry.total_loss - 2.0).abs() < f64::EPSILON);
crates/mcp-agent-mail-db/src/atc_queries.rs:2151:        assert!((probe.total_loss - 1.0).abs() < f64::EPSILON);
crates/mcp-agent-mail-db/src/atc_queries.rs:2152:        assert!((probe.total_regret - 0.25).abs() < f64::EPSILON);
crates/mcp-agent-mail-db/src/atc_queries.rs:2258:        assert!((row.total_loss - 0.25).abs() < f64::EPSILON);
crates/mcp-agent-mail-db/src/atc_queries.rs:2259:        assert!((row.total_regret - 0.05).abs() < f64::EPSILON);
crates/mcp-agent-mail-server/src/atc.rs:944:            (total - 1.0).abs() < 1e-10,
crates/mcp-agent-mail-server/src/atc.rs:1217:            (total - 1.0).abs() < 1e-6,
crates/mcp-agent-mail-server/src/atc.rs:1296:            (total - 1.0).abs() < 1e-10,
crates/mcp-agent-mail-server/src/atc.rs:8218:            (total - 1.0).abs() < 1e-10,
crates/mcp-agent-mail-server/src/tui_widgets.rs:7721:        assert!((tool_end_total - 1.0).abs() < f64::EPSILON);
crates/mcp-agent-mail-server/src/tui_widgets.rs:7722:        assert!((msg_sent_total - 1.0).abs() < f64::EPSILON);
crates/mcp-agent-mail-server/src/tui_widgets.rs:7740:            (total - 2.0).abs() < f64::EPSILON,
crates/mcp-agent-mail-server/src/tui_screens/atc.rs:604:            st.total_micros as f64 / 1000.0,
crates/mcp-agent-mail-server/src/tui_screens/analytics.rs:678:        (total_errors as f64 / total_calls as f64) * 100.0
crates/mcp-agent-mail-server/src/tui_screens/analytics.rs:926:        (snapshot.total_errors as f64 / snapshot.total_calls as f64) * 100.0
crates/mcp-agent-mail-server/src/tui_screens/analytics.rs:2000:                    (snapshot.total_errors as f64 / snapshot.total_calls as f64) * 100.0
crates/mcp-agent-mail-server/src/tui_screens/analytics.rs:2033:            (snapshot.total_errors as f64 / snapshot.total_calls as f64) * 100.0
crates/mcp-agent-mail-server/src/tui_screens/tool_metrics.rs:1017:            format!("{:.1}%", (total_errors as f64 / total_calls as f64) * 100.0)
crates/mcp-agent-mail-server/src/tui_screens/tool_metrics.rs:1046:        let trend_errors = if total_errors as f64 / (total_calls.max(1) as f64) > 0.05 {
```

## P33 local time / UTC drift candidates

```
crates/mcp-agent-mail-guard/src/lib.rs:760:    now = datetime.datetime.now(datetime.timezone.utc)
crates/mcp-agent-mail-storage/tests/fixtures/notifications/README.md:8:- timestamps are placeholders; tests should accept any valid ISO-8601 UTC string from datetime.now(timezone.utc).isoformat().
crates/mcp-agent-mail-server/src/tui_web_dashboard.rs:1143:  return `dash${Math.random().toString(16).slice(2, 10)}${Date.now().toString(16).slice(-8)}`;
crates/mcp-agent-mail-server/src/tui_web_dashboard.rs:1557:  lastFrameAgeMs = Math.max(0, Math.round((Date.now() * 1000 - lastTimestampUs) / 1000));
crates/mcp-agent-mail-server/src/tui_web_dashboard.rs:2014:    lastInputFlushAt = Date.now();
crates/mcp-agent-mail-server/templates/mail_unified_inbox.html:745:      const diffSeconds = Math.floor((Date.now() - this.lastRefreshTime.getTime()) / 1000);
crates/mcp-agent-mail-server/templates/mail_unified_inbox.html:768:        this.lastRefreshTime = new Date();
crates/mcp-agent-mail-server/templates/mail_unified_inbox.html:1111:        this.lastRefreshTime = new Date();
crates/mcp-agent-mail-server/templates/mail_search.html:522:      const seconds = Math.max(0, Math.floor((Date.now() - ts) / 1000));
crates/mcp-agent-mail-server/templates/base.html:2768:              const startTime = Date.now();
crates/mcp-agent-mail-server/templates/base.html:2775:                const elapsed = Date.now() - startTime;
crates/mcp-agent-mail-server/templates/base.html:2820:              toast.pausedAt = Date.now();
```

## P34 detailed internal errors exposed

```
crates/mcp-agent-mail/benches/benchmarks.rs:2220:        database_url: format!("sqlite+aiosqlite:///{}", db_path.display()),
crates/mcp-agent-mail-share/src/deploy.rs:1817:        DbConn::open_file(&db_path_str).map_err(|e| format!("cannot open mailbox.sqlite3: {e}"))?;
crates/mcp-agent-mail-share/src/deploy.rs:1851:        DbConn::open_file(&db_path_str).map_err(|e| format!("cannot open mailbox.sqlite3: {e}"))?;
crates/mcp-agent-mail-share/src/deploy.rs:1969:                        message: format!("mailbox.sqlite3.config.json is invalid: {err}"),
crates/mcp-agent-mail-share/src/deploy.rs:1979:                    message: format!("cannot read mailbox.sqlite3.config.json: {err}"),
crates/mcp-agent-mail-share/src/deploy.rs:2790:        message: format!("GET {url}/mailbox.sqlite3 should return 200 (if database included)"),
crates/mcp-agent-mail-tools/tests/tool_input_proptest.rs:268:    let db_path = format!("/tmp/tools-input-proptest-{suffix}.sqlite3");
crates/mcp-agent-mail-tools/tests/tool_input_proptest.rs:269:    let database_url = format!("sqlite://{db_path}");
crates/mcp-agent-mail-share/src/scrub.rs:533:        message: format!("scalar query failed: {e}"),
crates/mcp-agent-mail-tools/tests/validation_error_parity.rs:40:    let db_path = format!("/tmp/validation-error-parity-{env_suffix}.sqlite3");
crates/mcp-agent-mail-tools/tests/validation_error_parity.rs:41:    let database_url = format!("sqlite://{db_path}");
crates/mcp-agent-mail-cli/tests/integration_runs.rs:83:        format!("sqlite:///{}", self.db_path.display())
crates/mcp-agent-mail-tools/tests/system_error_parity.rs:41:    let db_path = format!("/tmp/system-error-parity-{env_suffix}.sqlite3");
crates/mcp-agent-mail-tools/tests/system_error_parity.rs:42:    let database_url = format!("sqlite://{db_path}");
crates/mcp-agent-mail-share/src/probe.rs:711:            format!("{scheme}://{}{}?{query}", base.authority(), normalized)
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
crates/mcp-agent-mail-tools/tests/messaging_error_parity.rs:37:    let db_path = format!("/tmp/messaging-error-parity-{env_suffix}.sqlite3");
crates/mcp-agent-mail-tools/tests/messaging_error_parity.rs:38:    let database_url = format!("sqlite://{db_path}");
crates/mcp-agent-mail-cli/tests/semantic_conformance.rs:62:        format!("sqlite:///{}", self.db_path.display())
```

## P35 suspicious ambiguous imports

```
crates/mcp-agent-mail-test-helpers/src/parity.rs:46:use std::path::Path;
crates/mcp-agent-mail-test-helpers/src/repo.rs:12:use std::path::{Path, PathBuf};
crates/mcp-agent-mail-test-helpers/src/shim_git.rs:23:use std::path::{Path, PathBuf};
crates/mcp-agent-mail-conformance/tests/conformance_debug.rs:5:use std::path::{Path, PathBuf};
crates/mcp-agent-mail-conformance/tests/resource_coverage_guard.rs:4:use std::path::{Path, PathBuf};
crates/mcp-agent-mail-conformance/tests/doc_consistency.rs:4:use std::path::{Path, PathBuf};
crates/mcp-agent-mail-conformance/tests/conformance.rs:15:use std::path::{Path, PathBuf};
crates/mcp-agent-mail-tools/src/identity.rs:17:use std::path::{Path, PathBuf};
crates/mcp-agent-mail-tools/src/identity.rs:1859:    use std::path::PathBuf;
crates/mcp-agent-mail/tests/archive_perf_reporting.rs:8:use std::path::Path;
crates/mcp-agent-mail-tools/src/products.rs:49:    use std::fmt::Write;
crates/mcp-agent-mail-tools/src/macros.rs:792:        use std::path::Path;
crates/mcp-agent-mail-tools/src/macros.rs:1114:        use std::path::Path;
crates/mcp-agent-mail-tools/src/resources.rs:19:use std::path::{Path, PathBuf};
crates/mcp-agent-mail-tools/src/resources.rs:4747:    use std::path::PathBuf;
crates/mcp-agent-mail/src/main.rs:10:use std::path::Path;
crates/mcp-agent-mail-tools/src/llm.rs:13:use std::fmt::Write as _;
crates/mcp-agent-mail-tools/src/reservations.rs:20:use std::fmt::Write;
crates/mcp-agent-mail-tools/src/reservations.rs:21:use std::path::PathBuf;
crates/mcp-agent-mail-tools/src/reservations.rs:1622:    use std::path::PathBuf;
crates/mcp-agent-mail-tools/src/build_slots.rs:13:use std::path::{Path, PathBuf};
crates/mcp-agent-mail-tools/src/lib.rs:62:    use std::path::{Path, PathBuf};
crates/mcp-agent-mail/benches/benchmarks.rs:21:use std::path::{Path, PathBuf};
crates/mcp-agent-mail-tools/src/messaging.rs:18:use std::path::Path;
crates/mcp-agent-mail-conformance/src/lib.rs:9:use std::path::Path;
crates/mcp-agent-mail-conformance/src/main.rs:4:use std::path::{Path, PathBuf};
crates/mcp-agent-mail-search-core/src/index_layout.rs:11:use std::path::{Path, PathBuf};
crates/mcp-agent-mail-guard/tests/guard_env_tests.rs:8:use std::path::Path;
crates/mcp-agent-mail-share/src/git.rs:1:use std::path::{Path, PathBuf};
crates/mcp-agent-mail-search-core/src/model2vec.rs:12:use std::path::{Path, PathBuf};
crates/mcp-agent-mail-share/src/detection.rs:17:use std::path::Path;
crates/mcp-agent-mail-guard/src/lib.rs:4:use std::path::{Path, PathBuf};
crates/mcp-agent-mail-agent-detect/src/lib.rs:10:use std::path::PathBuf;
crates/mcp-agent-mail-share/src/deploy.rs:9:use std::path::{Path, PathBuf};
crates/mcp-agent-mail-share/src/deploy.rs:2619:        use std::path::Component;
crates/mcp-agent-mail-share/src/deploy.rs:2666:        use std::path::Component;
crates/mcp-agent-mail-share/src/deploy.rs:3008:        use std::fmt::Write;
crates/mcp-agent-mail-share/src/scrub.rs:6:use std::path::Path;
crates/mcp-agent-mail-core/src/evidence_ledger.rs:16:use std::path::{Path, PathBuf};
crates/mcp-agent-mail-cli/tests/robot_golden_snapshots.rs:3:use std::path::{Path, PathBuf};
crates/mcp-agent-mail-core/src/agent_detect.rs:17:use std::path::PathBuf;
crates/mcp-agent-mail-share/src/crypto.rs:9:use std::path::{Path, PathBuf};
crates/mcp-agent-mail-share/src/crypto.rs:625:        use std::path::Component;
crates/mcp-agent-mail-cli/tests/integration_runs.rs:3:use std::path::{Path, PathBuf};
crates/mcp-agent-mail-cli/tests/tui_transport_harness.rs:13:use std::path::{Path, PathBuf};
crates/mcp-agent-mail-core/tests/toon_integration.rs:10:use std::path::PathBuf;
crates/mcp-agent-mail-cli/tests/tui_accessibility_harness.rs:11:use std::path::{Path, PathBuf};
crates/mcp-agent-mail-share/src/bundle.rs:7:use std::path::{Path, PathBuf};
crates/mcp-agent-mail-share/src/bundle.rs:1232:        use std::path::Component;
crates/mcp-agent-mail-share/src/bundle.rs:1274:        use std::path::Component;
crates/mcp-agent-mail-core/benches/toon_bench.rs:8:use std::path::PathBuf;
crates/mcp-agent-mail-cli/tests/share_verify_decrypt.rs:3:use std::path::{Path, PathBuf};
crates/mcp-agent-mail-core/tests/agent_detect_integration.rs:21:    use std::path::PathBuf;
crates/mcp-agent-mail-cli/tests/share_archive_harness.rs:12:use std::path::{Path, PathBuf};
crates/mcp-agent-mail-share/src/scope.rs:7:use std::path::Path;
crates/mcp-agent-mail-share/src/scope.rs:654:    use std::path::PathBuf;
crates/mcp-agent-mail-cli/tests/semantic_conformance.rs:12:use std::path::{Path, PathBuf};
crates/mcp-agent-mail-core/src/toon.rs:9:use std::path::Path;
crates/mcp-agent-mail-share/src/finalize.rs:5:use std::path::Path;
crates/mcp-agent-mail-cli/tests/security_privacy_harness.rs:10:use std::path::{Path, PathBuf};
crates/mcp-agent-mail-cli/tests/perf_security_regressions.rs:10:use std::path::{Path, PathBuf};
crates/mcp-agent-mail-share/src/planner.rs:16:use std::path::{Path, PathBuf};
crates/mcp-agent-mail-share/src/planner.rs:274:    use std::path::Component;
crates/mcp-agent-mail-cli/tests/perf_guardrails.rs:12:use std::path::{Path, PathBuf};
crates/mcp-agent-mail-core/src/setup.rs:11:use std::path::{Path, PathBuf};
crates/mcp-agent-mail-core/src/setup.rs:297:        use std::fmt::Write;
crates/mcp-agent-mail-share/src/snapshot.rs:10:use std::path::{Path, PathBuf};
crates/mcp-agent-mail-share/src/snapshot.rs:348:        use std::path::Component;
crates/mcp-agent-mail-cli/tests/mode_matrix_harness.rs:8:use std::path::{Path, PathBuf};
crates/mcp-agent-mail-core/src/identity.rs:10:use std::path::{Path, PathBuf};
crates/mcp-agent-mail-cli/tests/http_transport_harness.rs:12:use std::path::{Path, PathBuf};
crates/mcp-agent-mail-share/src/wizard.rs:17:use std::path::PathBuf;
crates/mcp-agent-mail-cli/tests/help_snapshots.rs:3:use std::path::{Path, PathBuf};
crates/mcp-agent-mail-cli/tests/golden_integration.rs:3:use std::path::PathBuf;
crates/mcp-agent-mail-server/tests/truthfulness_integration.rs:20:use std::path::PathBuf;
crates/mcp-agent-mail-cli/tests/flake_triage_integration.rs:7:use std::path::PathBuf;
crates/mcp-agent-mail-share/src/static_render.rs:18:use std::path::{Path, PathBuf};
crates/mcp-agent-mail-share/src/static_render.rs:1383:        use std::path::Component;
crates/mcp-agent-mail-server/tests/e2e_atc_learning_loop.rs:3:use std::fmt::Write as _;
crates/mcp-agent-mail-server/tests/e2e_atc_learning_loop.rs:6:use std::path::Path;
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
crates/mcp-agent-mail-tools/src/search.rs:8:use fastmcp::prelude::*;
crates/mcp-agent-mail-tools/src/identity.rs:12:use fastmcp::prelude::*;
crates/mcp-agent-mail-tools/src/build_slots.rs:9:use fastmcp::prelude::*;
crates/mcp-agent-mail-tools/src/products.rs:10:use fastmcp::prelude::*;
crates/mcp-agent-mail-tools/src/macros.rs:20:use fastmcp::prelude::*;
crates/mcp-agent-mail-tools/src/messaging.rs:12:use fastmcp::prelude::*;
crates/mcp-agent-mail-tools/src/contacts.rs:9:use fastmcp::prelude::*;
crates/mcp-agent-mail-tools/src/reservations.rs:12:use fastmcp::prelude::*;
crates/mcp-agent-mail-tools/src/resources.rs:13:use fastmcp::prelude::*;
crates/mcp-agent-mail-conformance/tests/conformance.rs:7:use proptest::prelude::*;
crates/mcp-agent-mail-core/src/proptest_generators.rs:7:use proptest::prelude::*;
crates/mcp-agent-mail-db/src/queries.rs:27:use sqlmodel::prelude::*;
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
