"""Bounded, owned-process execution for the GH362 evidence runners."""

from contextlib import nullcontext
from datetime import datetime, timezone
import hashlib
import json
import os
from pathlib import Path
import re
import secrets
import signal
import subprocess
import threading
import time


OWNER_VARIABLE = "AM_GH362_RUNNER_OWNER"
_PROCESS_DIRECTORIES = {}


BUILD_PARITY_FIELDS = ("compiler", "profile", "features", "rustflags", "wrapper_sha256")
REQUIRED_STORAGE_SOURCES = {
    "Cargo.toml", "Cargo.lock", "crates/mcp-agent-mail-storage/Cargo.toml",
    "crates/mcp-agent-mail-storage/src/lib.rs",
    "crates/mcp-agent-mail-storage/src/canonical_id_tests.rs",
}


def relative_source_name(name):
    return isinstance(name, str) and bool(name) and not Path(name).is_absolute() and ".." not in Path(name).parts


def declared_source_names(receipt, arm):
    try:
        names = receipt["arms"][arm]["sources"]["files_sha256_before"]
    except (TypeError, KeyError):
        return []
    return [name for name in names if relative_source_name(name)] if isinstance(names, dict) else []


def source_snapshot(source, names):
    source = source.resolve(strict=True)
    files = {}
    for name in names:
        if not relative_source_name(name):
            continue
        path = source / name
        if not path.is_file() or not path.resolve().is_relative_to(source):
            continue
        with path.open("rb") as data:
            files[name] = hashlib.file_digest(data, "sha256").hexdigest()
    return {
        "head": subprocess.check_output(["git", "-C", str(source), "rev-parse", "HEAD"], text=True).strip(),
        "files_sha256": files,
    }


def bind_build_receipt(receipt, actual_arms, paired=False):
    """Bind supplied records to the binaries and optional observed checkouts.

    Missing receipts remain explicitly unqualified. Supplied but inconsistent
    receipts are errors, never a reason to silently drop qualification checks.
    Paths in the receipt are informational; SHA256 values are authoritative.
    """
    result = {"supplied": receipt is not None, "qualified": False, "errors": [], "source_coverage": {}}
    errors = result["errors"]
    if receipt is None:
        return result
    if not isinstance(receipt, dict) or type(receipt.get("schema_version")) is not int or receipt["schema_version"] != 1:
        errors.append("build receipt must use schema_version 1")
        return result
    arms = receipt.get("arms")
    if not isinstance(arms, dict):
        errors.append("build receipt has no arms map")
        return result
    builds = {}
    for arm, actual in actual_arms.items():
        recorded = arms.get(arm)
        if not isinstance(recorded, dict):
            errors.append(f"build receipt is missing arm {arm}")
            continue
        binaries = recorded.get("binaries", {})
        if not isinstance(binaries, dict):
            binaries = {}
        for component, observed in actual["binaries"].items():
            artifact = binaries.get(component)
            expected = artifact.get("sha256") if isinstance(artifact, dict) else None
            if not isinstance(expected, str) or not re.fullmatch(r"[0-9a-f]{64}", expected) or expected != observed:
                errors.append(f"{arm}.{component} binary SHA256 differs from its build receipt")
        sources = recorded.get("sources", {})
        if not isinstance(sources, dict):
            sources = {}
        before = sources.get("files_sha256_before")
        after = sources.get("files_sha256_after")
        valid_sources = isinstance(before, dict) and bool(before) and before == after
        if valid_sources:
            valid_sources = all(
                relative_source_name(name) and isinstance(value, str) and re.fullmatch(r"[0-9a-f]{64}", value)
                for name, value in before.items()
            )
        required_sources = set(REQUIRED_STORAGE_SOURCES) if "storage" in actual["binaries"] else set()
        if arm == "candidate" and "storage" in actual["binaries"]:
            required_sources.add("crates/mcp-agent-mail-storage/src/canonical_watch.rs")
        if "core" in actual["binaries"]:
            required_sources.add("crates/mcp-agent-mail-core/Cargo.toml")
        if not valid_sources or not required_sources.issubset(before):
            errors.append(f"{arm} build sources are missing, changed during compilation, or incomplete")
        head = sources.get("head")
        if not isinstance(head, str) or not re.fullmatch(r"[0-9a-f]{40}|[0-9a-f]{64}", head):
            errors.append(f"{arm} build source HEAD is missing or invalid")
        observed_sources = actual.get("sources")
        if observed_sources is not None:
            if head != observed_sources.get("head"):
                errors.append(f"{arm} observed source HEAD differs from its build receipt")
            if valid_sources:
                observed_files = observed_sources.get("files_sha256", {})
                for name, expected in before.items():
                    if observed_files.get(name) != expected:
                        errors.append(f"{arm} observed source differs from its build receipt: {name}")
        result["source_coverage"][arm] = {
            "captured_files": sorted(name for name in before if isinstance(name, str)) if isinstance(before, dict) else [],
            "observed_checkout_checked": observed_sources is not None,
            "scope": "declared source files and HEAD; not a hash of every compiled dependency source",
        }
        build = recorded.get("build", {})
        if not isinstance(build, dict):
            build = {}
        if type(build.get("exit_code")) is not int or build["exit_code"] != 0:
            errors.append(f"{arm} build did not finish successfully")
        for field in BUILD_PARITY_FIELDS:
            if field not in build:
                errors.append(f"{arm} build is missing {field}")
        if not isinstance(build.get("compiler"), str) or not build["compiler"]:
            errors.append(f"{arm} compiler identity is invalid")
        if (
            not isinstance(build.get("profile"), (str, dict)) or not build["profile"]
            or not isinstance(build.get("features"), (dict, list)) or not build["features"]
            or not isinstance(build.get("rustflags"), str)
        ):
            errors.append(f"{arm} build profile, features, or Rust flags are invalid")
        wrapper = build.get("wrapper_sha256")
        if wrapper is not None and (not isinstance(wrapper, str) or not re.fullmatch(r"[0-9a-f]{64}", wrapper)):
            errors.append(f"{arm} compiler wrapper SHA256 is invalid")
        builds[arm] = build
    if paired:
        if set(actual_arms) != {"baseline", "candidate"} or set(builds) != {"baseline", "candidate"}:
            errors.append("paired qualification requires baseline and candidate build records")
        else:
            for field in BUILD_PARITY_FIELDS:
                if builds["baseline"].get(field) != builds["candidate"].get(field):
                    errors.append(f"paired build field differs: {field}")
    result["qualified"] = not errors
    return result


