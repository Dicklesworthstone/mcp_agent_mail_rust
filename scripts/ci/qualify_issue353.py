#!/usr/bin/env python3
"""Bounded, read-only GH353 qualification; never publish or contact a mailbox."""

from __future__ import annotations

import argparse
import hashlib
import json
import os
from pathlib import Path
import re
import shutil
import signal
import subprocess
import sys
import time

BASE = "23fe3dce55d00486cd034549627efd164fa31332"
TOOLCHAIN = "nightly-2026-08-31"
PATCH_DIR = Path("docs/evidence/gh353")
PATCHES = {
    "candidate": (
        "2026-10-28_gh353_request_latency_candidate.patch",
        "322ea263e8f5971ca85705233f4863615f0625c80ddc715d8b206914207e3349",
    ),
    "baseline": (
        "2026-10-28_gh353_request_latency_baseline.patch",
        "ca5e31e8a21f634ed8bb3b9fda3d38ed0ae52f06ef393c1b5e4fd726d9d681f4",
    ),
}
PACKAGES = ("mcp-agent-mail-db", "mcp-agent-mail-storage", "mcp-agent-mail-tools")
CASES = (
    ("mcp-agent-mail-storage", "fenced_repair_yields_a_busy_project_mutex_to_unrelated_publication",
     r"an unrelated request remained behind a repair's project-lock wait"),
    ("mcp-agent-mail-storage", "fenced_repair_yields_a_busy_project_flock_without_changing_its_owner",
     r"an unrelated request remained behind a repair's project-lock wait"),
    ("mcp-agent-mail-storage", "direct_reservation_cancellation_ends_the_project_lock_wait",
     r"cancelled request did not leave the held project lock"),
    ("mcp-agent-mail-storage", "direct_reservation_cancellation_after_admission_preserves_existing_bytes",
     r"panicked at [^\n]*mcp-agent-mail-storage/src/lib\.rs:\d+:\d+:\nOk\(\(\)\)"),
    ("mcp-agent-mail-tools", "cancelled_coordination_requests_finish_while_recovery_keeps_admission_closed",
     r"\[GH#353 write admission\] request_local=false completed=[0-9]/10 .*promotion_held=true active_writers=0"),
    ("mcp-agent-mail-tools", "request_local_cancellation_finishes_while_recovery_keeps_admission_closed",
     r"\[GH#353 write admission\] request_local=true completed=[0-9]/10 .*promotion_held=true active_writers=0"),
    ("mcp-agent-mail-tools", "fetch_inbox_peek_finishes_while_recovery_holds_write_admission",
     r"a completed inbox read must not wait for optional write admission"),
    ("mcp-agent-mail-tools", "fetch_inbox_peek_does_not_bootstrap_an_activity_write_pool",
     r"optional activity must never cold-bootstrap"),
)
CONTROLS = (
    ("mcp-agent-mail-db", "write_barrier::tests::", False),
    ("mcp-agent-mail-storage", "fenced_project_admission_rechecks_cancellation_before_publication", True),
    ("mcp-agent-mail-tools", "send_message_reply_is_bounded_and_db_durable_with_async_archive", True),
    ("mcp-agent-mail-tools", "reservation_fence_failure_keeps_unkeyed_lease_for_retry", True),
)
SAFETY = (
    "direct_reservation_public_cancellation_predicate_observes_request_token",
    "direct_reservation_writes_stage_outside_the_fence",
    "commit_all_never_publishes_a_staged_temp_file",
    "reservation_publication_never_reactivates_a_published_terminal_generation",
    "reservation_publication_preserves_immutable_and_guard_visible_identity",
    "reservation_publication_rechecks_a_release_that_wins_during_staging",
    "a_busy_project_defers_without_stranding_other_releases_and_is_revisited",
    "busy_publication_fence_preserves_archive_evidence_and_releases_database_admission",
    "busy_project_does_not_strand_other_leases_or_resurrect_a_later_release",
    "shutdown_during_project_contention_retains_the_candidate_for_resume",
    "a_busy_project_does_not_prevent_another_message_bundle_from_recovering",
    "a_held_flock_keeps_its_owner_evidence_and_all_message_destinations",
    "stop_refuses_publication_and_nested_admission_retains_its_native_owner",
    "attachment_authority_is_required_before_any_cancellable_message_publication",
    "busy_project_is_skipped_while_another_mailbox_project_repairs_then_is_revisited",
    "optimistic_epoch_drift_frees_saturated_retry_capacity_for_a_crash_orphan",
    "complete_bundles_verify_without_acquiring_the_global_publication_fence",
    "retention_admission_shutdown_during_project_wait_preserves_unconsumed_work",
)
GATES = (
    ("fmt", ["fmt", "--all", "--", "--check"]),
    ("check-all-targets", ["check", "--locked", "--workspace", "--all-targets"]),
    ("check-production", ["check", "--locked", "--workspace", "--lib", "--bins"]),
    ("clippy", ["clippy", "--locked", "--workspace", "--all-targets", "--", "-D", "warnings"]),
)
MIB = 1024 * 1024


