#!/usr/bin/env python3
"""Qualify GH407 on exact current source with real, locked native dependencies.

Run only on an authorized Linux build host. This does not install a toolchain,
change dependencies/features, publish results, or contact an existing mailbox.
Controller tests are separate from the native qualification produced here.
"""

from __future__ import annotations

import argparse
import ctypes
import hashlib
import json
import os
from pathlib import Path
import re
import shutil
import signal
import stat
import subprocess
import sys
import tempfile
import time
import tomllib


TESTS = (
    "tests::health_readiness_supervisor_refreshes_without_client_polls",
    "tests::health_readiness_fails_closed_on_pending_pool_acquisition",
    "tests::health_readiness_invalidates_another_pool_generation",
)
HARNESS_CASES = frozenset((
    "baseline", "idle_first_request_1", "idle_first_request_2", "idle_first_request_3",
    "concurrent_first_requests_after_idle", "idle_soak",
    "genuine_database_unavailability", "database_fault_restore",
    "recovery_without_readiness_polling", "stale_success_challenge",
    "stale_fault_restore", "stale_challenge_recovery", "bounded_observed_resources",
    "healthy_shutdown", "outage_shutdown_baseline", "shutdown_with_database_unavailable",
    "outage_process_cleanup", "outage_shutdown_schema_restore", "binary_identity_stable",
))
SUMMARY = re.compile(
    r"^test result: (ok|FAILED)\. (\d+) passed; (\d+) failed; (\d+) ignored; "
    r"(\d+) measured; (\d+) filtered out; finished in [0-9.]+s\s*$", re.MULTILINE,
)
MAX_LOG_BYTES = 128 * 1024 * 1024
MIN_FREE_BYTES = 2 * 1024 * 1024 * 1024
MIN_MEMORY_BYTES = 1024 * 1024 * 1024
STOP_SIGNAL = None


class Refusal(RuntimeError):
    """Evidence or prerequisites cannot support a native verdict."""


def sha256(path):
    digest = hashlib.sha256()
    with Path(path).open("rb") as stream:
        for block in iter(lambda: stream.read(1024 * 1024), b""):
            digest.update(block)
    return digest.hexdigest()


def tracked_identity(path, mode, expected_blob):
    path = Path(path)
    info = path.lstat()
    digest = hashlib.sha256()
    if mode == "120000":
        if not path.is_symlink():
            raise Refusal("Tracked symlink changed type: " + str(path))
        data = os.fsencode(os.readlink(path))
        length = len(data)
        blob = hashlib.sha1(b"blob " + str(length).encode() + b"\0" + data).hexdigest()
        digest.update(data)
    else:
        if not stat.S_ISREG(info.st_mode) or bool(info.st_mode & 0o111) != (mode == "100755"):
            raise Refusal("Tracked file type or executable mode differs from HEAD: " + str(path))
        length = info.st_size
        git_hash = hashlib.sha1(b"blob " + str(length).encode() + b"\0")
        observed = 0
        with path.open("rb") as stream:
            for block in iter(lambda: stream.read(1024 * 1024), b""):
                digest.update(block)
                git_hash.update(block)
                observed += len(block)
        after = path.lstat()
        stable_fields = ("st_dev", "st_ino", "st_mode", "st_size", "st_mtime_ns", "st_ctime_ns")
        if observed != length or any(getattr(after, key) != getattr(info, key) for key in stable_fields):
            raise Refusal("Tracked input changed while it was hashed: " + str(path))
        blob = git_hash.hexdigest()
    if blob != expected_blob:
        raise Refusal("Tracked bytes differ from HEAD, including hidden index edits: " + str(path))
    return {"mode": mode, "blob": blob, "sha256": digest.hexdigest(), "bytes": length,
            "filesystem_mode": stat.S_IMODE(info.st_mode)}


def write_json(path, value):
    with Path(path).open("x", encoding="utf-8") as stream:
        json.dump(value, stream, indent=2, sort_keys=True)
        stream.write("\n")


def beneath(path, directory):
    return Path(path).resolve().is_relative_to(Path(directory).resolve())


def libtest_verdict(output, returncode, name):
    """The final outer summary is decisive; the real child also prints one."""
    summaries = list(SUMMARY.finditer(output))
    named = re.search(r"^test " + re.escape(name) + r" \.\.\. ok\s*$", output, re.MULTILINE)
    terminal = summaries[-1] if summaries else None
    valid = bool(returncode == 0 and terminal and named
                 and terminal.groups()[:5] == ("ok", "1", "0", "0", "0")
                 and not output[terminal.end():].strip()
                 and all(match.group(1) == "ok" and match.group(3) == "0"
                         for match in summaries))
    return {"passed": valid, "expected_test": name,
            "terminal_summary": terminal.group(0).strip() if terminal else None,
            "summary_count": len(summaries), "named_success": bool(named)}


