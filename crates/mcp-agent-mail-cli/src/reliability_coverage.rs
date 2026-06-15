//! Reliability unit-test coverage standard and matrix (br-bvq1x.14.1 / N1).
//!
//! This module codifies the unit-test Definition-of-Done for the `br-bvq1x`
//! reliability program and provides a *lightweight, source-level* checker that
//! flags any tracked reliability module which lacks an **error-path** test.
//!
//! The historical reliability bugs this epic exists to prevent were almost all
//! error-path or degraded-state failures (corruption mislabelled as healthy,
//! recovery paths that silently lost rows, unbounded retries, owner-blind
//! checkpoints). A reliability module with happy-path tests but no error-path
//! test is exactly the kind of blind spot that lets those bugs regress.
//!
//! The check is deliberately cheap: it scans the inline `#[cfg(test)]` test
//! functions of a curated registry of reliability modules and asserts each one
//! exercises at least one error/degraded path. It is wired into `am ci`
//! (a gate) and `am verify reliability-coverage` (a lane) via the integration
//! test `tests/reliability_coverage_ci.rs`, which also keeps the committed
//! `docs/RELIABILITY_COVERAGE_MATRIX.md` in sync.
//!
//! Realism levels (`R0`..`R4`) reference `docs/VERIFICATION_COVERAGE_LEDGER.md`.
//!
//! Everything here is pure: [`analyze_source`] takes source text and returns a
//! [`ModuleScan`]; [`scan_modules`] reads the registry's files and aggregates a
//! [`CoverageReport`]. This makes the scanner itself trivially unit-testable —
//! and, fittingly, it carries its own error-path tests so it satisfies the very
//! contract it enforces.

use std::path::{Path, PathBuf};

use serde::Serialize;

/// Realism level of a test surface, per `docs/VERIFICATION_COVERAGE_LEDGER.md`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
pub enum Realism {
    /// Real production code path over real runtime/durable state (real engine,
    /// real git archive, real file mutations); local tempdirs are fine.
    R0,
    /// Real code over synthetic local files/fixtures/controlled inputs.
    R1,
    /// Sanctioned offline substitute preserving a contract boundary.
    R2,
    /// Explicit mock/stub/fake lane (branch coverage only).
    R3,
    /// Thin/indirect coverage; needs explicit ownership before confidence.
    R4,
}

impl Realism {
    /// Stable short label used in the committed matrix.
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::R0 => "R0",
            Self::R1 => "R1",
            Self::R2 => "R2",
            Self::R3 => "R3",
            Self::R4 => "R4",
        }
    }
}

/// A single reliability module tracked by the coverage matrix.
#[derive(Clone, Copy, Debug, Serialize)]
pub struct ReliabilityModule {
    /// Reliability-program track this module belongs to (e.g. `"A"`, `"K"`).
    pub track: &'static str,
    /// Path relative to the workspace root (forward slashes).
    pub rel_path: &'static str,
    /// One-line description of the reliability behavior this module owns.
    pub behavior: &'static str,
    /// Expected realism floor for this module's inline tests.
    pub realism: Realism,
}

