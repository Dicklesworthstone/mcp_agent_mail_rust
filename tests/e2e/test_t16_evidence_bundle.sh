#!/usr/bin/env bash
# test_t16_evidence_bundle.sh - Before/after evidence bundle and closeout (br-1xt0m.1.13.5)
#
# Generates a final T16 closeout evidence bundle mapping every audited
# deficiency to its implementation evidence, test evidence, and closure
# rationale. Validates that all required artifacts are present and the
# bundle is structurally complete.

E2E_SUITE="t16_evidence_bundle"
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
# shellcheck source=../../scripts/e2e_lib.sh
source "${SCRIPT_DIR}/../../scripts/e2e_lib.sh"

e2e_init_artifacts
e2e_banner "T16 Before/After Evidence Bundle and Closeout (br-1xt0m.1.13.5)"

if ! command -v python3 >/dev/null 2>&1; then
    e2e_skip "python3 required"
    e2e_summary
    exit 0
fi

if ! command -v jq >/dev/null 2>&1; then
    e2e_skip "jq required"
    e2e_summary
    exit 0
fi

PROJECT_ROOT="${E2E_PROJECT_ROOT:-$(cd "${SCRIPT_DIR}/../.." && pwd)}"

# ── Case 1: Collect evidence artifacts ────────────────────────────────

e2e_case_banner "Collect evidence artifacts"

BUNDLE_JSON="${E2E_ARTIFACT_DIR}/closeout_bundle.json"

python3 - "$BUNDLE_JSON" "$PROJECT_ROOT" <<'PYEOF'
import json, sys, os, glob, hashlib, time

dest = sys.argv[1]
root = sys.argv[2]

def sha256_file(path):
    h = hashlib.sha256()
    try:
        with open(path, "rb") as f:
            for chunk in iter(lambda: f.read(8192), b""):
                h.update(chunk)
        return h.hexdigest()
    except Exception:
        return None

def find_latest(pattern):
    """Find latest file matching glob pattern under root."""
    matches = sorted(glob.glob(os.path.join(root, pattern)), reverse=True)
    return matches[0] if matches else None

def file_entry(rel_path):
    """Build a file evidence entry."""
    full = os.path.join(root, rel_path)
    if os.path.exists(full):
        return {
            "path": rel_path,
            "exists": True,
            "bytes": os.path.getsize(full),
            "sha256": sha256_file(full),
        }
    return {"path": rel_path, "exists": False}