def cargo_artifact(output, package_id, target_name, target_kind, test_profile, target_dir):
    """Select the executable Cargo actually emitted, including cache hits."""
    events = []
    for number, line in enumerate(output.splitlines(), 1):
        if not line.strip():
            continue
        try:
            event = json.loads(line)
        except json.JSONDecodeError as error:
            raise Refusal(f"Cargo stdout line {number} is not JSON") from error
        if not isinstance(event, dict):
            raise Refusal("Cargo emitted a non-object event")
        events.append(event)
    if not events or events[-1].get("reason") != "build-finished" or events[-1].get("success") is not True:
        raise Refusal("Cargo did not end with a successful build-finished event")
    paths = set()
    for event in events:
        target = event.get("target", {})
        if (event.get("reason") == "compiler-artifact"
                and event.get("package_id") == package_id
                and target.get("name") == target_name
                and target_kind in target.get("kind", [])
                and event.get("profile", {}).get("test") is test_profile
                and event.get("executable")):
            paths.add(str(Path(event["executable"]).resolve()))
    if len(paths) != 1:
        raise Refusal(f"Expected one emitted {target_name} executable; found {len(paths)}")
    executable = Path(paths.pop())
    if not beneath(executable, target_dir) or not executable.is_file() or not os.access(executable, os.X_OK):
        raise Refusal("Emitted executable is absent, not executable, or outside the selected target directory")
    return {"path": str(executable), "sha256": sha256(executable), "bytes": executable.stat().st_size,
            "package_id": package_id, "target": target_name, "test_profile": test_profile}


def validate_metadata(metadata, lock, repo, target_dir, tracked_paths):
    if Path(metadata.get("workspace_root", "")).resolve() != repo:
        raise Refusal("Cargo metadata names another workspace")
    if Path(metadata.get("target_directory", "")).resolve() != target_dir:
        raise Refusal("Cargo metadata names another target directory")
    locked = {(p["name"], p["version"], p.get("source")) for p in lock["package"]}
    packages = metadata.get("packages")
    if not isinstance(packages, list) or not metadata.get("resolve", {}).get("nodes"):
        raise Refusal("Cargo returned no resolved dependency graph")
    local = {}
    for package in packages:
        key = (package["name"], package["version"], package.get("source"))
        if key not in locked:
            raise Refusal(f"Resolved package is absent from the committed lock: {key}")
        if package.get("source") is None:
            manifest = Path(package["manifest_path"]).resolve()
            if not beneath(manifest, repo) or str(manifest.relative_to(repo)) not in tracked_paths:
                raise Refusal(f"Untracked or external local dependency: {package['name']}")
            local[package["name"]] = package["id"]
        elif package["source"].startswith("git+") and not re.search(r"#[0-9a-f]{40}$", package["source"]):
            raise Refusal(f"Git dependency has no exact resolved revision: {package['name']}")
    for name in ("mcp-agent-mail-server", "mcp-agent-mail-cli"):
        if name not in local or local[name] not in metadata.get("workspace_members", []):
            raise Refusal(f"Missing native workspace package {name}")
    return local


def validate_harness_report(report, binary, source, harness_hash, output_root, report_path):
    if not isinstance(report, dict) or not beneath(report_path, output_root):
        raise Refusal("Harness report is invalid or escaped the owned output directory")
    identity = report.get("identity", {})
    if (report.get("schema_version") != 1 or report.get("evidence_class") != "supplied_binary"
            or identity.get("source_sha_asserted") != source
            or identity.get("binary_sha256") != binary["sha256"]
            or identity.get("harness_sha256") != harness_hash
            or Path(identity.get("binary", "")).resolve() != Path(binary["path"]).resolve()
            or Path(identity.get("run_directory", "")).resolve() != Path(report_path).parent.resolve()):
        raise Refusal("Harness report identity, source, or evidence class does not match this build")
    cases = report.get("cases")
    if not isinstance(cases, list) or any(not isinstance(case, dict) for case in cases):
        raise Refusal("Harness report has no valid case list")
    names = [case.get("name") for case in cases]
    if (any(not isinstance(name, str) for name in names)
            or len(names) != len(set(names)) or not HARNESS_CASES.issubset(names)):
        raise Refusal("Harness report has missing or duplicated required cases")
    configuration = report.get("configuration", {})
    for name, value in (("idleSeconds", 8), ("idleCycles", 3), ("concurrency", 16),
                        ("soakSeconds", 30), ("failureWait", 8), ("maxSampleAge", 6),
                        ("refreshSeconds", 2), ("maxIdleCpuCores", 0.25)):
        if configuration.get(name) != value:
            raise Refusal(f"Harness qualification default changed: {name}")
    statuses = {case.get("status") for case in cases}
    if not statuses.issubset({"PASS", "FAIL", "INCOMPLETE"}):
        raise Refusal("Harness report contains an unknown verdict")
    expected = "FAIL" if "FAIL" in statuses else "INCOMPLETE" if "INCOMPLETE" in statuses else "PASS"
    if report.get("status") != expected or report.get("exit_code") != {"PASS": 0, "FAIL": 1, "INCOMPLETE": 2}[expected]:
        raise Refusal("Harness summary disagrees with its case verdicts")
    return expected


def proc_census():
    result = {}
    for entry in Path("/proc").iterdir():
        if not entry.name.isdecimal():
            continue
        try:
            value = (entry / "stat").read_text()
            fields = value[value.rfind(")") + 2:].split()
            result[int(entry.name)] = {"state": fields[0], "parent": int(fields[1]),
                                      "group": int(fields[2]), "start": fields[19]}
        except (OSError, ValueError, IndexError):
            continue
    return result


