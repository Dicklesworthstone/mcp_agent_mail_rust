#!/usr/bin/env python3
"""Fail-closed coordinated-release monitor for the core Franken Rust stack."""

from __future__ import annotations

import argparse
import base64
import datetime as dt
import hashlib
import json
import os
import re
import sys
import time
import tomllib
import urllib.error
import urllib.parse
import urllib.request
from pathlib import Path
from typing import Any

OWNER = "Dicklesworthstone"
ISSUE_TITLE = "[release-risk] Coordinated bulk release blocked"
ISSUE_MARKER = "<!-- coordinated-release-risk -->"
FINGERPRINT_RE = re.compile(r"<!-- fingerprint:([0-9a-f]{64}) -->")
PIN_RE = re.compile(r"(?m)^\s*FRANKENSEARCH_COMMIT\s*:\s*['\"]?([0-9a-fA-F]{40})['\"]?\s*$")
CRITICAL_RE = re.compile(
    r"(?:\b(?:build|compile|test|tests|unit|integration|conformance|nextest|clippy|lint|fmt|format|"
    r"rustfmt|msrv|semver|release|publish|package|packaging)\b|cargo[ _-]?(?:check|test|build|"
    r"clippy|fmt|audit|deny)|dependency[ _-]?(?:audit|check)|supply[ _-]?chain)", re.I
)
ADVISORY_RE = re.compile(
    r"\b(?:docs?|documentation|pages|cloudflare|benchmark|bench|performance|perf|fuzz|fuzzing|"
    r"coverage|codecov|mutation|dashboard|drift|corpus|website|deploy|deployment|preview|advisory|"
    r"experimental|nightly-only|miri|loom|sanitizer|coordinated[ _-]?release[ _-]?risk|"
    r"assess coordinated release graph)\b", re.I
)
BLOCKING = {"failure", "error", "timed_out", "action_required"}
INCOMPLETE = {"cancelled", "neutral", "skipped", "stale", "startup_failure", "pending", "queued", "in_progress", None}

REPOS = {
    "asupersync": {
        "repo": f"{OWNER}/asupersync", "version_file": "Cargo.toml", "version_path": ("package", "version"),
        "impact": "FastMCP, FrankenSQLite, and FrankenSearch share Asupersync runtime/context types; a split line can prevent resolution or make public types non-interchangeable.",
    },
    "frankensqlite": {
        "repo": f"{OWNER}/frankensqlite", "version_file": "crates/fsqlite/Cargo.toml", "version_path": ("package", "version"),
        "impact": "MCP Agent Mail cannot validate storage, migrations, and SQLModel behavior against the proposed database release.",
    },
    "frankensearch": {
        "repo": f"{OWNER}/frankensearch", "version_file": "frankensearch/Cargo.toml", "version_path": ("package", "version"),
        "impact": "MCP Agent Mail remains on a gated search snapshot until current FrankenSearch compiles through the leaf workspace.",
    },
    "frankentui": {
        "repo": f"{OWNER}/frankentui", "version_file": "Cargo.toml", "version_path": ("workspace", "package", "version"),
        "impact": "FrankenSearch and MCP Agent Mail can be stranded on incompatible rendering, layout, or runtime APIs.",
    },
    "fastmcp_rust": {
        "repo": f"{OWNER}/fastmcp_rust", "version_file": "Cargo.toml", "version_path": ("workspace", "package", "version"),
        "impact": "MCP Agent Mail cannot move its server, protocol, transport, CLI, and Python bindings to current FastMCP.",
    },
    "mcp_agent_mail_rust": {
        "repo": f"{OWNER}/mcp_agent_mail_rust", "version_file": "Cargo.toml", "version_path": ("workspace", "package", "version"),
        "impact": "The integration leaf has not proven that the complete coordinated dependency set resolves and works together.",
    },
}

EDGES = (
    ("mcp_agent_mail_rust", "asupersync", ("asupersync",), False, "The leaf and current FastMCP/FrankenSearch cannot resolve one Asupersync type universe, so compilation can fail before tests."),
    ("mcp_agent_mail_rust", "fastmcp_rust", ("fastmcp", "fastmcp-rust"), False, "Server, protocol, transport, CLI, and Python layers remain on the older FastMCP API family."),
    ("mcp_agent_mail_rust", "frankensqlite", ("fsqlite",), False, "The leaf storage layer is outside the current database release line."),
    ("mcp_agent_mail_rust", "frankensearch", ("frankensearch",), False, "Current search API/runtime changes remain outside downstream integration coverage."),
    ("mcp_agent_mail_rust", "frankentui", ("ftui",), True, "MCP Agent Mail's terminal surfaces remain on a different pre-1.0 API family."),
    ("fastmcp_rust", "asupersync", ("asupersync",), False, "FastMCP cannot compile and publish on the proposed Asupersync line."),
    ("frankensqlite", "asupersync", ("asupersync",), False, "FrankenSQLite cannot share the proposed runtime line with the coordinated graph."),
    ("frankensqlite", "frankentui", ("ftui",), True, "FrankenSQLite's terminal tooling cannot compile against current FrankenTUI."),
    ("frankensearch", "asupersync", ("asupersync",), False, "FrankenSearch cannot share the proposed runtime line with the coordinated graph."),
    ("frankensearch", "frankentui", ("ftui",), True, "FrankenSearch's terminal tooling cannot compile against current FrankenTUI."),
)