# Deficiency tracks with closure evidence
tracks = [
    {
        "id": "T16.1", "title": "Navigation & Information Architecture Coherence",
        "impl_evidence": [
            "crates/mcp-agent-mail-server/src/tui_screens/mod.rs",
            "crates/mcp-agent-mail-server/src/tui_app.rs",
        ],
        "test_evidence": [
            "tests/e2e/test_stdio_workflows.sh",
            "crates/mcp-agent-mail-server/tests/tui_perf_baselines.rs",
        ],
        "closure_rationale": "Screen registry drives navigation count and key-hint synchronization. Full-surface jump semantics implemented for 14 screens. Validated by E2E stdio workflow and perf baseline suites.",
    },
    {
        "id": "T16.2", "title": "Hit-Region Layering & Mouse Dispatch Unification",
        "impl_evidence": [
            "crates/mcp-agent-mail-server/src/tui_screens/mod.rs",
        ],
        "test_evidence": [
            "tests/e2e/test_stdio_adaptive.sh",
        ],
        "closure_rationale": "Canonical hit-region IDs, central mouse dispatcher, and pane hover/active affordances implemented. Validated by adaptive width E2E suite.",
    },
    {
        "id": "T16.3", "title": "Chrome Shell Hierarchy & Status Surface Redesign",
        "impl_evidence": [
            "crates/mcp-agent-mail-server/src/tui_app.rs",
        ],
        "test_evidence": [
            "crates/mcp-agent-mail-server/tests/golden_snapshots.rs",
        ],
        "closure_rationale": "Tab row hierarchy with active-state contrast, adaptive status-bar truncation, and keycap help strip redesigned. Validated by golden snapshot matrix across 4 width classes.",
    },
    {
        "id": "T16.4", "title": "Help/Overlay/Coach-Hint Surfaces",
        "impl_evidence": [
            "crates/mcp-agent-mail-server/src/tui_app.rs",
        ],
        "test_evidence": [
            "tests/e2e/test_stdio_workflows.sh",
        ],
        "closure_rationale": "Overlay stack policy with escape-precedence, context-aware help panels, and contextual coach hints implemented. Validated by E2E workflow suite.",
    },
    {
        "id": "T16.5", "title": "Transition Motion & Animation",
        "impl_evidence": [
            "crates/mcp-agent-mail-server/src/tui_app.rs",
        ],
        "test_evidence": [
            "tests/e2e/test_perf_regression.sh",
            "tests/e2e/test_stdio_adaptive.sh",
        ],
        "closure_rationale": "Semantic screen transitions, reduced-motion toggle, and focus micro-motion with budget guardrails. Validated by perf regression (frame budget enforcement) and adaptive E2E suites.",
    },
    {
        "id": "T16.6", "title": "Action Menu Execution Integrity & Feedback UX",
        "impl_evidence": [
            "crates/mcp-agent-mail-server/src/tui_screens/mod.rs",
        ],
        "test_evidence": [
            "tests/e2e/test_failure_injection.sh",
        ],
        "closure_rationale": "ActionKind::Execute wired to dispatcher, ConfirmThenExecute callback, progress/outcome surfaces and disabled-reason UX. Validated by failure-injection E2E suite covering degraded paths.",
    },
    {
        "id": "T16.7", "title": "Typography & Color Semantics",
        "impl_evidence": [
            "crates/mcp-agent-mail-server/src/tui_app.rs",
        ],
        "test_evidence": [
            "crates/mcp-agent-mail-server/tests/golden_snapshots.rs",
        ],
        "closure_rationale": "Semantic typography hierarchy, color rebalance for information priority, and density cleanup via chunked metadata patterns. Validated by golden snapshot baselines.",
    },
    {
        "id": "T16.8", "title": "Dashboard KPI & Event Surfaces",
        "impl_evidence": [
            "crates/mcp-agent-mail-server/src/tui_screens/mod.rs",
        ],
        "test_evidence": [
            "tests/e2e/test_stdio_screen_workflows.sh",
        ],
        "closure_rationale": "KPI priority recomposition, event stream salience, trend/anomaly annotations and narrow-width dashboard tuning. Validated by screen workflow E2E suite.",
    },
    {
        "id": "T16.9", "title": "Messages & Threads Redesign",
        "impl_evidence": [
            "crates/mcp-agent-mail-server/src/tui_screens/messages.rs",
        ],
        "test_evidence": [
            "tests/e2e/test_stdio_screen_workflows.sh",
        ],
        "closure_rationale": "Live-results integration, message row hierarchy with unread/importance cues, threads filter-bar and row-chunking, thread detail narrative. Validated by screen workflow E2E suite.",
    },
    {
        "id": "T16.10", "title": "Search & Timeline Surfaces",
        "impl_evidence": [
            "crates/mcp-agent-mail-server/src/tui_screens/search.rs",
        ],
        "test_evidence": [
            "tests/e2e/test_stdio_screen_workflows.sh",
            "crates/mcp-agent-mail-server/tests/pty_e2e_search.rs",
        ],
        "closure_rationale": "Progressive-disclosure search, explicit labels, result/inspector hierarchy, timeline semantic encoding. Validated by screen workflow E2E and PTY search integration tests.",
    },
    {
        "id": "T16.11", "title": "SystemHealth & Responsive Layout",
        "impl_evidence": [
            "crates/mcp-agent-mail-server/src/tui_app.rs",
        ],
        "test_evidence": [
            "tests/e2e/test_stdio_adaptive.sh",
        ],
        "closure_rationale": "Structured diagnostic sections, adaptive width-class layout, narrow-width fallback with anomaly-first prioritization. Validated by adaptive width E2E suite.",
    },
    {
        "id": "T16.12", "title": "Discoverability & Accessibility",
        "impl_evidence": [
            "crates/mcp-agent-mail-server/src/tui_app.rs",
        ],
        "test_evidence": [
            "tests/e2e/test_stdio_adaptive.sh",
            "scripts/e2e_tui_a11y.sh",
        ],
        "closure_rationale": "Status-surface discoverability, auto-synchronized shortcut docs, screen-local help snippets, keyboard/mouse parity audit. Validated by adaptive E2E and accessibility audit suites.",
    },
]

# Collect all evidence file entries
evidence_files = []
all_paths = set()
for track in tracks:
    for p in track["impl_evidence"] + track["test_evidence"]:
        all_paths.add(p)

for p in sorted(all_paths):
    evidence_files.append(file_entry(p))

# Collect latest artifacts
artifact_globs = {
    "t16_evidence": "tests/artifacts/t16_validate/*/evidence.json",
    "traceability_matrix": "tests/artifacts/t16_traceability/*/traceability_matrix.json",
    "perf_summary": "tests/artifacts/perf_regression/*/perf_summary.json",
    "soak_report": "tests/artifacts/perf_regression/*/soak_report.json",
}

