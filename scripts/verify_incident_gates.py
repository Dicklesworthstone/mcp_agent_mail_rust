#!/usr/bin/env python3
"""verify_incident_gates.py - Regression-category verification for br-2k3qx incident.

Reads oracle gate artifacts (gate_verdict.json, culprit_surface_map.json,
mismatch_diffs.json) and checks three specific regression categories:

  1. False-empty regressions   - threads/agents/projects cardinality mismatch
  2. Body placeholder regressions - dashboard/messages body_md empty/placeholder
  3. Auth workflow regressions - health URL auth ordering / unauthorized responses

Exit codes:
  0  No regressions detected in any category
  1  One or more regression categories triggered
  2  Gate artifacts missing or malformed (infrastructure failure)

Bead: br-2k3qx.5.5
"""
from __future__ import annotations

import json
import sys
from pathlib import Path

# ── Regression Category Definitions ──────────────────────────────────────

# Surface prefixes and check_id patterns that map to each regression category.
# A mismatch is flagged under a category if its surface or check_id contains
# any of the listed patterns (case-insensitive substring match).

FALSE_EMPTY_PATTERNS = [
    "db.cardinality:threads",
    "db.cardinality:agents",
    "db.cardinality:projects",
    "false_empty",
    "cardinality:thread",
    "cardinality:agent",
    "cardinality:project",
    "threads.count",
    "agents.count",
    "projects.count",
]

BODY_PLACEHOLDER_PATTERNS = [
    "body_md",
    "body.placeholder",
    "body_excerpt",
    "source_of_truth:message_body",
    "dashboard.body",
    "messages.body",
    "empty body",
    "placeholder",
]

AUTH_WORKFLOW_PATTERNS = [
    "auth.workflow",
    "health_url",
    "health_unauthorized",
    "route.ordering",
    "mail_special_route",
    "unauthorized",
    "bearer_auth",
]


def matches_category(check_id: str, surface: str, patterns: list[str]) -> bool:
    """Return True if check_id or surface matches any pattern (case-insensitive)."""
    combined = f"{check_id} {surface}".lower()
    return any(p.lower() in combined for p in patterns)


def classify_mismatches(
    surface_map: dict[str, list[dict]],
    diffs: list[dict],
    probe_report: dict | None = None,
) -> dict[str, list[dict]]:
    """Classify mismatches into the 3 regression categories.

    Uses two classification strategies:
    1. Explicit regression_class field from truth probe checks (preferred)
    2. Pattern-based classification from check_id/surface (fallback)
    """
    categories: dict[str, list[dict]] = {
        "false_empty": [],
        "body_placeholder": [],
        "auth_workflow": [],
    }

    # Strategy 1: Use explicit regression_class from probe report checks
    seen_check_ids: set[str] = set()
    if probe_report:
        for check in probe_report.get("checks", []):
            rc = check.get("regression_class")
            status = check.get("status", "")
            if rc and rc in categories and status in ("mismatch", "fail", "FAIL"):
                categories[rc].append(check)
                seen_check_ids.add(check.get("check_id", ""))

    # Strategy 2: Pattern-based fallback for checks without regression_class
    all_mismatches: list[dict] = []
    for surface, entries in surface_map.items():
        for entry in entries:
            entry_with_surface = {**entry, "surface": surface}
            all_mismatches.append(entry_with_surface)

    for diff in diffs:
        if diff.get("check_id") not in {m.get("check_id") for m in all_mismatches}:
            all_mismatches.append(diff)

    for m in all_mismatches:
        check_id = m.get("check_id", "")
        surface = m.get("surface", "")
        whitelisted = m.get("whitelisted", False)

        # Skip whitelisted mismatches and already-classified checks
        if whitelisted or check_id in seen_check_ids:
            continue

        if matches_category(check_id, surface, FALSE_EMPTY_PATTERNS):
            categories["false_empty"].append(m)
        if matches_category(check_id, surface, BODY_PLACEHOLDER_PATTERNS):
            categories["body_placeholder"].append(m)
        if matches_category(check_id, surface, AUTH_WORKFLOW_PATTERNS):
            categories["auth_workflow"].append(m)

    return categories


