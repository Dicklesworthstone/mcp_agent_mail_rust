#!/usr/bin/env python3
"""Build real GH382 test binaries, then qualify them inside a measured cgroup.

The prepare step runs outside the resource budget. The run step refuses to
qualify an unconstrained process: use a cgroup v2 scope with CPUQuota=200%,
MemoryMax=4G and MemoryHigh=3G. No database or request path is substituted.
"""

import argparse
import hashlib
import json
import os
from pathlib import Path
import re
import subprocess
import sys
from fractions import Fraction

from gh362_runner_common import run_logged, utc_now


REPOSITORY = Path(__file__).resolve().parents[2]
LARGE_LEXICAL = (
    "search_response::tests::tantivy_tests::"
    "gh382_large_mailbox_lean_page_equivalence"
)
LARGE_INBOX = (
    "mail_ui::unified::tests::"
    "gh382_unified_mailbox_cardinality_preserves_results_with_bounded_project_work"
)
REQUIRED = {
    "db": [
        LARGE_LEXICAL,
        "schema::tests::v15_owned_external_cursor_trigger_survives_canonical_and_runtime_rollback_retry",
        "search_response::tests::tantivy_tests::gh382_lean_candidates_preserve_deep_cursor_pages_without_counts_or_stored_loads",
        "pool_impl::tests::failed_startup_repair_prevents_successful_file_initialization",
        "pool_impl::tests::pool_config_from_resolved_config_preserves_the_serving_authority_and_shape",
        "pool_impl::tests::startup_repairs_recount_normalized_timestamps_on_the_first_pass",
        "pool_impl::tests::startup_repair_failure_preserves_timestamps_counters_triggers_and_parent_writes",
        "pool_impl::tests::successful_startup_repair_does_not_commit_the_callers_transaction",
    ],
    "server": [
        "mail_ui::unified::tests::message_only_page_preserves_orphans_aliases_and_lifecycle_visibility",
        "mail_ui::unified::tests::message_only_page_reads_rosters_and_labels_only_for_candidate_projects",
        "mail_ui::unified::tests::message_only_page_keeps_lazy_roster_and_project_labels_on_its_snapshot",
        "mail_ui::unified::tests::live_collector_matches_the_original_on_a_real_migrated_mailbox",
        "tests::health_readiness_requires_serving_pool_initialization",
        "tests::health_readiness_fails_closed_on_pending_pool_acquisition",
        "tests::health_readiness_invalidates_another_pool_generation",
        "tests::health_readiness_reflects_real_archive_write_failure",
        "tests::health_readiness_refuses_mismatched_serving_configuration",
        "tests::health_readiness_uses_effective_search_backend",
        "tests::health_readiness_returns_ready_json",
        "tests::health_root_alias_returns_ready_json",
        "tests::health_readiness_fails_fast_when_sqlite_activity_lock_is_exclusive",
        "tests::startup_readiness_notifications_bound_owned_warmup_grace",
        "tests::startup_readiness_shutdown_cancels_pending_native_acquisition_and_retries",
        "tests::startup_search_delay_is_owned_and_next_startup_indexes_retained_mail",
        "tests::prepared_http_startup_runs_probes_once_and_callback_after_readiness",
    ],
}


def digest(path):
    with Path(path).open("rb") as stream:
        return hashlib.file_digest(stream, "sha256").hexdigest()


def write_json(path, value):
    with path.open("x") as stream:
        json.dump(value, stream, indent=2)
        stream.write("\n")


def checkout_identity():
    return {
        "head": subprocess.check_output(
            ["git", "-C", str(REPOSITORY), "rev-parse", "HEAD"], text=True
        ).strip(),
        "tracked_changes": subprocess.check_output(
            ["git", "-C", str(REPOSITORY), "status", "--porcelain", "--untracked-files=no"],
            text=True,
        ),
        "cargo_lock_sha256": digest(REPOSITORY / "Cargo.lock"),
    }


def command(root, label, arguments, timeout, environment):
    print(f"{label}: {' '.join(map(str, arguments))}", flush=True)
    result = run_logged(
        list(map(str, arguments)), environment, root / f"{label}.log", timeout,
        telemetry_path=root / f"{label}.process.jsonl",
    )
    write_json(root / f"{label}.execution.json", {"command": arguments, **result})
    if (
        result["exit_code"] != 0 or result["timed_out"]
        or result["interrupted"] or result["execution_error"]
    ):
        raise RuntimeError(f"{label} failed; raw output and execution receipt retained")
    return (root / f"{label}.log").read_text(errors="replace")