def version(raw: str) -> tuple[int, int, int]:
    value = raw.strip().lstrip("v").split("+", 1)[0]
    if "-" in value:
        raise ValueError(f"prerelease version is not supported: {raw!r}")
    parts = value.split(".")
    if not 1 <= len(parts) <= 3 or any(not part.isdigit() for part in parts):
        raise ValueError(f"unsupported semantic version: {raw!r}")
    nums = [int(part) for part in parts] + [0] * (3 - len(parts))
    return nums[0], nums[1], nums[2]


def partial(raw: str) -> tuple[tuple[int, int, int], int, int | None]:
    parts = raw.strip().lstrip("v").split(".")
    if len(parts) > 3:
        raise ValueError(f"unsupported version: {raw!r}")
    nums, wildcard = [], None
    for index, part in enumerate(parts):
        if part.lower() in {"*", "x"}:
            wildcard = index
            break
        if not part.isdigit():
            raise ValueError(f"unsupported version component: {part!r}")
        nums.append(int(part))
    specified = len(nums)
    nums += [0] * (3 - len(nums))
    return (nums[0], nums[1], nums[2]), specified, wildcard


def accepts(candidate: str, requirement: str) -> bool:
    current = version(candidate)
    for alternative in requirement.split("||"):
        ok = True
        terms = [term.strip() for term in alternative.split(",") if term.strip()]
        for term in terms:
            match = re.fullmatch(r"(>=|<=|>|<|=|\^|~)?\s*(.+)", term)
            if not match:
                raise ValueError(f"unsupported comparator: {term!r}")
            op, raw = match.group(1) or "^", match.group(2)
            target, specified, wildcard = partial(raw)
            if wildcard is not None:
                if op not in {"^", "="}:
                    raise ValueError(f"unsupported wildcard comparator: {term!r}")
                upper = None if wildcard == 0 else (target[0] + 1, 0, 0) if wildcard == 1 else (target[0], target[1] + 1, 0)
                term_ok = current >= target and (upper is None or current < upper)
            elif op == "=": term_ok = current == target
            elif op == ">=": term_ok = current >= target
            elif op == "<=": term_ok = current <= target
            elif op == ">": term_ok = current > target
            elif op == "<": term_ok = current < target
            else:
                if op == "~": upper = (target[0] + 1, 0, 0) if specified <= 1 else (target[0], target[1] + 1, 0)
                elif target[0] > 0: upper = (target[0] + 1, 0, 0)
                elif specified == 1: upper = (1, 0, 0)
                elif target[1] > 0: upper = (0, target[1] + 1, 0)
                elif specified == 2: upper = (0, 1, 0)
                else: upper = (0, 0, target[2] + 1)
                term_ok = current >= target and current < upper
            ok = ok and term_ok
        if terms and ok:
            return True
    return requirement.strip() in {"", "*"}