/// The curated registry of reliability modules whose error-path coverage is
/// enforced. Adding a module here makes the check require (and the matrix
/// document) at least one error-path test in that module.
///
/// Ordering is stable (by track, then path) so the rendered matrix diffs
/// minimally.
pub const RELIABILITY_MODULES: &[ReliabilityModule] = &[
    // ── Track A — Corruption diagnosis & honest error taxonomy ───────────────
    ReliabilityModule {
        track: "A",
        rel_path: "crates/mcp-agent-mail-core/src/error.rs",
        behavior: "Unified error taxonomy and classification",
        realism: Realism::R1,
    },
    ReliabilityModule {
        track: "A",
        rel_path: "crates/mcp-agent-mail-db/src/circuit_breaker.rs",
        behavior: "Corruption circuit breaker: trip only on edit-blocking corruption",
        realism: Realism::R1,
    },
    ReliabilityModule {
        track: "A",
        rel_path: "crates/mcp-agent-mail-db/src/error.rs",
        behavior: "DB error classification (transient vs corruption vs MVCC)",
        realism: Realism::R1,
    },
    // ── Track B — Doctor trust: integrity labels & safe mutation ─────────────
    ReliabilityModule {
        track: "B",
        rel_path: "crates/mcp-agent-mail-db/src/integrity.rs",
        behavior: "Schema/integrity invariant checks and scoped verdicts",
        realism: Realism::R1,
    },
    ReliabilityModule {
        track: "B",
        rel_path: "crates/mcp-agent-mail-cli/src/doctor/mutate.rs",
        behavior: "Doctor mutate() chokepoint: backup, atomic write, scope refusal",
        realism: Realism::R0,
    },
    // ── Track D — Live-owner & lock-contention intelligence ──────────────────
    ReliabilityModule {
        track: "D",
        rel_path: "crates/mcp-agent-mail-db/src/pool.rs",
        behavior: "Connection pool, owner/lock inspection, recovery & backoff",
        realism: Realism::R0,
    },
    // ── Track G — Schema gate, WAL/sidecar & startup invariants ──────────────
    ReliabilityModule {
        track: "G",
        rel_path: "crates/mcp-agent-mail-db/src/cache.rs",
        behavior: "Write-behind cache coherency and invalidation correctness",
        realism: Realism::R1,
    },
    ReliabilityModule {
        track: "G",
        rel_path: "crates/mcp-agent-mail-db/src/migrate.rs",
        behavior: "Schema migration gate and legacy-timestamp conversion",
        realism: Realism::R0,
    },
    ReliabilityModule {
        track: "G",
        rel_path: "crates/mcp-agent-mail-db/src/wal_classify.rs",
        behavior: "WAL/SHM sidecar classification & safe-checkpoint policy",
        realism: Realism::R0,
    },
    // ── Track K — Observability, snapshots & recovery ────────────────────────
    ReliabilityModule {
        track: "K",
        rel_path: "crates/mcp-agent-mail-db/src/retry.rs",
        behavior: "Bounded retry / circuit-breaker state machine",
        realism: Realism::R1,
    },
    ReliabilityModule {
        track: "K",
        rel_path: "crates/mcp-agent-mail-db/src/snapshot.rs",
        behavior: "Last-known-healthy verified snapshot & snapshot-preferred recovery",
        realism: Realism::R0,
    },
    ReliabilityModule {
        track: "K",
        rel_path: "crates/mcp-agent-mail-storage/src/recovery.rs",
        behavior: "Archive-to-DB reconstruction / recovery procedures",
        realism: Realism::R1,
    },
    ReliabilityModule {
        track: "K",
        rel_path: "crates/mcp-agent-mail-storage/src/boot_check.rs",
        behavior: "Boot-time consistency validation",
        realism: Realism::R1,
    },
    // ── Track I — Runtime liveness & metrics ─────────────────────────────────
    ReliabilityModule {
        track: "I",
        rel_path: "crates/mcp-agent-mail-core/src/metrics.rs",
        behavior: "Runtime metrics, heartbeats and watchdog counters",
        realism: Realism::R1,
    },
    // ── Storage / coalescer durability ───────────────────────────────────────
    ReliabilityModule {
        track: "Storage",
        rel_path: "crates/mcp-agent-mail-db/src/coalesce.rs",
        behavior: "Commit coalescing batch/flush correctness",
        realism: Realism::R1,
    },
    ReliabilityModule {
        track: "Storage",
        rel_path: "crates/mcp-agent-mail-storage/src/lib.rs",
        behavior: "Git-backed archive writes, WBQ and commit coalescer",
        realism: Realism::R0,
    },
];

/// Result of scanning one module's source for inline test coverage.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize)]
pub struct ModuleScan {
    /// Total number of `#[test]` functions found.
    pub total_tests: usize,
    /// Names of test functions classified as exercising an error/degraded path.
    pub error_path_tests: Vec<String>,
}

impl ModuleScan {
    /// True when at least one `#[test]` function was found.
    #[must_use]
    pub fn has_any_test(&self) -> bool {
        self.total_tests > 0
    }