def status_values(directory):
    return dict(line.split(":", 1) for line in (directory / "status").read_text().splitlines() if ":" in line)


try:
    # /proc can be mounted from an ancestor namespace. Popen and kill use this
    # process's PID namespace, whereas numeric /proc directory names may not.
    _PID_NAMESPACE_DEPTH = len(status_values(Path("/proc/self"))["NSpid"].split()) - 1
except (OSError, KeyError):
    _PID_NAMESPACE_DEPTH = None


def namespace_id(status, field):
    values = status.get(field, "").split()
    if _PID_NAMESPACE_DEPTH is None or len(values) <= _PID_NAMESPACE_DEPTH:
        return None
    return int(values[_PID_NAMESPACE_DEPTH])


def has_owner_marker(directory, token):
    marker = f"{OWNER_VARIABLE}={token}".encode()
    return marker in (directory / "environ").read_bytes().split(b"\0")


def owned_process_directory(pid, token):
    for directory in Path("/proc").iterdir():
        if not directory.name.isdigit():
            continue
        try:
            status = status_values(directory)
            if namespace_id(status, "NSpid") == pid and has_owner_marker(directory, token):
                return directory
        except (OSError, ValueError):
            continue
    return None


class RunnerSignalInterrupt(BaseException):
    def __init__(self, signum):
        self.signum = signum


def utc_now():
    return datetime.now(timezone.utc).isoformat()


def process_metrics(pid, token):
    try:
        directory = _PROCESS_DIRECTORIES.get((pid, token))
        if directory is None:
            directory = owned_process_directory(pid, token)
            if directory is None:
                return None
            _PROCESS_DIRECTORIES[(pid, token)] = directory
        status = status_values(directory)
    except OSError:
        return None
    values = {}
    for key in ("VmRSS", "VmHWM", "Threads"):
        if key in status:
            values[key] = int(status[key].strip().split()[0])
    try:
        values["open_fds"] = len(list((directory / "fd").iterdir()))
    except OSError:
        values["open_fds"] = None
    values["pid"] = pid
    values["proc_pid"] = int(directory.name)
    values["time"] = utc_now()
    return values


def owned_group_members(group_id, token):
    """Check only this session/group and require its inherited private marker.

    The marker prevents a recycled PID/process-group number from authorizing a
    signal to an unrelated process after the original leader has been reaped.
    Environment contents are never returned or logged.
    """
    members = []
    try:
        processes = Path("/proc").iterdir()
        for entry in processes:
            if not entry.name.isdigit():
                continue
            try:
                status = status_values(entry)
                if namespace_id(status, "NSpgid") != group_id or namespace_id(status, "NSsid") != group_id:
                    continue
                if status.get("State", "").strip().startswith("Z"):
                    continue
                pid = namespace_id(status, "NSpid")
                if pid is not None and has_owner_marker(entry, token):
                    members.append(pid)
            except (OSError, ValueError):
                continue
    except OSError:
        return []
    return members


def signal_verified_member(pid, group_id, token, signum):
    """Pin an orphan's process identity before checking its inherited marker."""
    if not hasattr(os, "pidfd_open") or not hasattr(signal, "pidfd_send_signal"):
        return False
    try:
        descriptor = os.pidfd_open(pid)
    except ProcessLookupError:
        return False
    try:
        if pid not in owned_group_members(group_id, token):
            return False
        try:
            signal.pidfd_send_signal(descriptor, signum)
        except ProcessLookupError:
            return False
        return True
    finally:
        os.close(descriptor)


