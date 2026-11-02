#!/usr/bin/env python3
"""Run retained incumbent/candidate libtest binaries in one paired invocation.

This is a storage-mechanism experiment, not an application or release gate.
The selected Rust worker performs real public writes and emits raw counters.
No verifier is implemented or substituted here, and no source file is edited.
"""

import argparse
import hashlib
import json
import os
from pathlib import Path
import platform
import re
import statistics
import subprocess
from datetime import datetime, timezone

from gh362_runner_common import bind_build_receipt, declared_source_names, run_logged, source_snapshot


SELECTOR = "canonical_id_tests::canonical_publication_public_writer_benchmark"
SAMPLE_KIND = "canonical_publication_public_sample"
SETUP_KIND = "canonical_publication_public_setup"
SCENARIOS = ("warm_single_writer", "foreign_append", "controlled_fence_deferral")
METRICS = (
    "publication_us", "canonical_read_dirs", "canonical_metadata_calls",
    "publication_thread_fence_held_us", "publication_thread_project_lock_held_us",
    "async_commit_flush_us",
)


def utc_now():
    return datetime.now(timezone.utc).isoformat()


def digest(path):
    with path.open("rb") as source:
        return hashlib.file_digest(source, "sha256").hexdigest()


def read_optional(path):
    try:
        return Path(path).read_text().strip()
    except OSError:
        return None


def git(source, *args):
    return subprocess.check_output(
        ["git", "-C", str(source), *args], text=True
    ).strip()


def source_receipt(source, declared_names=()):
    files = [
        "Cargo.toml", "Cargo.lock", "AGENTS.md",
        "crates/mcp-agent-mail-storage/Cargo.toml",
        "crates/mcp-agent-mail-core/Cargo.toml",
        "crates/mcp-agent-mail-storage/src/lib.rs",
        "crates/mcp-agent-mail-storage/src/canonical_id_tests.rs",
        "crates/mcp-agent-mail-storage/src/canonical_watch.rs",
    ]
    return {
        "directory": str(source),
        **source_snapshot(source, set(files).union(declared_names)),
        "storage_head_blob": git(
            source, "rev-parse", "HEAD:crates/mcp-agent-mail-storage/src/lib.rs"
        ),
        "status": git(source, "status", "--short"),
    }


def host_receipt(output):
    cpu_model = next((
        line.split(":", 1)[1].strip()
        for line in (read_optional("/proc/cpuinfo") or "").splitlines()
        if line.startswith("model name")
    ), None)
    space = os.statvfs(output)
    return {
        "time": utc_now(),
        "host": platform.node(),
        "platform": platform.platform(),
        "cpu_model": cpu_model,
        "logical_cpus": os.cpu_count(),
        "affinity": sorted(os.sched_getaffinity(0)) if hasattr(os, "sched_getaffinity") else None,
        "load_average": os.getloadavg(),
        "cpu_governor": read_optional("/sys/devices/system/cpu/cpu0/cpufreq/scaling_governor"),
        "inotify_max_user_watches": read_optional("/proc/sys/fs/inotify/max_user_watches"),
        "inotify_max_user_instances": read_optional("/proc/sys/fs/inotify/max_user_instances"),
        "cgroup_memory_max": read_optional("/sys/fs/cgroup/memory.max"),
        "cgroup_memory_current": read_optional("/sys/fs/cgroup/memory.current"),
        "cgroup_cpu_max": read_optional("/sys/fs/cgroup/cpu.max"),
        "free_disk_bytes": space.f_bavail * space.f_frsize,
    }


def parse_rows(log_path):
    rows = []
    for line in log_path.read_text(errors="replace").splitlines():
        offset = line.find("{")
        if offset < 0:
            continue
        try:
            value = json.loads(line[offset:])
        except ValueError:
            continue
        if isinstance(value, dict) and value.get("kind") in (SAMPLE_KIND, SETUP_KIND):
            rows.append(value)
    return rows