    /// True when at least one error-path test was found.
    #[must_use]
    pub fn has_error_path_test(&self) -> bool {
        !self.error_path_tests.is_empty()
    }
}

/// Why a tracked module fails the coverage check.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Gap {
    /// The registered source file does not exist.
    FileMissing,
    /// The file exists but has no inline `#[test]` functions at all.
    NoTests,
    /// The file has tests but none exercise an error/degraded path.
    NoErrorPathTest,
}

impl Gap {
    /// Human-readable explanation for reports.
    #[must_use]
    pub const fn describe(self) -> &'static str {
        match self {
            Self::FileMissing => "registered reliability module file is missing",
            Self::NoTests => "no inline #[test] functions found",
            Self::NoErrorPathTest => {
                "has tests but none exercise an error/degraded path (is_err/unwrap_err/should_panic/Err(...) or an error-named test)"
            }
        }
    }
}

/// Per-module entry in the coverage report.
#[derive(Clone, Debug, Serialize)]
pub struct ModuleReport {
    /// Track label.
    pub track: &'static str,
    /// Path relative to workspace root.
    pub rel_path: &'static str,
    /// Behavior description.
    pub behavior: &'static str,
    /// Expected realism floor.
    pub realism: Realism,
    /// Whether the source file was found on disk.
    pub exists: bool,
    /// Scan results (zeroed when the file is missing).
    pub scan: ModuleScan,
    /// The coverage gap, if any.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub gap: Option<Gap>,
}

impl ModuleReport {
    /// True when this module satisfies the coverage contract.
    #[must_use]
    pub fn is_covered(&self) -> bool {
        self.gap.is_none()
    }
}

/// Aggregate coverage report across the whole registry.
#[derive(Clone, Debug, Serialize)]
pub struct CoverageReport {
    /// Per-module entries (registry order).
    pub modules: Vec<ModuleReport>,
}

impl CoverageReport {
    /// Modules that fail the coverage contract.
    #[must_use]
    pub fn gaps(&self) -> Vec<&ModuleReport> {
        self.modules.iter().filter(|m| m.gap.is_some()).collect()
    }

    /// True when every tracked module satisfies the contract.
    #[must_use]
    pub fn is_pass(&self) -> bool {
        self.modules.iter().all(ModuleReport::is_covered)
    }

    /// Number of modules with error-path coverage.
    #[must_use]
    pub fn covered_count(&self) -> usize {
        self.modules.iter().filter(|m| m.is_covered()).count()
    }

    /// Renders the committed, low-churn coverage matrix document.
    ///
    /// Deliberately omits raw test counts and individual test names so that
    /// adding ordinary tests does not force a matrix re-bless — the document
    /// changes only when a module is added/removed or its coverage status flips.
    #[must_use]
    pub fn render_markdown(&self) -> String {
        let mut out = String::new();
        out.push_str("# Reliability Unit-Test Coverage Matrix\n\n");
        out.push_str(
            "<!-- AUTO-GENERATED by `mcp_agent_mail_cli::reliability_coverage`. Do not edit by hand.\n",
        );
        out.push_str(
            "     Regenerate: `UPDATE_GOLDEN=1 cargo test -p mcp-agent-mail-cli --test reliability_coverage_ci`\n",
        );
        out.push_str(
            "     Enforced by: `am ci` (gate) and `am verify reliability-coverage` (lane). -->\n\n",
        );
        out.push_str(
            "Tracks the unit-test Definition-of-Done for the `br-bvq1x` reliability program.\n",
        );
        out.push_str(
            "Each listed module must carry at least one inline **error-path** test. The check is\n",
        );
        out.push_str(
            "enforced in CI; this document is the human-readable map of what is covered.\n\n",
        );
        out.push_str("## Definition of Done\n\n");
        out.push_str(
            "- Every reliability code path has inline `#[cfg(test)]` tests for happy, edge **and** error cases.\n",
        );
        out.push_str(
            "- Tests are deterministic (asupersync `LabRuntime`/virtual time; no wall-clock sleeps, no tokio).\n",
        );
        out.push_str(
            "- User-facing / persistence / transport paths meet the `R0`/`R1` realism floor in\n",
        );
        out.push_str("  `docs/VERIFICATION_COVERAGE_LEDGER.md`.\n\n");
        out.push_str("## Coverage matrix\n\n");
        out.push_str("| Track | Module | Behavior | Realism | Error-path tested |\n");
        out.push_str("|-------|--------|----------|---------|-------------------|\n");
        for m in &self.modules {
            let mark = if m.is_covered() { "✓" } else { "✗" };
            out.push_str(&format!(
                "| {} | `{}` | {} | {} | {} |\n",
                m.track,
                m.rel_path,
                m.behavior,
                m.realism.label(),
                mark,
            ));
        }
        out.push_str("\n## Summary\n\n");
        out.push_str(&format!(
            "- Reliability modules tracked: {}\n",
            self.modules.len()
        ));
        out.push_str(&format!(
            "- With error-path coverage: {}/{}\n",
            self.covered_count(),
            self.modules.len()
        ));
        out
    }
}