def enable_subreaper():
    # Linux PR_SET_CHILD_SUBREAPER: orphaned detached harness descendants stay
    # children of this controller, so a Node timeout cannot strand its server.
    libc = ctypes.CDLL(None, use_errno=True)
    if libc.prctl(36, 1, 0, 0, 0) != 0:
        raise Refusal(f"Cannot establish child ownership: errno {ctypes.get_errno()}")


def memory_available():
    text = Path("/proc/meminfo").read_text()
    found = re.search(r"^MemAvailable:\s+(\d+) kB$", text, re.MULTILINE)
    if not found:
        raise Refusal("Linux host memory availability is unavailable")
    available = int(found.group(1)) * 1024
    # An unexposed cgroup remains unknown, not proof of unlimited memory.
    maximum, current = Path("/sys/fs/cgroup/memory.max"), Path("/sys/fs/cgroup/memory.current")
    if maximum.is_file() and current.is_file():
        limit = maximum.read_text().strip()
        if limit.isdecimal():
            available = min(available, max(0, int(limit) - int(current.read_text())))
    return available


class ProcessRunner:
    """Serial commands with raw files, bounded groups, and detached-child census."""

    def __init__(self, evidence, *, cancel=lambda: STOP_SIGNAL, grace=10,
                 minimum_free=MIN_FREE_BYTES, minimum_memory=MIN_MEMORY_BYTES, space_roots=()):
        self.evidence = Path(evidence)
        self.cancel = cancel
        self.grace = grace
        self.minimum_free = minimum_free
        self.minimum_memory = minimum_memory
        self.space_roots = [self.evidence, *map(Path, space_roots)]
        self.receipts = []
        enable_subreaper()

    def run(self, name, command, *, cwd, env, timeout, grace=None):
        index = len(self.receipts) + 1
        stem = f"{index:02d}-{name}"
        stdout = self.evidence / (stem + ".stdout.log")
        stderr = self.evidence / (stem + ".stderr.log")
        receipt = {"name": name, "command": [str(x) for x in command], "cwd": str(cwd),
                   "timeout_seconds": timeout, "stdout": str(stdout), "stderr": str(stderr),
                   "exit_code": None, "termination_reason": None, "forced": False,
                   "remaining_processes": [], "status": "INCOMPLETE"}
        self.receipts.append(receipt)
        started = time.monotonic()
        process = None
        known = {}
        before = proc_census()

        def has_space():
            return all(shutil.disk_usage(root).free >= self.minimum_free for root in self.space_roots)

        def observe():
            current = proc_census()
            if process and process.pid in current and process.pid not in known:
                known[process.pid] = current[process.pid]["start"]
            changed = True
            while changed:
                changed = False
                for pid, item in current.items():
                    parent_owned = item["parent"] in known and current.get(item["parent"], {}).get("start") == known[item["parent"]]
                    adopted = item["parent"] == os.getpid() and before.get(pid, {}).get("start") != item["start"]
                    if pid not in known and (parent_owned or adopted):
                        known[pid] = item["start"]
                        changed = True
            return {pid: item for pid, item in current.items() if known.get(pid) == item["start"]}

        def send(signum):
            owned = observe()
            groups = {item["group"] for item in owned.values() if item["state"] != "Z"}
            for group in groups:
                if group == os.getpgrp():
                    raise Refusal("Owned descendant unexpectedly joined the controller process group")
                try:
                    os.killpg(group, signum)
                except ProcessLookupError:
                    pass

        def live():
            return {pid: item for pid, item in observe().items() if item["state"] != "Z"}

        try:
            with stdout.open("xb") as out, stderr.open("xb") as err:
                if self.cancel():
                    raise Refusal("Qualification was interrupted before command start")
                if not has_space():
                    raise Refusal("Insufficient free space before command start")
                if self.minimum_memory and memory_available() < self.minimum_memory:
                    raise Refusal("Insufficient available memory before command start")
                process = subprocess.Popen(command, cwd=cwd, env=env, stdin=subprocess.DEVNULL,
                                           stdout=out, stderr=err, start_new_session=True)
                receipt["pid"] = process.pid
                observe()
                while True:
                    observe()
                    code = process.poll()
                    if code is not None:
                        receipt["exit_code"] = code
                        if stdout.stat().st_size + stderr.stat().st_size > MAX_LOG_BYTES:
                            receipt["termination_reason"] = "log_budget"
                        elif live():
                            receipt["termination_reason"] = "descendants_outlived_command"
                        break
                    if self.cancel():
                        receipt["termination_reason"] = "interrupted"
                    elif time.monotonic() - started >= timeout:
                        receipt["termination_reason"] = "timeout"
                    elif stdout.stat().st_size + stderr.stat().st_size > MAX_LOG_BYTES:
                        receipt["termination_reason"] = "log_budget"
                    elif not has_space():
                        receipt["termination_reason"] = "disk_budget"
                    elif self.minimum_memory and memory_available() < self.minimum_memory:
                        receipt["termination_reason"] = "memory_budget"
                    if receipt["termination_reason"]:
                        break
                    time.sleep(0.1)
        except (OSError, Refusal) as error:
            receipt["error"] = str(error)
            receipt["termination_reason"] = receipt["termination_reason"] or "prerequisite_or_io_error"
        finally:
            if process:
                if live():
                    send(signal.SIGCONT)
                    send(signal.SIGTERM)
                    until = time.monotonic() + (self.grace if grace is None else grace)
                    while live() and time.monotonic() < until:
                        time.sleep(0.05)
                    if live():
                        receipt["forced"] = True
                        send(signal.SIGCONT)
                        send(signal.SIGKILL)
                        until = time.monotonic() + 3
                        while live() and time.monotonic() < until:
                            time.sleep(0.05)
                try:
                    receipt["exit_code"] = process.wait(timeout=1)
                except subprocess.TimeoutExpired:
                    receipt["termination_reason"] = receipt["termination_reason"] or "unreaped_leader"
                for pid in observe():
                    if pid == process.pid:
                        continue
                    try:
                        os.waitpid(pid, os.WNOHANG)
                    except ChildProcessError:
                        pass
                receipt["remaining_processes"] = [{"pid": pid, **item} for pid, item in observe().items()]
            receipt["elapsed_seconds"] = time.monotonic() - started
            for key, file in (("stdout", stdout), ("stderr", stderr)):
                if file.exists():
                    receipt[key + "_sha256"] = sha256(file)
                    receipt[key + "_bytes"] = file.stat().st_size
            if not receipt["termination_reason"] and not receipt["remaining_processes"]:
                receipt["status"] = "PASS" if receipt["exit_code"] == 0 else "FAIL"
            write_json(self.evidence / (stem + ".json"), receipt)
        return receipt