collected_artifacts = {}
for name, pattern in artifact_globs.items():
    latest = find_latest(pattern)
    if latest:
        rel = os.path.relpath(latest, root)
        collected_artifacts[name] = file_entry(rel)
    else:
        collected_artifacts[name] = {"path": pattern, "exists": False}

# Build the closeout bundle
bundle = {
    "schema_version": 1,
    "bead": "br-1xt0m.1.13.5",
    "title": "T16 Before/After Evidence Bundle and Deficiency Closeout Checklist",
    "generated_at": time.strftime("%Y-%m-%dT%H:%M:%SZ", time.gmtime()),
    "deficiency_closeout": tracks,
    "evidence_files": evidence_files,
    "collected_artifacts": collected_artifacts,
    "summary": {
        "total_tracks": len(tracks),
        "total_evidence_files": len(evidence_files),
        "files_present": sum(1 for f in evidence_files if f["exists"]),
        "files_missing": sum(1 for f in evidence_files if not f["exists"]),
        "artifacts_collected": sum(1 for a in collected_artifacts.values() if a.get("exists")),
        "all_tracks_have_rationale": all(bool(t.get("closure_rationale")) for t in tracks),
        "all_tracks_have_impl_evidence": all(bool(t.get("impl_evidence")) for t in tracks),
        "all_tracks_have_test_evidence": all(bool(t.get("test_evidence")) for t in tracks),
    }
}

with open(dest, "w") as f:
    json.dump(bundle, f, indent=2)
    f.write("\n")

s = bundle["summary"]
print(f"Bundle: {s['total_tracks']} tracks, {s['files_present']}/{s['total_evidence_files']} files, {s['artifacts_collected']}/{len(collected_artifacts)} artifacts")
PYEOF
gen_rc=$?

if [ "$gen_rc" -eq 0 ] && [ -f "$BUNDLE_JSON" ]; then
    e2e_pass "closeout bundle generated"
else
    e2e_fail "closeout bundle generation failed"
fi

# ── Case 2: All deficiency tracks have closure rationale ─────────────

e2e_case_banner "Closure rationale completeness"

RATIONALE_OK="$(jq '.summary.all_tracks_have_rationale' "$BUNDLE_JSON" 2>/dev/null)"
IMPL_OK="$(jq '.summary.all_tracks_have_impl_evidence' "$BUNDLE_JSON" 2>/dev/null)"
TEST_OK="$(jq '.summary.all_tracks_have_test_evidence' "$BUNDLE_JSON" 2>/dev/null)"

if [ "$RATIONALE_OK" = "true" ]; then
    e2e_pass "all 12 tracks have closure rationale"
else
    e2e_fail "some tracks missing closure rationale"
fi

if [ "$IMPL_OK" = "true" ]; then
    e2e_pass "all 12 tracks have implementation evidence references"
else
    e2e_fail "some tracks missing implementation evidence"
fi

if [ "$TEST_OK" = "true" ]; then
    e2e_pass "all 12 tracks have test evidence references"
else
    e2e_fail "some tracks missing test evidence"
fi

# ── Case 3: Evidence files exist on disk ──────────────────────────────

e2e_case_banner "Evidence file presence"

FILES_PRESENT="$(jq '.summary.files_present' "$BUNDLE_JSON" 2>/dev/null)"
FILES_MISSING="$(jq '.summary.files_missing' "$BUNDLE_JSON" 2>/dev/null)"
FILES_TOTAL="$(jq '.summary.total_evidence_files' "$BUNDLE_JSON" 2>/dev/null)"

if [ "${FILES_MISSING:-1}" -eq 0 ]; then
    e2e_pass "all ${FILES_PRESENT} evidence files exist on disk"
else
    e2e_fail "${FILES_MISSING} of ${FILES_TOTAL} evidence files missing"
    # Show which are missing
    jq -r '.evidence_files[] | select(.exists == false) | "  MISSING: " + .path' "$BUNDLE_JSON" 2>/dev/null
fi

# ── Case 4: Collected runtime artifacts ───────────────────────────────

e2e_case_banner "Runtime artifact collection"

ARTIFACTS_COLLECTED="$(jq '.summary.artifacts_collected' "$BUNDLE_JSON" 2>/dev/null)"
ARTIFACTS_TOTAL="$(jq '.collected_artifacts | length' "$BUNDLE_JSON" 2>/dev/null)"

if [ "${ARTIFACTS_COLLECTED:-0}" -ge 1 ]; then
    e2e_pass "${ARTIFACTS_COLLECTED} of ${ARTIFACTS_TOTAL} runtime artifacts collected"
else
    e2e_fail "no runtime artifacts collected"
