#!/usr/bin/env python3
"""Explicit local GH362 qualification; no remote jobs or workflows are started."""

import argparse
from contextlib import redirect_stderr, redirect_stdout
import json
import os
from pathlib import Path
import re
import signal
import sys

import gh362_compare as compare
import gh362_run_focused as focused
from gh362_runner_common import relative_source_name, run_logged, utc_now


BASE = "bc5d0a02a5d23a39fbb2fce83bb352a4bcc3df97"
RUSTC_COMMIT = "90850177249efe0321573c569aec5d12b257f8d6"
NEGATIVE = "canonical_id_tests::historical_in_place_rewrite_is_detected_before_the_next_publication"


def write_json(path, value):
    with path.open("x") as output:
        output.write(json.dumps(value, indent=2) + "\n")


def command_stage(root, label, command, environment, timeout):
    print(f"Starting {label}; retained log: {root / (label + '.log')}", flush=True)
    execution = run_logged(command, environment, root / f"{label}.log", timeout)
    write_json(root / f"{label}.json", {"command": command, **execution})
    if (
        execution["exit_code"] != 0 or execution["timed_out"]
        or execution["interrupted"] or execution["execution_error"]
    ):
        raise RuntimeError(f"{label} failed; raw log and execution receipt retained")


def runner_stage(root, label, module, arguments):
    """Call the existing runner directly, retaining its own worker ownership."""
    print(f"Starting {label}; retained log: {root / (label + '.log')}", flush=True)
    previous_argv = sys.argv
    error = None
    try:
        sys.argv = [str(Path(module.__file__).resolve()), *map(str, arguments)]
        with (root / f"{label}.log").open("x") as output, redirect_stdout(output), redirect_stderr(output):
            module.main()
    except BaseException as failure:
        error = f"{type(failure).__name__}: {failure}"
        raise
    finally:
        sys.argv = previous_argv
        write_json(root / f"{label}.json", {"finished": utc_now(), "error": error})