def main() -> int:
    if len(sys.argv) < 2:
        print("Usage: verify_incident_gates.py <output-dir>", file=sys.stderr)
        return 2

    output_dir = Path(sys.argv[1])

    # Load gate artifacts
    verdict_path = output_dir / "gate_verdict.json"
    surface_map_path = output_dir / "culprit_surface_map.json"
    diffs_path = output_dir / "mismatch_diffs.json"

    if not verdict_path.is_file():
        print(f"ERROR: gate_verdict.json not found at {verdict_path}", file=sys.stderr)
        return 2

    try:
        verdict = json.loads(verdict_path.read_text(encoding="utf-8"))
    except (json.JSONDecodeError, OSError) as exc:
        print(f"ERROR: failed to parse gate_verdict.json: {exc}", file=sys.stderr)
        return 2

    # Load surface map (may be empty if PASS)
    surface_map: dict[str, list[dict]] = {}
    if surface_map_path.is_file():
        try:
            surface_map = json.loads(surface_map_path.read_text(encoding="utf-8"))
        except (json.JSONDecodeError, OSError):
            pass

    # Load mismatch diffs (may be empty if PASS)
    diffs: list[dict] = []
    if diffs_path.is_file():
        try:
            diffs = json.loads(diffs_path.read_text(encoding="utf-8"))
        except (json.JSONDecodeError, OSError):
            pass

    # Load probe report (for explicit regression_class classification)
    probe_report: dict | None = None
    probe_report_path = output_dir / "probe" / "truth_probe_report.json"
    if probe_report_path.is_file():
        try:
            probe_report = json.loads(probe_report_path.read_text(encoding="utf-8"))
        except (json.JSONDecodeError, OSError):
            pass

    # If overall verdict is PASS, no regressions possible
    overall_verdict = verdict.get("verdict", "UNKNOWN")
    if overall_verdict == "PASS":
        print("Incident regression gate: PASS (no mismatches)")
        print(f"  Total checks: {verdict.get('total_checks', '?')}")
        print(f"  Passing: {verdict.get('passing', '?')}")
        return 0

    # If ERROR, infrastructure failure
    if overall_verdict == "ERROR":
        print(f"ERROR: oracle gate infrastructure failure: {verdict.get('reason', 'unknown')}", file=sys.stderr)
        return 2

    # Classify mismatches into 3 categories (using both probe report and pattern matching)
    categories = classify_mismatches(surface_map, diffs, probe_report)

    # Report
    print("=" * 70)
    print("  INCIDENT REGRESSION GATE VERIFICATION (br-2k3qx.5.5)")
    print("=" * 70)
    print(f"  Overall oracle verdict: {overall_verdict}")
    print(f"  Total checks: {verdict.get('total_checks', '?')}")
    print(f"  Passing: {verdict.get('passing', '?')}")
    print(f"  Mismatches: {verdict.get('mismatches', '?')}")
    print(f"  Whitelisted: {verdict.get('whitelisted_mismatches', '?')}")
    print(f"  Non-whitelisted: {verdict.get('non_whitelisted_mismatches', '?')}")
    print()

    category_labels = {
        "false_empty": "FALSE-EMPTY REGRESSIONS (threads/agents/projects cardinality)",
        "body_placeholder": "BODY PLACEHOLDER REGRESSIONS (dashboard/messages body_md)",
        "auth_workflow": "AUTH WORKFLOW REGRESSIONS (health URL / route ordering)",
    }

    failed_categories: list[str] = []

    for cat_key, cat_label in category_labels.items():
        hits = categories[cat_key]
        status = "FAIL" if hits else "PASS"
        if hits:
            failed_categories.append(cat_key)

        print(f"  [{status}] {cat_label}")
        if hits:
            for h in hits[:5]:
                cid = h.get("check_id", "?")
                exp = h.get("expected", "?")
                obs = h.get("observed", "?")
                print(f"         - {cid}: expected={exp} observed={obs}")
            if len(hits) > 5:
                print(f"         ... and {len(hits) - 5} more")
        print()

    print("=" * 70)

    if failed_categories:
        print(
            f"  RESULT: FAIL ({len(failed_categories)} regression "
            f"{'category' if len(failed_categories) == 1 else 'categories'} triggered)",
        )
        print(f"  Failed: {', '.join(failed_categories)}")
        print("=" * 70)

        # Write verification result for downstream consumption
        result_path = output_dir / "incident_gate_verification.json"
        result = {
            "bead_id": "br-2k3qx.5.5",
            "result": "FAIL",
            "failed_categories": failed_categories,
            "category_details": {
                k: [{"check_id": m.get("check_id"), "surface": m.get("surface")} for m in v]
                for k, v in categories.items()
                if v
            },
        }
        result_path.write_text(json.dumps(result, indent=2, sort_keys=True) + "\n", encoding="utf-8")

        return 1

    print("  RESULT: PASS (no incident regressions detected)")
    print("=" * 70)

    # Write verification result
    result_path = output_dir / "incident_gate_verification.json"
    result = {
        "bead_id": "br-2k3qx.5.5",
        "result": "PASS",
        "failed_categories": [],
        "category_details": {},
    }
    result_path.write_text(json.dumps(result, indent=2, sort_keys=True) + "\n", encoding="utf-8")

    return 0


if __name__ == "__main__":
    raise SystemExit(main())
