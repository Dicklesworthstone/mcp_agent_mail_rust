#!/usr/bin/env bash
set -euo pipefail

usage() {
  cat <<'EOF'
Usage: scripts/regen_python_parity_fixtures.sh [options]

Regenerate Python-parity conformance fixtures against the upstream Python
reference implementation, emit a scenario catalog, and write a review report.

Options:
  --upstream-url URL        Python reference repo URL
                            default: https://github.com/Dicklesworthstone/mcp_agent_mail.git
  --upstream-ref REF        Branch, tag, or SHA to check out
                            default: main
  --upstream-dir DIR        Reuse an existing clone instead of creating a temp clone
  --python BIN              Python interpreter used to create the venv
                            default: python3
  --work-dir DIR            Keep all temp outputs under DIR instead of mktemp
  --output PATH             Generated fixture output path
                            default: temp path under the work dir
  --scenario-catalog PATH   Generated scenario catalog output path
                            default: temp path under the work dir
  --report PATH             Generated markdown review report path
                            default: temp path under the work dir
  --fixture-repo-root DIR   Scratch git repo root for generator-created fixture repos
                            default: <work-dir>/fixture-repos
  --scratch-root DIR        Scratch root for generator-created archive/db state
                            default: <work-dir>/scratch
  --apply                   Copy outputs into the tracked repo paths when fixture/catalog drift exists
  --keep-work-dir           Accepted for compatibility; temp work dirs are retained by default
  --dry-run                 Print the plan without cloning/installing/running the generator
  --help                    Show this help

Tracked outputs written by --apply:
  crates/mcp-agent-mail-conformance/tests/conformance/fixtures/python_reference.json
  crates/mcp-agent-mail-conformance/tests/conformance/fixtures/scenario_catalog.json
  crates/mcp-agent-mail-conformance/tests/conformance/fixtures/python_reference_regen_report.md
EOF
}

die() {
  printf 'regen-python-parity-fixtures: %s\n' "$*" >&2
  exit 1
}

need_cmd() {
  if ! command -v "$1" >/dev/null 2>&1; then
    die "required command not found: $1"
  fi
}

note() {
  printf '[fixture-regen] %s\n' "$*" >&2
}

repo_root="$(git rev-parse --show-toplevel 2>/dev/null || pwd)"
tracked_fixture_rel="crates/mcp-agent-mail-conformance/tests/conformance/fixtures/python_reference.json"
tracked_catalog_rel="crates/mcp-agent-mail-conformance/tests/conformance/fixtures/scenario_catalog.json"
tracked_report_rel="crates/mcp-agent-mail-conformance/tests/conformance/fixtures/python_reference_regen_report.md"
generator_script_rel="crates/mcp-agent-mail-conformance/tests/conformance/python_reference/generate_fixtures.py"

tracked_fixture="${repo_root}/${tracked_fixture_rel}"
tracked_catalog="${repo_root}/${tracked_catalog_rel}"
tracked_report="${repo_root}/${tracked_report_rel}"
generator_script="${repo_root}/${generator_script_rel}"

upstream_url="https://github.com/Dicklesworthstone/mcp_agent_mail.git"
upstream_ref="main"
upstream_dir=""
python_bin="python3"
work_dir=""
output_path=""
scenario_catalog_path=""
report_path=""
fixture_repo_root=""
scratch_root=""
apply_changes=0
dry_run=0

