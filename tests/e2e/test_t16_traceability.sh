#!/usr/bin/env bash
# test_t16_traceability.sh - Traceability matrix and evidence map (br-1xt0m.1.13.17)
#
# Generates and validates a deficiency-to-implementation-to-test traceability
# matrix. Future maintainers can validate T16 closure without referring back
# to the original planning conversation.

E2E_SUITE="t16_traceability"
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
# shellcheck source=../../scripts/e2e_lib.sh
source "${SCRIPT_DIR}/../../scripts/e2e_lib.sh"

e2e_init_artifacts
e2e_banner "T16 Traceability Matrix and Evidence Map (br-1xt0m.1.13.17)"

if ! command -v python3 >/dev/null 2>&1; then
    e2e_skip "python3 required"
    e2e_summary
    exit 0
fi

PROJECT_ROOT="${E2E_PROJECT_ROOT:-$(cd "${SCRIPT_DIR}/../.." && pwd)}"

# ── Case 1: Generate traceability matrix ─────────────────────────────

e2e_case_banner "Generate traceability matrix"

MATRIX_JSON="${E2E_ARTIFACT_DIR}/traceability_matrix.json"

python3 - "$MATRIX_JSON" <<'PYEOF'
import json, sys

dest = sys.argv[1]

# T16 deficiency tracks → implementation beads → validation beads → evidence
matrix = {
    "schema_version": 1,
    "epic": "br-1xt0m",
    "track": "br-1xt0m.1",
    "title": "T16: Showcase Parity Hardening",
    "deficiency_tracks": [
        {
            "id": "T16.1",
            "bead": "br-1xt0m.1.1",
            "title": "Navigation & Information Architecture Coherence",
            "impl_beads": ["br-1xt0m.1.1.1", "br-1xt0m.1.1.3"],
            "impl_descriptions": [
                "Registry-Driven Screen Count and Key-Hint Synchronization",
                "Full-Surface Screen Jump Semantics for 11+ Screens"
            ],
            "test_beads": ["br-1xt0m.1.13.6", "br-1xt0m.1.13.10"],
            "evidence": ["tests/e2e/test_stdio_workflows.sh", "crates/mcp-agent-mail-server/tests/tui_perf_baselines.rs"]
        },
        {
            "id": "T16.2",
            "bead": "br-1xt0m.1.2",
            "title": "Hit-Region Layering & Mouse Dispatch Unification",
            "impl_beads": ["br-1xt0m.1.2.1", "br-1xt0m.1.2.2", "br-1xt0m.1.2.3"],
            "impl_descriptions": [
                "Canonical Hit-Region IDs and Layer Classification API",
                "Central Mouse Dispatcher for Shell Interactions",
                "Pane Hit-Region Registration + Hover/Active Affordances"
            ],
            "test_beads": ["br-1xt0m.1.13.7", "br-1xt0m.1.13.12"],
            "evidence": ["tests/e2e/test_stdio_adaptive.sh"]
        },
        {
            "id": "T16.3",
            "bead": "br-1xt0m.1.3",
            "title": "Chrome Shell Hierarchy & Status Surface Redesign",
            "impl_beads": ["br-1xt0m.1.3.1", "br-1xt0m.1.3.2", "br-1xt0m.1.3.3"],
            "impl_descriptions": [
                "Tab Row Hierarchy and Active-State Contrast Pass",
                "Adaptive Status-Bar Semantic Truncation Engine",
                "Keycap/Action-Chip Help Strip Redesign"
            ],
            "test_beads": ["br-1xt0m.1.13.6", "br-1xt0m.1.13.9"],
            "evidence": ["crates/mcp-agent-mail-server/tests/golden_snapshots.rs"]
        },
        {
            "id": "T16.4",
            "bead": "br-1xt0m.1.4",
            "title": "Help/Overlay/Coach-Hint Surfaces",
            "impl_beads": ["br-1xt0m.1.4.1", "br-1xt0m.1.4.2", "br-1xt0m.1.4.3"],
            "impl_descriptions": [
                "Overlay Stack Policy and Escape-Precedence Contract",
                "Context-Aware Help Panels by Screen/Mode",
                "Non-Tour Contextual Coach Hints for First-Use Friction"
            ],
            "test_beads": ["br-1xt0m.1.13.6", "br-1xt0m.1.13.10"],
            "evidence": ["tests/e2e/test_stdio_workflows.sh"]
        },
        {
            "id": "T16.5",
            "bead": "br-1xt0m.1.5",
            "title": "Transition Motion & Animation",
            "impl_beads": ["br-1xt0m.1.5.1", "br-1xt0m.1.5.2", "br-1xt0m.1.5.3"],
            "impl_descriptions": [
                "Semantic Screen-Transition Redesign",
                "Reduced-Motion Toggle and Preference Persistence",
                "Focus/Selection Micro-Motion with Budget Guardrails"
            ],
            "test_beads": ["br-1xt0m.1.13.12", "br-1xt0m.1.13.14"],
            "evidence": ["tests/e2e/test_perf_regression.sh", "tests/e2e/test_stdio_adaptive.sh"]
        },
        {
            "id": "T16.6",
            "bead": "br-1xt0m.1.6",
            "title": "Action Menu Execution Integrity & Feedback UX",
            "impl_beads": ["br-1xt0m.1.6.1", "br-1xt0m.1.6.2", "br-1xt0m.1.6.3"],
            "impl_descriptions": [
                "Wire ActionKind::Execute to Real Dispatcher",
                "Wire ConfirmThenExecute Callback to Operation Dispatch",
                "Action Progress/Outcome Surfaces and Disabled-Reason UX"
            ],
            "test_beads": ["br-1xt0m.1.13.7", "br-1xt0m.1.13.15"],
            "evidence": ["tests/e2e/test_failure_injection.sh"]
        },
        {
            "id": "T16.7",
            "bead": "br-1xt0m.1.7",
            "title": "Typography & Color Semantics",
            "impl_beads": ["br-1xt0m.1.7.1", "br-1xt0m.1.7.2", "br-1xt0m.1.7.3"],
            "impl_descriptions": [
                "Semantic Typography Hierarchy Tokenization",
                "Semantic Color Rebalance for Information Priority",
                "Density Cleanup via Chunked Metadata Layout Patterns"
            ],
            "test_beads": ["br-1xt0m.1.13.8", "br-1xt0m.1.13.9"],
            "evidence": ["crates/mcp-agent-mail-server/tests/golden_snapshots.rs"]
        },
        {
            "id": "T16.8",
            "bead": "br-1xt0m.1.8",
            "title": "Dashboard KPI & Event Surfaces",
            "impl_beads": ["br-1xt0m.1.8.1", "br-1xt0m.1.8.2", "br-1xt0m.1.8.3"],
            "impl_descriptions": [
                "KPI Priority Recomposition and Panel Weighting",
                "Event Stream Salience and Change-Cue Refinement",
                "Trend/Anomaly Annotation + Narrow-Width Dashboard Tuning"
            ],
            "test_beads": ["br-1xt0m.1.13.8", "br-1xt0m.1.13.11"],
            "evidence": ["tests/e2e/test_stdio_screen_workflows.sh"]
        },
        {
            "id": "T16.9",
            "bead": "br-1xt0m.1.9",
            "title": "Messages & Threads Redesign",
            "impl_beads": ["br-1xt0m.1.9.1", "br-1xt0m.1.9.2", "br-1xt0m.1.9.3", "br-1xt0m.1.9.4"],
            "impl_descriptions": [
                "Complete Deferred Live-Results Integration in Messages",
                "Message Row Hierarchy + Unread/Importance Cue Redesign",
                "Threads Filter-Bar and Row-Chunking Redesign",
                "Thread Detail Narrative and In-Context Action Pass"
            ],
            "test_beads": ["br-1xt0m.1.13.8", "br-1xt0m.1.13.11"],
            "evidence": ["tests/e2e/test_stdio_screen_workflows.sh"]
        },
        {
            "id": "T16.10",
            "bead": "br-1xt0m.1.10",
            "title": "Search & Timeline Surfaces",
            "impl_beads": ["br-1xt0m.1.10.1", "br-1xt0m.1.10.2", "br-1xt0m.1.10.3", "br-1xt0m.1.10.4"],
            "impl_descriptions": [
                "Progressive-Disclosure Search Control Model",
                "Explicit Search Labels and Hinting (Abbreviation Reduction)",
                "Result/Inspector Hierarchy and Highlight Strategy Refinement",
                "Timeline Lane/Semantic Encoding and Inspector Readability Pass"
            ],
            "test_beads": ["br-1xt0m.1.13.8", "br-1xt0m.1.13.11"],
            "evidence": ["tests/e2e/test_stdio_screen_workflows.sh", "crates/mcp-agent-mail-server/tests/pty_e2e_search.rs"]
        },
        {
            "id": "T16.11",
            "bead": "br-1xt0m.1.11",
            "title": "SystemHealth & Responsive Layout",
            "impl_beads": ["br-1xt0m.1.11.1", "br-1xt0m.1.11.2", "br-1xt0m.1.11.3"],
            "impl_descriptions": [
                "Structured Diagnostic Sections for SystemHealth Text Mode",
                "Adaptive Width-Class Layout Policy for SystemHealth Dashboard",
                "Narrow-Width Fallback + Anomaly-First Prioritization"
            ],
            "test_beads": ["br-1xt0m.1.13.8", "br-1xt0m.1.13.12"],
            "evidence": ["tests/e2e/test_stdio_adaptive.sh"]
        },
        {
            "id": "T16.12",
            "bead": "br-1xt0m.1.12",
            "title": "Discoverability & Accessibility",
            "impl_beads": ["br-1xt0m.1.12.1", "br-1xt0m.1.12.2", "br-1xt0m.1.12.3", "br-1xt0m.1.12.4"],
            "impl_descriptions": [
                "Status-Surface Discoverability for A11y/Perf/Debug/Mouse State",
                "Auto-Synchronized Shortcut Documentation Pipeline",
                "Screen-Local Help Snippets and First-Use Hint Persistence",
                "Keyboard/Mouse Parity Audit and Remediation"
            ],
            "test_beads": ["br-1xt0m.1.13.6", "br-1xt0m.1.13.12"],
            "evidence": ["tests/e2e/test_stdio_adaptive.sh", "scripts/e2e_tui_a11y.sh"]
        }
    ],
    "validation_suites": [
        {"bead": "br-1xt0m.1.13.1", "type": "unit", "title": "Unit Tests for Navigation, Keymap, and Action Invariants"},
        {"bead": "br-1xt0m.1.13.2", "type": "snapshot", "title": "Snapshot Matrix for Chrome/Overlay/Screen Width Variants"},
        {"bead": "br-1xt0m.1.13.3", "type": "e2e-gate", "title": "E2E Workflow Suite for T16 Interaction Surfaces"},
        {"bead": "br-1xt0m.1.13.4", "type": "perf-gate", "title": "Performance Budget Enforcement for Render + Action Paths"},
        {"bead": "br-1xt0m.1.13.5", "type": "closeout", "title": "Before/After Evidence Bundle and Deficiency Closeout Checklist"},
        {"bead": "br-1xt0m.1.13.6", "type": "unit", "title": "Unit Matrix — Shell Navigation & Discoverability Contracts"},
        {"bead": "br-1xt0m.1.13.7", "type": "unit", "title": "Unit Matrix — Hit Regions, Dispatch Routing, and Action State Machines"},
        {"bead": "br-1xt0m.1.13.8", "type": "unit", "title": "Unit Matrix — Screen Logic, Density Heuristics, and Failure Paths"},
        {"bead": "br-1xt0m.1.13.9", "type": "snapshot", "title": "Snapshot Matrix — Width, Overlay, and Semantic Hierarchy Baselines"},
        {"bead": "br-1xt0m.1.13.10", "type": "e2e", "title": "E2E Script A — Shell Navigation, Overlays, and Action Execution"},
        {"bead": "br-1xt0m.1.13.11", "type": "e2e", "title": "E2E Script B — Dashboard + Messages/Threads + Search/Timeline + SystemHealth"},
        {"bead": "br-1xt0m.1.13.12", "type": "e2e", "title": "E2E Script C — Responsive Width Matrix, Reduced Motion, Mouse/Keyboard Parity"},
        {"bead": "br-1xt0m.1.13.13", "type": "harness", "title": "Test Logging Contract + Artifact Manifest for T16 Validation"},
        {"bead": "br-1xt0m.1.13.14", "type": "perf", "title": "Performance Regression Suite — Frame, Action Latency, and Memory Guardrails"},
        {"bead": "br-1xt0m.1.13.15", "type": "e2e", "title": "Failure-Injection E2E Suite for Degraded/Error UX Paths"},
        {"bead": "br-1xt0m.1.13.16", "type": "ci", "title": "Deterministic Test Orchestration and CI Gate Wiring for T16"},
        {"bead": "br-1xt0m.1.13.17", "type": "traceability", "title": "Traceability Matrix and Final Evidence Map"}
    ],
    "orchestration": {
        "entrypoint": "scripts/t16_validate.sh",
        "phases": ["build", "unit", "snapshot", "e2e", "perf", "evidence"]
    },
    "artifact_locations": {
        "perf_baselines": "tests/artifacts/tui/perf_baselines/*/summary.json",
        "soak_replay": "tests/artifacts/tui/soak_replay/*/report.json",
        "golden_snapshots": "crates/mcp-agent-mail-server/tests/snapshots/",
        "e2e_artifacts": "tests/artifacts/*/",
        "t16_evidence": "tests/artifacts/t16_validate/*/evidence.json"
    }
}

