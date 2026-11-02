#!/usr/bin/env python3
"""Deterministic runner controls; these are not Rust or native-gate results."""

import argparse
import copy
import json
import os
from pathlib import Path
import signal
import subprocess
import sys
import threading
from types import SimpleNamespace
import unittest
from unittest.mock import patch

import gh362_compare as compare
import gh362_run_focused as focused
import gh362_runner_common as common


OUTPUT = None
GOOD_EXECUTION = {
    "exit_code": 0, "timed_out": False, "interrupted": False,
    "execution_error": None,
}


def records(arm="candidate", scenarios=compare.SCENARIOS, rounds=2, oid="a" * 40):
    shared = {
        "history_messages": 5000,
        "watch_supported_and_enrolled": arm == "candidate" if arm == "candidate" else None,
    }
    return [{
        **shared, "kind": compare.SETUP_KIND, "scenarios": list(scenarios),
        "rounds": rounds, "historical_tracked_copies": 15000,
        "seeded_git_tree_oid": oid, "cold_publication_us": 10, "setup_ms": 20,
    }] + [{
        **shared, "kind": compare.SAMPLE_KIND, "scenario": scenario, "round": number,
        **{metric: 10 for metric in compare.METRICS},
    } for scenario in scenarios for number in range(rounds)]


def build_records():
    receipt = {"schema_version": 1, "arms": {}}
    actual = {}
    for arm, binary_hash in (("baseline", "a" * 64), ("candidate", "b" * 64)):
        names = set(common.REQUIRED_STORAGE_SOURCES)
        if arm == "candidate":
            names.add("crates/mcp-agent-mail-storage/src/canonical_watch.rs")
        files = {name: "c" * 64 for name in names}
        receipt["arms"][arm] = {
            "binaries": {"storage": {"sha256": binary_hash}},
            "sources": {"head": "d" * 40, "files_sha256_before": files, "files_sha256_after": files.copy()},
            "build": {
                "exit_code": 0, "compiler": "synthetic rustc identity", "profile": {"opt_level": 0},
                "features": {"packages": ["storage", "core"], "default_features": False},
                "rustflags": "synthetic flags", "wrapper_sha256": "e" * 64,
            },
        }
        actual[arm] = {"binaries": {"storage": binary_hash}, "sources": {"head": "d" * 40, "files_sha256": files.copy()}}
    return receipt, actual


