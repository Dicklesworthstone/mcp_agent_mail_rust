#!/usr/bin/env python3
"""Qualify exact GH383 source; no mocks, dependency edits or release actions."""

import argparse
import hashlib
import json
import os
from pathlib import Path
import platform
import re
import signal
import subprocess
import time


def checked_output(*command):
    return subprocess.check_output(command, text=True).strip()


def terminate_owned_group(process):
    """Keep the leader unreaped until every member of our group is stopped."""
    previous = {
        signum: signal.signal(signum, signal.SIG_IGN)
        for signum in (signal.SIGINT, signal.SIGTERM)
    }
    try:
        try:
            os.killpg(process.pid, signal.SIGTERM)
        except ProcessLookupError:
            pass
        time.sleep(0.25)
        # Do this even if TERM ended the leader: a descendant may ignore TERM.
        # The unreaped leader prevents its PID/group number from being reused.
        try:
            os.killpg(process.pid, signal.SIGKILL)
        except ProcessLookupError:
            pass
        process.wait()
    finally:
        for signum, handler in previous.items():
            signal.signal(signum, handler)


def interrupted(signum, _frame):
    raise InterruptedError(f"qualification interrupted by signal {signum}")


def run_gate(evidence, name, command, *, deadline, tests=False, required=(), timeout=7200):
    """Retain real status, reject zero tests, and bound the complete job."""
    started = time.monotonic()
    timeout = min(timeout, max(0, int(deadline - started - 15)))
    log_path = evidence / f"{name}.log"
    with log_path.open("w") as log:
        if timeout == 0:
            status = "job_budget_exhausted"
        else:
            process = subprocess.Popen(
                command,
                stdin=subprocess.DEVNULL,
                stdout=log,
                stderr=subprocess.STDOUT,
                start_new_session=True,
            )
            try:
                status = process.wait(timeout=timeout)
            except subprocess.TimeoutExpired:
                terminate_owned_group(process)
                status = "timeout"
            except BaseException:
                terminate_owned_group(process)
                raise
    output = log_path.read_text(errors="replace")
    summaries = re.findall(r"^test result:.*$", output, re.MULTILINE)
    final = summaries[-1] if summaries else None
    missing = [
        test for test in required
        if not re.search(
            r"^test (?:[^\s]+::)?" + re.escape(test) + r" \.\.\.",
            output, re.MULTILINE,
        )
    ]
    passed = status == 0 and not missing and (
        not tests
        or bool(re.match(r"test result: ok\. [1-9][0-9]* passed; 0 failed; 0 ignored;", final or ""))
    )
    cpu = None
    cpu_error = None
    if name == "archive-maintenance" and passed:
        matches = re.findall(r"GH383 populated maintenance receipt: (\{[^\n]+\})", output)
        try:
            cpu = json.loads(matches[-1]) if matches else None
        except json.JSONDecodeError as error:
            cpu_error = str(error)
        timing_valid = isinstance(cpu, dict) and all(
            type(cpu.get(key)) is int
            for key in ("wall_ms", "index_bytes", "thread_cpu_ms")
        )
        if not timing_valid and cpu_error is None:
            cpu_error = "missing or invalid populated-worker receipt"
        passed = bool(
            timing_valid
            and cpu.get("source_messages") == 256 and cpu.get("committed_messages") == 256
            and cpu["wall_ms"] >= 30_000 and cpu["index_bytes"] > 30 * 1024 * 1024
            and 0 <= cpu["thread_cpu_ms"] * 100 <= cpu["wall_ms"] * 35
            and cpu.get("reporting_enabled") is False and cpu.get("quota_enabled") is False
        )
    receipt = {
        "gate": name,
        "command": command,
        "exit": status,
        "elapsed_seconds": round(time.monotonic() - started, 3),
        "test_summary": final,
        "required_tests": list(required),
        "missing_tests": missing,
        "populated_worker": cpu,
        "populated_worker_error": cpu_error,
        "log_sha256": hashlib.sha256(log_path.read_bytes()).hexdigest(),
        "passed": passed,
    }
    (evidence / f"{name}.json").write_text(json.dumps(receipt, indent=2) + "\n")
    print(json.dumps(receipt), flush=True)
    if not passed:
        print("\n".join(output.splitlines()[-100:]), flush=True)
    return receipt


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--component", choices=("checks", "db", "storage", "server"), required=True)
    parser.add_argument("--expected-source", required=True)
    parser.add_argument("--evidence", type=Path, required=True)
    args = parser.parse_args()
    args.evidence.mkdir(parents=True, exist_ok=True)
    signal.signal(signal.SIGTERM, interrupted)
    signal.signal(signal.SIGINT, interrupted)
    # Leave thirty minutes of the workflow step for cleanup and artifact upload.
    deadline = time.monotonic() + 240 * 60
    source = checked_output("git", "rev-parse", "HEAD")
    if source != args.expected_source:
        raise SystemExit(f"source mismatch: expected {args.expected_source}, got {source}")
    dirty = checked_output("git", "status", "--porcelain", "--untracked-files=no")
    if dirty:
        raise SystemExit(f"qualification requires unchanged tracked source:\n{dirty}")
    environment = {
        "source": source,
        "component": args.component,
        "runtime_features": "portable" if args.component != "checks" else "workspace defaults",
        "platform": platform.platform(),
        "rustc": checked_output("rustc", "-vV"),
        "cargo": checked_output("cargo", "-vV"),
        "memory": Path("/proc/meminfo").read_text(),
        "settings": {
            key: os.environ.get(key)
            for key in (
                "CARGO_BUILD_JOBS", "CARGO_INCREMENTAL", "CARGO_PROFILE_DEV_DEBUG",
                "CARGO_PROFILE_TEST_DEBUG", "RUST_TEST_THREADS", "RUSTFLAGS", "TMPDIR",
            )
        },
    }
    (args.evidence / "environment.json").write_text(json.dumps(environment, indent=2) + "\n")
    # The source's package-specific debug=1 override otherwise defeats the
    # runner's global debug=0 setting. Dependency versions/features are intact.
    cargo = [
        "cargo",
        "--config", "profile.dev.package.mcp-agent-mail-server.debug=0",
        "--config", "profile.test.package.mcp-agent-mail-server.debug=0",
    ]
    receipts = []
    if args.component == "checks":
        for name, arguments in (
            ("format", ["fmt", "--all", "--", "--check"]),
            ("all-targets", ["check", "--locked", "--workspace", "--all-targets"]),
            ("production", ["check", "--locked", "--workspace", "--lib", "--bins"]),
            ("clippy", ["clippy", "--locked", "--workspace", "--all-targets", "--", "-D", "warnings"]),
        ):
            receipts.append(run_gate(args.evidence, name, cargo + arguments, deadline=deadline))
    else:
        package = f"mcp-agent-mail-{args.component}"
        native = cargo + [
            "test", "--locked", "-p", package, "--lib",
            "--no-default-features", "--features", "portable",
        ]
        build = run_gate(args.evidence, "build", native + ["--no-run"], deadline=deadline)
        receipts.append(build)
        if build["passed"]:
            scopes = {
                "db": [
                    ("snapshot", "snapshot::", (
                        "bounded_retention_search_resumes_and_reverifies_the_last_successful_pin",
                        "bounded_search_completes_more_than_one_hundred_twenty_eight_nonmatching_images",
                    )),
                    ("recovery-retention", "recovery_retention::", ()),
                ],
                "storage": [
                    ("staged-index", "recovery::message_reconcile::identity::staged::", (
                        "reporter_sized_index_reuses_decoding_without_raising_the_retention_bound",
                    )),
                    ("message-source", "recovery::message_reconcile::database::", (
                        "gh383_cooling_retry_survives_checkpoint_restart_while_new_tail_converges",
                    )),
                    ("agent-source", "recovery::agent_reconcile::", (
                        "gh383_cooling_roster_page_advances_without_consuming_debt",
                    )),
                    ("reservation-source", "recovery::reservation_reconcile::", ()),
                ],
                "server": [
                    ("backup-rotation", "backup_rotation::tests::", (
                        "disappearing_reservation_temp_does_not_refuse_backup_inventory",
                        "disappearing_backup_candidate_keeps_inventory_incomplete",
                        "two_hundred_twelve_mixed_images_drain_across_bounded_passes",
                    )),
                    ("integrity-guard", "integrity_guard::tests::", (
                        "bounded_rotation_continuations_rest_respect_gates_and_never_publish",
                        "deferred_pin_search_preserves_repair_allowance_until_a_complete_sweep",
                    )),
                    ("archive-maintenance", "retention::archive::", (
                        "gh383_populated_maintenance_paces_cpu_and_reaches_old_orphan",
                        "gh383_busy_lanes_share_one_quantum_and_twenty_percent_duty",
                    )),
                ],
            }[args.component]
            for name, test_filter, required in scopes:
                receipts.append(run_gate(
                    args.evidence, name,
                    native + [test_filter, "--", "--nocapture", "--test-threads=1"],
                    deadline=deadline, tests=True, required=required, timeout=1800,
                ))
            if args.component == "db":
                receipts.append(run_gate(
                    args.evidence, "linux-retention",
                    cargo + [
                        "test", "--locked", "-p", package, "--test", "it",
                        "--no-default-features", "--features", "portable",
                        "retention_linux::", "--", "--nocapture", "--test-threads=1",
                    ],
                    deadline=deadline, tests=True, timeout=3600,
                    required=(
                        "inaccessible_same_user_process_does_not_block_eight_backups",
                        "visible_reader_still_refuses_deletion_amid_inaccessible_processes",
                        "exited_thread_group_leader_does_not_hide_a_live_backup_reader",
                        "real_unlink_permission_refusal_preserves_bytes_and_accounting",
                        "concurrent_cleanup_and_process_restart_preserve_recovery_evidence",
                    ),
                ))
    unchanged = (
        checked_output("git", "rev-parse", "HEAD") == source
        and not checked_output("git", "status", "--porcelain", "--untracked-files=no")
    )
    result = {"source": source, "unchanged_source": unchanged, "gates": receipts}
    (args.evidence / "summary.json").write_text(json.dumps(result, indent=2) + "\n")
    return 0 if unchanged and all(receipt["passed"] for receipt in receipts) else 1


if __name__ == "__main__":
    raise SystemExit(main())