def signal_owned_group(process, token, signum):
    members = owned_group_members(process.pid, token)
    # While Popen's original child is still unreaped and running, its PID
    # cannot be reused. This also permits cleanup when /proc is unavailable.
    live_original_leader = False
    if process.poll() is None:
        try:
            live_original_leader = (
                os.getpgid(process.pid) == process.pid
                and os.getsid(process.pid) == process.pid
            )
        except ProcessLookupError:
            pass
    if live_original_leader:
        try:
            os.killpg(process.pid, signum)
        except ProcessLookupError:
            return False
        return True
    # With a reaped leader the numeric group can be recycled between a /proc
    # check and killpg. Signal pinned, marker-verified descendants individually.
    signaled = False
    for pid in members:
        signaled = signal_verified_member(pid, process.pid, token, signum) or signaled
    return signaled


def stop_owned_group(process, token, grace_seconds=2.0):
    signals = []
    if signal_owned_group(process, token, signal.SIGTERM):
        signals.append("SIGTERM")
    deadline = time.monotonic() + grace_seconds
    while time.monotonic() < deadline:
        process.poll()
        if process.returncode is not None and not owned_group_members(process.pid, token):
            break
        time.sleep(0.05)
    if signal_owned_group(process, token, signal.SIGKILL):
        signals.append("SIGKILL")
    if process.poll() is None:
        # An owned leader that changed groups is still ours, but does not
        # authorize signaling its new group. Popen rechecks child ownership.
        process.kill()
        signals.append("direct_child_SIGKILL")
    try:
        process.wait(timeout=5)
    except subprocess.TimeoutExpired:
        pass
    deadline = time.monotonic() + 1.0
    while time.monotonic() < deadline and owned_group_members(process.pid, token):
        time.sleep(0.05)
    return {
        "signals": signals,
        "remaining_owned_group_members": owned_group_members(process.pid, token),
    }


def run_logged(command, environment, log_path, timeout, telemetry_path=None):
    """Retain raw output and clean the owned group after all abnormal exits."""
    token = secrets.token_hex(24)
    environment = environment.copy()
    environment[OWNER_VARIABLE] = token
    started = time.monotonic()
    process = None
    interrupted = False
    timed_out = False
    error = None
    cleanup = None
    previous_term = None
    previous_int = None
    term_handler_installed = threading.current_thread() is threading.main_thread()
    if term_handler_installed:
        previous_term = signal.getsignal(signal.SIGTERM)
        previous_int = signal.getsignal(signal.SIGINT)
        def interrupt_on_term(signum, _frame):
            raise RunnerSignalInterrupt(signum)
        signal.signal(signal.SIGTERM, interrupt_on_term)
    try:
        with log_path.open("x") as log, (
            telemetry_path.open("x") if telemetry_path else nullcontext(None)
        ) as telemetry:
            process = subprocess.Popen(
                command, env=environment, stdout=log, stderr=subprocess.STDOUT,
                start_new_session=True,
            )
            while process.poll() is None:
                if telemetry:
                    snapshot = process_metrics(process.pid, token)
                    if snapshot is not None:
                        telemetry.write(json.dumps(snapshot) + "\n")
                        telemetry.flush()
                if time.monotonic() - started > timeout:
                    timed_out = True
                    break
                time.sleep(0.25)
    except KeyboardInterrupt:
        interrupted = True
        error = "KeyboardInterrupt"
    except RunnerSignalInterrupt as failure:
        interrupted = True
        error = signal.Signals(failure.signum).name
    except Exception as failure:
        error = f"{type(failure).__name__}: {failure}"
    finally:
        if term_handler_installed:
            # A second terminal signal must not abandon the bounded cleanup.
            signal.signal(signal.SIGINT, signal.SIG_IGN)
            signal.signal(signal.SIGTERM, signal.SIG_IGN)
        try:
            if process is not None:
                if timed_out or interrupted or error or process.poll() is None:
                    cleanup = stop_owned_group(process, token)
                else:
                    remaining = owned_group_members(process.pid, token)
                    if remaining:
                        error = "worker exited with live owned descendants"
                        cleanup = stop_owned_group(process, token)
        except Exception as failure:
            cleanup_error = f"{type(failure).__name__}: {failure}"
            error = f"{error}; cleanup failed: {cleanup_error}" if error else f"cleanup failed: {cleanup_error}"
            cleanup = {"cleanup_error": cleanup_error}
        finally:
            if process is not None:
                _PROCESS_DIRECTORIES.pop((process.pid, token), None)
            if term_handler_installed:
                signal.signal(signal.SIGTERM, previous_term)
                signal.signal(signal.SIGINT, previous_int)
    return {
        "elapsed_seconds": time.monotonic() - started,
        "exit_code": process.returncode if process else None,
        "timed_out": timed_out,
        "interrupted": interrupted,
        "execution_error": error,
        "cleanup": cleanup,
    }