class GitHub:
    def __init__(self, token: str | None):
        self.token = token

    def call(self, method: str, path: str, data: dict[str, Any] | None = None, anonymous: bool = False) -> Any:
        url = "https://api.github.com" + path
        headers = {"Accept": "application/vnd.github+json", "X-GitHub-Api-Version": "2022-11-28", "User-Agent": "coordinated-release-risk/1"}
        if self.token and not anonymous:
            headers["Authorization"] = f"Bearer {self.token}"
        body = json.dumps(data).encode() if data is not None else None
        if body is not None:
            headers["Content-Type"] = "application/json"
        for attempt in range(4):
            try:
                request = urllib.request.Request(url, body, headers, method=method)
                with urllib.request.urlopen(request, timeout=30) as response:
                    raw = response.read()
                    return json.loads(raw) if raw else None
            except urllib.error.HTTPError as error:
                if method == "GET" and self.token and not anonymous and error.code in {403, 404} and f"/repos/{OWNER}/" in path:
                    return self.call(method, path, data, True)
                if error.code in {429, 500, 502, 503, 504} and attempt < 3:
                    time.sleep(2**attempt)
                    continue
                message = error.read().decode(errors="replace")
                raise RuntimeError(f"GitHub {method} {path} returned {error.code}: {message}") from error
            except urllib.error.URLError as error:
                if attempt < 3:
                    time.sleep(2**attempt)
                    continue
                raise RuntimeError(f"GitHub {method} {path} failed: {error.reason}") from error
        raise AssertionError("unreachable")

    def head(self, repo: str) -> str:
        return str(self.call("GET", f"/repos/{repo}/branches/main")["commit"]["sha"])

    def text(self, repo: str, path: str, ref: str) -> str:
        path = urllib.parse.quote(path, safe="/")
        ref = urllib.parse.quote(ref, safe="")
        payload = self.call("GET", f"/repos/{repo}/contents/{path}?ref={ref}")
        return base64.b64decode(payload["content"]).decode()

    def checks(self, repo: str, ref: str) -> list[dict[str, Any]]:
        result = []
        for page in range(1, 4):
            payload = self.call("GET", f"/repos/{repo}/commits/{ref}/check-runs?filter=latest&per_page=100&page={page}")
            batch = list(payload.get("check_runs", []))
            result.extend(batch)
            if len(batch) < 100: break
        return result


def nested(document: dict[str, Any], keys: tuple[str, ...]) -> Any:
    value: Any = document
    for key in keys:
        value = value[key]
    return value


def dependencies(manifest: dict[str, Any]) -> list[tuple[str, str, str]]:
    result = []
    for key, value in manifest.get("workspace", {}).get("dependencies", {}).items():
        package, requirement = key, None
        if isinstance(value, str): requirement = value
        elif isinstance(value, dict): package, requirement = value.get("package", key), value.get("version")
        if isinstance(requirement, str): result.append((key, str(package), requirement))
    return result


def edge_requirements(state: dict[str, Any], names: tuple[str, ...], prefix: bool) -> list[tuple[str, str]]:
    found = []
    for key, package, requirement in dependencies(state["manifest"]):
        candidates = (key, package)
        matched = any(c == name or (prefix and c.startswith(name + "-")) for c in candidates for name in names)
        if matched: found.append((key, requirement))
    return sorted(set(found))


def critical(name: str) -> bool:
    return bool(CRITICAL_RE.search(name)) and not bool(ADVISORY_RE.search(name))


def finding(severity: str, code: str, repo: str, summary: str, evidence: str, impact: str) -> dict[str, str]:
    return {"severity": severity, "code": code, "repository": repo, "summary": summary, "evidence": evidence, "impact": impact}


def collect(gh: GitHub, key: str, spec: dict[str, Any]) -> dict[str, Any]:
    state = {"key": key, "repo": spec["repo"], "head": "", "version": "unknown", "manifest": {}, "checks": [], "errors": []}
    try: state["head"] = gh.head(spec["repo"])
    except Exception as error:
        state["errors"].append(f"main head: {error}")
        return state
    try:
        state["manifest"] = tomllib.loads(gh.text(spec["repo"], "Cargo.toml", state["head"]))
    except Exception as error: state["errors"].append(f"Cargo.toml: {error}")
    try:
        source = state["manifest"] if spec["version_file"] == "Cargo.toml" else tomllib.loads(gh.text(spec["repo"], spec["version_file"], state["head"]))
        state["version"] = str(nested(source, spec["version_path"]))
        version(state["version"])
    except Exception as error: state["errors"].append(f"release version: {error}")
    try: state["checks"] = gh.checks(spec["repo"], state["head"])
    except Exception as error: state["errors"].append(f"exact-head checks: {error}")
    return state


