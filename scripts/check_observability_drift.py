#!/usr/bin/env python3
"""Detect drift between git 2.51 observability traces and schemas."""

from __future__ import annotations

import argparse
import json
import re
import shutil
import subprocess
import sys
from dataclasses import dataclass
from pathlib import Path
from typing import Any


DEFAULT_TARGET_PREFIXES = (
    "mcp_agent_mail::git_locked",
    "mcp_agent_mail::git_binary",
    "mcp_agent_mail::git_lock",
    "mcp_agent_mail::guard::segfault_retry",
    "mcp_agent_mail::tools::reservations::activity",
    "mcp_agent_mail::tools::reservations::pathspec",
    "mcp_agent_mail::doctor::fix_orphan_refs",
    "mcp_agent_mail::storage::archive::batch_write",
)

SKIP_DIR_NAMES = {".git", "target", "__pycache__"}
TRACE_MACRO_RE = re.compile(r"tracing::(?:trace|debug|info|warn|error)!\s*\(")
TARGET_RE = re.compile(r'target\s*:\s*"([^"]+)"')
STRING_RE = re.compile(r'"(?:\\.|[^"\\])*"')
FIELD_RE = re.compile(r"(?:^|[,\s])([A-Za-z_][A-Za-z0-9_]*)\s*=")
METRIC_ROW_RE = re.compile(r"^\|\s*`([^`]+)`\s*\|")
SCHEMA_ROW_RE = re.compile(r"`((?:docs/schemas/git_251/)?[^`]+\.schema\.json)`")
BASE_TOP_LEVEL_REQUIRED = ["ts", "level", "target", "name", "fields"]


@dataclass(frozen=True)
class TraceEvent:
    target: str
    name: str
    fields: frozenset[str]
    location: str


@dataclass(frozen=True)
class SchemaSpec:
    name: str
    path: Path
    events: frozenset[str]
    target_values: frozenset[str]
    drift_required_fields: frozenset[str]
    planned: bool

    def matches_target(self, target: str) -> bool:
        return not self.target_values or target in self.target_values


def decode_json_string(token: str) -> str:
    return json.loads(token)


def find_matching_paren(text: str, open_index: int) -> int | None:
    depth = 1
    in_string = False
    escaped = False
    index = open_index + 1
    while index < len(text):
        char = text[index]
        if in_string:
            if escaped:
                escaped = False
            elif char == "\\":
                escaped = True
            elif char == '"':
                in_string = False
        elif char == '"':
            in_string = True
        elif char == "(":
            depth += 1
        elif char == ")":
            depth -= 1
            if depth == 0:
                return index
        index += 1
    return None


def line_number(text: str, offset: int) -> int:
    return text.count("\n", 0, offset) + 1


def parse_trace_events(path: Path, text: str, prefixes: tuple[str, ...]) -> list[TraceEvent]:
    events: list[TraceEvent] = []
    for match in TRACE_MACRO_RE.finditer(text):
        open_index = match.end() - 1
        close_index = find_matching_paren(text, open_index)
        if close_index is None:
            continue
        body = text[open_index + 1 : close_index]
        target_match = TARGET_RE.search(body)
        if target_match is None:
            continue
        target = target_match.group(1)
        if not any(target.startswith(prefix) for prefix in prefixes):
            continue

        strings = list(STRING_RE.finditer(body))
        if not strings:
            continue
        event_name = decode_json_string(strings[-1].group(0))
        field_slice = body[: strings[-1].start()]
        fields = {
            field_match.group(1)
            for field_match in FIELD_RE.finditer(field_slice)
            if field_match.group(1) != "target"
        }
        events.append(
            TraceEvent(
                target=target,
                name=event_name,
                fields=frozenset(fields),
                location=f"{path}:{line_number(text, match.start())}",
            )
        )
    return events


def iter_rust_files_python(root: Path) -> list[Path]:
    if root.is_file():
        return [root] if root.suffix == ".rs" else []
    files: list[Path] = []
    for path in root.rglob("*.rs"):
        if any(part in SKIP_DIR_NAMES for part in path.parts):
            continue
        files.append(path)
    return sorted(files)


def run_discovery_command(command: list[str]) -> list[Path] | None:
    try:
        completed = subprocess.run(
            command,
            check=False,
            stdout=subprocess.PIPE,
            stderr=subprocess.DEVNULL,
            text=True,
        )
    except OSError:
        return None
    if completed.returncode not in (0, 1):
        return None
    return sorted({Path(line) for line in completed.stdout.splitlines() if line.strip()})


