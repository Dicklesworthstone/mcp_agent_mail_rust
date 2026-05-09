#!/usr/bin/env python3
"""Unit tests for check_observability_drift.py."""

from __future__ import annotations

import contextlib
import importlib.util
import io
import json
import sys
import tempfile
import unittest
from pathlib import Path


SCRIPT_PATH = Path(__file__).resolve().parents[1] / "check_observability_drift.py"
SPEC = importlib.util.spec_from_file_location("check_observability_drift", SCRIPT_PATH)
assert SPEC is not None and SPEC.loader is not None
DRIFT = importlib.util.module_from_spec(SPEC)
sys.modules[SPEC.name] = DRIFT
SPEC.loader.exec_module(DRIFT)


def write_schema(schemas_dir: Path, events: list[str], required_fields: list[str]) -> None:
    schemas_dir.mkdir(parents=True, exist_ok=True)
    field_properties = {
        field: {"type": "string"}
        for field in sorted(set(required_fields + ["repo_slug", "caller", "duration_ms"]))
    }
    schema = {
        "$schema": "https://json-schema.org/draft/2020-12/schema",
        "$id": "https://schemas.mcp-agent-mail/git_251/unit.schema.json",
        "title": "unit event",
        "description": "Unit-test schema.",
        "type": "object",
        "additionalProperties": False,
        "required": ["ts", "level", "target", "name", "fields"],
        "x-event-names": events,
        "properties": {
            "ts": {"type": "string"},
            "level": {"type": "string"},
            "target": {"type": "string"},
            "name": {"type": "string"},
            "fields": {
                "type": "object",
                "additionalProperties": False,
                "required": required_fields,
                "properties": field_properties,
            },
        },
    }
    (schemas_dir / "unit.schema.json").write_text(json.dumps(schema), encoding="utf-8")


def write_code(scan_root: Path, body: str) -> None:
    src = scan_root / "src"
    src.mkdir(parents=True, exist_ok=True)
    (src / "lib.rs").write_text(body, encoding="utf-8")


class ObservabilityDriftTests(unittest.TestCase):
    def run_fixture(
        self, events: list[str], required_fields: list[str], code: str
    ) -> dict[str, object]:
        with tempfile.TemporaryDirectory() as temp:
            root = Path(temp)
            schemas_dir = root / "docs" / "schemas" / "git_251"
            write_schema(schemas_dir, events, required_fields)
            write_code(root, code)
            return DRIFT.build_output(
                [root],
                schemas_dir,
                root / "docs" / "OBSERVABILITY_git_251.md",
                ("mcp_agent_mail::git_locked",),
            )

    def categories(self, output: dict[str, object]) -> list[str]:
        findings = output["findings"]
        assert isinstance(findings, list)
        return [str(finding["category"]) for finding in findings]

    def test_drift_no_drift_returns_zero_findings(self) -> None:
        output = self.run_fixture(
            ["git_locked_exit_ok"],
            ["caller"],
            '''
pub fn emit() {
    tracing::info!(
        target: "mcp_agent_mail::git_locked",
        caller = "unit_test",
        "git_locked_exit_ok"
    );
}
''',
        )
        self.assertEqual([], output["findings"])

    def test_drift_code_ahead_emits_finding(self) -> None:
        output = self.run_fixture(
            ["git_locked_exit_ok"],
            [],
            '''
pub fn emit() {
    tracing::warn!(
        target: "mcp_agent_mail::git_locked",
        caller = "unit_test",
        "git_new_event"
    );
}
''',
        )
        self.assertIn("code_ahead", self.categories(output))

    def test_drift_doc_ahead_emits_finding(self) -> None:
        output = self.run_fixture(
            ["git_old_event"],
            [],
            '''
pub fn emit() {
}
''',
        )
        self.assertIn("doc_ahead", self.categories(output))

    def test_drift_field_mismatch_emits_finding(self) -> None:
        output = self.run_fixture(
            ["git_locked_exit_ok"],
            ["repo_slug", "caller"],
            '''
pub fn emit() {
    tracing::info!(
        target: "mcp_agent_mail::git_locked",
        repo_slug = "repo",
        "git_locked_exit_ok"
    );
}
''',
        )
        self.assertIn("field_mismatch", self.categories(output))

    def test_drift_ignores_out_of_scope_targets(self) -> None:
        output = self.run_fixture(
            [],
            [],
            '''
pub fn emit() {
    tracing::info!(
        target: "tests::observability",
        caller = "unit_test",
        "git_new_event"
    );
}
''',
        )
        self.assertEqual([], output["findings"])

    def test_main_emits_json_envelope_and_exit_status(self) -> None:
        with tempfile.TemporaryDirectory() as temp:
            root = Path(temp)
            schemas_dir = root / "docs" / "schemas" / "git_251"
            write_schema(schemas_dir, ["git_old_event"], [])
            write_code(root, "pub fn emit() {}\n")
            stdout = io.StringIO()
            with contextlib.redirect_stdout(stdout):
                exit_code = DRIFT.main(
                    [
                        "--scan-root",
                        str(root),
                        "--schemas-dir",
                        str(schemas_dir),
                        "--spec-doc",
                        str(root / "missing.md"),
                        "--target-prefix",
                        "mcp_agent_mail::git_locked",
                    ]
                )
            payload = json.loads(stdout.getvalue())
            self.assertEqual(1, exit_code)
            self.assertIn("_meta", payload)
            self.assertIn("findings", payload)


if __name__ == "__main__":
    unittest.main()