/// Classifies a test function name as targeting an error/degraded path.
///
/// Token-based to avoid false positives (e.g. `query` must not match on `err`).
fn name_signals_error(name: &str) -> bool {
    // Substrings that are themselves unambiguous error signals anywhere in the
    // name (they do not collide with common happy-path identifiers).
    const SUBSTRINGS: &[&str] = &[
        "error",
        "panic",
        "corrupt",
        "malformed",
        "invalid",
        "refuse",
        "reject",
        "timeout",
        "overflow",
        "exhaust",
        "missing",
        "conflict",
        "mismatch",
        "orphan",
        "unsupported",
        "poison",
        "truncat",
        "stale",
        "denied",
        "unauthor",
        "forbidden",
        "saturat",
        "wedged",
        "abort",
        "degrad",
        "fallback",
        "not_run",
        "noop_on",
    ];
    let lower = name.to_ascii_lowercase();
    if SUBSTRINGS.iter().any(|s| lower.contains(s)) {
        return true;
    }
    // Token-exact matches for short, collision-prone words.
    const TOKENS: &[&str] = &[
        "err", "fail", "fails", "failed", "bad", "trip", "trips", "busy", "empty", "deny", "guard",
        "blocked", "refused", "rejects", "skip", "skipped",
    ];
    lower.split('_').any(|tok| TOKENS.contains(&tok))
}

/// Returns true when a test body contains an explicit error-path assertion.
fn body_signals_error(body: &str) -> bool {
    const MARKERS: &[&str] = &[
        "is_err()",
        "unwrap_err()",
        "expect_err(",
        ".is_err",
        "assert_err",
        "panic!(",
        "Err(",
        "Error(",
        "should_panic",
    ];
    MARKERS.iter().any(|m| body.contains(m))
}