# Compute summary statistics
total_impl = sum(len(t["impl_beads"]) for t in matrix["deficiency_tracks"])
total_test = len(matrix["validation_suites"])
total_tracks = len(matrix["deficiency_tracks"])

matrix["summary"] = {
    "deficiency_tracks": total_tracks,
    "implementation_beads": total_impl,
    "validation_beads": total_test,
    "coverage": "all 12 tracks have both impl and test bead references"
}

with open(dest, "w") as f:
    json.dump(matrix, f, indent=2)
    f.write("\n")

print(f"Generated: {total_tracks} tracks, {total_impl} impl beads, {total_test} validation beads")
PYEOF
gen_rc=$?

if [ "$gen_rc" -eq 0 ] && [ -f "$MATRIX_JSON" ]; then
    e2e_pass "traceability matrix generated"
else
    e2e_fail "traceability matrix generation failed"
fi

# ── Case 2: Validate matrix structure ────────────────────────────────

e2e_case_banner "Matrix structure validation"

VALID_RESULT="$(python3 - "$MATRIX_JSON" <<'PYEOF'
import json, sys

with open(sys.argv[1]) as f:
    m = json.load(f)

errors = []

# Every track has impl_beads and test_beads
for track in m["deficiency_tracks"]:
    if not track.get("impl_beads"):
        errors.append(f"{track['id']}: missing impl_beads")
    if not track.get("test_beads"):
        errors.append(f"{track['id']}: missing test_beads")
    if not track.get("evidence"):
        errors.append(f"{track['id']}: missing evidence")