def stop_group(process: subprocess.Popen) -> None:
    # The test wrapper may exit before a stuck child. Kill the whole original
    # group after the grace period even when its leader has already exited.
    try:
        os.killpg(process.pid, signal.SIGTERM)
    except ProcessLookupError:
        pass
    try:
        process.wait(timeout=10)
    except subprocess.TimeoutExpired:
        pass
    try:
        os.killpg(process.pid, signal.SIGKILL)
    except ProcessLookupError:
        pass
    process.wait()


def sha(path: Path) -> str:
    return hashlib.sha256(path.read_bytes()).hexdigest()


def shared_additions(patch: str) -> list[tuple[str, str]]:
    """Baseline is an additive test patch; verify its exact added blocks in both trees."""
    result = []
    path, block, in_hunk = "", [], False

    def flush() -> None:
        if block and "".join(block).strip():
            result.append((path, "".join(block)))
        block.clear()

    for line in patch.splitlines(keepends=True):
        if line.startswith("diff --git "):
            flush()
            in_hunk = False
        elif line.startswith("+++ b/"):
            path = line[6:].strip()
            if not path.startswith("crates/") or not path.endswith(".rs") or ".." in Path(path).parts:
                raise ValueError("baseline patch touches a non-Rust source path")
        elif line.startswith("@@ "):
            flush()
            in_hunk = True
        elif in_hunk and line.startswith("-"):
            raise ValueError("baseline patch changes existing implementation")
        elif in_hunk and line.startswith("+"):
            block.append(line[1:])
        else:
            flush()
    flush()
    if not result:
        raise ValueError("baseline patch contains no shared fixtures")
    return result


def test_verdict(text: str, returncode: int, name: str, expected_failure: str | None,
                 expected_count: int) -> bool:
    if expected_failure is not None:
        summary = re.search(r"test result: FAILED\. 0 passed; 1 failed; 0 ignored;", text)
        named = re.search(r"(?m)^\s*" + re.escape(name) + r"\s*$", text)
        cause = re.search(expected_failure, text)
        if "write admission" in expected_failure:
            cause = cause and "assertion `left == right` failed" in text
        return returncode == 101 and bool(summary and named and cause)
    matches = re.findall(r"test result: ok\. (\d+) passed; 0 failed; (\d+) ignored;", text)
    return returncode == 0 and expected_count > 0 and bool(matches) and matches[-1] == (str(expected_count), "0")