/// Skips a Rust string/char literal or comment starting at `bytes[i]`, returning
/// the index just past it. If `bytes[i]` does not start a literal/comment,
/// returns `i` unchanged.
///
/// Handles `//` line comments, `/* */` block comments (nested), `"..."` and
/// `'...'` literals with escapes, and raw strings `r"..."` / `r#"..."#`.
fn skip_literal_or_comment(bytes: &[u8], i: usize) -> usize {
    let len = bytes.len();
    let b = bytes[i];
    // Line comment.
    if b == b'/' && i + 1 < len && bytes[i + 1] == b'/' {
        let mut j = i + 2;
        while j < len && bytes[j] != b'\n' {
            j += 1;
        }
        return j;
    }
    // Block comment (supports nesting, like rustc).
    if b == b'/' && i + 1 < len && bytes[i + 1] == b'*' {
        let mut j = i + 2;
        let mut depth = 1u32;
        while j < len && depth > 0 {
            if bytes[j] == b'/' && j + 1 < len && bytes[j + 1] == b'*' {
                depth += 1;
                j += 2;
            } else if bytes[j] == b'*' && j + 1 < len && bytes[j + 1] == b'/' {
                depth -= 1;
                j += 2;
            } else {
                j += 1;
            }
        }
        return j;
    }
    // Raw string: r"..." or r#"..."# (any number of '#').
    if b == b'r' && i + 1 < len && (bytes[i + 1] == b'"' || bytes[i + 1] == b'#') {
        let mut j = i + 1;
        let mut hashes = 0usize;
        while j < len && bytes[j] == b'#' {
            hashes += 1;
            j += 1;
        }
        if j < len && bytes[j] == b'"' {
            j += 1;
            // Find closing quote followed by `hashes` '#'.
            while j < len {
                if bytes[j] == b'"' {
                    let mut k = j + 1;
                    let mut closing = 0usize;
                    while k < len && closing < hashes && bytes[k] == b'#' {
                        closing += 1;
                        k += 1;
                    }
                    if closing == hashes {
                        return k;
                    }
                }
                j += 1;
            }
            return len;
        }
    }
    // Normal string literal.
    if b == b'"' {
        let mut j = i + 1;
        while j < len {
            match bytes[j] {
                b'\\' => j += 2,
                b'"' => return j + 1,
                _ => j += 1,
            }
        }
        return len;
    }
    // Char literal (also covers lifetimes like 'a — those have no closing quote
    // immediately, but treating them as 1-char skips is harmless for brace
    // matching since they contain no braces).
    if b == b'\'' {
        // Lookahead for a closing quote within a few bytes; otherwise treat as
        // a lifetime tick and advance by one.
        let mut j = i + 1;
        let mut steps = 0;
        while j < len && steps < 4 {
            match bytes[j] {
                b'\\' => {
                    j += 2;
                    steps += 1;
                }
                b'\'' => return j + 1,
                _ => {
                    j += 1;
                    steps += 1;
                }
            }
        }
        return i + 1;
    }
    i
}

/// Given the index of an opening `{`, returns the index just past the matching
/// `}` (string/comment-aware). Returns `bytes.len()` if unbalanced.
fn match_brace_block(bytes: &[u8], open: usize) -> usize {
    let len = bytes.len();
    let mut i = open;
    let mut depth = 0i32;
    while i < len {
        let skipped = skip_literal_or_comment(bytes, i);
        if skipped != i {
            i = skipped;
            continue;
        }
        match bytes[i] {
            b'{' => depth += 1,
            b'}' => {
                depth -= 1;
                if depth == 0 {
                    return i + 1;
                }
            }
            _ => {}
        }
        i += 1;
    }
    len
}

/// Returns true if the byte slice starting at `i` is the `#[test]` attribute,
/// allowing internal whitespace (`# [ test ]`).
fn is_test_attr_at(bytes: &[u8], i: usize) -> bool {
    // Match `#`, optional ws, `[`, optional ws, `test`, optional ws, `]`.
    let len = bytes.len();
    if bytes[i] != b'#' {
        return false;
    }
    let mut j = i + 1;
    let skip_ws = |mut k: usize| {
        while k < len && bytes[k].is_ascii_whitespace() {
            k += 1;
        }
        k
    };
    j = skip_ws(j);
    if j >= len || bytes[j] != b'[' {
        return false;
    }
    j = skip_ws(j + 1);
    if j + 4 > len || &bytes[j..j + 4] != b"test" {
        return false;
    }
    j = skip_ws(j + 4);
    j < len && bytes[j] == b']'
}