while (($# > 0)); do
  case "$1" in
    --upstream-url)
      upstream_url="${2:?missing value for --upstream-url}"
      shift 2
      ;;
    --upstream-ref)
      upstream_ref="${2:?missing value for --upstream-ref}"
      shift 2
      ;;
    --upstream-dir)
      upstream_dir="${2:?missing value for --upstream-dir}"
      shift 2
      ;;
    --python)
      python_bin="${2:?missing value for --python}"
      shift 2
      ;;
    --work-dir)
      work_dir="${2:?missing value for --work-dir}"
      shift 2
      ;;
    --output)
      output_path="${2:?missing value for --output}"
      shift 2
      ;;
    --scenario-catalog)
      scenario_catalog_path="${2:?missing value for --scenario-catalog}"
      shift 2
      ;;
    --report)
      report_path="${2:?missing value for --report}"
      shift 2
      ;;
    --fixture-repo-root)
      fixture_repo_root="${2:?missing value for --fixture-repo-root}"
      shift 2
      ;;
    --scratch-root)
      scratch_root="${2:?missing value for --scratch-root}"
      shift 2
      ;;
    --apply)
      apply_changes=1
      shift
      ;;
    --keep-work-dir)
      shift
      ;;
    --dry-run)
      dry_run=1
      shift
      ;;
    --help|-h)
      usage
      exit 0
      ;;
    *)
      die "unknown argument: $1"
      ;;
  esac
done

need_cmd git
need_cmd "${python_bin}"

if [[ ! -f "${generator_script}" ]]; then
  die "generator script not found at ${generator_script}"
fi
if [[ ! -f "${tracked_fixture}" ]]; then
  die "tracked fixture not found at ${tracked_fixture}"
fi

if [[ -z "${work_dir}" ]]; then
  work_dir="$(mktemp -d "${TMPDIR:-/tmp}/am-python-parity-regen.XXXXXX")"
else
  mkdir -p "${work_dir}"
fi

if [[ -z "${upstream_dir}" ]]; then
  upstream_dir="${work_dir}/upstream_python_reference"
fi
if [[ -z "${output_path}" ]]; then
  output_path="${work_dir}/python_reference.generated.json"
fi
if [[ -z "${scenario_catalog_path}" ]]; then
  scenario_catalog_path="${work_dir}/scenario_catalog.generated.json"
fi
if [[ -z "${report_path}" ]]; then
  report_path="${work_dir}/python_reference_regen_report.md"
fi
if [[ -z "${fixture_repo_root}" ]]; then
  fixture_repo_root="${work_dir}/fixture-repos"
fi
if [[ -z "${scratch_root}" ]]; then
  scratch_root="${work_dir}/scratch"
fi

venv_dir="${work_dir}/venv"
venv_python="${venv_dir}/bin/python"

note "repo root: ${repo_root}"
note "upstream url: ${upstream_url}"
note "upstream ref: ${upstream_ref}"
note "upstream dir: ${upstream_dir}"
note "generated fixture: ${output_path}"
note "scenario catalog: ${scenario_catalog_path}"
note "report: ${report_path}"
note "fixture repo root: ${fixture_repo_root}"
note "scratch root: ${scratch_root}"
note "apply tracked outputs: ${apply_changes}"

if [[ "${dry_run}" -eq 1 ]]; then
  note "dry-run requested; exiting before clone/install/generate"
  exit 0
fi

mkdir -p "$(dirname "${output_path}")" "$(dirname "${scenario_catalog_path}")" "$(dirname "${report_path}")"
mkdir -p "${fixture_repo_root}" "${scratch_root}"

update_upstream_clone() {
  if [[ -d "${upstream_dir}/.git" ]]; then
    if ! git -C "${upstream_dir}" diff --quiet || ! git -C "${upstream_dir}" diff --cached --quiet; then
      die "upstream dir ${upstream_dir} has local changes; refuse to overwrite"
    fi
    note "refreshing existing upstream clone"
    git -C "${upstream_dir}" fetch --depth 1 origin "${upstream_ref}"
    git -C "${upstream_dir}" switch --detach FETCH_HEAD >/dev/null 2>&1
  elif [[ -e "${upstream_dir}" ]]; then
    die "upstream dir exists but is not a git clone: ${upstream_dir}"
  else
    note "cloning upstream reference repo"
    git clone --depth 1 "${upstream_url}" "${upstream_dir}" >/dev/null
    git -C "${upstream_dir}" fetch --depth 1 origin "${upstream_ref}"
    git -C "${upstream_dir}" switch --detach FETCH_HEAD >/dev/null 2>&1
  fi
}

