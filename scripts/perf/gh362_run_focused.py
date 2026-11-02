#!/usr/bin/env python3
"""Run focused native storage/core regressions without invoking Cargo."""

import argparse
from datetime import datetime, timezone
import hashlib
import json
import os
from pathlib import Path
import re

from gh362_runner_common import bind_build_receipt, declared_source_names, run_logged, source_snapshot


ALLOWED_IGNORED_TESTS = {
    "canonical_id_tests::canonical_publication_public_writer_benchmark",
    "canonical_id_tests::canonical_publication_scale_benchmark",
    "reservation_artifact::tests::exact_generation_lookup_history_scaling",
}


STORAGE_CASES = [
    ("watch-adapter", "canonical_watch::", False),
    ("publication-regressions", "canonical_id_tests::", False),
    ("existing-canonical-index", "tests::canonical_id_", False),
    ("existing-publication", "tests::message_publication_", False),
    ("batch-preflight", "tests::message_batch_", False),
    ("batch-publication", "tests::write_message_batch_bundle_", False),
    ("durable-staging", "tests::staged_archive_writes_", False),
    (
        "git-index-preservation",
        "tests::archive_commit_strategies_preserve_staged_locked_and_corrupt_indexes",
        True,
    ),
    ("background-index-contract", "tests::archive_index_sync_", False),
    ("reservation-publication", "tests::reservation_publication_", False),
    (
        "generation-stamped-artifact",
        "tests::test_write_file_reservation_record_stamps_generation",
        True,
    ),
    (
        "reservation-background-fence",
        "tests::reservation_writes_stage_outside_the_fence_and_publish_inside_it",
        True,
    ),
    (
        "reservation-direct-fence",
        "tests::direct_reservation_writes_stage_outside_the_fence",
        True,
    ),
    (
        "unrelated-reservation-progress",
        "tests::wbq_message_identity_scan_allows_unrelated_reservations",
        True,
    ),
    (
        "missing-project-before-fence",
        "tests::message_preparation_refuses_a_missing_project_without_waiting_for_the_fence",
        True,
    ),
]


def digest(path):
    with path.open("rb") as source:
        return hashlib.file_digest(source, "sha256").hexdigest()


def inventory(binary, component, output, timeout, ignored=False):
    label = f"{component}-{'ignored-' if ignored else ''}inventory"
    log_path = output / f"{label}.log"
    command = [str(binary), "--list"]
    if ignored:
        command.append("--ignored")
    execution = run_logged(command, os.environ.copy(), log_path, timeout)
    content = log_path.read_text(errors="replace") if log_path.exists() else ""
    names = [line[:-6] for line in content.splitlines() if line.endswith(": test")]
    valid = (
        execution["exit_code"] == 0 and not execution["timed_out"]
        and not execution["interrupted"] and not execution["execution_error"]
        and len(names) == len(set(names)) and (bool(names) or ignored)
    )
    receipt = {
        "command": command, **execution, "tests": names, "valid": valid,
        "log": log_path.name, "log_sha256": digest(log_path) if log_path.exists() else None,
    }
    (output / f"{label}.json").write_text(json.dumps(receipt, indent=2) + "\n")
    if not valid:
        raise RuntimeError(f"native test inventory failed; see {label}.json")
    return names


def assess_summary(content, selected, ignored, execution):
    summaries = re.findall(
        r"test result: (ok|FAILED)\. (\d+) passed; (\d+) failed; (\d+) ignored;",
        content,
    )
    summary = summaries[-1] if summaries else None
    counts = {
        "passed": int(summary[1]), "failed": int(summary[2]), "ignored": int(summary[3]),
    } if summary else None
    expected_ignored = len(set(selected).intersection(ignored))
    expected_active = len(selected) - expected_ignored
    unexpected_ignored = sorted(set(selected).intersection(ignored) - ALLOWED_IGNORED_TESTS)
    complete = bool(counts and sum(counts.values()) == len(selected))
    success = bool(
        execution["exit_code"] == 0 and not execution["timed_out"]
        and not execution["interrupted"] and not execution["execution_error"]
        and complete and summary[0] == "ok" and counts["failed"] == 0
        and counts["passed"] == expected_active and counts["passed"] > 0
        and counts["ignored"] == expected_ignored
        and not unexpected_ignored
    )
    return {
        "counts": counts, "terminal_verdict": summary[0] if summary else None,
        "expected_active_tests": expected_active, "expected_ignored_tests": expected_ignored,
        "unexpected_ignored_tests": unexpected_ignored,
        "complete_test_record": complete, "success": success,
    }