/// Analyzes Rust source text and returns its inline-test coverage scan.
///
/// Anchors on the `#[test]` attribute (the idiom in this codebase — async tests
/// are plain `#[test]` fns that drive `block_on`). For each test, the function
/// name and body are classified for error-path signals.
#[must_use]
pub fn analyze_source(src: &str) -> ModuleScan {
    let bytes = src.as_bytes();
    let len = bytes.len();
    let mut scan = ModuleScan::default();
    let mut i = 0usize;
    while i < len {
        // Fast-skip strings/comments at the top level so a `#[test]` inside a
        // string/comment is not miscounted.
        let skipped = skip_literal_or_comment(bytes, i);
        if skipped != i {
            i = skipped;
            continue;
        }
        if bytes[i] == b'#' && is_test_attr_at(bytes, i) {
            // Found a test attribute. Scan forward to the `fn <name>`, noting any
            // `should_panic` attribute that appears before it.
            let mut j = i;
            let mut saw_should_panic = false;
            let mut name: Option<String> = None;
            // Walk forward token-wise until we hit `fn`.
            while j < len {
                let sk = skip_literal_or_comment(bytes, j);
                if sk != j {
                    j = sk;
                    continue;
                }
                if bytes[j] == b'#' {
                    // Another attribute; check for should_panic in its span.
                    // Find the end of this attribute by brace/bracket match.
                    let attr_end = attr_span_end(bytes, j);
                    if src[j..attr_end].contains("should_panic") {
                        saw_should_panic = true;
                    }
                    j = attr_end;
                    continue;
                }
                if starts_with_keyword(bytes, j, b"fn") {
                    // Parse the function name.
                    let mut k = j + 2;
                    while k < len && bytes[k].is_ascii_whitespace() {
                        k += 1;
                    }
                    let name_start = k;
                    while k < len && (bytes[k].is_ascii_alphanumeric() || bytes[k] == b'_') {
                        k += 1;
                    }
                    if k > name_start {
                        name = Some(src[name_start..k].to_string());
                    }
                    // Advance to the opening brace of the body.
                    while k < len && bytes[k] != b'{' && bytes[k] != b';' {
                        k += 1;
                    }
                    let body = if k < len && bytes[k] == b'{' {
                        let end = match_brace_block(bytes, k);
                        &src[k..end]
                    } else {
                        ""
                    };
                    scan.total_tests += 1;
                    let fn_name = name.unwrap_or_default();
                    if saw_should_panic
                        || name_signals_error(&fn_name)
                        || body_signals_error(body)
                    {
                        scan.error_path_tests.push(fn_name);
                    }
                    // Resume scanning after the body.
                    i = if k < len && bytes[k] == b'{' {
                        match_brace_block(bytes, k)
                    } else {
                        k + 1
                    };
                    break;
                }
                j += 1;
            }
            if j >= len {
                break;
            }
            continue;
        }
        i += 1;
    }
    scan
}

/// Returns the index just past an attribute beginning at `bytes[i] == '#'`,
/// matching the `[...]` (bracket-balanced, string/comment-aware).
fn attr_span_end(bytes: &[u8], i: usize) -> usize {
    let len = bytes.len();
    let mut j = i + 1;
    while j < len && bytes[j].is_ascii_whitespace() {
        j += 1;
    }
    // Optional `!` for inner attributes.
    if j < len && bytes[j] == b'!' {
        j += 1;
    }
    if j >= len || bytes[j] != b'[' {
        return j;
    }
    let mut depth = 0i32;
    while j < len {
        let sk = skip_literal_or_comment(bytes, j);
        if sk != j {
            j = sk;
            continue;
        }
        match bytes[j] {
            b'[' => depth += 1,
            b']' => {
                depth -= 1;
                if depth == 0 {
                    return j + 1;
                }
            }
            _ => {}
        }
        j += 1;
    }
    len
}

/// True when `bytes[i..]` begins with `kw` as a standalone keyword (preceded by
/// a non-identifier byte and followed by a non-identifier byte).
fn starts_with_keyword(bytes: &[u8], i: usize, kw: &[u8]) -> bool {
    let end = i + kw.len();
    if end > bytes.len() || &bytes[i..end] != kw {
        return false;
    }
    let before_ok = i == 0 || !is_ident_byte(bytes[i - 1]);
    let after_ok = end == bytes.len() || !is_ident_byte(bytes[end]);
    before_ok && after_ok
}

const fn is_ident_byte(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b == b'_'
}

/// Scans every registered reliability module under `workspace_root` and returns
/// the aggregate coverage report.
#[must_use]
pub fn scan_modules(workspace_root: &Path) -> CoverageReport {
    let modules = RELIABILITY_MODULES
        .iter()
        .map(|m| scan_one(workspace_root, m))
        .collect();
    CoverageReport { modules }
}