def verbose_identity(output):
    return dict(line.split(": ", 1) for line in output.splitlines() if ": " in line)


def verify_toolchain_manifest(path, channel, rustc_output, cargo_output):
    manifest = tomllib.loads(Path(path).read_text())
    if channel != "nightly-" + str(manifest.get("date")):
        raise Refusal("Toolchain manifest date disagrees with rust-toolchain.toml")
    compiler = verbose_identity(rustc_output)
    cargo = verbose_identity(cargo_output)
    host = compiler.get("host")
    if not host or cargo.get("host") != host:
        raise Refusal("Cargo and rustc do not report the same native host")
    for name, identity in (("rustc", compiler), ("cargo", cargo)):
        package = manifest.get("pkg", {}).get(name, {})
        version = package.get("version", "")
        commit = re.search(r"\(([0-9a-f]{7,40}) (\d{4}-\d{2}-\d{2})\)", version)
        if (not commit or not identity.get("commit-hash", "").startswith(commit.group(1))
                or identity.get("commit-date") != commit.group(2)
                or package.get("target", {}).get(host, {}).get("available") is not True):
            raise Refusal(f"{name} identity does not match the pinned manifest")
        # Cargo's distribution package version differs from its CLI release.
        if name == "rustc" and identity.get("release") != version.split()[0]:
            raise Refusal("rustc release does not match the pinned manifest")
    return {"manifest": str(Path(path).resolve()), "manifest_sha256": sha256(path),
            "channel": channel, "host": host, "rustc": compiler, "cargo": cargo,
            "authority": "Caller-supplied distribution manifest; retain its authenticated provisioning receipt"}


def build_environment(inherited):
    forbidden = [key for key, value in inherited.items() if value and (
        key in {"RUSTFLAGS", "CARGO_ENCODED_RUSTFLAGS", "CARGO_BUILD_RUSTFLAGS",
                "RUSTC_WRAPPER", "RUSTC_WORKSPACE_WRAPPER", "RUSTC_BOOTSTRAP",
                "CARGO_BUILD_RUSTC", "CARGO_BUILD_RUSTC_WRAPPER", "CARGO_BUILD_RUSTC_WORKSPACE_WRAPPER"}
        or (key.startswith("CARGO_TARGET_") and key.endswith(("_RUSTFLAGS", "_RUNNER")))
        or (key.startswith("CARGO_PROFILE_") and key not in {
            "CARGO_PROFILE_DEV_DEBUG", "CARGO_PROFILE_TEST_DEBUG"})
    )]
    if forbidden:
        raise Refusal("Build-altering environment requires removal: " + ", ".join(sorted(forbidden)))
    env = dict(inherited)
    env.update(CARGO_BUILD_JOBS="1", CARGO_INCREMENTAL="0", CARGO_PROFILE_DEV_DEBUG="0",
               CARGO_PROFILE_TEST_DEBUG="0", CARGO_TERM_COLOR="never", RUST_TEST_THREADS="1",
               RUST_BACKTRACE="1")
    return env