create_or_refresh_venv() {
  note "creating isolated Python environment"
  if [[ -e "${venv_dir}" ]]; then
    die "venv dir already exists; pass a fresh --work-dir or remove it manually: ${venv_dir}"
  fi
  "${python_bin}" -m venv "${venv_dir}"
  "${venv_python}" -m pip install --upgrade pip setuptools wheel >/dev/null
  note "installing upstream Python reference package"
  "${venv_python}" -m pip install -e "${upstream_dir}" >/dev/null
}

generate_fixture() {
  local upstream_sha
  upstream_sha="$(git -C "${upstream_dir}" rev-parse HEAD)"
  note "running generator against upstream ${upstream_sha}"
  PYTHONPATH="${upstream_dir}/src${PYTHONPATH:+:${PYTHONPATH}}" \
  AM_FIXTURE_REPO_ROOT="${fixture_repo_root}" \
  MCP_AGENT_MAIL_CONFORMANCE_SCRATCH_ROOT="${scratch_root}" \
  MCP_AGENT_MAIL_CONFORMANCE_FIXTURE_PATH="${output_path}" \
  "${venv_python}" "${generator_script}"
  note "generated fixture at ${output_path}"
}

write_catalog() {
  "${venv_python}" - "${output_path}" "${scenario_catalog_path}" <<'PY'
from __future__ import annotations

import json
import sys
from pathlib import Path

fixture_path = Path(sys.argv[1])
catalog_path = Path(sys.argv[2])

fixture = json.loads(fixture_path.read_text(encoding="utf-8"))

catalog = {
    "source_fixture": fixture_path.name,
    "fixture_version": fixture.get("version", ""),
    "generated_at": fixture.get("generated_at", ""),
    "tools": {},
    "resources": {},
}

for tool_name, entry in sorted(fixture.get("tools", {}).items()):
    catalog["tools"][tool_name] = [
        {
            "name": case.get("name", ""),
            "input": case.get("input", {}),
        }
        for case in entry.get("cases", [])
    ]

for uri, entry in sorted(fixture.get("resources", {}).items()):
    catalog["resources"][uri] = [
        {
            "name": case.get("name", ""),
            "input": case.get("input", {}),
        }
        for case in entry.get("cases", [])
    ]

catalog_path.write_text(json.dumps(catalog, indent=2, sort_keys=True) + "\n", encoding="utf-8")
PY
  note "wrote scenario catalog to ${scenario_catalog_path}"
}