# Validation suites have required fields
for vs in m["validation_suites"]:
    for field in ("bead", "type", "title"):
        if field not in vs:
            errors.append(f"validation suite missing {field}")

# Orchestration entrypoint
if not m.get("orchestration", {}).get("entrypoint"):
    errors.append("missing orchestration entrypoint")

# Summary
if not m.get("summary"):
    errors.append("missing summary")

if errors:
    for e in errors:
        print(f"ERROR: {e}", file=sys.stderr)
    print("FAIL")
else:
    print("OK")
PYEOF
)"

if [ "$VALID_RESULT" = "OK" ]; then
    e2e_pass "matrix structure valid (all tracks have impl + test + evidence refs)"
else
    e2e_fail "matrix structure invalid"
fi

# ── Case 3: Evidence files exist ────────────────────────────────────

e2e_case_banner "Evidence file existence"

EVIDENCE_CHECK="$(python3 - "$MATRIX_JSON" "$PROJECT_ROOT" <<'PYEOF'
import json, sys, os

with open(sys.argv[1]) as f:
    m = json.load(f)
root = sys.argv[2]

# Collect all evidence paths referenced
evidence_paths = set()
for track in m["deficiency_tracks"]:
    for ep in track.get("evidence", []):
        evidence_paths.add(ep)