def validate_rows(rows, count, rounds, scenarios, arm):
    """Validate against the requested experiment, never the worker's claims."""
    errors = []
    setups = [row for row in rows if row.get("kind") == SETUP_KIND]
    samples = [row for row in rows if row.get("kind") == SAMPLE_KIND]
    if len(setups) != 1:
        errors.append(f"expected one setup record, received {len(setups)}")
    else:
        setup = setups[0]
        if setup.get("scenarios") != list(scenarios):
            errors.append("worker scenario list differs from the requested experiment")
        if type(setup.get("rounds")) is not int or setup["rounds"] != rounds:
            errors.append("worker round count differs from the requested experiment")
        if (
            type(setup.get("historical_tracked_copies")) is not int
            or setup["historical_tracked_copies"] != count * 3
        ):
            errors.append("historical tracked-copy count differs from the fixture contract")
        oid = setup.get("seeded_git_tree_oid")
        if not isinstance(oid, str) or not re.fullmatch(r"[0-9a-f]{40}|[0-9a-f]{64}", oid):
            errors.append("missing or invalid seeded Git tree identity")
        for field in ("cold_publication_us", "setup_ms"):
            if type(setup.get(field)) is not int or setup[field] < 0:
                errors.append(f"invalid setup metric {field}")
    expected_keys = {(scenario, number) for scenario in scenarios for number in range(rounds)}
    actual_keys = []
    for index, sample in enumerate(samples):
        scenario = sample.get("scenario")
        number = sample.get("round")
        if scenario not in scenarios or type(number) is not int or not 0 <= number < rounds:
            errors.append(f"invalid scenario/round in sample {index}")
        else:
            actual_keys.append((scenario, number))
        for field in METRICS:
            if type(sample.get(field)) is not int or sample[field] < 0:
                errors.append(f"invalid sample {index} metric {field}")
    if len(samples) != len(expected_keys) or set(actual_keys) != expected_keys:
        errors.append("sample set is incomplete, duplicated, or differs from the requested experiment")
    for index, row in enumerate(rows):
        if type(row.get("history_messages")) is not int or row["history_messages"] != count:
            errors.append(f"history count differs in record {index}")
        watch = row.get("watch_supported_and_enrolled")
        if "watch_supported_and_enrolled" not in row or (
            arm == "baseline" and watch is not None
        ) or (arm == "candidate" and type(watch) is not bool):
            errors.append(f"invalid watch enrollment observer in record {index}")
    return errors