def assess(gh: GitHub) -> dict[str, Any]:
    states = {key: collect(gh, key, spec) for key, spec in REPOS.items()}
    findings = []
    for key, state in states.items():
        spec = REPOS[key]
        if state["errors"]:
            findings.append(finding("blocker", "verification.collection_failed", state["repo"], "Release evidence could not be collected; failing closed.", "; ".join(state["errors"]), spec["impact"]))
            continue
        release_checks = [check for check in state["checks"] if critical(str(check.get("name", "")))]
        if not release_checks:
            findings.append(finding("warning", "ci.no_release_critical_evidence", state["repo"], "No release-critical build/test check is attached to the exact main head.", state["head"][:12], "The commit has no observable compile, lint, package, or test result."))
        else:
            failed = sorted({str(c.get("name")) for c in release_checks if c.get("conclusion") in BLOCKING})
            incomplete = sorted({str(c.get("name")) for c in release_checks if (c.get("conclusion") or c.get("status")) in INCOMPLETE})
            if failed: findings.append(finding("blocker", "ci.release_critical_failure", state["repo"], "Release-critical checks fail on exact main: " + ", ".join(failed[:12]), state["head"][:12], spec["impact"]))
            if incomplete: findings.append(finding("warning", "ci.release_critical_incomplete", state["repo"], "Release-critical checks are incomplete: " + ", ".join(incomplete[:12]), state["head"][:12], "Wait for conclusive exact-head evidence before publishing."))
    for consumer_key, producer_key, names, prefix, impact in EDGES:
        consumer, producer = states[consumer_key], states[producer_key]
        if consumer["errors"] or producer["errors"]: continue
        reqs = edge_requirements(consumer, names, prefix)
        bad, unknown = [], []
        for dep, requirement in reqs:
            try:
                if not accepts(producer["version"], requirement): bad.append((dep, requirement))
            except ValueError as error: unknown.append((dep, requirement, str(error)))
        if bad:
            rendered = ", ".join(f"{dep} {req}" for dep, req in bad)
            findings.append(finding("blocker", f"dependency.incompatible.{consumer_key}.{producer_key}", consumer["repo"], f"{producer['repo']} main is {producer['version']}, outside consumer constraint(s): {rendered}.", f"consumer {consumer['head'][:12]}; producer {producer['head'][:12]}", impact))
        for dep, requirement, error in unknown:
            findings.append(finding("warning", f"dependency.unparsed.{consumer_key}.{producer_key}.{dep}", consumer["repo"], f"Could not evaluate {dep} {requirement!r} against {producer['version']}.", error, "The dependency edge needs manual review."))
    mcp, search = states["mcp_agent_mail_rust"], states["frankensearch"]
    if not mcp["errors"] and not search["errors"]:
        try: dist = gh.text(mcp["repo"], ".github/workflows/dist.yml", mcp["head"])
        except Exception as error:
            findings.append(finding("blocker", "dist.frankensearch_pin_unreadable", mcp["repo"], "The dist workflow's FrankenSearch pin could not be inspected.", str(error), REPOS["mcp_agent_mail_rust"]["impact"]))
        else:
            match = PIN_RE.search(dist)
            if not match:
                findings.append(finding("blocker", "dist.frankensearch_pin_missing", mcp["repo"], "The dist workflow lacks a full FRANKENSEARCH_COMMIT pin.", "No 40-character pin found.", REPOS["mcp_agent_mail_rust"]["impact"]))
            elif match.group(1).lower() != search["head"].lower():
                pin = match.group(1).lower()
                try: gh.call("GET", f"/repos/{search['repo']}/commits/{pin}")
                except Exception as error:
                    findings.append(finding("blocker", "dist.frankensearch_pin_unresolvable", mcp["repo"], "The pinned FrankenSearch revision cannot be resolved.", str(error), "Tag builds can fail before compilation because the gated release source is unavailable."))
                else:
                    findings.append(finding("warning", "integration.frankensearch_snapshot_drift", mcp["repo"], "MCP Agent Mail's gated FrankenSearch source differs from current main.", f"pin {pin[:12]}; main {search['head'][:12]}", "Current search API/runtime changes are outside Agent Mail's dist and source-install integration coverage."))
    findings.sort(key=lambda item: (item["severity"] != "blocker", item["repository"], item["code"]))
    blockers = [item for item in findings if item["severity"] == "blocker"]
    fingerprint_source = [{k: item[k] for k in ("code", "repository", "summary", "impact")} for item in blockers]
    fingerprint = hashlib.sha256(json.dumps(fingerprint_source, sort_keys=True, separators=(",", ":")).encode()).hexdigest()
    return {
        "generated_at": dt.datetime.now(dt.timezone.utc).replace(microsecond=0).isoformat().replace("+00:00", "Z"),
        "fingerprint": fingerprint, "status": "blocked" if blockers else "warning" if findings else "clear",
        "blocker_count": len(blockers), "warning_count": len(findings) - len(blockers), "states": states, "findings": findings,
    }


def ci_state(state: dict[str, Any]) -> str:
    if state["errors"]: return "unavailable"
    checks = [c for c in state["checks"] if critical(str(c.get("name", "")))]
    if not checks: return "missing"
    if any(c.get("conclusion") in BLOCKING for c in checks): return "failing"
    if any((c.get("conclusion") or c.get("status")) in INCOMPLETE for c in checks): return "incomplete"
    return "passing"