def identity_case(root, label, binary, expected_failure, environment):
    fixture_root = root / f"{label}-fixtures"
    fixture_root.mkdir()
    env = environment.copy()
    for variable in (
        "AM_CANONICAL_ID_REGRESSION_CHILD", "AM_CANONICAL_ID_WORKER_ACTION",
        "AM_CANONICAL_BENCH_FOREIGN_ROOT", "AM_CANONICAL_BENCH_FOREIGN_ID",
    ):
        env.pop(variable, None)
    env.update({"TMPDIR": str(fixture_root), "STORAGE_ROOT": str(fixture_root),
                "AM_COALESCER_MAX_WORKERS": "1", "RUST_TEST_THREADS": "1"})
    log_path = root / f"{label}.log"
    command = [str(binary), "--exact", NEGATIVE, "--nocapture", "--test-threads=1"]
    execution = run_logged(command, env, log_path, 300)
    content = log_path.read_text(errors="replace") if log_path.exists() else ""
    summaries = re.findall(r"test result: (ok|FAILED)\. (\d+) passed; (\d+) failed; (\d+) ignored;", content)
    clean_execution = not execution["timed_out"] and not execution["interrupted"] and not execution["execution_error"]
    if expected_failure:
        matched = bool(clean_execution and execution["exit_code"] is not None and execution["exit_code"] > 0
                       and summaries and summaries[-1] == ("FAILED", "0", "1", "0")
                       and "old in-place rewrite was missed" in content)
    else:
        matched = bool(clean_execution and execution["exit_code"] == 0
                       and summaries and summaries[-1] == ("ok", "1", "0", "0"))
    write_json(root / f"{label}.json", {
        "command": command, **execution, "expected_failure": expected_failure,
        "expectation_matched": matched,
        "interpretation": "incumbent refusal bug negative control; never a passing correctness test" if expected_failure else "candidate counterpart must pass",
        "log_sha256": compare.digest(log_path) if log_path.exists() else None,
    })
    if not matched:
        raise RuntimeError(f"{label} did not match its explicit expectation; raw evidence retained")


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--repository", type=Path, required=True, help="local repository containing the exact baseline commit")
    parser.add_argument("--work-root", type=Path, required=True, help="new dedicated output directory; never reused")
    parser.add_argument("--toolchain-bin", type=Path, help="bin directory of repository-pinned nightly-2026-08-31")
    parser.add_argument("--cargo-home", type=Path, help="dedicated Cargo cache; defaults to work-root/cargo-home")
    parser.add_argument("--prepare-only", action="store_true", help="verify packet and create patched detached worktrees; do not invoke Cargo")
    parser.add_argument("--build-timeout-seconds", type=int, default=7200)
    arguments = parser.parse_args()
    if arguments.build_timeout_seconds <= 0:
        parser.error("build timeout must be positive")
    if not arguments.prepare_only and arguments.toolchain_bin is None:
        parser.error("--toolchain-bin is required for native execution")
    repository = arguments.repository.resolve(strict=True)
    root = arguments.work_root.resolve()
    root.mkdir(parents=True, exist_ok=False)
    tools = Path(__file__).resolve().parent
    packet_repository = tools.parents[1]
    packet = packet_repository / "docs/evidence/gh362/canonical-id-candidate"
    result = {"started": utc_now(), "base": BASE, "prepare_only": arguments.prepare_only,
              "scope": "candidate qualification packet; no runtime deployment or release verdict"}
    environment = os.environ.copy()
    previous_term = signal.getsignal(signal.SIGTERM)
    def interrupt_on_term(_signum, _frame):
        raise KeyboardInterrupt("SIGTERM")
    signal.signal(signal.SIGTERM, interrupt_on_term)
    try:
        inputs = json.loads((packet / "frozen-inputs.json").read_text())
        if inputs["base_commit"] != BASE:
            raise RuntimeError("packet baseline differs from the fixed experiment")
        for name, expected in inputs["files_sha256"].items():
            path = packet_repository / name
            if not relative_source_name(name) or not path.resolve().is_relative_to(packet_repository):
                raise RuntimeError(f"invalid packet path: {name}")
            if compare.digest(path) != expected:
                raise RuntimeError(f"frozen packet input changed: {name}")
        write_json(root / "frozen-inputs.json", inputs)
        write_json(root / "host-before.json", compare.host_receipt(root))
        for arm, patch_name in (("candidate", "final-runtime.patch"), ("baseline", "baseline-fixture.patch")):
            checkout = root / arm
            command_stage(root, f"{arm}-checkout", ["git", "-C", str(repository), "worktree", "add", "--detach", str(checkout), BASE], environment, 300)
            command_stage(root, f"{arm}-patch-check", ["git", "-C", str(checkout), "apply", "--check", str(packet / patch_name)], environment, 300)
            command_stage(root, f"{arm}-patch-apply", ["git", "-C", str(checkout), "apply", str(packet / patch_name)], environment, 300)
            for name, expected in inputs["patched_sources_sha256"][arm].items():
                if compare.digest(checkout / name) != expected:
                    raise RuntimeError(f"{arm} patched source differs: {name}")
            command_stage(root, f"{arm}-whitespace", ["git", "-C", str(checkout), "diff", "--check"], environment, 300)
        if arguments.prepare_only:
            result["outcome"] = "patches reproduced; native gates not invoked"
            return
        toolchain = arguments.toolchain_bin.resolve(strict=True)
        cargo_home = (arguments.cargo_home or (root / "cargo-home")).resolve()
        cargo_home.mkdir(parents=True, exist_ok=True)
        environment.update({"GH362_RUN_ROOT": str(root), "GH362_TOOLCHAIN_BIN": str(toolchain),
                            "GH362_CARGO_HOME": str(cargo_home)})
        command_stage(root, "compiler-identity", [str(toolchain / "rustc"), "-vV"], environment, 30)
        if f"commit-hash: {RUSTC_COMMIT}" not in (root / "compiler-identity.log").read_text():
            raise RuntimeError("compiler does not match the repository-pinned nightly identity")
        candidate_label = "candidate-hint-native"
        baseline_label = "baseline-native"
        driver = tools / "gh362_build_native.py"
        emitter = tools / "gh362_emit_receipt.py"
        command_stage(root, "candidate-build-driver", [sys.executable, str(driver), candidate_label, str(root / "candidate")], environment, arguments.build_timeout_seconds)
        candidate_manifest = root / f"{candidate_label}-libtests-build.json"
        command_stage(root, "candidate-receipt-emitter", [sys.executable, str(emitter), str(candidate_manifest)], environment, 60)
        candidate_receipt = root / "native-binaries/candidate-build-receipt.json"
        candidate = json.loads(candidate_receipt.read_text())["arms"]["candidate"]["binaries"]
        runner_stage(root, "focused-driver", focused, [
            "--storage-binary", candidate["storage"]["path"], "--core-binary", candidate["core"]["path"],
            "--build-receipt", candidate_receipt, "--source", root / "candidate", "--output", root / "focused",
        ])
        identity_case(root, "candidate-historical-counterpart", Path(candidate["storage"]["path"]), False, environment)
        command_stage(root, "baseline-build-driver", [sys.executable, str(driver), baseline_label, str(root / "baseline")], environment, arguments.build_timeout_seconds)
        baseline_manifest = root / f"{baseline_label}-libtests-build.json"
        command_stage(root, "paired-receipt-emitter", [sys.executable, str(emitter), str(candidate_manifest), str(baseline_manifest)], environment, 60)
        paired_receipt = root / "native-binaries/paired-build-receipt.json"
        baseline = json.loads(paired_receipt.read_text())["arms"]["baseline"]["binaries"]["storage"]
        identity_case(root, "baseline-historical-negative", Path(baseline["path"]), True, environment)
        runner_stage(root, "paired-driver", compare, [
            "--baseline", baseline["path"], "--candidate", candidate["storage"]["path"],
            "--baseline-source", root / "baseline", "--candidate-source", root / "candidate",
            "--build-receipt", paired_receipt, "--output", root / "paired", "--rounds", "8",
            "--counts", "5000", "50000", "150000", "--scenario", "all",
        ])
        result["outcome"] = "focused correctness and explicit controls completed; paired raw measurements retained for review"
    except BaseException as failure:
        result["failure"] = f"{type(failure).__name__}: {failure}"
        result["outcome"] = "NO_VERDICT; failure or interruption retained"
        raise
    finally:
        result["finished"] = utc_now()
        write_json(root / "qualification.json", result)
        signal.signal(signal.SIGTERM, previous_term)


if __name__ == "__main__":
    main()