write_report() {
  "${venv_python}" - "${tracked_fixture}" "${output_path}" "${report_path}" "${upstream_url}" "${upstream_ref}" "${upstream_dir}" <<'PY'
from __future__ import annotations

import json
import sys
from dataclasses import dataclass
from pathlib import Path
from typing import Any

tracked_fixture_path = Path(sys.argv[1])
generated_fixture_path = Path(sys.argv[2])
report_path = Path(sys.argv[3])
upstream_url = sys.argv[4]
upstream_ref = sys.argv[5]
upstream_dir = Path(sys.argv[6])


def load_json(path: Path) -> Any:
    return json.loads(path.read_text(encoding="utf-8"))


def json_kind(value: Any) -> str:
    if value is None:
        return "null"
    if isinstance(value, bool):
        return "bool"
    if isinstance(value, int) and not isinstance(value, bool):
        return "int"
    if isinstance(value, float):
        return "float"
    if isinstance(value, str):
        return "str"
    if isinstance(value, list):
        return "list"
    if isinstance(value, dict):
        return "dict"
    return type(value).__name__


def flatten_kinds(value: Any, path: str = "") -> dict[str, str]:
    result: dict[str, str] = {}
    result[path or "/"] = json_kind(value)
    if isinstance(value, dict):
        for key, child in value.items():
            child_path = f"{path}/{key}" if path else f"/{key}"
            result.update(flatten_kinds(child, child_path))
    elif isinstance(value, list):
        for index, child in enumerate(value):
            child_path = f"{path}/{index}" if path else f"/{index}"
            result.update(flatten_kinds(child, child_path))
    return result


def flatten_values(value: Any, path: str = "") -> dict[str, str]:
    result: dict[str, str] = {}
    if isinstance(value, dict):
        for key, child in value.items():
            child_path = f"{path}/{key}" if path else f"/{key}"
            result.update(flatten_values(child, child_path))
    elif isinstance(value, list):
        for index, child in enumerate(value):
            child_path = f"{path}/{index}" if path else f"/{index}"
            result.update(flatten_values(child, child_path))
    else:
        result[path or "/"] = repr(value)
    return result


def case_index(section: dict[str, Any], *, surface: str) -> dict[tuple[str, str], dict[str, Any]]:
    indexed: dict[tuple[str, str], dict[str, Any]] = {}
    for key, entry in section.items():
        for case in entry.get("cases", []):
            indexed[(key, case.get("name", ""))] = {
                "surface": surface,
                "key": key,
                "case_name": case.get("name", ""),
                "input": case.get("input", {}),
                "expect": case.get("expect", {}),
            }
    return indexed


def classify_case(old_case: dict[str, Any], new_case: dict[str, Any]) -> tuple[str, str]:
    old_expect = old_case["expect"]
    new_expect = new_case["expect"]

    if old_expect == new_expect:
        return ("unchanged", "no fixture payload delta")

    old_err = old_expect.get("err")
    new_err = new_expect.get("err")
    if old_err is not None or new_err is not None:
        if old_err == new_err:
            return ("unchanged", "error expectation unchanged")
        return ("needs-review", "error expectation changed")

    old_ok = old_expect.get("ok")
    new_ok = new_expect.get("ok")
    old_kinds = flatten_kinds(old_ok)
    new_kinds = flatten_kinds(new_ok)

    old_paths = set(old_kinds)
    new_paths = set(new_kinds)
    added_paths = sorted(new_paths - old_paths)
    removed_paths = sorted(old_paths - new_paths)
    type_changes = sorted(
        path for path in (old_paths & new_paths) if old_kinds[path] != new_kinds[path]
    )

    if removed_paths or type_changes:
        notes = []
        if removed_paths:
            notes.append(f"removed paths: {', '.join(removed_paths[:4])}")
        if type_changes:
            notes.append(f"type changes: {', '.join(type_changes[:4])}")
        return ("blocker", "; ".join(notes))

    if added_paths:
        return ("trivial", f"added paths: {', '.join(added_paths[:4])}")

    old_values = flatten_values(old_ok)
    new_values = flatten_values(new_ok)
    changed_values = sorted(
        path for path in (set(old_values) & set(new_values)) if old_values[path] != new_values[path]
    )
    if changed_values:
        return ("needs-review", f"value-only changes: {', '.join(changed_values[:4])}")

    return ("needs-review", "shape unchanged but payload differs")


@dataclass
class Row:
    surface: str
    key: str
    case_name: str
    classification: str
    note: str


tracked = load_json(tracked_fixture_path)
generated = load_json(generated_fixture_path)
indexed_tracked = {
    **case_index(tracked.get("tools", {}), surface="tool"),
    **case_index(tracked.get("resources", {}), surface="resource"),
}
indexed_generated = {
    **case_index(generated.get("tools", {}), surface="tool"),
    **case_index(generated.get("resources", {}), surface="resource"),
}

rows: list[Row] = []
for key in sorted(set(indexed_tracked) | set(indexed_generated)):
    old_case = indexed_tracked.get(key)
    new_case = indexed_generated.get(key)
    if old_case is None and new_case is not None:
        rows.append(
            Row(
                surface=new_case["surface"],
                key=new_case["key"],
                case_name=new_case["case_name"],
                classification="needs-review",
                note="new fixture case added upstream",
            )
        )
        continue
    if old_case is not None and new_case is None:
        rows.append(
            Row(
                surface=old_case["surface"],
                key=old_case["key"],
                case_name=old_case["case_name"],
                classification="blocker",
                note="fixture case removed upstream",
            )
        )
        continue
    assert old_case is not None and new_case is not None
    classification, note = classify_case(old_case, new_case)
    if classification != "unchanged":
        rows.append(
            Row(
                surface=new_case["surface"],
                key=new_case["key"],
                case_name=new_case["case_name"],
                classification=classification,
                note=note,
            )
        )

counts = {
    "trivial": sum(1 for row in rows if row.classification == "trivial"),
    "needs-review": sum(1 for row in rows if row.classification == "needs-review"),
    "blocker": sum(1 for row in rows if row.classification == "blocker"),
}

try:
    upstream_sha = (
        Path(upstream_dir / ".git")
        and __import__("subprocess")
        .run(
            ["git", "-C", str(upstream_dir), "rev-parse", "HEAD"],
            check=True,
            capture_output=True,
            text=True,
        )
        .stdout.strip()
    )
except Exception:
    upstream_sha = "unknown"

lines = [
    "# Python Parity Fixture Regeneration Report",
    "",
    "## Inputs",
    f"- Upstream repo: `{upstream_url}`",
    f"- Upstream ref: `{upstream_ref}`",
    f"- Upstream commit: `{upstream_sha}`",
    f"- Tracked fixture: `{tracked_fixture_path}`",
    f"- Generated fixture: `{generated_fixture_path}`",
    "",
    "## Summary",
    f"- Tool fixtures in tracked baseline: {len(tracked.get('tools', {}))}",
    f"- Tool fixtures in generated run: {len(generated.get('tools', {}))}",
    f"- Resource fixtures in tracked baseline: {len(tracked.get('resources', {}))}",
    f"- Resource fixtures in generated run: {len(generated.get('resources', {}))}",
    f"- Trivial deltas: {counts['trivial']}",
    f"- Needs-review deltas: {counts['needs-review']}",
    f"- Blocker deltas: {counts['blocker']}",
    "",
    "## Review Heuristics",
    "- `trivial`: additive shape change only (new field/path, no removals or type changes)",
    "- `needs-review`: value-only drift, new case, or error-format change",
    "- `blocker`: removed case, removed field/path, or type change",
    "",
]

if not rows:
    lines.extend(
        [
            "## Verdict",
            "No fixture drift detected against the tracked baseline.",
            "",
        ]
    )
else:
    lines.extend(
        [
            "## Drift Table",
            "| Surface | Key | Case | Classification | Note |",
            "|---|---|---|---|---|",
        ]
    )
    for row in rows:
        lines.append(
            f"| {row.surface} | `{row.key}` | `{row.case_name}` | `{row.classification}` | {row.note} |"
        )
    lines.append("")

report_path.write_text("\n".join(lines) + "\n", encoding="utf-8")
PY
  note "wrote review report to ${report_path}"
}