def run_case(binary, tests, ignored, label, selector, exact, output, timeout):
    selected = [name for name in tests if name == selector or (not exact and selector in name)]
    if not selected:
        raise RuntimeError(f"selector {selector!r} matches no native tests")
    fixture_root = output / f"{label}-fixtures"
    fixture_root.mkdir()
    environment = os.environ.copy()
    for variable in (
        "AM_CANONICAL_ID_REGRESSION_CHILD",
        "AM_CANONICAL_ID_WORKER_ACTION",
        "AM_CANONICAL_BENCH_FOREIGN_ROOT",
        "AM_CANONICAL_BENCH_FOREIGN_ID",
    ):
        environment.pop(variable, None)
    environment.update({
        "STORAGE_ROOT": str(fixture_root),
        "TMPDIR": str(fixture_root),
        "AM_COALESCER_MAX_WORKERS": "1",
        "RUST_TEST_THREADS": "1",
    })
    command = [str(binary), selector, "--nocapture", "--test-threads=1"]
    if exact:
        command.append("--exact")
    log_path = output / f"{label}.log"
    execution = run_logged(command, environment, log_path, timeout)
    content = log_path.read_text(errors="replace") if log_path.exists() else ""
    result = {
        "label": label,
        "command": command,
        "selected_tests": selected,
        **execution,
        **assess_summary(content, selected, ignored, execution),
        "log": log_path.name,
        "log_sha256": digest(log_path) if log_path.exists() else None,
    }
    (output / f"{label}.json").write_text(json.dumps(result, indent=2) + "\n")
    print(json.dumps({key: result[key] for key in (
        "label", "elapsed_seconds", "exit_code", "counts", "success"
    )}), flush=True)
    return result


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--storage-binary", type=Path, required=True)
    parser.add_argument("--core-binary", type=Path)
    parser.add_argument("--build-receipt", type=Path)
    parser.add_argument("--source", type=Path, help="optional candidate checkout to bind to captured build source hashes")
    parser.add_argument("--output", type=Path, required=True)
    parser.add_argument("--timeout-seconds", type=int, default=300)
    arguments = parser.parse_args()
    if arguments.timeout_seconds <= 0:
        parser.error("timeout must be positive")
    arguments.output = arguments.output.resolve()
    arguments.output.mkdir(parents=True, exist_ok=False)
    binaries = {"storage": arguments.storage_binary.resolve(strict=True)}
    if arguments.core_binary:
        binaries["core"] = arguments.core_binary.resolve(strict=True)
    build_receipt = json.loads(arguments.build_receipt.read_text()) if arguments.build_receipt else None
    binary_receipts = {name: {"path": str(binary), "sha256": digest(binary)} for name, binary in binaries.items()}
    observed_source = source_snapshot(
        arguments.source, declared_source_names(build_receipt, "candidate"),
    ) if arguments.source else None
    binding = bind_build_receipt(build_receipt, {"candidate": {
        "binaries": {name: value["sha256"] for name, value in binary_receipts.items()},
        "sources": observed_source,
    }})
    manifest = {
        "started": datetime.now(timezone.utc).isoformat(),
        "runner_sha256": digest(Path(__file__)),
        "runner_common_sha256": digest(Path(__file__).with_name("gh362_runner_common.py")),
        "binaries": binary_receipts,
        "source": observed_source,
        "build_receipt": build_receipt,
        "build_receipt_binding": binding,
        "test_inventories": {},
        "ignored_test_inventories": {},
        "allowed_ignored_tests": sorted(ALLOWED_IGNORED_TESTS),
        "runs": [],
    }
    manifest_path = arguments.output / "manifest.json"
    manifest_path.write_text(json.dumps(manifest, indent=2) + "\n")
    tests = {}
    ignored = {}
    cases = [("storage", *case) for case in STORAGE_CASES]
    if "core" in binaries:
        cases.append(("core", "reservation-generation-lookup", "reservation_artifact::tests::", False))
    try:
        if binding["errors"]:
            raise RuntimeError("build receipt binding failed: " + "; ".join(binding["errors"]))
        for component, binary in binaries.items():
            tests[component] = inventory(binary, component, arguments.output, arguments.timeout_seconds)
            ignored[component] = inventory(
                binary, component, arguments.output, arguments.timeout_seconds, ignored=True,
            )
            if not set(ignored[component]).issubset(tests[component]):
                raise RuntimeError(f"{component} ignored inventory differs from complete inventory")
            manifest["test_inventories"][component] = tests[component]
            manifest["ignored_test_inventories"][component] = ignored[component]
            manifest_path.write_text(json.dumps(manifest, indent=2) + "\n")
        for component, label, selector, exact in cases:
            result = run_case(
                binaries[component], tests[component], ignored[component], label, selector, exact,
                arguments.output, arguments.timeout_seconds,
            )
            manifest["runs"].append(result)
            manifest_path.write_text(json.dumps(manifest, indent=2) + "\n")
            if not result["success"]:
                raise RuntimeError(f"focused native case {label} failed or ran no active tests")
    except BaseException as failure:
        manifest["failure"] = f"{type(failure).__name__}: {failure}"
        manifest["finished"] = datetime.now(timezone.utc).isoformat()
        manifest_path.write_text(json.dumps(manifest, indent=2) + "\n")
        raise
    manifest["finished"] = datetime.now(timezone.utc).isoformat()
    manifest_path.write_text(json.dumps(manifest, indent=2) + "\n")


if __name__ == "__main__":
    main()