class Qualification:
    def __init__(self, args: argparse.Namespace):
        self.repo = args.repo.resolve()
        self.work = args.work_root.resolve()
        self.evidence = args.evidence.resolve()
        self.work.mkdir(parents=True, exist_ok=False)
        self.evidence.mkdir(parents=True, exist_ok=True)
        self.deadline = time.monotonic() + 150 * 60
        self.receipt = {"schema": 1, "base": BASE, "toolchain": TOOLCHAIN,
                        "status": "running", "patches": PATCHES, "steps": [], "sources": {},
                        "claims": {"full_swarm_qualified": False, "incident_reproduced": False}}
        self.env = os.environ.copy()
        for key in list(self.env):
            if key.startswith("CARGO_PROFILE_") or key in (
                "RUSTFLAGS", "CARGO_ENCODED_RUSTFLAGS", "RUSTC_WRAPPER", "RUSTC_WORKSPACE_WRAPPER",
                "RUSTUP_TOOLCHAIN", "CARGO_BUILD_TARGET", "CARGO_BUILD_RUSTFLAGS",
            ):
                del self.env[key]
        self.env.update(CARGO_BUILD_JOBS="1", CARGO_INCREMENTAL="0", CARGO_PROFILE_DEV_DEBUG="0",
                        CARGO_PROFILE_TEST_DEBUG="0", CARGO_TERM_COLOR="never", RUST_TEST_THREADS="1",
                        RUST_BACKTRACE="0", CARGO_TARGET_DIR=str(self.work / "target"),
                        GIT_AUTHOR_NAME="MCP qualification", GIT_AUTHOR_EMAIL="qualification@example.invalid",
                        GIT_COMMITTER_NAME="MCP qualification", GIT_COMMITTER_EMAIL="qualification@example.invalid")
        self.receipt["build_environment"] = {k: self.env[k] for k in (
            "CARGO_BUILD_JOBS", "CARGO_INCREMENTAL", "CARGO_PROFILE_DEV_DEBUG", "CARGO_PROFILE_TEST_DEBUG")}
        self.save()

    def save(self) -> None:
        (self.evidence / "receipt.json").write_text(json.dumps(self.receipt, indent=2) + "\n")

    def command(self, label: str, argv: list[str], cwd: Path, timeout: int = 3600) -> dict:
        log = self.evidence / (label + ".log")
        step = {"label": label, "argv": argv, "log": log.name, "status": "running"}
        self.receipt["steps"].append(step)
        self.save()
        free = shutil.disk_usage(self.work).free
        step["free_bytes_before"] = free
        if free < 1024 * MIB or time.monotonic() >= self.deadline:
            step.update(status="prerequisite_failed", reason="disk or overall wall-time budget exhausted")
            self.save()
            return step
        started = time.monotonic()
        env = dict(self.env)
        sandbox = self.work / "sandbox" / cwd.name
        sandbox.mkdir(parents=True, exist_ok=True)
        (sandbox / "tmp").mkdir(exist_ok=True)
        env.update(TMPDIR=str(sandbox / "tmp"), DATABASE_URL=f"sqlite://{sandbox}/mail.sqlite3",
                   STORAGE_ROOT=str(sandbox / "archive"), HTTP_PORT="1", ALLOW_EPHEMERAL_PROJECTS_IN_DEFAULT_STORAGE="1")
        with log.open("wb") as stream:
            process = subprocess.Popen(argv, cwd=cwd, env=env, stdin=subprocess.DEVNULL,
                                       stdout=stream, stderr=subprocess.STDOUT, start_new_session=True)
            stop = None
            try:
                while process.poll() is None:
                    if time.monotonic() >= min(self.deadline, started + timeout):
                        stop = "timeout"
                    elif shutil.disk_usage(self.work).free < 512 * MIB:
                        stop = "disk_budget"
                    elif log.stat().st_size > 64 * MIB:
                        stop = "log_budget"
                    if stop:
                        stop_group(process)
                        break
                    try:
                        process.wait(timeout=2)
                    except subprocess.TimeoutExpired:
                        pass
            finally:
                # A finished leader can leave descendants in the session we
                # own. Always clean up that group; wait() retains the leader's
                # recorded return code even when its descendants need killing.
                stop_group(process)
        step.update(returncode=process.returncode, elapsed_seconds=round(time.monotonic() - started, 3),
                    free_bytes_after=shutil.disk_usage(self.work).free,
                    status=stop or ("passed" if process.returncode == 0 else "failed"))
        self.save()
        print(f"{label}: {step['status']} (exit {process.returncode})", flush=True)
        return step

    def require(self, label: str, argv: list[str], cwd: Path, timeout: int = 3600) -> dict:
        step = self.command(label, argv, cwd, timeout)
        if step["status"] != "passed":
            raise RuntimeError(f"prerequisite failed: {label}")
        return step

    def output(self, step: dict) -> str:
        return (self.evidence / step["log"]).read_text(errors="replace")

    def source(self, tree: Path) -> dict:
        changed = subprocess.check_output(["git", "diff", "--cached", "--name-only", BASE], cwd=tree, text=True, timeout=30).splitlines()
        if not changed or any(not p.startswith("crates/") or not p.endswith(".rs") for p in changed):
            raise RuntimeError("passive source patch changed a non-Rust path")
        tracked = changed + ["Cargo.lock", "Cargo.toml", "rust-toolchain.toml", ".cargo/config.toml"]
        return {"tree": subprocess.check_output(["git", "write-tree"], cwd=tree, text=True, timeout=30).strip(),
                "sha256": {p: sha(tree / p) for p in tracked}}

    def prepare(self) -> dict[str, Path]:
        for variant, (name, digest) in PATCHES.items():
            if sha(self.repo / PATCH_DIR / name) != digest:
                raise RuntimeError(f"{variant} patch SHA256 mismatch")
        have = self.command("base-present", ["git", "cat-file", "-e", BASE + "^{commit}"], self.repo, 30)
        if have["status"] != "passed":
            self.require("fetch-base", ["git", "fetch", "--no-tags", "--depth=1", "origin", BASE], self.repo, 120)
        self.require("main-native-source-matches-base", ["git", "diff", "--exit-code", BASE, "HEAD", "--",
                     "crates", "Cargo.lock", "Cargo.toml", "rust-toolchain.toml", ".cargo/config.toml"], self.repo, 30)
        trees = {}
        for variant, (name, _) in PATCHES.items():
            tree = self.work / variant
            self.require(variant + "-worktree", ["git", "worktree", "add", "--detach", str(tree), BASE], self.repo, 120)
            self.require(variant + "-patch", ["git", "apply", "--index", str(self.repo / PATCH_DIR / name)], tree, 60)
            trees[variant] = tree
            self.receipt["sources"][variant] = self.source(tree)
        blocks = shared_additions((self.repo / PATCH_DIR / PATCHES["baseline"][0]).read_text())
        for path, block in blocks:
            for tree in trees.values():
                if block not in (tree / path).read_text():
                    raise RuntimeError(f"shared test fixture differs: {path}")
        self.receipt["identical_shared_fixture_blocks"] = len(blocks)
        self.save()
        return trees

    def build(self, variant: str, tree: Path) -> dict[str, tuple[Path, list[str]]]:
        args = ["cargo", "+" + TOOLCHAIN, "test", "--locked", "--no-run", "--lib", "--message-format=json"]
        for package in PACKAGES:
            args += ["-p", package]
        step = self.require(variant + "-compile-tests", args, tree)
        binaries = {}
        for line in self.output(step).splitlines():
            try:
                event = json.loads(line)
            except ValueError:
                continue
            if event.get("reason") != "compiler-artifact" or not event.get("executable") or not event.get("profile", {}).get("test"):
                continue
            package = event.get("target", {}).get("name", "").replace("_", "-")
            if package in PACKAGES:
                if package in binaries:
                    raise RuntimeError(f"duplicate test binary: {package}")
                executable = Path(event["executable"])
                listing = self.require(variant + "-list-" + package, [str(executable), "--list", "--format=terse"], tree, 60)
                names = [line[:-6] for line in self.output(listing).splitlines() if line.endswith(": test")]
                if not names:
                    raise RuntimeError(f"empty test binary: {package}")
                binaries[package] = (executable, names)
        if set(binaries) != set(PACKAGES):
            raise RuntimeError("not all three native test binaries were produced")
        return binaries

    def test(self, variant: str, tree: Path, binaries: dict, package: str, query: str,
             exact: bool, failure: str | None = None, skips: tuple[str, ...] = ()) -> bool:
        executable, names = binaries[package]
        selected = [name for name in names if (name == query or name.endswith("::" + query))] if exact else [name for name in names if query in name]
        selected = [name for name in selected if not any(name.endswith("::" + skip) for skip in skips)]
        if not selected or (exact and len(selected) != 1):
            raise RuntimeError(f"unexpected test selection: {query}: {selected}")
        name = selected[0] if exact else query
        args = [str(executable), name, "--nocapture", "--test-threads=1"]
        if exact:
            args.append("--exact")
        for skip in skips:
            args += ["--skip", skip]
        label = variant + "-test-" + re.sub(r"[^a-zA-Z0-9_-]", "_", query)
        step = self.command(label, args, tree, 180 if exact else 900)
        good = step["status"] in ("passed", "failed") and test_verdict(self.output(step), step.get("returncode", -1), name, failure, len(selected))
        step.update(expected="specific_baseline_assertion" if failure else "pass", selected_tests=selected,
                    accepted=good)
        self.save()
        return good

    def run(self) -> bool:
        trees = self.prepare()
        results = []
        for variant in ("candidate", "baseline"):
            tree = trees[variant]
            binaries = self.build(variant, tree)
            for package, query, exact in CONTROLS:
                good = self.test(variant, tree, binaries, package, query, exact)
                results.append(good)
                if not good:
                    raise RuntimeError(f"{variant} positive control failed: {query}")
            for package, query, cause in CASES:
                results.append(self.test(variant, tree, binaries, package, query, True,
                                         cause if variant == "baseline" else None))
            if variant == "candidate":
                for query in SAFETY:
                    results.append(self.test(variant, tree, binaries, "mcp-agent-mail-storage", query, True))
            self.require(variant + "-source-unchanged", ["git", "diff", "--exit-code"], tree, 30)
            if self.source(tree) != self.receipt["sources"][variant]:
                raise RuntimeError(f"{variant} source changed during qualification")
        # Obtain both native A/B arms before spending the remaining budget on
        # full-workspace compiler gates. Every gate still controls the verdict.
        for label, args in GATES:
            step = self.command("candidate-" + label, ["cargo", "+" + TOOLCHAIN] + args, trees["candidate"])
            results.append(step["status"] == "passed")
        self.require("candidate-final-source-unchanged", ["git", "diff", "--exit-code"], trees["candidate"], 30)
        if self.source(trees["candidate"]) != self.receipt["sources"]["candidate"]:
            raise RuntimeError("candidate source changed during compiler gates")
        return all(results)


