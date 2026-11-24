#!/usr/bin/env python3
"""Adversarial controller tests; these do not qualify native Agent Mail."""

import argparse
import copy
import importlib.util
import json
import os
from pathlib import Path
import signal
import subprocess
import sys
import tempfile
import time
import unittest
from unittest import mock


REPO = Path(__file__).resolve().parents[2]
SCRIPT = REPO / "scripts/ci/qualify_issue407.py"
SPEC = importlib.util.spec_from_file_location("qualify_issue407", SCRIPT)
qualification = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(qualification)
TEST = qualification.TESTS[0]
SUMMARY = "test result: ok. 1 passed; 0 failed; 0 ignored; 0 measured; 19 filtered out; finished in 0.10s\n"


class RetainedFixtures(unittest.TestCase):
    def setUp(self):
        # Retain failures and original fixture bytes. No repository cleanup or
        # temporary-directory deletion is performed by these controller tests.
        self.root = Path(tempfile.mkdtemp(prefix="gh407-controller-test-"))

    def executable(self, name="target/am"):
        path = self.root / name
        path.parent.mkdir(parents=True, exist_ok=True)
        path.write_text("#!/bin/sh\nexit 0\n")
        path.chmod(0o700)
        return path


class LibtestAdmission(unittest.TestCase):
    def verdict(self, text, code=0):
        return qualification.libtest_verdict(text, code, TEST)

    def test_real_nested_child_and_outer_transcript(self):
        text = f"running 1 test\ntest {TEST} ... \nrunning 1 test\ntest {TEST} ... ok\n\n" + SUMMARY + "\nok\n\n" + SUMMARY
        result = self.verdict(text)
        self.assertTrue(result["passed"])
        self.assertEqual(result["summary_count"], 2)

    def test_child_success_cannot_hide_outer_failure_zero_ignore_or_missing_summary(self):
        child = f"test {TEST} ... ok\n" + SUMMARY
        invalid = [
            SUMMARY.replace("ok. 1 passed; 0 failed", "FAILED. 0 passed; 1 failed"),
            SUMMARY.replace("1 passed", "0 passed"),
            SUMMARY.replace("1 passed; 0 failed; 0 ignored", "0 passed; 0 failed; 1 ignored"),
            SUMMARY.replace("0 measured", "1 measured"),
            "test result: incomplete\n",
            "outer process never completed\n",
        ]
        for outer in invalid:
            with self.subTest(outer=outer):
                self.assertFalse(self.verdict(child + outer)["passed"])

    def test_name_in_diagnostic_and_nonzero_exit_are_not_success(self):
        self.assertFalse(self.verdict(f"panic: expected test {TEST} ... ok\n" + SUMMARY)["passed"])
        self.assertFalse(self.verdict(f"test {TEST}_different ... ok\n" + SUMMARY)["passed"])
        self.assertFalse(self.verdict(f"test {TEST} ... ok\n" + SUMMARY, 101)["passed"])
        self.assertFalse(self.verdict(f"test {TEST} ... ok\n" + SUMMARY + "late failure\n")["passed"])