fn scan_one(workspace_root: &Path, module: &ReliabilityModule) -> ModuleReport {
    let path: PathBuf = workspace_root.join(module.rel_path);
    match std::fs::read_to_string(&path) {
        Ok(src) => {
            let scan = analyze_source(&src);
            let gap = if !scan.has_any_test() {
                Some(Gap::NoTests)
            } else if !scan.has_error_path_test() {
                Some(Gap::NoErrorPathTest)
            } else {
                None
            };
            ModuleReport {
                track: module.track,
                rel_path: module.rel_path,
                behavior: module.behavior,
                realism: module.realism,
                exists: true,
                scan,
                gap,
            }
        }
        Err(_) => ModuleReport {
            track: module.track,
            rel_path: module.rel_path,
            behavior: module.behavior,
            realism: module.realism,
            exists: false,
            scan: ModuleScan::default(),
            gap: Some(Gap::FileMissing),
        },
    }
}

/// Resolves the workspace root from this crate's `CARGO_MANIFEST_DIR`
/// (`crates/mcp-agent-mail-cli` -> workspace root).
#[must_use]
pub fn workspace_root_from_manifest() -> PathBuf {
    let manifest = Path::new(env!("CARGO_MANIFEST_DIR"));
    manifest
        .parent()
        .and_then(Path::parent)
        .map_or_else(|| manifest.to_path_buf(), Path::to_path_buf)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn analyze_counts_tests_and_detects_error_by_name() {
        let src = r#"
            #[cfg(test)]
            mod tests {
                #[test]
                fn happy_path_works() { assert_eq!(1 + 1, 2); }

                #[test]
                fn rejects_invalid_input() { assert_eq!(2, 2); }
            }
        "#;
        let scan = analyze_source(src);
        assert_eq!(scan.total_tests, 2);
        assert_eq!(scan.error_path_tests, vec!["rejects_invalid_input"]);
        assert!(scan.has_error_path_test());
    }

    #[test]
    fn analyze_detects_error_by_body_assertion() {
        // Happy name, but the body asserts an error result.
        let src = r"
            #[test]
            fn round_trip() {
                let r = parse();
                assert!(r.is_err());
            }
        ";
        let scan = analyze_source(src);
        assert_eq!(scan.total_tests, 1);
        assert_eq!(scan.error_path_tests, vec!["round_trip"]);
    }

    #[test]
    fn analyze_detects_should_panic_attribute() {
        let src = r"
            #[test]
            #[should_panic]
            fn overflows_when_too_big() { build(u64::MAX); }
        ";
        let scan = analyze_source(src);
        assert_eq!(scan.total_tests, 1);
        // Both the attribute and the name signal error; counted once.
        assert_eq!(scan.error_path_tests.len(), 1);
    }

    #[test]
    fn analyze_reports_no_error_path_when_only_happy_tests() {
        let src = r"
            #[test]
            fn builds_widget() { let _ = make(); }
            #[test]
            fn renders_ok() { let _ = render(); }
        ";
        let scan = analyze_source(src);
        assert_eq!(scan.total_tests, 2);
        assert!(!scan.has_error_path_test());
        assert!(scan.error_path_tests.is_empty());
    }

    #[test]
    fn analyze_handles_empty_and_testless_source() {
        assert_eq!(analyze_source("").total_tests, 0);
        let scan = analyze_source("pub fn f() -> i32 { 0 }");
        assert_eq!(scan.total_tests, 0);
        assert!(!scan.has_any_test());
    }

    #[test]
    fn analyze_ignores_test_attr_inside_string_or_comment() {
        // A `#[test]` mentioned inside a string and a comment must NOT count.
        // (Outer delimiter needs two hashes: the payload contains `"#`.)
        let src = r##"
            // example in a comment: #[test] fn fake_error() {}
            const DOC: &str = "#[test] fn also_fake_err() { is_err() }";
            #[test]
            fn real_failure_case() { assert!(x.is_err()); }
        "##;
        let scan = analyze_source(src);
        assert_eq!(scan.total_tests, 1, "only the real #[test] counts");
        assert_eq!(scan.error_path_tests, vec!["real_failure_case"]);
    }

    #[test]
    fn analyze_brace_matching_survives_braces_in_strings() {
        // The body contains a `}` inside a string literal; the next test must
        // still be detected (brace matcher must not end early).
        let src = r#"
            #[test]
            fn writes_payload() {
                let s = "}{ not a real brace }";
                let _ = s;
            }
            #[test]
            fn detects_corruption() { assert!(check().is_err()); }
        "#;
        let scan = analyze_source(src);
        assert_eq!(scan.total_tests, 2);
        assert_eq!(scan.error_path_tests, vec!["detects_corruption"]);
    }

    #[test]
    fn analyze_handles_raw_strings_with_braces() {
        let src = r###"
            #[test]
            fn emits_json() {
                let j = r#"{"a": {"b": 1}}"#;
                let _ = j;
            }
            #[test]
            fn fails_on_bad_json() { assert!(parse().is_err()); }
        "###;
        let scan = analyze_source(src);
        assert_eq!(scan.total_tests, 2);
        assert_eq!(scan.error_path_tests, vec!["fails_on_bad_json"]);
    }

    #[test]
    fn name_signals_error_avoids_false_positives() {
        assert!(!name_signals_error("query_returns_rows"));
        assert!(!name_signals_error("renders_dashboard"));
        assert!(!name_signals_error("inserts_and_reads_back"));
        assert!(name_signals_error("rejects_bad_token"));
        assert!(name_signals_error("pool_exhausted_returns_err"));
        assert!(name_signals_error("malformed_header_detected"));
        assert!(name_signals_error("checkpoint_refused_while_live"));
    }

    #[test]
    fn registry_entries_are_well_formed_and_sorted_within_track() {
        // No empty fields; paths look like crate sources.
        for m in RELIABILITY_MODULES {
            assert!(!m.track.is_empty());
            assert!(m.rel_path.starts_with("crates/"), "{}", m.rel_path);
            assert!(m.rel_path.ends_with(".rs"), "{}", m.rel_path);
            assert!(!m.behavior.is_empty(), "{}", m.rel_path);
        }
        // No duplicate paths.
        let mut paths: Vec<&str> = RELIABILITY_MODULES.iter().map(|m| m.rel_path).collect();
        let before = paths.len();
        paths.sort_unstable();
        paths.dedup();
        assert_eq!(before, paths.len(), "duplicate rel_path in registry");
    }

    #[test]
    fn coverage_report_detects_missing_file_gap() {
        // Scanning a registry against an empty temp dir yields FileMissing gaps.
        let tmp = std::env::temp_dir().join(format!(
            "amrc-missing-{}",
            std::process::id()
        ));
        let report = scan_modules(&tmp);
        assert!(!report.is_pass());
        let gaps = report.gaps();
        assert_eq!(gaps.len(), report.modules.len());
        assert!(gaps.iter().all(|g| g.gap == Some(Gap::FileMissing)));
        assert!(!gaps[0].exists);
    }

    #[test]
    fn render_markdown_is_deterministic_and_lists_every_module() {
        let report = CoverageReport {
            modules: vec![ModuleReport {
                track: "K",
                rel_path: "crates/x/src/y.rs",
                behavior: "does a thing",
                realism: Realism::R0,
                exists: true,
                scan: ModuleScan {
                    total_tests: 3,
                    error_path_tests: vec!["t_err".into()],
                },
                gap: None,
            }],
        };
        let a = report.render_markdown();
        let b = report.render_markdown();
        assert_eq!(a, b, "render must be deterministic");
        assert!(a.contains("crates/x/src/y.rs"));
        assert!(a.contains("| K |"));
        assert!(a.contains("R0"));
        assert!(a.contains("✓"));
        assert!(a.contains("With error-path coverage: 1/1"));
    }

    #[test]
    fn gap_describe_is_nonempty_for_all_variants() {
        for g in [Gap::FileMissing, Gap::NoTests, Gap::NoErrorPathTest] {
            assert!(!g.describe().is_empty());
        }
    }
}