def iter_rust_files(root: Path) -> tuple[str, list[Path]]:
    if root.is_file():
        return "python", iter_rust_files_python(root)

    if shutil.which("ast-grep"):
        files = run_discovery_command(
            [
                "ast-grep",
                "run",
                "--lang",
                "rust",
                "--pattern",
                "tracing::$LEVEL!(target: $TARGET, $$$ARGS)",
                "--files-with-matches",
                str(root),
            ]
        )
        if files is not None:
            return "ast-grep", files

    if shutil.which("rg"):
        files = run_discovery_command(
            [
                "rg",
                "--files-with-matches",
                r"tracing::(trace|debug|info|warn|error)!\s*\(",
                "-g",
                "*.rs",
                str(root),
            ]
        )
        if files is not None:
            return "rg", files

    return "python", iter_rust_files_python(root)


def scan_trace_events(roots: list[Path], prefixes: tuple[str, ...]) -> list[TraceEvent]:
    events: list[TraceEvent] = []
    for root in roots:
        _, files = iter_rust_files(root)
        for path in files:
            try:
                text = path.read_text(encoding="utf-8")
            except UnicodeDecodeError:
                continue
            events.extend(parse_trace_events(path, text, prefixes))
    return sorted(events, key=lambda event: (event.name, event.location))


def scanner_name(roots: list[Path]) -> str:
    names = {iter_rust_files(root)[0] for root in roots}
    if len(names) == 1:
        return names.pop()
    return "+".join(sorted(names))


def schema_target_values(schema: dict[str, Any]) -> frozenset[str]:
    target = schema.get("properties", {}).get("target", {})
    if "const" in target:
        return frozenset([str(target["const"])])
    if "enum" in target:
        return frozenset(str(value) for value in target["enum"])
    return frozenset()


def schema_required_fields(schema: dict[str, Any]) -> frozenset[str]:
    if "x-drift-required-fields" in schema:
        return frozenset(str(field) for field in schema["x-drift-required-fields"])
    fields = schema.get("properties", {}).get("fields", {})
    return frozenset(str(field) for field in fields.get("required", []))


def load_schema_specs(schemas_dir: Path) -> list[SchemaSpec]:
    specs: list[SchemaSpec] = []
    for path in sorted(schemas_dir.glob("*.schema.json")):
        schema = json.loads(path.read_text(encoding="utf-8"))
        specs.append(
            SchemaSpec(
                name=path.name,
                path=path,
                events=frozenset(str(name) for name in schema.get("x-event-names", [])),
                target_values=schema_target_values(schema),
                drift_required_fields=schema_required_fields(schema),
                planned=bool(schema.get("x-planned", False)),
            )
        )
    return specs


def parse_spec_doc(spec_doc: Path) -> tuple[list[str], list[str]]:
    if not spec_doc.exists():
        return [], []
    metrics: list[str] = []
    schemas: list[str] = []
    seen_schemas: set[str] = set()
    for line in spec_doc.read_text(encoding="utf-8").splitlines():
        metric_match = METRIC_ROW_RE.match(line)
        if metric_match and metric_match.group(1).endswith(("_total", "_seconds", "_in_flight")):
            metrics.append(metric_match.group(1))
        for schema_path in SCHEMA_ROW_RE.findall(line):
            if schema_path.startswith("docs/schemas/git_251/"):
                normalized = schema_path
            else:
                normalized = f"docs/schemas/git_251/{schema_path}"
            if normalized not in seen_schemas:
                seen_schemas.add(normalized)
                schemas.append(normalized)
    return metrics, schemas


def finding(category: str, name: str, location: str, detail: str, **extra: Any) -> dict[str, Any]:
    out: dict[str, Any] = {
        "category": category,
        "name": name,
        "location": location,
        "detail": detail,
    }
    out.update(extra)
    return out