class CargoArtifactAdmission(RetainedFixtures):
    def setUp(self):
        super().setUp()
        self.binary = self.executable()
        self.target = self.root / "target"
        self.package = "path+file:///source/crates/mcp-agent-mail-cli#mcp-agent-mail-cli@0.3.41"
        self.event = {"reason": "compiler-artifact", "package_id": self.package,
                      "target": {"name": "am", "kind": ["bin"]}, "profile": {"test": False},
                      "executable": str(self.binary), "fresh": True}

    def admit(self, events):
        output = "\n".join(json.dumps(value) for value in events)
        return qualification.cargo_artifact(output, self.package, "am", "bin", False, self.target)

    def test_emitted_cached_binary_is_bound_to_actual_bytes(self):
        artifact = self.admit([self.event, {"reason": "build-finished", "success": True}])
        self.assertEqual(artifact["path"], str(self.binary))
        self.assertEqual(artifact["sha256"], qualification.sha256(self.binary))

    def test_wrong_package_target_profile_and_missing_completion_refuse(self):
        for field, value in (("package_id", "registry+dependency#am@0.3.41"),
                             ("target", {"name": "am", "kind": ["lib"]}),
                             ("profile", {"test": True}), ("executable", None)):
            with self.subTest(field=field):
                event = {**self.event, field: value}
                with self.assertRaises(qualification.Refusal):
                    self.admit([event, {"reason": "build-finished", "success": True}])
        for events in ([self.event], [self.event, {"reason": "build-finished", "success": False}]):
            with self.assertRaises(qualification.Refusal):
                self.admit(events)
        with self.assertRaises(qualification.Refusal):
            qualification.cargo_artifact("not-json\n", self.package, "am", "bin", False, self.target)

    def test_ambiguous_external_and_symlinked_executables_refuse(self):
        second = self.executable("target/second-am")
        ending = {"reason": "build-finished", "success": True}
        with self.assertRaises(qualification.Refusal):
            self.admit([self.event, {**self.event, "executable": str(second)}, ending])
        external = self.executable("elsewhere/am")
        link = self.target / "escaped-am"
        link.symlink_to(external)
        for path in (external, link, self.target / "absent"):
            with self.subTest(path=path):
                with self.assertRaises(qualification.Refusal):
                    self.admit([{**self.event, "executable": str(path)}, ending])


class HarnessAdmission(RetainedFixtures):
    def setUp(self):
        super().setUp()
        self.binary = {"path": str(self.executable()), "sha256": "b" * 64}
        self.source = "a" * 40
        self.output = self.root / "black-box"
        self.path = self.output / "agent-mail-gh407-test" / "report.json"
        self.path.parent.mkdir(parents=True)
        self.report = {"schema_version": 1, "evidence_class": "supplied_binary", "status": "PASS", "exit_code": 0,
            "identity": {"source_sha_asserted": self.source, "binary_sha256": self.binary["sha256"],
                         "harness_sha256": "c" * 64, "binary": self.binary["path"], "run_directory": str(self.path.parent)},
            "configuration": {"idleSeconds": 8, "idleCycles": 3, "concurrency": 16, "soakSeconds": 30,
                              "failureWait": 8, "maxSampleAge": 6, "refreshSeconds": 2, "maxIdleCpuCores": 0.25},
            "cases": [{"name": name, "status": "PASS"} for name in sorted(qualification.HARNESS_CASES)]}

    def admit(self, report=None, path=None):
        return qualification.validate_harness_report(report or self.report, self.binary, self.source,
                                                      "c" * 64, self.output, path or self.path)

    def test_complete_bound_report_passes_and_real_failure_stays_failure(self):
        self.assertEqual(self.admit(), "PASS")
        report = copy.deepcopy(self.report)
        report["status"], report["exit_code"] = "FAIL", 1
        report["cases"][0]["status"] = "FAIL"
        self.assertEqual(self.admit(report), "FAIL")

    def test_empty_missing_duplicate_or_failing_cases_cannot_hide_under_pass(self):
        for cases in ([], self.report["cases"][:-1], self.report["cases"] * 2,
                      [{**case, "status": "FAIL"} for case in self.report["cases"]]):
            with self.subTest(count=len(cases)):
                with self.assertRaises(qualification.Refusal):
                    self.admit({**self.report, "cases": cases})

    def test_wrong_identity_surrogate_weakened_configuration_and_escaped_path_refuse(self):
        for key in ("source_sha_asserted", "binary_sha256", "harness_sha256", "binary", "run_directory"):
            with self.subTest(key=key):
                report = copy.deepcopy(self.report)
                report["identity"][key] = "different"
                with self.assertRaises(qualification.Refusal):
                    self.admit(report)
        with self.assertRaises(qualification.Refusal):
            self.admit({**self.report, "evidence_class": "harness_surrogate"})
        with self.assertRaises(qualification.Refusal):
            self.admit({**self.report, "configuration": {**self.report["configuration"], "maxIdleCpuCores": 1}})
        with self.assertRaises(qualification.Refusal):
            self.admit(path=self.root / "foreign-report.json")


