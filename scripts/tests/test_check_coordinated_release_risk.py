#!/usr/bin/env python3

from __future__ import annotations

import importlib.util
import pathlib
import sys
import unittest
import json

SCRIPT = pathlib.Path(__file__).resolve().parents[1] / "check_coordinated_release_risk.py"
SPEC = importlib.util.spec_from_file_location("release_risk", SCRIPT)
assert SPEC and SPEC.loader
monitor = importlib.util.module_from_spec(SPEC)
sys.modules[SPEC.name] = monitor
SPEC.loader.exec_module(monitor)


class RequirementTests(unittest.TestCase):
    def test_exact_and_pre_one_minor_boundary(self):
        self.assertTrue(monitor.accepts("0.4.9", "=0.4.9"))
        self.assertFalse(monitor.accepts("0.4.10", "=0.4.9"))
        self.assertTrue(monitor.accepts("0.7.9", "0.7.1"))
        self.assertFalse(monitor.accepts("0.8.1", "0.7.1"))

    def test_ranges_tilde_and_wildcard(self):
        self.assertTrue(monitor.accepts("0.4.10", ">=0.4.4, <0.5"))
        self.assertFalse(monitor.accepts("0.5.0", ">=0.4.4, <0.5"))
        self.assertTrue(monitor.accepts("1.2.9", "~1.2.3"))
        self.assertFalse(monitor.accepts("1.3.0", "~1.2.3"))
        self.assertTrue(monitor.accepts("0.5.8", "0.5.*"))
        self.assertFalse(monitor.accepts("0.6.0", "0.5.*"))


class ClassificationTests(unittest.TestCase):
    def test_release_checks_prioritize_code_not_docs_or_perf(self):
        self.assertTrue(monitor.critical("Test Suite (ubuntu-latest)"))
        self.assertTrue(monitor.critical("cargo check (windows-latest)"))
        self.assertFalse(monitor.critical("Cloudflare Pages"))
        self.assertFalse(monitor.critical("Performance benchmark"))

    def test_monitor_does_not_recursively_block_itself(self):
        self.assertFalse(monitor.critical("Assess coordinated release graph"))
        self.assertFalse(monitor.critical("Coordinated release risk / monitor"))


class FakeGitHub:
    def __init__(self, manifests=None, checks=None, pin_exists=True, issues=None):
        self.manifests = manifests or {}
        self.check_data = checks or {}
        self.pin_exists = pin_exists
        self.issues = list(issues or [])
        self.created = []
        self.updated = []
        self.comments = []

    def head(self, repo):
        return (repo.rsplit("/", 1)[1][0] * 40)[:40]

    def text(self, repo, path, ref):
        if path == ".github/workflows/dist.yml":
            return "env:\n  FRANKENSEARCH_COMMIT: " + "d" * 40 + "\n"
        return self.manifests[(repo, path)]

    def checks(self, repo, ref):
        return self.check_data.get(repo, [{"name": "cargo test", "status": "completed", "conclusion": "success"}])

    def call(self, method, path, data=None, anonymous=False):
        if method == "GET" and "/commits/" in path:
            if self.pin_exists: return {"sha": "d" * 40}
            raise RuntimeError("missing commit")
        if method == "GET" and path.endswith("issues?state=all&per_page=100"):
            return self.issues
        if method == "POST" and path.endswith("/issues"):
            self.created.append(data)
            return {"number": 7, "state": "open", "html_url": "https://example.test/7", **data}
        if method == "POST" and path.endswith("/comments"):
            self.comments.append(data)
            return {"html_url": "https://example.test/comment"}
        if method == "PATCH":
            self.updated.append(data)
            return {"html_url": "https://example.test/7", **data}
        raise AssertionError((method, path))