def detect_findings(
    events: list[TraceEvent],
    specs: list[SchemaSpec],
    spec_doc: Path,
    metrics: list[str],
    indexed_schemas: list[str],
) -> list[dict[str, Any]]:
    findings: list[dict[str, Any]] = []
    schema_by_event: dict[str, list[SchemaSpec]] = {}
    for spec in specs:
        for event_name in spec.events:
            schema_by_event.setdefault(event_name, []).append(spec)

    for event in events:
        candidates = [
            spec for spec in schema_by_event.get(event.name, []) if spec.matches_target(event.target)
        ]
        if not candidates:
            findings.append(
                finding(
                    "code_ahead",
                    event.name,
                    event.location,
                    f"{event.target} is emitted but is not covered by a git_251 schema",
                    target=event.target,
                    fields=sorted(event.fields),
                )
            )
            continue

        missing_by_schema = [
            (spec, sorted(spec.drift_required_fields - event.fields))
            for spec in candidates
            if spec.drift_required_fields
        ]
        if missing_by_schema and all(missing for _, missing in missing_by_schema):
            best_schema, missing = min(missing_by_schema, key=lambda item: len(item[1]))
            findings.append(
                finding(
                    "field_mismatch",
                    event.name,
                    event.location,
                    f"{best_schema.name} requires fields missing from the emitted event",
                    schema=best_schema.name,
                    missing_fields=missing,
                    emitted_fields=sorted(event.fields),
                    target=event.target,
                )
            )

    emitted_names = {event.name for event in events}
    reported_doc_ahead: set[tuple[str, str]] = set()
    for spec in specs:
        if spec.planned:
            continue
        for event_name in sorted(spec.events):
            key = (spec.name, event_name)
            if event_name not in emitted_names and key not in reported_doc_ahead:
                reported_doc_ahead.add(key)
                findings.append(
                    finding(
                        "doc_ahead",
                        event_name,
                        str(spec.path),
                        f"{spec.name} lists an event that was not found in scanned Rust code",
                        schema=spec.name,
                    )
                )

    schema_files = {f"docs/schemas/git_251/{spec.name}" for spec in specs}
    indexed_schema_set = set(indexed_schemas)
    for schema_path in sorted(indexed_schema_set - schema_files):
        findings.append(
            finding(
                "doc_ahead",
                Path(schema_path).name,
                str(spec_doc),
                "spec index references a schema file that does not exist",
                schema=schema_path,
            )
        )
    for schema_path in sorted(schema_files - indexed_schema_set):
        if indexed_schemas:
            findings.append(
                finding(
                    "code_ahead",
                    Path(schema_path).name,
                    schema_path,
                    "schema file exists but is not listed in the spec index",
                    schema=schema_path,
                )
            )

    if metrics and len(metrics) != 17:
        findings.append(
            finding(
                "doc_ahead",
                "metrics_catalog",
                str(spec_doc),
                f"expected 17 metric rows, found {len(metrics)}",
                metric_count=len(metrics),
            )
        )

    return sorted(findings, key=lambda item: (item["category"], item["name"], item["location"]))


def build_output(
    scan_roots: list[Path],
    schemas_dir: Path,
    spec_doc: Path,
    prefixes: tuple[str, ...],
) -> dict[str, Any]:
    events = scan_trace_events(scan_roots, prefixes)
    specs = load_schema_specs(schemas_dir)
    metrics, indexed_schemas = parse_spec_doc(spec_doc)
    findings = detect_findings(events, specs, spec_doc, metrics, indexed_schemas)
    return {
        "_meta": {
            "scanner": scanner_name(scan_roots),
            "scan_roots": [str(root) for root in scan_roots],
            "schemas_dir": str(schemas_dir),
            "spec_doc": str(spec_doc),
            "target_prefixes": list(prefixes),
            "events_scanned": len(events),
            "schema_count": len(specs),
            "metrics_catalog_count": len(metrics),
            "schema_index_count": len(indexed_schemas),
        },
        "findings": findings,
    }


def parse_args(argv: list[str]) -> argparse.Namespace:
    parser = argparse.ArgumentParser(
        description=(
            "Check git 2.51 observability tracing events against "
            "docs/OBSERVABILITY_git_251.md and docs/schemas/git_251."
        )
    )
    parser.add_argument(
        "--scan-root",
        action="append",
        default=None,
        help="Rust file or directory to scan. Repeatable. Defaults to ./crates.",
    )
    parser.add_argument(
        "--schemas-dir",
        default="docs/schemas/git_251",
        help="Directory containing git_251 JSON Schemas.",
    )
    parser.add_argument(
        "--spec-doc",
        default="docs/OBSERVABILITY_git_251.md",
        help="Observability spec markdown document.",
    )
    parser.add_argument(
        "--target-prefix",
        action="append",
        default=None,
        help="Tracing target prefix to include. Repeatable. Defaults to the git_251 target set.",
    )
    parser.add_argument("--pretty", action="store_true", help="Pretty-print JSON output.")
    return parser.parse_args(argv)


def main(argv: list[str] | None = None) -> int:
    args = parse_args(sys.argv[1:] if argv is None else argv)
    scan_roots = [Path(root) for root in (args.scan_root or ["crates"])]
    schemas_dir = Path(args.schemas_dir)
    spec_doc = Path(args.spec_doc)
    prefixes = tuple(args.target_prefix or DEFAULT_TARGET_PREFIXES)
    output = build_output(scan_roots, schemas_dir, spec_doc, prefixes)
    indent = 2 if args.pretty else None
    print(json.dumps(output, indent=indent, sort_keys=True))
    return 1 if output["findings"] else 0


if __name__ == "__main__":
    raise SystemExit(main())