class SourceAndEnvironment(RetainedFixtures):
    def git(self, *arguments):
        return subprocess.check_output(["git", "-c", "core.hooksPath=/dev/null",
            "-c", "commit.gpgsign=false", "-C", str(self.repo), *arguments],
            text=True, stderr=subprocess.STDOUT).strip()

    def create_repo(self):
        self.repo = self.root / "repo"
        self.repo.mkdir()
        self.git("init", "-b", "main")
        for name in ("Cargo.toml", "Cargo.lock", "rust-toolchain.toml", "scripts/qualify_health_idle.mjs"):
            path = self.repo / name
            path.parent.mkdir(parents=True, exist_ok=True)
            path.write_text("fixture\n")
        driver = self.repo / "scripts/ci/qualify_issue407.py"
        driver.parent.mkdir(parents=True)
        driver.write_bytes(SCRIPT.read_bytes())
        self.git("add", ".")
        self.git("-c", "user.name=Qualification control", "-c", "user.email=control@example.invalid",
                 "commit", "-m", "controller fixture")
        evidence = self.root / "evidence"
        evidence.mkdir()
        subject = object.__new__(qualification.Qualification)
        subject.repo, subject.evidence = self.repo, evidence
        subject.args = argparse.Namespace(expected_source=self.git("rev-parse", "HEAD"))
        subject.env = dict(os.environ)
        subject.deadline = time.monotonic() + 120
        subject.runner = qualification.ProcessRunner(evidence, grace=0.2, minimum_free=0, minimum_memory=0)
        return subject

    def test_committed_source_snapshot_binds_driver_and_inputs(self):
        subject = self.create_repo()
        snapshot = subject.source_snapshot("control")
        self.assertEqual(snapshot["source"], subject.args.expected_source)
        self.assertEqual(snapshot["files"]["scripts/ci/qualify_issue407.py"]["sha256"], qualification.sha256(SCRIPT))

    def test_assume_unchanged_source_modification_is_refused(self):
        subject = self.create_repo()
        self.git("update-index", "--assume-unchanged", "Cargo.toml")
        (self.repo / "Cargo.toml").write_text("changed but hidden\n")
        self.assertEqual(self.git("status", "--porcelain", "--untracked-files=no"), "")
        with self.assertRaisesRegex(qualification.Refusal, "Tracked bytes differ"):
            subject.source_snapshot("hidden-edit")

    def test_untracked_cargo_config_is_refused(self):
        subject = self.create_repo()
        (self.repo / ".cargo").mkdir()
        (self.repo / ".cargo/config.toml").write_text('[build]\nrustflags = ["--cfg", "unexpected"]\n')
        with self.assertRaisesRegex(qualification.Refusal, "Untracked or external Cargo"):
            subject.source_snapshot("untracked-config")

    def test_executing_driver_must_match_expected_checkout(self):
        subject = self.create_repo()
        driver = self.repo / "scripts/ci/qualify_issue407.py"
        driver.write_text("# another committed controller\n")
        self.git("add", str(driver))
        self.git("-c", "user.name=Qualification control", "-c", "user.email=control@example.invalid",
                 "commit", "-m", "different controller")
        subject.args.expected_source = self.git("rev-parse", "HEAD")
        with self.assertRaisesRegex(qualification.Refusal, "Executing driver differs"):
            subject.source_snapshot("wrong-driver")

    def test_wrapper_aliases_and_cfg_overrides_refuse(self):
        for key in ("RUSTFLAGS", "CARGO_ENCODED_RUSTFLAGS", "RUSTC_WRAPPER", "RUSTC_WORKSPACE_WRAPPER",
                    "CARGO_BUILD_RUSTC_WRAPPER", "CARGO_BUILD_RUSTC_WORKSPACE_WRAPPER",
                    "CARGO_TARGET_X86_64_UNKNOWN_LINUX_GNU_RUSTFLAGS"):
            with self.subTest(key=key):
                with self.assertRaises(qualification.Refusal):
                    qualification.build_environment({"PATH": "/usr/bin", key: "control"})
        env = qualification.build_environment({"CARGO_PROFILE_DEV_DEBUG": "0", "CARGO_HOME": "/private/cache"})
        self.assertEqual(env["CARGO_HOME"], "/private/cache")
        self.assertEqual(env["CARGO_BUILD_JOBS"], "1")

    def test_external_forced_wrapper_configuration_refuses(self):
        repo = self.root / "repo"
        repo.mkdir()
        cache = self.root / "cargo-home"
        cache.mkdir()
        (cache / "config.toml").write_text('[env]\nRUSTC_WRAPPER = { value = "/tmp/wrapper", force = true }\n')
        with self.assertRaises(qualification.Refusal):
            qualification.external_cargo_configs(repo, {"CARGO_HOME": str(cache)})

    def test_existing_evidence_directory_is_never_written(self):
        evidence = self.root / "existing"
        evidence.mkdir()
        original = evidence / "original.txt"
        original.write_text("preserve me\n")
        result = subprocess.run([sys.executable, str(SCRIPT), "--expected-source", "a" * 40,
                                 "--evidence", str(evidence)], capture_output=True, text=True, timeout=10)
        self.assertEqual(result.returncode, 2, result.stdout + result.stderr)
        self.assertEqual(list(evidence.iterdir()), [original])
        self.assertEqual(original.read_text(), "preserve me\n")

    def test_metadata_binds_locked_revisions_and_local_manifest_authority(self):
        repo = self.root / "repo"
        repo.mkdir()
        target = self.root / "target"
        target.mkdir()
        tracked = set()
        packages = []
        for suffix in ("server", "cli"):
            name = "mcp-agent-mail-" + suffix
            manifest = repo / "crates" / name / "Cargo.toml"
            manifest.parent.mkdir(parents=True)
            manifest.write_text("# controller metadata fixture\n")
            tracked.add(str(manifest.relative_to(repo)))
            packages.append({"name": name, "version": "0.3.41", "id": suffix,
                             "source": None, "manifest_path": str(manifest)})
        git_source = "git+https://example.invalid/asupersync?rev=" + "d" * 40 + "#" + "d" * 40
        packages.append({"name": "asupersync", "version": "0.5.0", "id": "runtime", "source": git_source})
        lock = {"package": [{key: package[key] for key in ("name", "version", "source")} for package in packages]}
        metadata = {"workspace_root": str(repo), "target_directory": str(target), "packages": packages,
                    "workspace_members": ["server", "cli"], "resolve": {"nodes": [{"id": "runtime"}]}}
        self.assertEqual(qualification.validate_metadata(metadata, lock, repo, target, tracked),
                         {"mcp-agent-mail-server": "server", "mcp-agent-mail-cli": "cli"})
        for change in ("external_manifest", "different_revision", "missing_graph"):
            altered = copy.deepcopy(metadata)
            if change == "external_manifest":
                altered["packages"][0]["manifest_path"] = str(self.root / "foreign/Cargo.toml")
            elif change == "different_revision":
                altered["packages"][-1]["source"] = git_source[:-40] + "e" * 40
            else:
                altered["resolve"]["nodes"] = []
            with self.subTest(change=change):
                with self.assertRaises(qualification.Refusal):
                    qualification.validate_metadata(altered, lock, repo, target, tracked)