class RunnerControls(unittest.TestCase):
    def setUp(self):
        self.directory = OUTPUT / self._testMethodName
        self.directory.mkdir()

    def retain_execution(self, label, result):
        (self.directory / f"{label}.json").write_text(json.dumps(result, indent=2) + "\n")
        return result

    def run_python(self, label, source, timeout=5, telemetry=None):
        return self.retain_execution(label, common.run_logged(
            [sys.executable, "-u", "-c", source], os.environ.copy(),
            self.directory / f"{label}.log", timeout, telemetry,
        ))

    def fake_native(self, source):
        binary = self.directory / "fake-native"
        binary.write_text(f"#!{sys.executable}\n" + source + "\n")
        binary.chmod(0o700)
        return binary

    def test_exact_requested_schema(self):
        for arm in ("baseline", "candidate"):
            self.assertEqual(compare.validate_rows(records(arm), 5000, 2, compare.SCENARIOS, arm), [])
        mutations = []
        mutations.append(records(scenarios=()))
        mutations.append(records(scenarios=compare.SCENARIOS[:2]))
        mutations.append(records()[:-1])
        duplicate = records()
        duplicate[-1] = duplicate[-2].copy()
        mutations.append(duplicate)
        for field, value in (
            ("publication_us", -1), ("canonical_metadata_calls", True),
            ("history_messages", 50000), ("watch_supported_and_enrolled", None),
        ):
            bad = records()
            bad[-1][field] = value
            mutations.append(bad)
        for bad in mutations:
            self.assertTrue(compare.validate_rows(bad, 5000, 2, compare.SCENARIOS, "candidate"))

    def test_build_receipts_bind_binaries_sources_and_paired_flags(self):
        receipt, actual = build_records()
        self.assertTrue(common.bind_build_receipt(receipt, actual, paired=True)["qualified"])
        absent = common.bind_build_receipt(None, actual, paired=True)
        self.assertFalse(absent["qualified"])
        self.assertEqual(absent["errors"], [])
        mutations = []
        bad = copy.deepcopy(receipt)
        bad["arms"]["candidate"]["binaries"]["storage"]["sha256"] = "0" * 64
        mutations.append(bad)
        bad = copy.deepcopy(receipt)
        bad["arms"]["candidate"]["sources"]["files_sha256_after"]["Cargo.lock"] = "0" * 64
        mutations.append(bad)
        bad = copy.deepcopy(receipt)
        bad["arms"]["candidate"]["sources"]["files_sha256_before"] = None
        mutations.append(bad)
        bad = copy.deepcopy(receipt)
        bad["arms"]["candidate"]["build"]["exit_code"] = 1
        mutations.append(bad)
        for field, value in (
            ("compiler", "different compiler"), ("profile", {"opt_level": 1}),
            ("features", {"requested": ["different"]}), ("rustflags", "different flags"),
            ("wrapper_sha256", "0" * 64),
        ):
            bad = copy.deepcopy(receipt)
            bad["arms"]["candidate"]["build"][field] = value
            mutations.append(bad)
        for bad in mutations:
            result = common.bind_build_receipt(bad, actual, paired=True)
            self.assertFalse(result["qualified"])
            self.assertTrue(result["errors"])
        changed_source = copy.deepcopy(actual)
        changed_source["candidate"]["sources"]["files_sha256"]["Cargo.lock"] = "0" * 64
        self.assertFalse(common.bind_build_receipt(receipt, changed_source, paired=True)["qualified"])
        no_checkout = {"candidate": {"binaries": actual["candidate"]["binaries"], "sources": None}}
        result = common.bind_build_receipt(receipt, no_checkout)
        self.assertTrue(result["qualified"])
        self.assertFalse(result["source_coverage"]["candidate"]["observed_checkout_checked"])

    def test_malformed_receipts_fail_without_following_external_source_paths(self):
        receipt, actual = build_records()
        for invalid in ([], {}, {"schema_version": True}, {"schema_version": 1, "arms": []}):
            self.assertTrue(common.bind_build_receipt(invalid, actual, paired=True)["errors"])
        malformed = copy.deepcopy(receipt)
        for field in ("files_sha256_before", "files_sha256_after"):
            malformed["arms"]["candidate"]["sources"][field]["../external"] = "0" * 64
            malformed["arms"]["candidate"]["sources"][field][17] = "0" * 64
        self.assertNotIn("../external", common.declared_source_names(malformed, "candidate"))
        self.assertTrue(common.bind_build_receipt(malformed, actual, paired=True)["errors"])

    def test_receipt_mismatch_stops_both_runners_before_workers(self):
        receipt, actual = build_records()
        receipt_path = self.directory / "build-receipt.json"
        receipt_path.write_text(json.dumps(receipt))
        binary = self.directory / "wrong-binary"
        binary.write_bytes(b"intentionally differs from the supplied binary hash")
        paired_output = self.directory / "paired-rejected"
        paired_argv = [
            "gh362_compare.py", "--baseline", str(binary), "--candidate", str(binary),
            "--baseline-source", str(self.directory), "--candidate-source", str(self.directory),
            "--output", str(paired_output), "--build-receipt", str(receipt_path),
        ]
        with patch.object(sys, "argv", paired_argv), \
             patch.object(compare, "source_receipt", return_value=actual["candidate"]["sources"]), \
             patch.object(compare, "run_arm") as worker:
            with self.assertRaisesRegex(RuntimeError, "build receipt binding failed"):
                compare.main()
            worker.assert_not_called()
        self.assertTrue((paired_output / "failure.json").exists())
        focused_output = self.directory / "focused-rejected"
        focused_argv = [
            "gh362_run_focused.py", "--storage-binary", str(binary),
            "--build-receipt", str(receipt_path), "--output", str(focused_output),
        ]
        with patch.object(sys, "argv", focused_argv), patch.object(focused, "inventory") as inventory:
            with self.assertRaisesRegex(RuntimeError, "build receipt binding failed"):
                focused.main()
            inventory.assert_not_called()
        self.assertIn("binding failed", json.loads((focused_output / "manifest.json").read_text())["failure"])

    def test_prefixed_json_and_nested_summary(self):
        path = self.directory / "synthetic-worker.log"
        expected = records()
        path.write_text("test worker ... " + "\n".join(json.dumps(row) for row in expected) + "\nok\n")
        self.assertEqual(compare.parse_rows(path), expected)
        selected = ["ordinary_a", "ordinary_b", next(iter(focused.ALLOWED_IGNORED_TESTS))]
        content = (
            "test result: ok. 1 passed; 0 failed; 0 ignored;\n"
            "test result: ok. 2 passed; 0 failed; 1 ignored;\n"
        )
        result = focused.assess_summary(content, selected, selected[-1:], GOOD_EXECUTION)
        self.assertTrue(result["success"])
        self.assertEqual(result["expected_active_tests"], 2)

    def test_ignored_or_missing_functional_cases_fail(self):
        allowed = sorted(focused.ALLOWED_IGNORED_TESTS)[:2]
        self.assertFalse(focused.assess_summary(
            "test result: ok. 0 passed; 0 failed; 2 ignored;", allowed, allowed,
            GOOD_EXECUTION,
        )["success"])
        selected = ["worker_a", "worker_b", "substantive_regression"]
        result = focused.assess_summary(
            "test result: ok. 2 passed; 0 failed; 1 ignored;", selected,
            ["substantive_regression"], GOOD_EXECUTION,
        )
        self.assertFalse(result["success"])
        self.assertEqual(result["unexpected_ignored_tests"], ["substantive_regression"])
        for content in (
            "", "test result: ok. 0 passed; 0 failed; 0 ignored;",
            "test result: FAILED. 1 passed; 1 failed; 0 ignored;",
            "test result: ok. 1 passed; 0 failed; 0 ignored;",
        ):
            self.assertFalse(focused.assess_summary(content, ["a", "b"], [], GOOD_EXECUTION)["success"])

    def test_success_and_nonzero_are_retained(self):
        good = self.run_python("good", "print('raw success')")
        self.assertEqual(good["exit_code"], 0)
        self.assertIsNone(good["execution_error"])
        failed = self.run_python("failed", "print('raw failure'); raise SystemExit(17)")
        self.assertEqual(failed["exit_code"], 17)
        self.assertIn("raw failure", (self.directory / "failed.log").read_text())

    def test_telemetry_is_for_the_owned_namespace_process(self):
        result = self.run_python(
            "telemetry", "import os,time; print(os.getpid(),flush=True); time.sleep(0.6)",
            telemetry=self.directory / "telemetry.jsonl",
        )
        self.assertEqual(result["exit_code"], 0)
        pid = int((self.directory / "telemetry.log").read_text().strip())
        snapshots = [json.loads(line) for line in (self.directory / "telemetry.jsonl").read_text().splitlines()]
        self.assertTrue(snapshots)
        self.assertTrue(all(snapshot["pid"] == pid and snapshot["Threads"] == 1 for snapshot in snapshots))

    def test_timeout_kills_owned_leader_and_descendant(self):
        child = "import signal,time; signal.signal(signal.SIGTERM, signal.SIG_IGN); time.sleep(60)"
        source = (
            "import os,signal,subprocess,sys,time; "
            "signal.signal(signal.SIGTERM, signal.SIG_IGN); "
            f"p=subprocess.Popen([sys.executable,'-c',{child!r}]); "
            "print('child',p.pid,flush=True); print('leader',os.getpid(),flush=True); time.sleep(60)"
        )
        result = self.run_python("timeout", source, timeout=0.5)
        self.assertTrue(result["timed_out"])
        self.assertIn("SIGKILL", result["cleanup"]["signals"])
        self.assertEqual(result["cleanup"]["remaining_owned_group_members"], [])

    @unittest.skipUnless(hasattr(os, "pidfd_open") and hasattr(signal, "pidfd_send_signal"), "Linux pidfds required")
    def test_reaped_leader_orphan_is_cleaned_by_identity(self):
        child = "import signal,time; signal.signal(signal.SIGTERM, signal.SIG_IGN); time.sleep(60)"
        source = (
            "import subprocess,sys; "
            f"p=subprocess.Popen([sys.executable,'-c',{child!r}]); "
            "print('orphan',p.pid,flush=True)"
        )
        result = self.run_python("orphan", source)
        self.assertEqual(result["exit_code"], 0)
        self.assertEqual(result["execution_error"], "worker exited with live owned descendants")
        self.assertEqual(result["cleanup"]["remaining_owned_group_members"], [])

    def test_io_failure_cleans_child(self):
        with patch.object(common, "process_metrics", side_effect=OSError("synthetic recorder failure")):
            result = self.run_python(
                "recorder", "import time; time.sleep(60)",
                telemetry=self.directory / "recorder.telemetry.jsonl",
            )
        self.assertIn("synthetic recorder failure", result["execution_error"])
        self.assertEqual(result["cleanup"]["remaining_owned_group_members"], [])

    def test_interrupts_preserve_receipts_and_clean_children(self):
        for signum in (signal.SIGINT, signal.SIGTERM):
            timer = threading.Timer(0.5, os.kill, args=(os.getpid(), signum))
            timer.start()
            try:
                result = self.run_python(
                    signal.Signals(signum).name,
                    "import time; print('started',flush=True); time.sleep(60)",
                )
            finally:
                timer.cancel()
                timer.join()
            self.assertTrue(result["interrupted"])
            self.assertEqual(result["cleanup"]["remaining_owned_group_members"], [])

    def test_recycled_group_without_marker_is_not_signaled(self):
        environment = os.environ.copy()
        environment[common.OWNER_VARIABLE] = "real-owner"
        actual = subprocess.Popen(
            [sys.executable, "-c", "import time; time.sleep(60)"],
            env=environment, start_new_session=True,
        )
        class ReapedLeader:
            pid = actual.pid
            @staticmethod
            def poll():
                return 0
        try:
            self.assertFalse(common.signal_owned_group(ReapedLeader(), "wrong-owner", signal.SIGTERM))
            self.assertIsNone(actual.poll())
        finally:
            common.stop_owned_group(actual, "real-owner")

    def test_stale_worker_schema_retains_raw_failure_receipt(self):
        stale = records(scenarios=compare.SCENARIOS[:2])
        source = "\n".join(f"print({json.dumps(row)!r})" for row in stale)
        binary = self.fake_native(source + "\nprint('test result: ok. 1 passed; 0 failed; 0 ignored;')")
        arguments = SimpleNamespace(
            output=self.directory, rounds=2, scenario="all", timeout_seconds=5,
            minimum_free_gib=0,
        )
        with self.assertRaisesRegex(RuntimeError, "incomplete record"):
            compare.run_arm(arguments, {"candidate": binary}, "stale", "candidate", 5000, "control")
        receipt = json.loads((self.directory / "stale.json").read_text())
        self.assertFalse(receipt["complete_raw_record"])
        self.assertTrue(receipt["validation_errors"])
        self.assertEqual(receipt["rows"], stale)
        self.assertTrue(receipt["log_sha256"])

    def test_focused_all_ignored_receipt_is_not_success(self):
        binary = self.fake_native("print('test result: ok. 0 passed; 0 failed; 2 ignored;')")
        tests = sorted(name for name in focused.ALLOWED_IGNORED_TESTS if name.startswith("canonical_id_tests::"))
        result = focused.run_case(
            binary, tests, tests, "ignored", "canonical_id_tests::", False, self.directory, 5,
        )
        self.assertFalse(result["success"])
        self.assertTrue((self.directory / "ignored.json").exists())

    def test_interrupted_benchmark_retains_raw_receipt(self):
        binary = self.fake_native("import time; print('started',flush=True); time.sleep(60)")
        arguments = SimpleNamespace(
            output=self.directory, rounds=2, scenario="all", timeout_seconds=5,
            minimum_free_gib=0,
        )
        timer = threading.Timer(0.5, os.kill, args=(os.getpid(), signal.SIGTERM))
        timer.start()
        try:
            with self.assertRaisesRegex(RuntimeError, "incomplete record"):
                compare.run_arm(arguments, {"candidate": binary}, "interrupt", "candidate", 5000, "control")
        finally:
            timer.cancel()
            timer.join()
        receipt = json.loads((self.directory / "interrupt.json").read_text())
        self.assertTrue(receipt["interrupted"])
        self.assertEqual(receipt["cleanup"]["remaining_owned_group_members"], [])
        self.assertIn("started", (self.directory / "interrupt.log").read_text())

    def test_aa_and_ab_ba_schedule_and_corpus_check(self):
        baseline = self.directory / "baseline"
        candidate = self.directory / "candidate"
        baseline.write_bytes(b"baseline fixture identity")
        candidate.write_bytes(b"candidate fixture identity")
        output = self.directory / "paired"
        argv = [
            "gh362_compare.py", "--baseline", str(baseline), "--candidate", str(candidate),
            "--baseline-source", str(self.directory), "--candidate-source", str(self.directory),
            "--output", str(output), "--counts", "5000", "--rounds", "2",
        ]
        def synthetic_run(_args, _binaries, run_id, arm, _count, purpose):
            return {"run_id": run_id, "purpose": purpose, "rows": records(arm)}
        with patch.object(sys, "argv", argv), patch.object(compare, "source_receipt", return_value={}), \
             patch.object(compare, "run_arm", side_effect=synthetic_run):
            compare.main()
        manifest = json.loads((output / "manifest.json").read_text())
        self.assertEqual([arm for arm, _, _ in manifest["schedule"]], [
            "baseline", "baseline", "baseline", "candidate", "candidate", "baseline",
        ])
        self.assertTrue((output / "summary.json").exists())
        mismatch = self.directory / "mismatched"
        argv[argv.index(str(output))] = str(mismatch)
        invocation = 0
        def mismatched_run(*args):
            nonlocal invocation
            result = synthetic_run(*args)
            invocation += 1
            if invocation == 2:
                result["rows"][0]["seeded_git_tree_oid"] = "b" * 40
            return result
        with patch.object(sys, "argv", argv), patch.object(compare, "source_receipt", return_value={}), \
             patch.object(compare, "run_arm", side_effect=mismatched_run):
            with self.assertRaisesRegex(RuntimeError, "seeded corpus tree differs"):
                compare.main()
        self.assertTrue((mismatch / "failure.json").exists())
        self.assertFalse((mismatch / "summary.json").exists())


def main():
    global OUTPUT
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--output", type=Path, required=True)
    arguments = parser.parse_args()
    OUTPUT = arguments.output.resolve()
    OUTPUT.mkdir(parents=True, exist_ok=False)
    suite = unittest.defaultTestLoader.loadTestsFromTestCase(RunnerControls)
    result = unittest.TextTestRunner(verbosity=2).run(suite)
    receipt = {
        "scope": "Python runner controls with synthetic records and tiny owned subprocesses; no Rust/native gate claim",
        "tests_run": result.testsRun, "failures": len(result.failures),
        "errors": len(result.errors), "skipped": len(result.skipped), "successful": result.wasSuccessful(),
        "scripts_sha256": {
            name: compare.digest(Path(__file__).with_name(name)) for name in (
                "gh362_compare.py", "gh362_run_focused.py", "gh362_runner_common.py", "gh362_runner_checks.py",
            )
        },
    }
    (OUTPUT / "controls.json").write_text(json.dumps(receipt, indent=2) + "\n")
    raise SystemExit(0 if result.wasSuccessful() else 1)


if __name__ == "__main__":
    main()