def self_test() -> None:
    name = "tests::regression"
    failure = "root cause"
    text = "thread 'tests::regression' panicked:\nroot cause\nfailures:\n    tests::regression\ntest result: FAILED. 0 passed; 1 failed; 0 ignored;"
    assert test_verdict(text, 101, name, failure, 1)
    for bad in (text.replace("root cause", "fixture failed"), text.replace(name, "other::test"), "error: could not compile"):
        assert not test_verdict(bad, 101, name, failure, 1)
    assert not test_verdict(text, -9, name, failure, 1)
    assert not test_verdict("test result: ok. 0 passed; 0 failed; 0 ignored;", 0, name, None, 1)
    assert not test_verdict("test result: ok. 1 passed; 0 failed; 0 ignored;", 0, name, None, 2)
    assert test_verdict("test result: ok. 1 passed; 0 failed; 0 ignored;", 0, name, None, 1)
    assert shared_additions("diff --git a/crates/a.rs b/crates/a.rs\n+++ b/crates/a.rs\n@@ -1 +1,2 @@\n context\n+fixture\n") == [("crates/a.rs", "fixture\n")]
    try:
        shared_additions("diff --git a/crates/a.rs b/crates/a.rs\n+++ b/crates/a.rs\n@@ -1 +1 @@\n-implementation\n+fixture\n")
    except ValueError:
        pass
    else:
        raise AssertionError("baseline implementation change accepted")
    print("qualification classifier self-tests passed; no Cargo was run")


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--self-test", action="store_true")
    parser.add_argument("--repo", type=Path)
    parser.add_argument("--work-root", type=Path)
    parser.add_argument("--evidence", type=Path)
    args = parser.parse_args()
    if args.self_test:
        self_test()
        return 0
    if not all((args.repo, args.work_root, args.evidence)):
        parser.error("--repo, --work-root and --evidence are required")
    job = Qualification(args)
    try:
        good = job.run()
    except Exception as error:
        job.receipt["error"] = f"{type(error).__name__}: {error}"
        good = False
    job.receipt["status"] = "passed" if good else "failed"
    job.save()
    print(json.dumps({"status": job.receipt["status"], "error": job.receipt.get("error")}), flush=True)
    return 0 if good else 1


if __name__ == "__main__":
    sys.exit(main())