@unittest.skipUnless(sys.platform == "linux", "process ownership requires Linux /proc")
class RealProcessOwnership(RetainedFixtures):
    def runner(self, **options):
        evidence = self.root / "evidence"
        evidence.mkdir()
        return qualification.ProcessRunner(evidence, grace=0.2, minimum_free=0, minimum_memory=0, **options)

    def run_command(self, runner, source, timeout=3):
        return runner.run("control", [sys.executable, "-c", source], cwd=self.root,
                          env=dict(os.environ), timeout=timeout)

    def test_ordinary_process_records_real_exit_and_raw_output(self):
        result = self.run_command(self.runner(), "print('native controller control only')")
        self.assertEqual(result["status"], "PASS")
        self.assertEqual(result["exit_code"], 0)
        self.assertEqual(result["remaining_processes"], [])
        self.assertEqual(Path(result["stdout"]).read_text(), "native controller control only\n")

    def test_timeout_kills_sigterm_ignoring_process(self):
        result = self.run_command(self.runner(), "import signal,time; signal.signal(signal.SIGTERM, signal.SIG_IGN); time.sleep(60)", timeout=0.25)
        self.assertEqual(result["termination_reason"], "timeout")
        self.assertTrue(result["forced"])
        self.assertEqual(result["exit_code"], -signal.SIGKILL)
        self.assertEqual(result["remaining_processes"], [])
        self.assertLess(result["elapsed_seconds"], 4)
        self.assertNotEqual(result["status"], "PASS")

    def test_interruption_cleans_owned_child_and_is_incomplete(self):
        started = time.monotonic()
        runner = self.runner(cancel=lambda: signal.SIGINT if time.monotonic() - started > 0.25 else None)
        result = self.run_command(runner, "import time; time.sleep(60)")
        self.assertEqual(result["termination_reason"], "interrupted")
        self.assertEqual(result["status"], "INCOMPLETE")
        self.assertEqual(result["remaining_processes"], [])

    def test_detached_descendant_outliving_parent_is_found_and_killed(self):
        child = "import signal,time; signal.signal(signal.SIGTERM, signal.SIG_IGN); time.sleep(60)"
        source = ("import subprocess,sys,time\n"
                  f"p=subprocess.Popen([sys.executable,'-c',{child!r}],start_new_session=True)\n"
                  "print(p.pid,flush=True)\ntime.sleep(0.15)\n")
        result = self.run_command(self.runner(), source)
        descendant = int(Path(result["stdout"]).read_text().strip())
        self.assertEqual(result["termination_reason"], "descendants_outlived_command")
        self.assertTrue(result["forced"])
        self.assertEqual(result["remaining_processes"], [])
        self.assertFalse(Path(f"/proc/{descendant}").exists())
        self.assertNotEqual(result["status"], "PASS")

    def test_fast_oversized_output_cannot_pass_after_exit(self):
        with mock.patch.object(qualification, "MAX_LOG_BYTES", 1024):
            result = self.run_command(self.runner(), "print('x'*2048)")
        self.assertEqual(result["termination_reason"], "log_budget")
        self.assertGreater(result["stdout_bytes"], 1024)
        self.assertNotEqual(result["status"], "PASS")