def external_cargo_configs(repo, env):
    """Record external config hashes; reject hidden compiler/source overrides."""
    roots = list(repo.parents)
    cargo_home = Path(env.get("CARGO_HOME", str(Path.home() / ".cargo"))).resolve()
    paths = [parent / ".cargo" / name for parent in roots for name in ("config", "config.toml")]
    paths.extend(cargo_home / name for name in ("config", "config.toml"))
    records = []
    for path in dict.fromkeys(paths):
        if not path.is_file() or beneath(path, repo):
            continue
        value = tomllib.loads(path.read_text())
        build = value.get("build", {})
        if (any(key in build for key in ("rustflags", "rustc", "rustc-wrapper", "rustc-workspace-wrapper", "target"))
                or any("rustflags" in target or "runner" in target for target in value.get("target", {}).values())
                or any(key in value for key in ("patch", "replace", "paths", "source", "env"))):
            raise Refusal(f"External Cargo configuration changes qualification inputs: {path}")
        records.append({"path": str(path), "sha256": sha256(path)})
    return records


class Qualification:
    def __init__(self, args):
        self.repo = args.repo.resolve()
        self.evidence = args.evidence.resolve()
        # main reserves this directory exclusively before constructing us.
        if not self.evidence.is_dir():
            raise Refusal("Evidence directory was not reserved")
        self.args = args
        self.env = build_environment(os.environ)
        chosen = self.env.get("CARGO_TARGET_DIR")
        self.target = Path(chosen).resolve() if chosen else Path(tempfile.mkdtemp(
            prefix="gh407-native-target-", dir=self.evidence.parent)).resolve()
        if beneath(self.target, self.evidence):
            raise Refusal("CARGO_TARGET_DIR must be outside the evidence upload directory")
        self.target.mkdir(mode=0o700, parents=True, exist_ok=True)
        target_stat = self.target.stat()
        if target_stat.st_uid != os.getuid() or target_stat.st_mode & 0o022:
            raise Refusal("Selected Cargo target directory must be owned by this user and not writable by others")
        self.env["CARGO_TARGET_DIR"] = str(self.target)
        roots = [self.target]
        cargo_home = Path(self.env.get("CARGO_HOME", str(Path.home() / ".cargo"))).resolve()
        if cargo_home.is_dir():
            roots.append(cargo_home)
        self.runner = ProcessRunner(self.evidence, space_roots=roots)
        self.deadline = time.monotonic() + 180 * 60
        self.source_before = None
        self.artifacts = []
        self.report = {"schema_version": 1, "evidence_class": "native_current_source",
                       "expected_source": args.expected_source, "status": "INCOMPLETE",
                       "native_qualified": False, "stages": [], "native_tests": [],
                       "commands": self.runner.receipts, "target_directory": str(self.target),
                       "target_owner_uid": target_stat.st_uid, "resource_controls": {
                           key: self.env[key] for key in ("CARGO_BUILD_JOBS", "CARGO_INCREMENTAL",
                           "CARGO_PROFILE_DEV_DEBUG", "CARGO_PROFILE_TEST_DEBUG")},
                       "limits": {"compile_seconds": 3600, "native_test_seconds": 180,
                                  "black_box_seconds": 900, "total_seconds": 10800,
                                  "minimum_free_bytes": MIN_FREE_BYTES,
                                  "minimum_available_memory_bytes": MIN_MEMORY_BYTES},
                       "limitations": [
                           "Focused GH407 qualification does not replace workspace compiler/lint or long-soak gates.",
                           "Controller self-tests do not execute Agent Mail native behavior.",
                           "The provisioned toolchain and Cargo cache are trusted build inputs; metadata verifies locked identities, not every cached transitive source byte.",
                       ]}
        write_json(self.evidence / "request.json", {
            "expected_source": args.expected_source, "repo": str(self.repo),
            "evidence": str(self.evidence), "target": str(self.target),
        })

    def command(self, name, argv, *, timeout=30, env=None, grace=None):
        remaining = self.deadline - time.monotonic() - 60
        if remaining <= 0 or STOP_SIGNAL:
            raise Refusal("Overall qualification deadline exhausted or interrupted")
        return self.runner.run(name, argv, cwd=self.repo, env=env or self.env,
                               timeout=min(timeout, remaining), grace=grace)

    def output(self, receipt):
        if receipt["status"] != "PASS":
            raise Refusal(f"Command {receipt['name']} did not complete successfully; see its raw receipt")
        path = Path(receipt["stdout"])
        if path.stat().st_size > MAX_LOG_BYTES:
            raise Refusal("Command stdout exceeded its parsing budget")
        return path.read_text(errors="replace")

    def source_snapshot(self, label):
        head = self.output(self.command(label + "-head", ["git", "rev-parse", "HEAD"])).strip()
        dirty = self.output(self.command(label + "-dirty", [
            "git", "status", "--porcelain=v1", "--untracked-files=no"])).strip()
        if head != self.args.expected_source or dirty:
            raise Refusal(f"Expected unchanged tracked source at {self.args.expected_source}; actual HEAD={head}, dirty={bool(dirty)}")
        listing = self.output(self.command(label + "-files", ["git", "ls-tree", "-r", "--full-tree", "-z", "HEAD"]))
        files = {}
        for record in listing.split("\0"):
            if not record:
                continue
            metadata, relative = record.split("\t", 1)
            mode, kind, blob = metadata.split()
            path = self.repo / relative
            if kind != "blob" or mode not in {"100644", "100755", "120000"}:
                raise Refusal("Unsupported tracked input: " + relative)
            files[relative] = tracked_identity(path, mode, blob)
        for required in ("Cargo.toml", "Cargo.lock", "rust-toolchain.toml",
                         "scripts/ci/qualify_issue407.py", "scripts/qualify_health_idle.mjs"):
            if required not in files:
                raise Refusal("Required qualification input is not committed: " + required)
        for relative in (".cargo/config", ".cargo/config.toml"):
            path = self.repo / relative
            if path.exists() and (relative not in files or not beneath(path, self.repo)):
                raise Refusal("Untracked or external Cargo configuration: " + relative)
        if sha256(__file__) != files["scripts/ci/qualify_issue407.py"]["sha256"]:
            raise Refusal("Executing driver differs from the driver at the expected source")
        snapshot = {"source": head, "files": files}
        write_json(self.evidence / (label + "-inputs.json"), snapshot)
        return snapshot

    def toolchain(self):
        config = tomllib.loads((self.repo / "rust-toolchain.toml").read_text())
        channel = config["toolchain"]["channel"]
        if not re.fullmatch(r"nightly-\d{4}-\d{2}-\d{2}", channel):
            raise Refusal("Source must name an immutable dated nightly toolchain")
        rustup = shutil.which("rustup", path=self.env.get("PATH"))
        if rustup and not self.args.toolchain_manifest:
            cargo = self.output(self.command("pinned-cargo", [rustup, "which", "--toolchain", channel, "cargo"])).strip()
            compiler = self.output(self.command("pinned-rustc", [rustup, "which", "--toolchain", channel, "rustc"])).strip()
            if self.env.get("RUSTC") and Path(self.env["RUSTC"]).resolve() != Path(compiler).resolve():
                raise Refusal("Inherited RUSTC differs from the source-pinned rustup compiler")
            authority = {"authority": "rustup which --toolchain " + channel}
        else:
            cargo = shutil.which("cargo", path=self.env.get("PATH"))
            compiler = self.env.get("RUSTC") or shutil.which("rustc", path=self.env.get("PATH"))
            if not cargo or not compiler or not self.args.toolchain_manifest:
                raise Refusal("Install the pinned toolchain, or provide its distribution manifest with PATH cargo/rustc")
            authority = {}
        self.cargo, self.compiler = str(Path(cargo).resolve()), str(Path(compiler).resolve())
        # Cargo uses the very compiler whose version and hash are recorded.
        self.env.update(RUSTC=self.compiler, RUSTUP_TOOLCHAIN=channel)
        rustc_output = self.output(self.command("rustc-identity", [self.compiler, "-vV"]))
        cargo_output = self.output(self.command("cargo-identity", [self.cargo, "-vV"]))
        if self.args.toolchain_manifest:
            authority = verify_toolchain_manifest(self.args.toolchain_manifest, channel, rustc_output, cargo_output)
        self.host = verbose_identity(rustc_output).get("host")
        if not self.host or self.host != verbose_identity(cargo_output).get("host"):
            raise Refusal("Toolchain has no matching native host identities")
        if self.env.get("CARGO_BUILD_TARGET") not in (None, "", self.host):
            raise Refusal("An inherited cross-compilation target is not native qualification")
        for name in ("config", "config.toml"):
            path = self.repo / ".cargo" / name
            if path.is_file():
                config = tomllib.loads(path.read_text())
                if config.get("build", {}).get("target") not in (None, self.host):
                    raise Refusal("Tracked Cargo configuration selects a non-native target")
        self.report["toolchain"] = {**authority, "channel": channel, "host": self.host,
                                    "rustc": self.compiler, "cargo": self.cargo,
                                    "rustc_sha256": sha256(self.compiler), "cargo_sha256": sha256(self.cargo),
                                    "rustc_verbose": rustc_output, "cargo_verbose": cargo_output}
        self.report["external_cargo_configs"] = external_cargo_configs(self.repo, self.env)
        # Resource controls preserve dependency revisions and default features.
        self.cargo_command = [self.cargo,
            "--config", "profile.dev.package.mcp-agent-mail-server.debug=0",
            "--config", "profile.test.package.mcp-agent-mail-server.debug=0"]
        self.report["resource_controls"]["cargo_config"] = self.cargo_command[1:]

    def private_test_env(self):
        root = self.evidence / "native-mailbox"
        root.mkdir(mode=0o700)
        for relative in ("config/mcp-agent-mail", "archive", "data", "cache", "state", "tmp"):
            (root / relative).mkdir(mode=0o700, parents=True, exist_ok=True)
        (root / "config/mcp-agent-mail/config.env").write_text("# Private native qualification authority\n")
        env = {key: value for key, value in self.env.items()
               if not key.startswith(("AM_", "HTTP_", "ATC_"))
               and key not in {"DATABASE_URL", "STORAGE_ROOT", "NOTIFY_SOCKET"}}
        env.update(DATABASE_URL="sqlite://" + str(root / "mailbox.sqlite3"), STORAGE_ROOT=str(root / "archive"),
                   XDG_CONFIG_HOME=str(root / "config"), XDG_DATA_HOME=str(root / "data"),
                   XDG_CACHE_HOME=str(root / "cache"), XDG_STATE_HOME=str(root / "state"),
                   TMPDIR=str(root / "tmp"), HTTP_HOST="127.0.0.1", HTTP_PORT="0",
                   AM_ATC_ENABLED="false", ATC_LEARNING_DISABLED="1")
        return env

    def stage(self, name, action):
        started = time.monotonic()
        try:
            details = action()
            result = {"name": name, "status": details.pop("status", "PASS"), "details": details}
        except Exception as error:
            result = {"name": name, "status": "INCOMPLETE",
                      "error": f"{type(error).__name__}: {error}"}
        result["elapsed_seconds"] = time.monotonic() - started
        self.report["stages"].append(result)
        write_json(self.evidence / f"stage-{len(self.report['stages']):02d}-{name}.json", result)
        return result["status"] == "PASS"

    def preflight(self):
        self.source_before = self.source_snapshot("before")
        self.toolchain()
        self.node = shutil.which(self.args.node, path=self.env.get("PATH"))
        if not self.node:
            raise Refusal("Node.js is unavailable")
        version = self.output(self.command("node-identity", [self.node, "--version"])).strip()
        if not re.fullmatch(r"v\d+\.\d+\.\d+", version) or int(version.split(".")[0][1:]) < 22:
            raise Refusal("Node.js 22 or newer is required")
        self.report["node"] = {"path": str(Path(self.node).resolve()), "version": version}
        metadata_receipt = self.command("locked-dependency-metadata", self.cargo_command + [
            "metadata", "--locked", "--format-version=1", "--filter-platform", self.host], timeout=900)
        metadata = json.loads(self.output(metadata_receipt))
        lock = tomllib.loads((self.repo / "Cargo.lock").read_text())
        self.packages = validate_metadata(metadata, lock, self.repo, self.target, self.source_before["files"])
        self.report["dependency_metadata"] = {"receipt": metadata_receipt["stdout"],
            "sha256": metadata_receipt["stdout_sha256"], "packages": len(metadata["packages"]),
            "lock_sha256": self.source_before["files"]["Cargo.lock"]["sha256"]}
        self.test_env = self.private_test_env()
        return {"source": self.args.expected_source, "resolved_packages": len(metadata["packages"])}

    def compile(self, test):
        package = "mcp-agent-mail-server" if test else "mcp-agent-mail-cli"
        arguments = ["test", "--locked", "-p", package, "--lib", "--no-run"] if test else [
            "build", "--locked", "--release", "-p", package, "--bin", "am"]
        # No explicit --target: retain the normal native-host cache layout.
        receipt = self.command("compile-tests" if test else "compile-am", self.cargo_command + arguments + [
            "--message-format=json-render-diagnostics"], timeout=3600)
        if receipt["status"] != "PASS":
            return {"status": receipt["status"], "command": receipt["name"], "executed_native_tests": 0}
        artifact = cargo_artifact(self.output(receipt), self.packages[package],
                                  "mcp_agent_mail_server" if test else "am", "lib" if test else "bin",
                                  test, self.target)
        self.artifacts.append(artifact)
        if test:
            self.test_binary = artifact
        else:
            self.binary = artifact
        return {"artifact": artifact}

    def run_native_test(self, name):
        if sha256(self.test_binary["path"]) != self.test_binary["sha256"]:
            raise Refusal("Native test executable changed before execution")
        receipt = self.command("native-" + name.rsplit("::", 1)[-1], [self.test_binary["path"],
            "--exact", name, "--nocapture", "--test-threads=1"], timeout=180, env=self.test_env)
        text = Path(receipt["stdout"]).read_text(errors="replace") if receipt.get("stdout_bytes", 0) <= MAX_LOG_BYTES else ""
        verdict = libtest_verdict(text, receipt["exit_code"], name)
        execution_complete = not receipt["termination_reason"] and not receipt["remaining_processes"]
        passed = execution_complete and verdict["passed"]
        if sha256(self.test_binary["path"]) != self.test_binary["sha256"]:
            raise Refusal("Native test executable changed during execution")
        status = "PASS" if passed else "INCOMPLETE" if receipt["termination_reason"] in {
            "interrupted", "disk_budget", "memory_budget", "log_budget", "prerequisite_or_io_error"} else "FAIL"
        result = {"status": status, **verdict, "command_receipt": receipt["name"],
                  "binary_sha256": self.test_binary["sha256"]}
        self.report["native_tests"].append({"name": name, **result})
        return result

    def black_box(self):
        if sha256(self.binary["path"]) != self.binary["sha256"]:
            raise Refusal("Agent Mail executable changed before black-box execution")
        harness = self.repo / "scripts/qualify_health_idle.mjs"
        output = self.evidence / "black-box"
        output.mkdir(mode=0o700)
        receipt = self.command("black-box", [self.node, str(harness), "--binary", self.binary["path"],
            "--source-sha", self.args.expected_source, "--expected-sha256", self.binary["sha256"],
            "--output-parent", str(output)], timeout=900, grace=45)
        reports = list(output.glob("agent-mail-gh407-*/report.json"))
        if len(reports) != 1 or reports[0].stat().st_size > MAX_LOG_BYTES:
            raise Refusal("Black-box runner did not retain one bounded qualification report")
        report = json.loads(reports[0].read_text())
        status = validate_harness_report(report, self.binary, self.args.expected_source,
            self.source_before["files"]["scripts/qualify_health_idle.mjs"]["sha256"], output, reports[0])
        if receipt["termination_reason"] or receipt["remaining_processes"]:
            status = "INCOMPLETE"
        elif receipt["exit_code"] != {"PASS": 0, "FAIL": 1, "INCOMPLETE": 2}[status]:
            raise Refusal("Black-box exit status disagrees with its report")
        return {"status": status, "report": str(reports[0]), "report_sha256": sha256(reports[0]),
                "cases": len(report["cases"]), "binary_sha256": self.binary["sha256"]}

    def recheck(self):
        after = self.source_snapshot("after")
        if after != self.source_before:
            raise Refusal("Tracked source hashes or modes changed during qualification")
        for artifact in self.artifacts:
            if sha256(artifact["path"]) != artifact["sha256"]:
                raise Refusal("Emitted native executable changed during qualification: " + artifact["path"])
        if (sha256(self.compiler) != self.report["toolchain"]["rustc_sha256"]
                or sha256(self.cargo) != self.report["toolchain"]["cargo_sha256"]):
            raise Refusal("Compiler or Cargo executable changed during qualification")
        if external_cargo_configs(self.repo, self.env) != self.report["external_cargo_configs"]:
            raise Refusal("External Cargo configuration changed during qualification")
        return {"tracked_inputs": len(after["files"]), "unchanged_artifacts": self.artifacts}

    def execute(self):
        try:
            if self.stage("source-and-prerequisites", self.preflight):
                if self.stage("native-test-build", lambda: self.compile(True)):
                    for name in TESTS:
                        self.stage(name.rsplit("::", 1)[-1], lambda name=name: self.run_native_test(name))
                # A large test build can fail while the production binary is
                # still buildable. Preserve and attempt this separate branch.
                if not STOP_SIGNAL and self.stage("production-binary-build", lambda: self.compile(False)):
                    self.stage("independent-black-box", self.black_box)
        finally:
            if self.source_before is not None and hasattr(self, "compiler"):
                self.stage("source-and-binary-recheck", self.recheck)
            statuses = {item["status"] for item in self.report["stages"]}
            complete = (len(self.report["native_tests"]) == len(TESTS)
                        and all(item["status"] == "PASS" for item in self.report["native_tests"])
                        and any(item["name"] == "independent-black-box" and item["status"] == "PASS"
                                for item in self.report["stages"])
                        and any(item["name"] == "source-and-binary-recheck" and item["status"] == "PASS"
                                for item in self.report["stages"]))
            self.report["native_qualified"] = complete and statuses == {"PASS"} and not STOP_SIGNAL
            self.report["status"] = "PASS" if self.report["native_qualified"] else "FAIL" if "FAIL" in statuses else "INCOMPLETE"
            self.report["interrupted_signal"] = STOP_SIGNAL
            self.report["artifacts"] = self.artifacts
            self.report["exit_code"] = {"PASS": 0, "FAIL": 1, "INCOMPLETE": 2}[self.report["status"]]
            write_json(self.evidence / "summary.json", self.report)
        return self.report