def run_arm(args, binaries, run_id, arm, count, purpose):
    log_path = args.output / f"{run_id}.log"
    telemetry_path = args.output / f"{run_id}.telemetry.jsonl"
    temp_root = args.output / f"{run_id}-temporary-fixtures"
    temp_root.mkdir()
    env = os.environ.copy()
    for variable in (
        "AM_CANONICAL_ID_WORKER_ACTION", "AM_CANONICAL_BENCH_FOREIGN_ROOT",
        "AM_CANONICAL_BENCH_FOREIGN_ID",
    ):
        env.pop(variable, None)
    env.update({
        "AM_CANONICAL_ID_REGRESSION_CHILD": SELECTOR,
        "AM_CANONICAL_BENCH_MESSAGES": str(count),
        "AM_CANONICAL_BENCH_ROUNDS": str(args.rounds),
        "AM_CANONICAL_BENCH_SCENARIO": args.scenario,
        "AM_COALESCER_MAX_WORKERS": "1",
        "RUST_TEST_THREADS": "1",
        "STORAGE_ROOT": str(temp_root),
        "TMPDIR": str(temp_root),
    })
    command = [str(binaries[arm]), "--exact", SELECTOR, "--ignored", "--nocapture", "--test-threads=1"]
    before = host_receipt(args.output)
    if before["free_disk_bytes"] < args.minimum_free_gib * 1024 ** 3:
        raise RuntimeError("insufficient free disk for the next retained fixture run")
    execution = run_logged(command, env, log_path, args.timeout_seconds, telemetry_path)
    scenarios = SCENARIOS if args.scenario == "all" else (args.scenario,)
    try:
        rows = parse_rows(log_path)
        errors = validate_rows(rows, count, args.rounds, scenarios, arm)
        summaries = re.findall(
            r"test result: (ok|FAILED)\. (\d+) passed; (\d+) failed; (\d+) ignored;",
            log_path.read_text(errors="replace"),
        )
        if not summaries or summaries[-1] != ("ok", "1", "0", "0"):
            errors.append("exact benchmark selector did not finish as one passing native test")
    except Exception as failure:
        rows = []
        errors = [f"record parsing failed: {type(failure).__name__}: {failure}"]
    expected = args.rounds * len(scenarios)
    valid = (
        execution["exit_code"] == 0 and not execution["timed_out"]
        and not execution["interrupted"] and not execution["execution_error"] and not errors
    )
    result = {
        "run_id": run_id, "purpose": purpose, "arm": arm,
        "history_messages": count, "command": command,
        "before": before, "after": host_receipt(args.output),
        **execution,
        "complete_raw_record": valid, "expected_samples": expected,
        "validation_errors": errors,
        "log": log_path.name, "log_sha256": digest(log_path) if log_path.exists() else None,
        "telemetry": telemetry_path.name,
        "telemetry_scope": "primary worker process, 250ms observations; foreign sender resources excluded",
        "rows": rows,
    }
    (args.output / f"{run_id}.json").write_text(json.dumps(result, indent=2) + "\n")
    print(json.dumps({key: result[key] for key in (
        "run_id", "arm", "history_messages", "elapsed_seconds", "exit_code", "complete_raw_record"
    )}), flush=True)
    if not valid:
        raise RuntimeError(f"{run_id} failed or produced an incomplete record; raw evidence retained")
    return result


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--baseline", type=Path, required=True)
    parser.add_argument("--candidate", type=Path, required=True)
    parser.add_argument("--baseline-source", type=Path, required=True)
    parser.add_argument("--candidate-source", type=Path, required=True)
    parser.add_argument("--output", type=Path, required=True)
    parser.add_argument("--build-receipt", type=Path)
    parser.add_argument("--rounds", type=int, default=8)
    parser.add_argument("--counts", type=int, nargs="+", default=[5000, 50000, 150000])
    parser.add_argument("--scenario", choices=("all", *SCENARIOS), default="all")
    parser.add_argument("--skip-small-null", action="store_true")
    parser.add_argument("--timeout-seconds", type=int, default=1800)
    parser.add_argument("--minimum-free-gib", type=int, default=4)
    args = parser.parse_args()
    if any(count not in (5000, 50000, 150000) for count in args.counts):
        parser.error("the fixture supports only 5000, 50000, and 150000 messages")
    if not 1 <= args.rounds <= 1000:
        parser.error("rounds must be in 1..=1000")
    if args.timeout_seconds <= 0 or args.minimum_free_gib <= 0:
        parser.error("timeout and minimum free disk must be positive")
    args.output = args.output.resolve()
    args.output.mkdir(parents=True, exist_ok=False)
    binaries = {arm: getattr(args, arm).resolve(strict=True) for arm in ("baseline", "candidate")}
    sources = {arm: getattr(args, f"{arm}_source").resolve(strict=True) for arm in binaries}
    schedule = []
    if not args.skip_small_null:
        schedule.extend(("baseline", 5000, "small_AA_null") for _ in range(2))
    for count in args.counts:
        schedule.extend((arm, count, "AB_BA") for arm in ("baseline", "candidate", "candidate", "baseline"))
    build_receipt = json.loads(args.build_receipt.read_text()) if args.build_receipt else None
    binary_receipts = {arm: {"path": str(path), "sha256": digest(path)} for arm, path in binaries.items()}
    source_receipts = {
        arm: source_receipt(path, declared_source_names(build_receipt, arm)) for arm, path in sources.items()
    }
    binding = bind_build_receipt(build_receipt, {
        arm: {"binaries": {"storage": binary_receipts[arm]["sha256"]}, "sources": source_receipts[arm]}
        for arm in binaries
    }, paired=True)
    manifest = {
        "started": utc_now(), "scope": "storage public-writer mechanism experiment; no release or operational verdict",
        "runner_sha256": digest(Path(__file__)),
        "runner_common_sha256": digest(Path(__file__).with_name("gh362_runner_common.py")),
        "selector": SELECTOR, "rounds": args.rounds, "scenario": args.scenario,
        "schedule": schedule,
        "warm_fixture": "three tracked copies; historical mailbox copies hardlinked; new copies independent",
        "foreign_append_span": "completed independent public write and flush precede timed parent publication",
        "controlled_fence_span": "controlled scheduling, if enabled; wall time includes orchestration and is not production p99",
        "canonical_counter_scope": "instrumented identity-check metadata/enumeration, not total syscalls",
        "small_AA_tolerance": "publication median ratio in [1/1.2,1.2], descriptive noise flag only; not a release budget",
        "host": host_receipt(args.output),
        "binaries": binary_receipts,
        "sources": source_receipts,
        "build_receipt": build_receipt,
        "build_receipt_binding": binding,
        "profile_status": "build receipt checked for identity and paired compiler/profile parity; absent receipt is unqualified",
    }
    manifest_path = args.output / "manifest.json"
    manifest_path.write_text(json.dumps(manifest, indent=2) + "\n")
    if binding["errors"]:
        (args.output / "failure.json").write_text(json.dumps({
            "time": utc_now(), "error": "build receipt binding failed", "details": binding["errors"],
            "comparative_verdict": "NO_VERDICT; no benchmark worker was started",
        }, indent=2) + "\n")
        raise RuntimeError("build receipt binding failed; manifest and failure receipt retained")
    results = []
    corpus_oids = {}
    for index, (arm, count, purpose) in enumerate(schedule):
        run_id = f"{index:02d}-{count}-{arm}"
        try:
            result = run_arm(args, binaries, run_id, arm, count, purpose)
            tree_oid = next(row["seeded_git_tree_oid"] for row in result["rows"] if row["kind"] == SETUP_KIND)
            if count in corpus_oids and corpus_oids[count] != tree_oid:
                raise RuntimeError(f"seeded corpus tree differs at {count}: {corpus_oids[count]} versus {tree_oid}")
            corpus_oids[count] = tree_oid
            results.append(result)
        except BaseException as error:
            (args.output / "failure.json").write_text(json.dumps({
                "time": utc_now(), "run_id": run_id, "error": str(error),
                "comparative_verdict": "NO_VERDICT; failed or mismatched run retained",
            }, indent=2) + "\n")
            raise
    null = [result for result in results if result["purpose"] == "small_AA_null"]
    null_summary = {}
    if len(null) == 2:
        scenarios = next(row["scenarios"] for row in null[0]["rows"] if row["kind"] == SETUP_KIND)
        for scenario in scenarios:
            medians = [statistics.median(
                row["publication_us"] for row in result["rows"]
                if row["kind"] == SAMPLE_KIND and row["scenario"] == scenario
            ) for result in null]
            ratio = medians[1] / medians[0] if medians[0] else None
            null_summary[scenario] = {
                "publication_medians_us": medians, "second_over_first": ratio,
                "within_descriptive_20_percent_tolerance": ratio is not None and 1 / 1.2 <= ratio <= 1.2,
            }
    summary = {
        "finished": utc_now(), "small_AA_null": null_summary,
        "seeded_git_tree_oids": corpus_oids,
        "runs": [result["run_id"] for result in results],
        "comparative_verdict": "not assigned; inspect raw scenarios, both orders, null, host conditions, and correctness separately",
    }
    (args.output / "summary.json").write_text(json.dumps(summary, indent=2) + "\n")


if __name__ == "__main__":
    main()