class OrchestrationAdmission(RetainedFixtures):
    def subject(self):
        subject = object.__new__(qualification.Qualification)
        subject.evidence = self.root
        subject.source_before = None
        subject.artifacts = []
        subject.report = {"stages": [], "native_tests": []}
        subject.preflight = lambda: {}
        subject.recheck = lambda: {}
        return subject

    def test_failed_test_build_does_not_skip_separate_production_branch(self):
        subject = self.subject()
        seen = []
        subject.compile = lambda test: seen.append("test" if test else "am") or {"status": "INCOMPLETE" if test else "PASS"}
        subject.black_box = lambda: seen.append("harness") or {"status": "PASS"}
        report = subject.execute()
        self.assertEqual(seen, ["test", "am", "harness"])
        self.assertEqual(report["status"], "INCOMPLETE")
        self.assertFalse(report["native_qualified"])

    def test_no_native_pass_when_one_exact_regression_fails(self):
        subject = self.subject()
        subject.source_before = {}
        subject.compiler = "controller fixture, not rustc"
        subject.compile = lambda test: {}
        subject.black_box = lambda: {}

        def native(name):
            value = {"name": name, "status": "FAIL" if name == qualification.TESTS[1] else "PASS"}
            subject.report["native_tests"].append(value)
            return {"status": value["status"]}

        subject.run_native_test = native
        report = subject.execute()
        self.assertEqual(report["status"], "FAIL")
        self.assertFalse(report["native_qualified"])
        self.assertEqual(len(report["native_tests"]), 3)
        self.assertTrue(any(stage["name"] == "independent-black-box" for stage in report["stages"]))


if __name__ == "__main__":
    unittest.main(verbosity=2)