class AssessmentTests(unittest.TestCase):
    def setUp(self):
        self.contents = {}
        versions = {
            "asupersync": "0.4.10", "frankensqlite": "0.3.16", "frankensearch": "0.4.2",
            "frankentui": "0.6.0", "fastmcp_rust": "0.8.1", "mcp_agent_mail_rust": "0.3.32",
        }
        deps = {
            "fastmcp_rust": {"asupersync": "=0.4.10"},
            "mcp_agent_mail_rust": {
                "asupersync": "=0.4.9", "fastmcp": {"version": "0.7.1", "package": "fastmcp-rust"},
                "fsqlite": "=0.3.16", "frankensearch": {"version": "0.4.0", "path": "../frankensearch-rel"},
                "ftui": "0.5.0",
            },
        }
        for key, spec in monitor.REPOS.items():
            repo = spec["repo"]
            root = {"workspace": {"package": {"version": versions[key]}, "dependencies": deps.get(key, {})}}
            if key == "asupersync": root = {"package": {"version": versions[key]}}
            if key == "frankensqlite":
                root = {"workspace": {"dependencies": {"asupersync": "=0.4.10"}}}
                self.contents[(repo, "crates/fsqlite/Cargo.toml")] = '[package]\nname="fsqlite"\nversion="0.3.16"\n'
            if key == "frankensearch":
                root = {"workspace": {"dependencies": {"asupersync": ">=0.4.4, <0.5", "ftui-core": "0.5.0"}}}
                self.contents[(repo, "frankensearch/Cargo.toml")] = '[package]\nname="frankensearch"\nversion="0.4.2"\n'
            self.contents[(repo, "Cargo.toml")] = self.dump(root)

    @staticmethod
    def dump(value):
        lines = []
        def emit(prefix, table):
            scalars = {k: v for k, v in table.items() if not isinstance(v, dict)}
            nested = {k: v for k, v in table.items() if isinstance(v, dict)}
            if prefix: lines.append("[" + ".".join(prefix) + "]")
            for key, value in scalars.items(): lines.append(f'{key} = {json_value(value)}')
            if scalars and nested: lines.append("")
            for key, child in nested.items(): emit(prefix + [key], child)
        emit([], value)
        return "\n".join(lines) + "\n"

    def test_current_boundaries_are_blockers_and_pin_drift_is_warning(self):
        report = monitor.assess(FakeGitHub(self.contents))
        codes = {item["code"] for item in report["findings"]}
        self.assertIn("dependency.incompatible.mcp_agent_mail_rust.asupersync", codes)
        self.assertIn("dependency.incompatible.mcp_agent_mail_rust.fastmcp_rust", codes)
        self.assertIn("dependency.incompatible.mcp_agent_mail_rust.frankentui", codes)
        self.assertIn("integration.frankensearch_snapshot_drift", codes)
        self.assertNotIn("dependency.incompatible.mcp_agent_mail_rust.frankensqlite", codes)
        self.assertFalse(any(item["code"] == "verification.collection_failed" and item["repository"].endswith("frankensqlite") for item in report["findings"]))

    def test_failed_tests_block_but_pages_failure_does_not(self):
        checks = {
            monitor.REPOS["fastmcp_rust"]["repo"]: [
                {"name": "Test Suite (ubuntu)", "status": "completed", "conclusion": "failure"},
                {"name": "Cloudflare Pages", "status": "completed", "conclusion": "failure"},
            ]
        }
        report = monitor.assess(FakeGitHub(self.contents, checks))
        ci = [item for item in report["findings"] if item["code"] == "ci.release_critical_failure" and item["repository"].endswith("fastmcp_rust")]
        self.assertEqual(1, len(ci))
        self.assertIn("Test Suite", ci[0]["summary"])
        self.assertNotIn("Cloudflare", ci[0]["summary"])

    def test_unresolvable_release_pin_blocks(self):
        report = monitor.assess(FakeGitHub(self.contents, pin_exists=False))
        self.assertIn("dist.frankensearch_pin_unresolvable", {item["code"] for item in report["findings"]})


def json_value(value):
    if isinstance(value, str): return json.dumps(value)
    return json.dumps(value)


class IssueTests(unittest.TestCase):
    def report(self, blocked):
        blocker = monitor.finding("blocker", "x", "owner/repo", "broken", "evidence", "impact")
        return {"blocker_count": 1 if blocked else 0, "warning_count": 0, "fingerprint": "a" * 64 if blocked else "b" * 64, "findings": [blocker] if blocked else []}

    def test_opens_new_issue_for_blocker(self):
        gh = FakeGitHub()
        monitor.sync_issue(gh, "owner/leaf", self.report(True), "body", monitor.OWNER)
        self.assertEqual(1, len(gh.created))

    def test_unchanged_fingerprint_does_not_spam(self):
        body = monitor.ISSUE_MARKER + "\n<!-- fingerprint:" + "a" * 64 + " -->"
        gh = FakeGitHub(issues=[{"number": 7, "title": monitor.ISSUE_TITLE, "body": body, "state": "open", "html_url": "x"}])
        monitor.sync_issue(gh, "owner/leaf", self.report(True), body, monitor.OWNER)
        self.assertFalse(gh.updated)
        self.assertFalse(gh.comments)

    def test_closes_and_comments_when_blockers_clear(self):
        body = monitor.ISSUE_MARKER + "\n<!-- fingerprint:" + "a" * 64 + " -->"
        gh = FakeGitHub(issues=[{"number": 7, "title": monitor.ISSUE_TITLE, "body": body, "state": "open", "html_url": "x"}])
        monitor.sync_issue(gh, "owner/leaf", self.report(False), "clear", monitor.OWNER)
        self.assertEqual("closed", gh.updated[0]["state"])
        self.assertEqual(1, len(gh.comments))


if __name__ == "__main__":
    unittest.main()