def prepare(root):
    root.mkdir(parents=True, exist_ok=False)
    environment = os.environ.copy()
    environment.update({
        "CARGO_BUILD_JOBS": "1", "CARGO_INCREMENTAL": "0",
        "CARGO_PROFILE_DEV_DEBUG": "0", "CARGO_PROFILE_TEST_DEBUG": "0",
        "CARGO_TERM_COLOR": "never", "RUST_TEST_THREADS": "1",
    })
    before = checkout_identity()
    if before["tracked_changes"]:
        raise RuntimeError("qualification requires a clean tracked checkout")
    write_json(root / "source-before.json", before)
    compiler = command(root, "compiler", ["rustc", "-vV"], 30, environment)
    command(
        root, "production",
        [
            "cargo", "check", "--locked", "-p", "mcp-agent-mail-db",
            "-p", "mcp-agent-mail-server", "--lib",
            "--no-default-features", "--features", "portable",
        ],
        3600, environment,
    )
    binaries = {}
    for component in ("db", "server"):
        package = f"mcp-agent-mail-{component}"
        output = command(
            root, f"build-{component}",
            [
                "cargo", "test", "--locked", "-p", package, "--lib",
                "--no-default-features", "--features", "portable", "--no-run",
                "--message-format=json-render-diagnostics",
            ],
            3600, environment,
        )
        candidates = set()
        for line in output.splitlines():
            try:
                entry = json.loads(line)
            except ValueError:
                continue
            if (
                entry.get("reason") == "compiler-artifact"
                and entry.get("profile", {}).get("test")
                and entry.get("target", {}).get("name") == package.replace("-", "_")
                and entry.get("executable")
            ):
                candidates.add(str(Path(entry["executable"]).resolve(strict=True)))
        if len(candidates) != 1:
            raise RuntimeError(f"expected one native {package} library test executable")
        binary = candidates.pop()
        binaries[component] = {"path": binary, "sha256": digest(binary)}
    after = checkout_identity()
    if before != after:
        raise RuntimeError("tracked source changed while compiling")
    write_json(root / "build.json", {
        "schema_version": 1, "source": before, "compiler": compiler,
        "features": "portable", "binaries": binaries, "prepared_at": utc_now(),
        "rustflags": environment.get("RUSTFLAGS"),
        "encoded_rustflags": environment.get("CARGO_ENCODED_RUSTFLAGS"),
        "scope": "native portable production libraries and library test binaries",
    })


def resource_limits():
    mount = Path("/sys/fs/cgroup").resolve(strict=True)
    memberships = Path("/proc/self/cgroup").read_text().splitlines()
    paths = [line.split(":", 2)[2] for line in memberships if line.startswith("0::")]
    if len(paths) != 1:
        raise RuntimeError("cgroup v2 unified membership is required")
    group = (mount / paths[0].lstrip("/")).resolve(strict=True)
    if not group.is_relative_to(mount):
        raise RuntimeError("cgroup membership escapes its mount")
    levels = []
    cpu_limits = []
    memory_limits = []
    high_limits = []
    current = group
    while True:
        observed = {"path": str(current)}
        for field in ("cpu.max", "memory.max", "memory.high", "cpuset.cpus.effective"):
            path = current / field
            observed[field] = path.read_text().strip() if path.exists() else None
        if observed["cpu.max"]:
            quota, period = observed["cpu.max"].split()
            if quota != "max":
                cpu_limits.append(Fraction(int(quota), int(period)))
        for field, limits in (("memory.max", memory_limits), ("memory.high", high_limits)):
            if observed[field] not in (None, "max"):
                limits.append(int(observed[field]))
        levels.append(observed)
        if current == mount:
            break
        current = current.parent
    cpu = min(cpu_limits) if cpu_limits else None
    memory = min(memory_limits) if memory_limits else None
    high = min(high_limits) if high_limits else None
    if cpu != 2 or memory != 4 * 1024**3 or high != 3 * 1024**3:
        raise RuntimeError(
            f"requires effective 2 CPU quota, 4 GiB max and 3 GiB high; "
            f"observed cpu={cpu}, memory={memory}, high={high}"
        )
    return {
        "effective_cpu_quota": str(cpu), "effective_memory_max": memory,
        "effective_memory_high": high, "hierarchy": levels,
        "nice": os.getpriority(os.PRIO_PROCESS, 0),
    }


def resource_counters(limits):
    group = Path(limits["hierarchy"][0]["path"])
    return {
        field: (group / field).read_text().strip()
        for field in ("cpu.stat", "memory.current", "memory.peak", "memory.events")
        if (group / field).exists()
    }