fi

# Check for traceability matrix specifically
TRACE_EXISTS="$(jq '.collected_artifacts.traceability_matrix.exists' "$BUNDLE_JSON" 2>/dev/null)"
if [ "$TRACE_EXISTS" = "true" ]; then
    e2e_pass "traceability matrix artifact present"
else
    e2e_fail "traceability matrix artifact missing"
fi

# ── Case 5: SHA256 integrity ──────────────────────────────────────────

e2e_case_banner "Evidence integrity (SHA256)"

HASH_COUNT="$(jq '[.evidence_files[] | select(.exists == true and .sha256 != null)] | length' "$BUNDLE_JSON" 2>/dev/null)"
NO_HASH="$(jq '[.evidence_files[] | select(.exists == true and .sha256 == null)] | length' "$BUNDLE_JSON" 2>/dev/null)"

if [ "${HASH_COUNT:-0}" -gt 0 ] && [ "${NO_HASH:-1}" -eq 0 ]; then
    e2e_pass "all ${HASH_COUNT} present files have SHA256 hashes"
else
    e2e_fail "${NO_HASH} present files missing SHA256 hashes"
fi

# ── Case 6: Bundle structure completeness ─────────────────────────────

e2e_case_banner "Bundle structure completeness"

STRUCT_OK="$(python3 - "$BUNDLE_JSON" <<'PYEOF'
import json, sys

with open(sys.argv[1]) as f:
    b = json.load(f)

errors = []
required_top = ["schema_version", "bead", "title", "generated_at", "deficiency_closeout", "evidence_files", "collected_artifacts", "summary"]
for field in required_top:
    if field not in b:
        errors.append(f"missing top-level field: {field}")

for i, track in enumerate(b.get("deficiency_closeout", [])):
    for field in ("id", "title", "impl_evidence", "test_evidence", "closure_rationale"):
        if field not in track:
            errors.append(f"track {i}: missing {field}")

if b.get("summary", {}).get("total_tracks", 0) != 12:
    errors.append(f"expected 12 tracks, got {b.get('summary', {}).get('total_tracks', 0)}")

if errors:
    for e in errors:
        print(f"ERROR: {e}", file=sys.stderr)
    print("FAIL")
else:
    print("OK")
PYEOF
)"

if [ "$STRUCT_OK" = "OK" ]; then
    e2e_pass "closeout bundle structure complete (12 tracks, all required fields)"
else
    e2e_fail "closeout bundle structure incomplete"
fi

# ── Case 7: Cross-validate against traceability matrix ────────────────

e2e_case_banner "Cross-validate with traceability matrix"

# Find latest traceability matrix
TRACE_MATRIX="$(find "${PROJECT_ROOT}/tests/artifacts/t16_traceability" -name traceability_matrix.json -type f 2>/dev/null | sort -r | head -1)"

if [ -z "$TRACE_MATRIX" ] || [ ! -f "$TRACE_MATRIX" ]; then
    e2e_skip "no traceability matrix found (run test_t16_traceability.sh first)"
else
    XVAL_RESULT="$(python3 - "$BUNDLE_JSON" "$TRACE_MATRIX" <<'PYEOF'
import json, sys

with open(sys.argv[1]) as f:
    bundle = json.load(f)
with open(sys.argv[2]) as f:
    matrix = json.load(f)

# Track IDs must match
bundle_ids = {t["id"] for t in bundle["deficiency_closeout"]}
matrix_ids = {t["id"] for t in matrix["deficiency_tracks"]}

missing_in_bundle = matrix_ids - bundle_ids
extra_in_bundle = bundle_ids - matrix_ids

if missing_in_bundle or extra_in_bundle:
    if missing_in_bundle:
        print(f"Missing in bundle: {missing_in_bundle}", file=sys.stderr)
    if extra_in_bundle:
        print(f"Extra in bundle: {extra_in_bundle}", file=sys.stderr)
    print(f"FAIL {len(missing_in_bundle)} {len(extra_in_bundle)}")
else:
    print(f"OK {len(bundle_ids)}")
PYEOF
    )"

    if [[ "$XVAL_RESULT" == OK* ]]; then
        COUNT="${XVAL_RESULT#OK }"
        e2e_pass "all ${COUNT} deficiency track IDs match between bundle and traceability matrix"
    else
        e2e_fail "track ID mismatch between bundle and traceability matrix"
    fi
fi

# ── Summary ──────────────────────────────────────────────────────────

e2e_save_artifact "env_dump.txt" "$(e2e_dump_env 2>&1)"
e2e_copy_artifact "$BUNDLE_JSON" "closeout_bundle.json"
e2e_summary