missing = []
found = 0
for ep in sorted(evidence_paths):
    full = os.path.join(root, ep)
    if os.path.exists(full):
        found += 1
    else:
        missing.append(ep)

if missing:
    for mp in missing:
        print(f"MISSING: {mp}", file=sys.stderr)
print(f"{found} {len(missing)}")
PYEOF
)"
read -r FOUND MISSING_COUNT <<< "$EVIDENCE_CHECK"

if [ "${MISSING_COUNT:-0}" -eq 0 ]; then
    e2e_pass "all ${FOUND} evidence files exist"
else
    e2e_fail "${MISSING_COUNT} evidence files missing (${FOUND} found)"
fi

# ── Case 4: Orchestration entrypoint exists ──────────────────────────

e2e_case_banner "Orchestration entrypoint"

ORCH="$(python3 -c "import json; print(json.load(open('$MATRIX_JSON'))['orchestration']['entrypoint'])")"
if [ -f "${PROJECT_ROOT}/${ORCH}" ] && [ -x "${PROJECT_ROOT}/${ORCH}" ]; then
    e2e_pass "orchestration entrypoint exists: ${ORCH}"
else
    e2e_fail "orchestration entrypoint missing: ${ORCH}"
fi

# ── Case 5: Cross-reference completeness ─────────────────────────────

e2e_case_banner "Cross-reference completeness"

XREF_RESULT="$(python3 - "$MATRIX_JSON" <<'PYEOF'
import json, sys

with open(sys.argv[1]) as f:
    m = json.load(f)

# All test beads referenced by tracks should appear in validation_suites
test_suite_beads = {vs["bead"] for vs in m["validation_suites"]}
track_test_beads = set()
for track in m["deficiency_tracks"]:
    track_test_beads.update(track.get("test_beads", []))

orphan = track_test_beads - test_suite_beads
if orphan:
    for b in sorted(orphan):
        print(f"ORPHAN: {b} referenced by track but not in validation_suites", file=sys.stderr)
    print(f"FAIL {len(orphan)}")
else:
    print(f"OK {len(track_test_beads)}")
PYEOF
)"

if [[ "$XREF_RESULT" == OK* ]]; then
    COUNT="${XREF_RESULT#OK }"
    e2e_pass "all ${COUNT} test bead cross-references resolve to validation suites"
else
    e2e_fail "orphan test bead references found"
fi

# ── Summary ──────────────────────────────────────────────────────────

e2e_save_artifact "env_dump.txt" "$(e2e_dump_env 2>&1)"
e2e_summary