def run(root):
    build = json.loads((root / "build.json").read_text())
    if build.get("schema_version") != 1 or set(build.get("binaries", {})) != set(REQUIRED):
        raise RuntimeError("receipt must contain both real native test binaries")
    if build["source"] != checkout_identity():
        raise RuntimeError("checkout no longer matches the compiled source receipt")
    limits = resource_limits()
    write_json(root / "limits.json", limits)
    environment = os.environ.copy()
    environment["RUST_TEST_THREADS"] = "1"
    environment["NO_COLOR"] = "1"
    cases = []
    for component, binary in build["binaries"].items():
        if digest(binary["path"]) != binary["sha256"]:
            raise RuntimeError(f"{component} executable differs from its build receipt")
        output = command(
            root, f"list-{component}",
            [binary["path"], "--list", "--format", "terse"], 60, environment,
        )
        available = {
            line.removesuffix(": test") for line in output.splitlines()
            if line.endswith(": test")
        }
        ignored_output = command(
            root, f"list-ignored-{component}",
            [binary["path"], "--list", "--ignored", "--format", "terse"],
            60, environment,
        )
        ignored = {
            line.removesuffix(": test") for line in ignored_output.splitlines()
            if line.endswith(": test")
        }
        missing = set(REQUIRED[component]) - available
        if missing:
            raise RuntimeError(f"missing native cases: {sorted(missing)}")
        selected = set(REQUIRED[component])
        if component == "db":
            search = {name for name in available if "::gh382_" in name}
            if not search:
                raise RuntimeError("no GH382 native search regressions were compiled")
            selected.update(search)
        else:
            if LARGE_INBOX not in available:
                raise RuntimeError("the exact-cardinality native inbox comparison is missing")
            selected.add(LARGE_INBOX)
        for name in sorted(selected):
            cases.append((component, binary["path"], name, name in ignored))

    results = []
    failure = None
    try:
        for index, (component, binary, name, opt_in) in enumerate(cases):
            label = f"case-{index:02d}-{component}"
            arguments = [binary, "--exact", name, "--nocapture", "--test-threads=1"]
            large = name == LARGE_INBOX
            if opt_in:
                arguments.append("--ignored")
            before = resource_counters(limits)
            output = command(root, label, arguments, 1800 if opt_in else 300, environment)
            summaries = re.findall(
                r"test result: (ok|FAILED)\. (\d+) passed; (\d+) failed; (\d+) ignored;",
                output,
            )
            if not summaries or summaries[-1] != ("ok", "1", "0", "0"):
                raise RuntimeError(f"{name} did not execute exactly one passing native test")
            if resource_limits() != limits:
                raise RuntimeError("resource controls changed during qualification")
            metrics = []
            decoder = json.JSONDecoder()
            for line in output.splitlines():
                start = line.find("{")
                if start < 0:
                    continue
                try:
                    entry, _end = decoder.raw_decode(line[start:])
                except ValueError:
                    continue
                if isinstance(entry, dict) and entry.get("benchmark"):
                    metrics.append(entry)
            if large:
                expected = {
                    (retired, limit, importance, round_number)
                    for retired in (0, 6393)
                    for limit, importance in ((20, None), (1000, None), (20, "normal"))
                    for round_number in range(4)
                }
                observed = {
                    (entry.get("retired_agents"), entry.get("limit"),
                     entry.get("importance"), entry.get("round"))
                    for entry in metrics
                    if entry.get("benchmark") == "issue-382-unified-collector"
                    and (entry.get("projects"), entry.get("agents"), entry.get("messages"))
                    == (242, 12_236, 203_498)
                }
                if len(metrics) != 24 or observed != expected:
                    raise RuntimeError("large inbox fixture did not emit every paired scenario")
            if name == LARGE_LEXICAL:
                if len(metrics) != 1:
                    raise RuntimeError("large lexical fixture emitted no unique comparison")
                entry = metrics[0]
                expected_fields = {
                    "benchmark": "gh382_lexical_collector",
                    "projects": 242, "agents": 12_236, "messages": 203_498,
                    "limit": 20, "scope": "collector_only",
                    "rich_count_collectors": 1, "rich_stored_document_loads": 20,
                    "lean_count_collectors": 0, "lean_stored_document_loads": 0,
                }
                if any(entry.get(key) != value for key, value in expected_fields.items()):
                    raise RuntimeError("large lexical fixture changed its comparison scope")
                for key in ("rich_samples_us", "lean_samples_us"):
                    samples = entry.get(key)
                    if (
                        not isinstance(samples, list) or len(samples) != 6
                        or any(type(value) is not int or value < 0 for value in samples)
                    ):
                        raise RuntimeError("large lexical fixture lacks six paired samples")
            results.append({
                "name": name, "component": component, "log": f"{label}.log",
                "resource_counters_before": before,
                "resource_counters_after": resource_counters(limits),
                "measurements": metrics,
            })
    except BaseException as error:
        failure = f"{type(error).__name__}: {error}"
        raise
    finally:
        write_json(root / "qualification.json", {
            "schema_version": 1, "source": build["source"], "limits": limits,
            "finished_at": utc_now(), "failure": failure, "cases": results,
            "passed": failure is None and len(results) == len(cases),
            "scope": "native portable collector comparison and readiness/search regressions",
            "excluded": [
                "the reporter's production mailbox and archive",
                "end-to-end startup/HTTP latency or idle CPU attribution",
                "full workspace/default-hybrid qualification",
            ],
        })


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("phase", choices=("prepare", "run"))
    parser.add_argument("--output", type=Path, required=True)
    args = parser.parse_args()
    os.chdir(REPOSITORY)
    root = args.output.resolve()
    if args.phase == "prepare":
        prepare(root)
    else:
        run(root)


if __name__ == "__main__":
    main()