files_differ() {
  local left="$1"
  local right="$2"
  if [[ ! -f "${left}" || ! -f "${right}" ]]; then
    return 0
  fi
  if ! cmp -s "${left}" "${right}"; then
    return 0
  fi
  return 1
}

update_upstream_clone
create_or_refresh_venv
generate_fixture
write_catalog
write_report

needs_update=0
if files_differ "${tracked_fixture}" "${output_path}"; then
  needs_update=1
fi
if files_differ "${tracked_catalog}" "${scenario_catalog_path}"; then
  needs_update=1
fi

if [[ "${apply_changes}" -eq 1 && "${needs_update}" -eq 1 ]]; then
  note "applying updated tracked outputs"
  cp "${output_path}" "${tracked_fixture}"
  cp "${scenario_catalog_path}" "${tracked_catalog}"
  cp "${report_path}" "${tracked_report}"
elif [[ "${apply_changes}" -eq 1 ]]; then
  note "no fixture/catalog drift; leaving tracked files unchanged"
fi

printf 'needs_update=%s\n' "${needs_update}"
printf 'generated_fixture=%s\n' "${output_path}"
printf 'scenario_catalog=%s\n' "${scenario_catalog_path}"
printf 'report=%s\n' "${report_path}"
printf 'tracked_fixture=%s\n' "${tracked_fixture}"
printf 'tracked_catalog=%s\n' "${tracked_catalog}"
printf 'tracked_report=%s\n' "${tracked_report}"
printf 'work_dir=%s\n' "${work_dir}"