def main(argv=None):
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--expected-source", required=True)
    parser.add_argument("--evidence", type=Path, required=True, help="New directory; existing evidence is never overwritten")
    parser.add_argument("--repo", type=Path, default=Path(__file__).resolve().parents[2])
    parser.add_argument("--node", default="node")
    parser.add_argument("--toolchain-manifest", type=Path,
                        help="Pinned distribution TOML for provisioned cargo/rustc when rustup is absent")
    args = parser.parse_args(argv)
    if sys.platform != "linux" or not re.fullmatch(r"[0-9a-f]{40}", args.expected_source):
        parser.error("Linux and a full lowercase expected commit SHA are required")

    def interrupted(signum, _frame):
        global STOP_SIGNAL
        STOP_SIGNAL = signum

    old_handlers = {signum: signal.signal(signum, interrupted) for signum in (signal.SIGTERM, signal.SIGINT)}
    owned_evidence = False
    try:
        args.evidence = args.evidence.resolve()
        args.evidence.mkdir(mode=0o700, parents=True, exist_ok=False)
        owned_evidence = True
        qualification = Qualification(args)
        result = qualification.execute()
        print(json.dumps({"status": result["status"], "native_qualified": result["native_qualified"],
                          "summary": str(qualification.evidence / "summary.json")}))
        return result["exit_code"]
    except (Refusal, OSError, ValueError) as error:
        print("Qualification incomplete: " + str(error), file=sys.stderr)
        if owned_evidence and args.evidence.is_dir() and not (args.evidence / "summary.json").exists():
            write_json(args.evidence / "summary.json", {"schema_version": 1,
                "evidence_class": "native_current_source", "status": "INCOMPLETE",
                "native_qualified": False, "exit_code": 2, "error": str(error)})
        return 2
    finally:
        for signum, handler in old_handlers.items():
            signal.signal(signum, handler)


if __name__ == "__main__":
    raise SystemExit(main())