def render(report: dict[str, Any], run_url: str | None = None) -> str:
    status = f"BLOCKED — {report['blocker_count']} blocker(s), {report['warning_count']} warning(s)" if report["blocker_count"] else f"AT RISK — {report['warning_count']} warning(s)" if report["warning_count"] else "CLEAR"
    lines = [ISSUE_MARKER, f"<!-- fingerprint:{report['fingerprint']} -->", "# Coordinated bulk release risk", "", f"**{status}**", "", f"Generated `{report['generated_at']}` from exact `main` heads."]
    if run_url: lines += ["", f"Workflow run: {run_url}"]
    lines += ["", "## Findings", ""]
    if report["findings"]:
        lines += ["| Severity | Repository | Signal |", "|---|---|---|"]
        for item in report["findings"]:
            summary = item["summary"].replace("|", "\\|").replace("\n", " ")
            lines.append(f"| **{item['severity'].upper()}** | `{item['repository']}` | {summary} |")
        lines += ["", "## Likely downstream impact", ""]
        for item in report["findings"]:
            lines += [f"### {item['severity'].upper()}: `{item['repository']}` — `{item['code']}`", "", item["impact"], "", f"Evidence: {item['evidence']}", ""]
    else: lines.append("No release-blocking or warning signals were found.")
    lines += ["## Release graph snapshot", "", "| Repository | Head | Version | Release-critical CI |", "|---|---:|---:|---|"]
    for key in REPOS:
        state = report["states"][key]
        lines.append(f"| `{state['repo']}` | `{state['head'][:12] or 'unknown'}` | `{state['version']}` | {ci_state(state)} |")
    lines += ["", "The issue stays open while blockers exist. It comments only when the blocker fingerprint changes, and closes automatically when blockers clear. Warnings do not independently notify."]
    return "\n".join(lines) + "\n"


def sync_issue(gh: GitHub, repo: str, report: dict[str, Any], body: str, assignee: str | None) -> str | None:
    issues = gh.call("GET", f"/repos/{repo}/issues?state=all&per_page=100")
    issue = next((item for item in issues if "pull_request" not in item and (item.get("title") == ISSUE_TITLE or ISSUE_MARKER in str(item.get("body") or ""))), None)
    blocked = report["blocker_count"] > 0
    payload: dict[str, Any] = {"title": ISSUE_TITLE, "body": body}
    if assignee: payload["assignees"] = [assignee]
    if issue is None:
        if not blocked: return None
        return gh.call("POST", f"/repos/{repo}/issues", payload).get("html_url")
    desired = "open" if blocked else "closed"
    old_match = FINGERPRINT_RE.search(str(issue.get("body") or ""))
    if issue.get("state") == desired and old_match and old_match.group(1) == report["fingerprint"]:
        return issue.get("html_url")
    if blocked:
        event = "## Coordinated release risk changed\n\n" + "\n".join(f"- `{item['repository']}`: {item['summary']}" for item in report["findings"] if item["severity"] == "blocker")
    else:
        event = f"## Coordinated release blockers cleared\n\nNo blockers remain; {report['warning_count']} warning(s) remain in the report."
    number = int(issue["number"])
    gh.call("POST", f"/repos/{repo}/issues/{number}/comments", {"body": event})
    payload["state"] = desired
    if not blocked: payload["state_reason"] = "completed"
    updated = gh.call("PATCH", f"/repos/{repo}/issues/{number}", payload)
    return updated.get("html_url") or issue.get("html_url")


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--json-out", type=Path)
    parser.add_argument("--markdown-out", type=Path)
    parser.add_argument("--sync-issue", action="store_true")
    parser.add_argument("--issue-repository", default=os.environ.get("GITHUB_REPOSITORY"))
    parser.add_argument("--assignee", default=OWNER)
    parser.add_argument("--run-url")
    args = parser.parse_args(argv)
    gh = GitHub(os.environ.get("GH_TOKEN") or os.environ.get("GITHUB_TOKEN"))
    report = assess(gh)
    markdown = render(report, args.run_url)
    if args.json_out:
        args.json_out.write_text(json.dumps(report, indent=2, sort_keys=True) + "\n")
    if args.markdown_out:
        args.markdown_out.write_text(markdown)
    print(markdown, end="")
    if args.sync_issue:
        if not args.issue_repository:
            print("--sync-issue requires --issue-repository or GITHUB_REPOSITORY", file=sys.stderr)
            return 3
        try: sync_issue(gh, args.issue_repository, report, markdown, args.assignee)
        except Exception as error:
            print(f"issue synchronization failed: {error}", file=sys.stderr)
            return 3
    return 2 if report["blocker_count"] else 1 if report["warning_count"] else 0


if __name__ == "__main__":
    raise SystemExit(main())
