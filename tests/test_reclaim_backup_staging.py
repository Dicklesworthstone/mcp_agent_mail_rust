"""Real filesystem/CLI tests for the offline backup-stage recovery command.

Fixtures are deliberately retained: recovery testing must not delete evidence.
The command itself uses real flock, renameat2 and fsync. Injected IO faults test
failure reporting, not a substitute implementation of the successful path.
"""

from contextlib import contextmanager
import errno
import importlib.util
import json
import os
from pathlib import Path
import sqlite3
import subprocess
import sys
import tempfile
import time
import unittest
from unittest import mock

SCRIPT = Path(__file__).resolve().parents[1] / "scripts/reclaim_backup_staging.py"
SPEC = importlib.util.spec_from_file_location("reclaim_backup_staging", SCRIPT)
reclaim = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(reclaim)


@unittest.skipUnless(sys.platform.startswith("linux"), "Linux no-replace recovery contract")
class StagingRecoveryTests(unittest.TestCase):
    def setUp(self):
        self.root = Path(tempfile.mkdtemp(prefix="am-staging-recovery-test-"))
        self.parent = self.root / "database"
        self.archive = self.root / "archive"
        self.parent.mkdir()
        self.archive.mkdir()
        self.database = self.parent / "custom.sqlite3"
        connection = sqlite3.connect(self.database)
        connection.execute("CREATE TABLE sentinel (value TEXT)")
        connection.execute("INSERT INTO sentinel VALUES ('acknowledged mail')")
        connection.commit()
        connection.close()
        self.protected = {self.database: self.database.read_bytes()}
        for suffix in ("-wal", "-shm", ".bak", ".bak.meta.json", ".automatic-backup-state"):
            path = self.parent / (self.database.name + suffix)
            path.write_bytes(b"protected recovery state: " + suffix.encode())
            self.protected[path] = path.read_bytes()

    def stage(self, suffix="000001", *, old=True):
        stage = self.parent / (reclaim.STAGE_PREFIX + suffix)
        stage.mkdir()
        (stage / "snapshot.sqlite3").write_bytes(b"retained snapshot")
        (stage / "live-export.sqlite3").write_bytes(b"retained export")
        (stage / "nested").mkdir()
        (stage / "nested/sidecar").write_bytes(b"retained sidecar")
        if old:
            when = time.time_ns() - 7200 * 1_000_000_000
            for path in [*stage.rglob("*"), stage]:
                os.utime(path, ns=(when, when))
        return stage

    def mailbox(self):
        return reclaim.Mailbox(self.database, self.archive)

    def preview(self):
        with self.mailbox() as mailbox:
            return mailbox.plan(3600)

    def apply(self, plan=None):
        plan = self.preview() if plan is None else plan
        with self.mailbox() as mailbox:
            return reclaim.apply_plan(mailbox, plan["plan_sha256"], 3600, offline=True)

    def assert_protected(self):
        for path, contents in self.protected.items():
            self.assertEqual(path.read_bytes(), contents, str(path))

    def cli(self, *args, timeout=10):
        completed = subprocess.run(
            [sys.executable, str(SCRIPT), "--database", str(self.database),
             "--storage-root", str(self.archive), *args],
            text=True, capture_output=True, timeout=timeout,
        )
        return completed, json.loads(completed.stdout)

    @contextmanager
    def exporter(self):
        # Exactly the directory-inode flock used by the production Rust budget.
        # There is no fabricated PID record or separate test-only lock name.
        code = """
import fcntl, os, sys
fd = os.open(sys.argv[1], os.O_RDONLY | os.O_DIRECTORY)
fcntl.flock(fd, fcntl.LOCK_EX | fcntl.LOCK_NB)
print('LOCKED', flush=True)
sys.stdin.readline()
os.close(fd)
"""
        child = subprocess.Popen([sys.executable, "-c", code, str(self.parent)],
                                 text=True, stdin=subprocess.PIPE,
                                 stdout=subprocess.PIPE, stderr=subprocess.PIPE)
        try:
            self.assertEqual(child.stdout.readline().strip(), "LOCKED")
            yield child
        finally:
            stdout, stderr = child.communicate("release\n", timeout=10)
            self.assertEqual(child.returncode, 0, stdout + stderr)

    def test_preview_counts_more_than_three_stages_and_changes_nothing(self):
        stages = [self.stage(f"{i:06}") for i in range(10)]
        before = sorted(str(path) for path in self.root.rglob("*"))
        plan = self.preview()
        self.assertEqual(len(plan["eligible"]), 10)
        self.assertEqual(plan["retained"], [])
        self.assertEqual(sum(item["bytes"] for item in plan["eligible"]), 10 * 48)
        self.assertEqual(plan, self.preview(), "the digest must not depend on the observation time")
        self.assertEqual(before, sorted(str(path) for path in self.root.rglob("*")))
        self.assertTrue(all(stage.exists() for stage in stages))
        self.assert_protected()

    def test_apply_moves_all_old_stages_and_preserves_each_inode_and_payload(self):
        stages = [self.stage(f"{i:06}") for i in range(10)]
        identities = {stage.name: stage.stat().st_ino for stage in stages}
        plan = self.preview()
        result = self.apply(plan)
        self.assertTrue(result["ok"], result)
        self.assertEqual(len(result["moved"]), 10)
        self.assertEqual(result["freed_bytes"], 0)
        self.assertTrue(result["backup_retry_state_unchanged"])
        run = Path(result["run_directory"])
        manifest = json.loads((run / "manifest.json").read_text())
        self.assertEqual(manifest["plan"], plan)
        actions = [json.loads(line) for line in (run / "actions.jsonl").read_text().splitlines()]
        self.assertEqual(len(actions), 20)
        self.assertEqual([action["phase"] for action in actions], ["intent", "completed"] * 10)
        for stage in stages:
            moved = run / stage.name
            self.assertFalse(stage.exists())
            self.assertEqual(moved.stat().st_ino, identities[stage.name])
            self.assertEqual((moved / "snapshot.sqlite3").read_bytes(), b"retained snapshot")
            self.assertEqual((moved / "nested/sidecar").read_bytes(), b"retained sidecar")
        self.assertEqual(self.preview()["eligible"], [])
        self.assert_protected()

    def test_age_uses_the_newest_nested_file_and_retains_future_timestamps(self):
        old = self.stage("old")
        recent = self.stage("recent")
        future = self.stage("future")
        os.utime(recent / "nested/sidecar", None)
        when = time.time_ns() + 86400 * 1_000_000_000
        os.utime(future / "snapshot.sqlite3", ns=(when, when))
        plan = self.preview()
        self.assertEqual([item["name"] for item in plan["eligible"]], [old.name])
        self.assertEqual({item["name"] for item in plan["retained"]}, {recent.name, future.name})
        result = self.apply(plan)
        self.assertTrue(result["ok"], result)
        self.assertTrue(recent.exists())
        self.assertTrue(future.exists())

    def test_apply_requires_offline_declaration_and_exact_preview_digest(self):
        self.stage()
        plan = self.preview()
        with self.mailbox() as mailbox:
            with self.assertRaisesRegex(reclaim.Refused, "offline"):
                reclaim.apply_plan(mailbox, plan["plan_sha256"], 3600)
            for value in ("", "0" * 64):
                with self.assertRaisesRegex(reclaim.Refused, "plan changed"):
                    reclaim.apply_plan(mailbox, value, 3600, offline=True)
        self.assertFalse((self.archive / "doctor").exists())

    def test_changed_or_additional_stage_invalidates_preview_before_any_mutation(self):
        stage = self.stage()
        plan = self.preview()
        (stage / "snapshot.sqlite3").write_bytes(b"new generation")
        with self.assertRaisesRegex(reclaim.Refused, "plan changed"):
            self.apply(plan)
        plan = self.preview()
        self.stage("additional")
        with self.assertRaisesRegex(reclaim.Refused, "plan changed"):
            self.apply(plan)
        self.assertFalse((self.archive / "doctor").exists())
        self.assertTrue(stage.exists())

    def test_cross_process_export_lease_refuses_even_with_offline_flag(self):
        stage = self.stage()
        plan = self.preview()
        with self.exporter():
            completed, result = self.cli("--apply", "--offline", "--expect-plan", plan["plan_sha256"])
            self.assertEqual(completed.returncode, 3, completed.stderr)
            self.assertIn("lease", result["error"])
            self.assertFalse((self.archive / "doctor").exists())
            self.assertTrue(stage.exists())
        self.assertTrue(self.apply(plan)["ok"])

    def test_different_mailboxes_in_one_parent_share_exporter_exclusion(self):
        other = self.parent / "another.sqlite3"
        other.write_bytes(b"another mailbox")
        with self.mailbox():
            with self.assertRaisesRegex(reclaim.Refused, "lease"):
                with reclaim.Mailbox(other, self.archive):
                    self.fail("second mailbox bypassed the shared parent lease")

    def test_symlinked_ancestor_or_primary_is_never_followed(self):
        alias = self.root / "alias"
        alias.symlink_to(self.parent, target_is_directory=True)
        with self.assertRaises(OSError):
            with reclaim.Mailbox(alias / self.database.name, self.archive):
                self.fail("symlinked ancestor was followed")
        link = self.parent / "linked.sqlite3"
        link.symlink_to(self.database)
        with self.assertRaises(reclaim.Refused):
            with reclaim.Mailbox(link, self.archive):
                self.fail("symlinked primary was accepted")
        self.assert_protected()

    def test_stage_symlinks_special_files_and_hardlinks_are_refused(self):
        for kind in ("symlink", "fifo", "hardlink"):
            with self.subTest(kind=kind):
                stage = self.stage(kind)
                special = stage / "unexpected"
                if kind == "symlink":
                    special.symlink_to(self.database)
                elif kind == "fifo":
                    os.mkfifo(special)
                else:
                    os.link(self.database, special)
                with self.assertRaises((reclaim.Refused, OSError)):
                    self.preview()
                # Preserve the fixture outside the staging namespace, not delete it.
                stage.rename(self.root / ("retained-" + kind))
                if kind == "hardlink":
                    break  # The deliberately linked primary must remain refused.
        self.assertFalse((self.archive / "doctor").exists())
        self.assert_protected()

    def test_stage_shaped_regular_file_is_not_a_directory_to_move(self):
        path = self.parent / (reclaim.STAGE_PREFIX + "not-a-stage")
        path.write_bytes(b"unrelated file")
        with self.assertRaises(OSError):
            self.preview()
        self.assertEqual(path.read_bytes(), b"unrelated file")

    def test_inventory_budget_and_depth_fail_without_a_partial_plan(self):
        stage = self.stage()
        with self.mailbox() as mailbox:
            with self.assertRaisesRegex(reclaim.Refused, "entry limit"):
                mailbox.plan(3600, max_entries=2)
        nested = stage
        for _ in range(reclaim.MAX_DEPTH + 1):
            nested = nested / "d"
            nested.mkdir()
        with self.assertRaisesRegex(reclaim.Refused, "depth limit"):
            self.preview()
        self.assertFalse((self.archive / "doctor").exists())

    def test_empty_plan_is_a_side_effect_free_noop(self):
        result = self.apply()
        self.assertTrue(result["ok"])
        self.assertEqual(result["moved"], [])
        self.assertNotIn("run_directory", result)
        self.assertFalse((self.archive / "doctor").exists())

    def test_nonunicode_stage_names_and_sparse_sizes_are_byte_exact(self):
        stage = self.stage(os.fsdecode(b"nonunicode-\xff"))
        sparse = stage / "sparse"
        with sparse.open("wb") as file:
            file.truncate(2**32 + 17)
        when = time.time_ns() - 7200 * 1_000_000_000
        os.utime(sparse, ns=(when, when))
        os.utime(stage, ns=(when, when))
        plan = self.preview()
        self.assertEqual(plan["eligible"][0]["bytes"], 2**32 + 17 + 48)
        result = self.apply(plan)
        self.assertTrue(result["ok"], result)
        moved = Path(result["run_directory"]) / stage.name
        self.assertEqual((moved / "sparse").stat().st_size, 2**32 + 17)

    def test_no_replace_rename_preserves_existing_or_dangling_destinations(self):
        source = self.stage()
        dest = self.root / "destination"
        dest.mkdir()
        with reclaim.open_directory(self.parent) as parent, reclaim.open_directory(dest) as target:
            (dest / "occupied").mkdir()
            (dest / "dangling").symlink_to("absent")
            for name in ("occupied", "dangling"):
                with self.assertRaises(FileExistsError):
                    reclaim.rename_noreplace(parent, source.name, target, name)
                self.assertTrue(source.exists())
            self.assertTrue((dest / "occupied").is_dir())
            self.assertEqual(os.readlink(dest / "dangling"), "absent")

    def test_quarantine_paths_and_records_are_private(self):
        self.stage()
        result = self.apply()
        self.assertTrue(result["ok"], result)
        run = Path(result["run_directory"])
        for directory in (self.archive / "doctor", self.archive / "doctor/reclaimable", run):
            self.assertEqual(directory.stat().st_mode & 0o077, 0)
        for name in ("manifest.json", "actions.jsonl", "result.json"):
            self.assertEqual((run / name).stat().st_mode & 0o077, 0)

    def test_symlinked_quarantine_parent_is_refused_without_touching_target(self):
        stage = self.stage()
        outside = self.root / "outside"
        outside.mkdir()
        (self.archive / "doctor").symlink_to(outside, target_is_directory=True)
        with self.assertRaises(OSError):
            self.apply()
        self.assertTrue(stage.exists())
        self.assertEqual(list(outside.iterdir()), [])

    def test_archive_inside_stage_is_refused_before_writing_recovery_records(self):
        stage = self.stage()
        with reclaim.Mailbox(self.database, stage) as mailbox:
            with self.assertRaisesRegex(reclaim.Refused, "inside a staging"):
                mailbox.plan(3600)
        self.assertFalse((stage / "doctor").exists())

    def test_primary_or_parent_replacement_cannot_reuse_retained_authority(self):
        self.stage()
        with self.mailbox() as mailbox:
            self.database.rename(self.parent / "retained-primary")
            self.database.write_bytes(b"replacement")
            with self.assertRaisesRegex(reclaim.Refused, "identity changed"):
                mailbox.plan(3600)
        with self.mailbox() as mailbox:
            self.parent.rename(self.root / "retained-parent")
            self.parent.mkdir()
            self.database.write_bytes(b"unrelated replacement namespace")
            with self.assertRaisesRegex(reclaim.Refused, "namespace changed"):
                mailbox.plan(3600)
        self.assertFalse((self.archive / "doctor").exists())

    def test_post_rename_sync_failure_preserves_evidence_and_reports_uncertainty(self):
        first = self.stage("first")
        second = self.stage("second")
        plan = self.preview()
        renamed = False
        real_rename = reclaim.rename_noreplace
        real_sync = os.fsync
        with self.mailbox() as mailbox:
            def rename(*args):
                nonlocal renamed
                real_rename(*args)
                renamed = True

            def sync(fd):
                if renamed and fd == mailbox.parent:
                    raise OSError(errno.EIO, "injected directory sync failure")
                return real_sync(fd)

            with mock.patch.object(reclaim, "rename_noreplace", side_effect=rename), \
                 mock.patch.object(reclaim.os, "fsync", side_effect=sync):
                result = reclaim.apply_plan(mailbox, plan["plan_sha256"], 3600, offline=True)
        self.assertFalse(result["ok"])
        self.assertEqual(result["moved"], [])
        self.assertTrue(result["failures"][0]["rename_completed"])
        run = Path(result["run_directory"])
        self.assertFalse(first.exists())
        self.assertTrue(second.exists())
        self.assertEqual((run / first.name / "snapshot.sqlite3").read_bytes(), b"retained snapshot")
        self.assertEqual(len((run / "actions.jsonl").read_text().splitlines()), 1)
        self.assertTrue((run / "manifest.json").is_file())
        self.assert_protected()

    def test_cross_device_refusal_does_not_copy_or_remove_the_source(self):
        if not Path("/dev/shm").is_dir() or self.parent.stat().st_dev == Path("/dev/shm").stat().st_dev:
            self.skipTest("a second filesystem is unavailable")
        self.archive = Path(tempfile.mkdtemp(prefix="am-stage-cross-device-", dir="/dev/shm"))
        stage = self.stage()
        result = self.apply()
        self.assertFalse(result["ok"])
        self.assertFalse(result["failures"][0]["rename_completed"])
        self.assertTrue(stage.exists())
        self.assertFalse((Path(result["run_directory"]) / stage.name).exists())

    def test_cli_preview_apply_and_noop_are_real_subprocesses(self):
        self.stage()
        completed, preview = self.cli()
        self.assertEqual(completed.returncode, 0, completed.stderr)
        self.assertEqual(preview["mode"], "dry_run")
        self.assertFalse((self.archive / "doctor").exists())
        completed, applied = self.cli("--apply", "--offline", "--expect-plan", preview["plan_sha256"])
        self.assertEqual(completed.returncode, 0, completed.stderr)
        self.assertEqual(len(applied["moved"]), 1)
        completed, preview = self.cli()
        self.assertEqual(completed.returncode, 0, completed.stderr)
        self.assertEqual(preview["eligible"], [])
        self.assert_protected()

    def recover(self, run, mode="inspect", plan=None):
        plan = json.loads((run / "manifest.json").read_text())["plan"] if plan is None else plan
        with self.mailbox() as mailbox:
            return reclaim.recover_run(mailbox, run, mode=mode, offline=mode != "inspect",
                                       expected_digest=plan["plan_sha256"])

    def replace_manifest(self, run, value):
        path = run / "manifest.json"
        ordinal = len(list(run.iterdir()))
        path.rename(run / f"retained-manifest-{ordinal}.json")
        path.write_bytes(value if isinstance(value, bytes) else reclaim.canonical_json(value))
        path.chmod(0o600)

    def test_actual_process_exit_after_rename_can_resume_and_undo_in_fresh_processes(self):
        stages = [self.stage("first"), self.stage("second")]
        ids = {stage.name: stage.stat().st_ino for stage in stages}
        plan = self.preview()
        code = """
import importlib.util, os, sys
spec = importlib.util.spec_from_file_location('recovery_child', sys.argv[1])
module = importlib.util.module_from_spec(spec)
spec.loader.exec_module(module)
real = module.rename_noreplace
def crash(*args):
    real(*args)
    os._exit(77)
module.rename_noreplace = crash
with module.Mailbox(sys.argv[2], sys.argv[3]) as mailbox:
    module.apply_plan(mailbox, sys.argv[4], 3600, offline=True)
raise AssertionError('the process never reached an actual rename')
"""
        child = subprocess.run([sys.executable, "-c", code, str(SCRIPT), str(self.database),
                                str(self.archive), plan["plan_sha256"]], capture_output=True, timeout=10)
        self.assertEqual(child.returncode, 77, child.stderr)
        runs = list((self.archive / "doctor/reclaimable").iterdir())
        self.assertEqual(len(runs), 1)
        run = runs[0]
        original_actions = (run / "actions.jsonl").read_bytes()
        self.assertEqual(len(original_actions.splitlines()), 1, "intent exists but completion does not")
        self.assertFalse((run / "result.json").exists())
        before = sorted(path.name for path in run.iterdir())
        completed, inspection = self.cli("--inspect-run", str(run))
        self.assertEqual(completed.returncode, 0, completed.stderr)
        self.assertEqual({item["location"] for item in inspection["locations"]}, {"source", "quarantine"})
        self.assertEqual(before, sorted(path.name for path in run.iterdir()), "inspection never writes")
        completed, resumed = self.cli("--resume", str(run), "--offline", "--expect-plan", plan["plan_sha256"])
        self.assertEqual(completed.returncode, 0, completed.stderr)
        self.assertEqual(len(resumed["moved"]), 1)
        self.assertEqual(len(resumed["unchanged"]), 1)
        self.assertTrue(all(item["location"] == "quarantine" for item in resumed["locations_after"]))
        again = self.recover(run, "resume", plan)
        self.assertTrue(again["ok"])
        self.assertEqual(again["moved"], [])
        self.assertEqual(len(again["unchanged"]), 2)
        completed, restored = self.cli("--restore", str(run), "--offline", "--expect-plan", plan["plan_sha256"])
        self.assertEqual(completed.returncode, 0, completed.stderr)
        self.assertEqual(len(restored["moved"]), 2)
        self.assertTrue(all(item["location"] == "source" for item in restored["locations_after"]))
        again = self.recover(run, "restore", plan)
        self.assertTrue(again["ok"])
        self.assertEqual(again["moved"], [])
        self.assertEqual(len(again["unchanged"]), 2)
        self.assertEqual((run / "actions.jsonl").read_bytes(), original_actions)
        for stage in stages:
            self.assertEqual(stage.stat().st_ino, ids[stage.name])
            self.assertEqual((stage / "snapshot.sqlite3").read_bytes(), b"retained snapshot")
        self.assert_protected()

    def test_restore_collision_refuses_every_stage_before_new_records(self):
        first = self.stage("first")
        second = self.stage("second")
        run = Path(self.apply()["run_directory"])
        second.mkdir()
        (second / "unrelated").write_bytes(b"new active generation")
        before = sorted(path.name for path in run.iterdir())
        for mode in ("inspect", "resume", "restore"):
            with self.assertRaisesRegex(reclaim.Refused, "occupied"):
                self.recover(run, mode)
        self.assertFalse(first.exists(), "a later conflict prevents partial restoration")
        self.assertEqual((second / "unrelated").read_bytes(), b"new active generation")
        self.assertEqual(before, sorted(path.name for path in run.iterdir()))

    def test_restore_never_overwrites_a_dangling_source_symlink(self):
        stage = self.stage()
        run = Path(self.apply()["run_directory"])
        stage.symlink_to("missing")
        with self.assertRaisesRegex(reclaim.Refused, "occupied"):
            self.recover(run, "restore")
        self.assertEqual(os.readlink(stage), "missing")
        self.assertTrue((run / stage.name).is_dir())

    def test_edited_quarantined_payload_does_not_match_the_original_witness(self):
        stage = self.stage()
        run = Path(self.apply()["run_directory"])
        changed = run / stage.name / "snapshot.sqlite3"
        changed.write_bytes(b"operator modified evidence")
        before = sorted(path.name for path in run.iterdir())
        for mode in ("resume", "restore"):
            with self.assertRaisesRegex(reclaim.Refused, "changed since preview"):
                self.recover(run, mode)
        self.assertEqual(changed.read_bytes(), b"operator modified evidence")
        self.assertEqual(before, sorted(path.name for path in run.iterdir()))

    def test_missing_recorded_stage_cannot_be_reported_as_completed(self):
        stage = self.stage()
        run = Path(self.apply()["run_directory"])
        (run / stage.name).rename(self.root / "operator-retained-stage")
        with self.assertRaisesRegex(reclaim.Refused, "exactly one"):
            self.recover(run, "resume")
        self.assertTrue((self.root / "operator-retained-stage/snapshot.sqlite3").is_file())

    def test_other_database_or_archive_cannot_use_a_manifest(self):
        self.stage()
        run = Path(self.apply()["run_directory"])
        other = self.parent / "other.sqlite3"
        other.write_bytes(b"other mailbox")
        with reclaim.Mailbox(other, self.archive) as mailbox:
            with self.assertRaisesRegex(reclaim.Refused, "another mailbox"):
                reclaim.recover_run(mailbox, run)
        with reclaim.Mailbox(self.database, self.root) as mailbox:
            with self.assertRaisesRegex(reclaim.Refused, "under this archive"):
                reclaim.recover_run(mailbox, run)

    def test_corrupt_or_oversized_manifest_is_retained_without_new_actions(self):
        self.stage()
        run = Path(self.apply()["run_directory"])
        for payload in (b'{"schema":', b"x" * (reclaim.MAX_MANIFEST_BYTES + 1)):
            self.replace_manifest(run, payload)
            before = sorted(path.name for path in run.iterdir())
            with self.mailbox() as mailbox:
                with self.assertRaises(reclaim.Refused):
                    reclaim.recover_run(mailbox, run, mode="resume", offline=True, expected_digest="0" * 64)
            self.assertEqual((run / "manifest.json").read_bytes(), payload)
            self.assertEqual(before, sorted(path.name for path in run.iterdir()))

    def test_duplicate_json_keys_in_manifest_fail_closed(self):
        self.stage()
        run = Path(self.apply()["run_directory"])
        data = (run / "manifest.json").read_bytes()
        self.replace_manifest(run, b'{"schema":999,' + data[1:])
        with self.mailbox() as mailbox:
            with self.assertRaisesRegex(reclaim.Refused, "duplicate key"):
                reclaim.recover_run(mailbox, run)

    def test_manifest_traversal_duplicate_stages_and_unknown_schema_are_refused(self):
        self.stage()
        run = Path(self.apply()["run_directory"])
        original = json.loads((run / "manifest.json").read_text())
        for issue in ("traversal", "duplicate", "schema", "bounds", "checksum"):
            manifest = json.loads(json.dumps(original))
            plan = manifest["plan"]
            if issue == "traversal":
                plan["eligible"][0]["name"] = reclaim.STAGE_PREFIX + "x/../../custom.sqlite3"
            elif issue == "duplicate":
                plan["eligible"].append(plan["eligible"][0].copy())
            elif issue == "schema":
                manifest["schema"] = 999
            elif issue == "bounds":
                plan["eligible"][0]["entries"] = reclaim.MAX_ENTRIES + 1
            if issue == "checksum":
                plan["plan_sha256"] = "0" * 64
            else:
                plan["plan_sha256"] = reclaim.digest({key: value for key, value in plan.items() if key != "plan_sha256"})
            self.replace_manifest(run, manifest)
            with self.subTest(issue=issue), self.mailbox() as mailbox:
                with self.assertRaises(reclaim.Refused):
                    reclaim.recover_run(mailbox, run)
        self.assert_protected()

    def test_symlinked_manifest_is_not_read_or_replaced(self):
        self.stage()
        run = Path(self.apply()["run_directory"])
        manifest = run / "manifest.json"
        preserved = run / "preserved-manifest.json"
        manifest.rename(preserved)
        data = preserved.read_bytes()
        manifest.symlink_to(preserved)
        with self.mailbox() as mailbox:
            with self.assertRaises(OSError):
                reclaim.recover_run(mailbox, run)
        self.assertEqual(preserved.read_bytes(), data)
        self.assertEqual(os.readlink(manifest), str(preserved))

    def test_restore_requires_offline_and_original_plan_digest(self):
        self.stage()
        run = Path(self.apply()["run_directory"])
        plan = json.loads((run / "manifest.json").read_text())["plan"]
        before = sorted(path.name for path in run.iterdir())
        with self.mailbox() as mailbox:
            with self.assertRaisesRegex(reclaim.Refused, "offline"):
                reclaim.recover_run(mailbox, run, mode="restore", expected_digest=plan["plan_sha256"])
            with self.assertRaisesRegex(reclaim.Refused, "digest"):
                reclaim.recover_run(mailbox, run, mode="restore", offline=True)
        self.assertEqual(before, sorted(path.name for path in run.iterdir()))

    def test_restore_still_refuses_a_concurrent_exporter(self):
        self.stage()
        run = Path(self.apply()["run_directory"])
        plan = json.loads((run / "manifest.json").read_text())["plan"]
        with self.exporter():
            completed, result = self.cli("--restore", str(run), "--offline", "--expect-plan", plan["plan_sha256"])
            self.assertEqual(completed.returncode, 3)
            self.assertIn("lease", result["error"])

    def test_resume_after_post_rename_failure_rechecks_actual_evidence(self):
        stage = self.stage()
        plan = self.preview()
        real_rename = reclaim.rename_noreplace
        real_sync = os.fsync
        renamed = False
        with self.mailbox() as mailbox:
            def rename(*args):
                nonlocal renamed
                real_rename(*args)
                renamed = True

            def sync(fd):
                if renamed and fd == mailbox.parent:
                    raise OSError(errno.EIO, "unconfirmed publication")
                real_sync(fd)

            with mock.patch.object(reclaim, "rename_noreplace", side_effect=rename), \
                 mock.patch.object(reclaim.os, "fsync", side_effect=sync):
                result = reclaim.apply_plan(mailbox, plan["plan_sha256"], 3600, offline=True)
        self.assertFalse(result["ok"])
        run = Path(result["run_directory"])
        recovered = self.recover(run, "resume", plan)
        self.assertTrue(recovered["ok"])
        self.assertEqual(recovered["moved"], [])
        self.assertEqual(len(recovered["unchanged"]), 1)
        self.assertFalse(stage.exists())
        restored = self.recover(run, "restore", plan)
        self.assertTrue(restored["ok"])
        self.assertEqual((stage / "snapshot.sqlite3").read_bytes(), b"retained snapshot")

    def test_retained_young_stages_never_enter_resume_or_restore(self):
        old = self.stage("old")
        young = self.stage("young", old=False)
        run = Path(self.apply()["run_directory"])
        plan = json.loads((run / "manifest.json").read_text())["plan"]
        self.assertEqual(len(plan["retained"]), 1)
        for mode in ("inspect", "resume", "restore"):
            result = self.recover(run, mode, plan)
            self.assertTrue(result["ok"])
            self.assertTrue(young.exists())
            self.assertEqual((young / "snapshot.sqlite3").read_bytes(), b"retained snapshot")
        self.assertTrue(old.exists())

    def test_unsupported_platform_has_no_fallback_or_mutation(self):
        self.stage()
        with mock.patch.object(reclaim.sys, "platform", "win32"):
            with self.assertRaisesRegex(reclaim.Refused, "requires Linux"):
                self.preview()
        self.assertFalse((self.archive / "doctor").exists())


@unittest.skipUnless(sys.platform.startswith("linux"), "Linux no-replace recovery contract")
class BackupRotationTests(unittest.TestCase):
    mailbox = StagingRecoveryTests.mailbox
    cli = StagingRecoveryTests.cli
    exporter = StagingRecoveryTests.exporter
    assert_protected = StagingRecoveryTests.assert_protected
    recover = StagingRecoveryTests.recover
    replace_manifest = StagingRecoveryTests.replace_manifest
    stage = StagingRecoveryTests.stage

    def setUp(self):
        StagingRecoveryTests.setUp(self)
        self.meta = self.parent / (self.database.name + ".bak.meta.json")
        preserved = self.root / "original-unparseable-metadata"
        self.meta.rename(preserved)
        self.protected[preserved] = self.protected.pop(self.meta)

    def backup(self, ordinal=0, *, generation=None, old=True):
        generation = generation or f"20260101_{ordinal // 60:02}{ordinal % 60:02}00"
        path = self.parent / (self.database.name + ".bak." + generation)
        connection = sqlite3.connect(path)
        connection.execute("CREATE TABLE mail (generation INTEGER, payload TEXT)")
        connection.execute("INSERT INTO mail VALUES (?, ?)", (ordinal, "retained acknowledged mail"))
        connection.commit()
        connection.close()
        if old:
            when = time.time_ns() - 7200 * 1_000_000_000
            os.utime(path, ns=(when, when))
        return path

    def claim(self, source, **overrides):
        value = {"schema": 1, "created_us": 1_800_000_000_000_000,
                 "integrity_verified": True, "integrity_kind": "integrity_check",
                 "schema_version": 1, "row_counts": {"messages": 1},
                 "source_path": str(self.database), "snapshot_path": str(self.database) + ".bak",
                 "snapshot_size_bytes": source.stat().st_size,
                 "snapshot_sha256": reclaim.hashlib.sha256(source.read_bytes()).hexdigest(),
                 "binary_version": "test-fixture", **overrides}
        self.meta.write_bytes(reclaim.canonical_json(value))
        return value

    def preview(self, **kwargs):
        with self.mailbox() as mailbox:
            return mailbox.backup_plan(3600, **kwargs)

    def apply(self, plan=None, **kwargs):
        plan = self.preview(**kwargs) if plan is None else plan
        with self.mailbox() as mailbox:
            return reclaim.apply_plan(mailbox, plan["plan_sha256"], 3600, offline=True,
                                      rotated_backups=True, keep_backups=plan["keep_backups"],
                                      max_hash_bytes=plan["max_hash_bytes"])

    def rehash_manifest(self, manifest):
        plan = manifest["plan"]
        plan["plan_sha256"] = reclaim.digest({key: value for key, value in plan.items() if key != "plan_sha256"})

    def test_fifty_stale_metadata_backups_rotate_without_deleting_or_rewriting_authority(self):
        backups = [self.backup(i) for i in range(50)]
        self.claim(backups[0], snapshot_sha256="0" * 64)
        meta_before = self.meta.read_bytes()
        identities = {path.name: path.stat().st_ino for path in backups}
        before = sorted(str(path) for path in self.root.rglob("*"))
        plan = self.preview()
        self.assertEqual(len(plan["eligible"]), 47)
        self.assertEqual({item["name"] for item in plan["retained"]}, {path.name for path in backups[-3:]})
        self.assertEqual(before, sorted(str(path) for path in self.root.rglob("*")))
        self.assertEqual(plan, self.preview())
        result = self.apply(plan)
        self.assertTrue(result["ok"], result)
        self.assertEqual(len(result["moved"]), 47)
        self.assertEqual(result["freed_bytes"], 0)
        run = Path(result["run_directory"])
        for index, path in enumerate(backups):
            actual = path if index >= 47 else run / path.name
            self.assertEqual(actual.stat().st_ino, identities[path.name])
            with sqlite3.connect(f"file:{actual}?mode=ro&immutable=1", uri=True) as conn:
                self.assertEqual(conn.execute("SELECT generation FROM mail").fetchone()[0], index)
        self.assertEqual(self.meta.read_bytes(), meta_before)
        self.assertEqual(self.preview()["eligible"], [])
        self.assert_protected()

    def test_every_metadata_bound_copy_is_pinned_in_addition_to_the_newest_three(self):
        backups = [self.backup(i) for i in range(8)]
        # Two different inodes carrying the recorded bytes must both survive.
        backups[1].write_bytes(backups[0].read_bytes())
        when = time.time_ns() - 7200 * 1_000_000_000
        os.utime(backups[1], ns=(when, when))
        self.claim(backups[0])
        plan = self.preview()
        self.assertEqual({item["name"] for item in plan["retained"]},
                         {path.name for path in [*backups[:2], *backups[-3:]]})
        matches = [item for item in plan["retained"] if "snapshot_metadata_match" in item["retention_reasons"]]
        self.assertEqual(len(matches), 2)
        self.assertTrue(self.apply(plan)["ok"])
        for path in [*backups[:2], *backups[-3:]]:
            self.assertTrue(path.exists())
        self.assert_protected()

    def test_generation_order_uses_filename_timestamp_and_numeric_collision_not_mtime(self):
        names = ["20260101_000000", "20260102_000000", "20260102_000000-02", "20260102_000000-10"]
        backups = [self.backup(i, generation=name) for i, name in enumerate(names)]
        for index, path in enumerate(backups):
            when = time.time_ns() - (7200 + index * 3600) * 1_000_000_000
            os.utime(path, ns=(when, when))
        plan = self.preview(keep_backups=2)
        self.assertEqual({item["name"] for item in plan["retained"]}, {path.name for path in backups[-2:]})

    def test_complete_native_timestamp_grammar_rejects_private_and_lookalike_names(self):
        primary = "mail.db"
        valid = ["20260824_120102", "20260824_120102-01", "20260824_120102-100",
                 "20260824_120102-4294967295", "20240229_120000"]
        for suffix in valid:
            self.assertIsNotNone(reclaim.backup_generation(primary, primary + ".bak." + suffix))
        invalid = ["20260824_120102_345", "20260824_120102-00", "20260824_120102-1",
                   "20260824_120102-001", "20260824_120102-4294967296", "20260824_120102-wal",
                   "20260824_120102.meta.json", "20260824_120102-１", "2026082_120102",
                   "20260229_120000", "20260101_250000", "20260824_120102/child", ""]
        for suffix in invalid:
            self.assertIsNone(reclaim.backup_generation(primary, primary + ".bak." + suffix), suffix)
        for name in [primary + ".bak", primary + ".backup-20260824-120102",
                     "mail.db2.bak.20260824_120102", primary + "-wal.bak.20260824_120102"]:
            self.assertIsNone(reclaim.backup_generation(primary, name), name)

    def test_young_and_future_backups_survive_even_outside_the_keep_count(self):
        backups = [self.backup(i) for i in range(6)]
        os.utime(backups[0], None)
        future = time.time_ns() + 3600 * 1_000_000_000
        os.utime(backups[1], ns=(future, future))
        plan = self.preview(keep_backups=1)
        self.assertEqual({item["name"] for item in plan["retained"]},
                         {backups[0].name, backups[1].name, backups[-1].name})

    def test_sqlite_and_historical_companions_keep_the_whole_generation_in_place(self):
        backups = [self.backup(i) for i in range(5)]
        (self.parent / (backups[0].name + "-wal")).write_bytes(b"committed WAL")
        (self.parent / (self.database.name + "-shm" + backups[1].name[len(self.database.name):])).write_bytes(b"historical SHM")
        (self.parent / (backups[2].name + "-wal-cert")).symlink_to("missing-certificate")
        plan = self.preview(keep_backups=1)
        self.assertEqual([item["name"] for item in plan["eligible"]], [backups[3].name])
        self.assertTrue(self.apply(plan)["ok"])
        self.assertTrue(all(path.exists() for path in backups[:3]))
        self.assertEqual((self.parent / (backups[0].name + "-wal")).read_bytes(), b"committed WAL")

    def test_unknown_or_malformed_snapshot_authority_refuses_before_quarantine(self):
        backups = [self.backup(i) for i in range(5)]
        for change in [{"schema": 99}, {"schema": True}, {"integrity_verified": False},
                       {"snapshot_sha256": "garbage"}, {"snapshot_size_bytes": True},
                       {"source_path": "/another/mailbox"}, {"snapshot_path": str(backups[0])},
                       {"integrity_kind": "quick_check"}]:
            self.claim(backups[0], **change)
            with self.subTest(change=change), self.assertRaises(reclaim.Refused):
                self.preview()
            self.assertFalse((self.archive / "doctor").exists())
        for data in [b"not json", b'{"schema":1,"schema":1}', b"x" * (reclaim.MAX_SNAPSHOT_METADATA_BYTES + 1)]:
            self.meta.write_bytes(data)
            with self.assertRaises(reclaim.Refused):
                self.preview()
            self.assertEqual(self.meta.read_bytes(), data)

    def test_snapshot_metadata_symlinks_and_hardlinks_are_not_followed(self):
        self.backup()
        target = self.root / "outside-metadata"
        target.write_bytes(b"untouched")
        self.meta.symlink_to(target)
        with self.assertRaises(OSError):
            self.preview()
        self.meta.rename(self.root / "preserved-metadata-link")
        os.link(target, self.meta)
        with self.assertRaises(reclaim.Refused):
            self.preview()
        self.assertEqual(target.read_bytes(), b"untouched")
        self.assertFalse((self.archive / "doctor").exists())

    def test_file_fingerprints_detect_same_size_edits_even_with_restored_mtime(self):
        backups = [self.backup(i) for i in range(5)]
        plan = self.preview()
        path = backups[0]
        before = path.stat()
        changed = bytearray(path.read_bytes())
        changed[-1] ^= 1
        path.write_bytes(changed)
        os.utime(path, ns=(before.st_atime_ns, before.st_mtime_ns))
        with self.assertRaisesRegex(reclaim.Refused, "plan changed"):
            self.apply(plan)
        self.assertFalse((self.archive / "doctor").exists())

    def test_new_metadata_or_backup_or_companion_invalidates_preview(self):
        backups = [self.backup(i) for i in range(5)]
        plan = self.preview()
        self.claim(backups[0])
        with self.assertRaisesRegex(reclaim.Refused, "plan changed"):
            self.apply(plan)
        plan = self.preview()
        self.backup(6)
        with self.assertRaisesRegex(reclaim.Refused, "plan changed"):
            self.apply(plan)
        plan = self.preview()
        (self.parent / (backups[1].name + "-journal")).write_bytes(b"new companion")
        with self.assertRaisesRegex(reclaim.Refused, "plan changed"):
            self.apply(plan)
        self.assertFalse((self.archive / "doctor").exists())

    def test_hash_budget_refuses_sparse_oversize_before_reading_or_mutating(self):
        backup = self.backup()
        with backup.open("r+b") as file:
            file.truncate(2**32 + 17)
        with mock.patch.object(reclaim.os, "read", side_effect=AssertionError("oversized payload must not be read")):
            with self.assertRaisesRegex(reclaim.Refused, "hash byte budget"):
                self.preview(max_hash_bytes=1024)
        self.assertEqual(backup.stat().st_size, 2**32 + 17)
        self.assertFalse((self.archive / "doctor").exists())

    def test_policy_bounds_and_entry_budget_are_enforced(self):
        self.backup()
        for kwargs in [{"keep_backups": 0}, {"keep_backups": -1}, {"keep_backups": True},
                       {"max_hash_bytes": 0}, {"max_hash_bytes": 2**64}, {"max_entries": 0}]:
            with self.subTest(kwargs=kwargs), self.assertRaises(reclaim.Refused):
                self.preview(**kwargs)

    def test_backup_namespace_is_byte_exact_for_nonunicode_database_names(self):
        original = self.database
        self.database = self.parent / os.fsdecode(b"mail-\xff.db")
        original.rename(self.database)
        self.protected[self.database] = self.protected.pop(original)
        self.meta = self.parent / (self.database.name + ".bak.meta.json")
        backups = [self.backup(i) for i in range(5)]
        neighbor = self.parent / "mail-�.db.bak.20260101_000000"
        neighbor.write_bytes(b"another database's backup")
        plan = self.preview()
        self.assertEqual(len(plan["eligible"]), 2)
        self.assertTrue(self.apply(plan)["ok"])
        self.assertEqual(neighbor.read_bytes(), b"another database's backup")
        self.assertTrue(all(path.exists() for path in backups[-3:]))
        self.assert_protected()

    def test_stage_and_backup_scopes_do_not_move_each_others_artifacts(self):
        stage = self.stage()
        backups = [self.backup(i) for i in range(5)]
        plan = self.preview()
        self.assertTrue(self.apply(plan)["ok"])
        self.assertTrue(stage.exists())
        with self.mailbox() as mailbox:
            stage_plan = mailbox.plan(3600)
        self.assertEqual([item["name"] for item in stage_plan["eligible"]], [stage.name])
        self.assertTrue(all(path.exists() for path in backups[-3:]))

    def test_backup_cli_apply_inspect_restore_and_resume_use_the_original_manifest(self):
        backups = [self.backup(i) for i in range(6)]
        inodes = {path.name: path.stat().st_ino for path in backups}
        completed, plan = self.cli("--rotated-backups", "--keep-backups", "2")
        self.assertEqual(completed.returncode, 0, completed.stderr)
        self.assertEqual(plan["scope"], "rotated_backups")
        completed, applied = self.cli("--rotated-backups", "--keep-backups", "2", "--apply", "--offline",
                                      "--expect-plan", plan["plan_sha256"])
        self.assertEqual(completed.returncode, 0, completed.stderr)
        self.assertEqual(len(applied["moved"]), 4)
        run = Path(applied["run_directory"])
        before = sorted(path.name for path in run.iterdir())
        completed, inspection = self.cli("--inspect-run", str(run))
        self.assertEqual(completed.returncode, 0, completed.stderr)
        self.assertEqual(before, sorted(path.name for path in run.iterdir()))
        self.assertEqual(len(inspection["locations"]), 4)
        for mode in ("restore", "restore", "resume", "resume"):
            completed, result = self.cli("--" + mode, str(run), "--offline", "--expect-plan", plan["plan_sha256"])
            self.assertEqual(completed.returncode, 0, (completed.stderr, result))
        self.assertEqual(result["moved"], [])
        self.assertEqual(len(result["unchanged"]), 4)
        for index, path in enumerate(backups):
            actual = path if index >= 4 else run / path.name
            self.assertEqual(actual.stat().st_ino, inodes[path.name])
        self.assert_protected()

    def test_real_process_crash_after_backup_rename_recovers_without_duplicate_moves(self):
        backups = [self.backup(i) for i in range(5)]
        plan = self.preview()
        code = """
import importlib.util, os, sys
spec = importlib.util.spec_from_file_location('recovery_child', sys.argv[1])
module = importlib.util.module_from_spec(spec)
spec.loader.exec_module(module)
real = module.rename_noreplace
def crash(*args):
    real(*args)
    os._exit(77)
module.rename_noreplace = crash
with module.Mailbox(sys.argv[2], sys.argv[3]) as mailbox:
    module.apply_plan(mailbox, sys.argv[4], 3600, offline=True, rotated_backups=True)
raise AssertionError('no actual backup rename')
"""
        child = subprocess.run([sys.executable, "-c", code, str(SCRIPT), str(self.database), str(self.archive),
                                plan["plan_sha256"]], capture_output=True, timeout=10)
        self.assertEqual(child.returncode, 77, child.stderr)
        run, = (self.archive / "doctor/reclaimable").iterdir()
        completed, inspection = self.cli("--inspect-run", str(run))
        self.assertEqual(completed.returncode, 0, (completed.stderr, inspection))
        self.assertEqual({item["location"] for item in inspection["locations"]}, {"source", "quarantine"})
        completed, resumed = self.cli("--resume", str(run), "--offline", "--expect-plan", plan["plan_sha256"])
        self.assertEqual(completed.returncode, 0, (completed.stderr, resumed))
        self.assertEqual(len(resumed["moved"]), 1)
        self.assertEqual(len(resumed["unchanged"]), 1)
        completed, restored = self.cli("--restore", str(run), "--offline", "--expect-plan", plan["plan_sha256"])
        self.assertEqual(completed.returncode, 0, (completed.stderr, restored))
        self.assertEqual(len(restored["moved"]), 2)
        self.assertTrue(all(path.exists() for path in backups))

    def test_metadata_change_before_rename_stops_publication_without_destroying_evidence(self):
        backups = [self.backup(i) for i in range(5)]
        plan = self.preview()
        write = reclaim.write_record
        def inject(fd, value):
            if isinstance(value, dict) and value.get("phase") == "intent":
                self.claim(backups[0])
            return write(fd, value)
        with mock.patch.object(reclaim, "write_record", side_effect=inject):
            result = self.apply(plan)
        self.assertFalse(result["ok"])
        self.assertFalse(result["failures"][0]["rename_completed"])
        self.assertIn("metadata changed", result["failures"][0]["error"])
        self.assertTrue(all(path.exists() for path in backups))

    def test_new_companion_before_rename_is_refused(self):
        backups = [self.backup(i) for i in range(5)]
        plan = self.preview()
        write = reclaim.write_record
        def inject(fd, value):
            if isinstance(value, dict) and value.get("phase") == "intent":
                (self.parent / (value["name"] + "-wal")).write_bytes(b"raced WAL")
            return write(fd, value)
        with mock.patch.object(reclaim, "write_record", side_effect=inject):
            result = self.apply(plan)
        self.assertFalse(result["ok"])
        self.assertFalse(result["failures"][0]["rename_completed"])
        self.assertIn("companion", result["failures"][0]["error"])
        self.assertTrue(all(path.exists() for path in backups))

    def test_quarantined_backup_edit_is_detected_even_if_mtime_is_restored(self):
        backups = [self.backup(i) for i in range(5)]
        run = Path(self.apply()["run_directory"])
        path = run / backups[0].name
        before = path.stat()
        data = bytearray(path.read_bytes())
        data[-1] ^= 1
        path.write_bytes(data)
        os.utime(path, ns=(before.st_atime_ns, before.st_mtime_ns))
        entries = sorted(path.name for path in run.iterdir())
        for mode in ("inspect", "resume", "restore"):
            with self.assertRaisesRegex(reclaim.Refused, "changed since preview"):
                self.recover(run, mode)
        self.assertEqual(entries, sorted(path.name for path in run.iterdir()))

    def test_backup_restore_collision_refuses_before_moving_any_generation(self):
        backups = [self.backup(i) for i in range(5)]
        run = Path(self.apply()["run_directory"])
        backups[1].write_bytes(b"replacement generation")
        with self.assertRaisesRegex(reclaim.Refused, "occupied"):
            self.recover(run, "restore")
        self.assertFalse(backups[0].exists())
        self.assertEqual(backups[1].read_bytes(), b"replacement generation")

    def test_changed_snapshot_authority_blocks_recorded_backup_operations(self):
        backups = [self.backup(i) for i in range(5)]
        run = Path(self.apply()["run_directory"])
        self.claim(backups[-1])
        before = sorted(path.name for path in run.iterdir())
        with self.assertRaisesRegex(reclaim.Refused, "metadata changed"):
            self.recover(run, "resume")
        self.assertEqual(before, sorted(path.name for path in run.iterdir()))

    def test_manifest_cannot_select_current_backup_or_violate_the_keep_floor(self):
        for issue in ("current", "newest", "zero_keep"):
            with self.subTest(issue=issue):
                # Independent retained fixture for each malformed plan.
                self.setUp()
                for i in range(5):
                    self.backup(i)
                run = Path(self.apply()["run_directory"])
                manifest = json.loads((run / "manifest.json").read_text())
                plan = manifest["plan"]
                if issue == "current":
                    plan["eligible"][0]["name"] = self.database.name + ".bak"
                elif issue == "newest":
                    item = plan["retained"].pop()
                    item["retention_reasons"] = []
                    plan["eligible"].append(item)
                else:
                    plan["keep_backups"] = 0
                self.rehash_manifest(manifest)
                self.replace_manifest(run, manifest)
                with self.assertRaises(reclaim.Refused):
                    self.recover(run, "restore")
                self.assert_protected()

    def test_backup_symlinks_and_hardlinks_are_not_reclaimed(self):
        backups = [self.backup(i) for i in range(5)]
        victim = backups[0]
        preserved = self.root / "preserved-backup"
        victim.rename(preserved)
        victim.symlink_to(preserved)
        with self.assertRaises(OSError):
            self.preview()
        victim.rename(self.root / "preserved-backup-link")
        os.link(preserved, victim)
        with self.assertRaises(reclaim.Refused):
            self.preview()
        self.assertFalse((self.archive / "doctor").exists())

    def test_backup_apply_and_recovery_refuse_an_independent_exporter(self):
        for i in range(5):
            self.backup(i)
        plan = self.preview()
        with self.exporter():
            completed, result = self.cli("--rotated-backups", "--apply", "--offline", "--expect-plan", plan["plan_sha256"])
            self.assertEqual(completed.returncode, 3)
            self.assertIn("lease", result["error"])
        run = Path(self.apply(plan)["run_directory"])
        with self.exporter():
            completed, result = self.cli("--restore", str(run), "--offline", "--expect-plan", plan["plan_sha256"])
            self.assertEqual(completed.returncode, 3)
            self.assertIn("lease", result["error"])

    def test_backup_cross_device_moves_have_no_copy_and_delete_fallback(self):
        if not Path("/dev/shm").is_dir() or self.parent.stat().st_dev == Path("/dev/shm").stat().st_dev:
            self.skipTest("a second filesystem is unavailable")
        self.archive = Path(tempfile.mkdtemp(prefix="am-backup-cross-device-", dir="/dev/shm"))
        backups = [self.backup(i) for i in range(5)]
        result = self.apply()
        self.assertFalse(result["ok"])
        self.assertFalse(result["failures"][0]["rename_completed"])
        self.assertTrue(all(path.exists() for path in backups))

    def test_cli_rejects_ignored_scope_options_and_unsafe_keep_counts(self):
        for args in [("--keep-backups", "2"), ("--rotated-backups", "--keep-backups", "0"),
                     ("--rotated-backups", "--max-hash-bytes", "0"),
                     ("--rotated-backups", "--inspect-run", str(self.archive))]:
            completed, result = self.cli(*args)
            self.assertEqual(completed.returncode, 3, (args, result))
            self.assertFalse(result["ok"])
        self.assertFalse((self.archive / "doctor").exists())

    def test_oversized_recovery_manifest_is_refused_before_creating_a_run(self):
        for i in range(5):
            self.backup(i)
        plan = self.preview()
        with mock.patch.object(reclaim, "MAX_MANIFEST_BYTES", 64):
            with self.assertRaisesRegex(reclaim.Refused, "manifest would exceed"):
                self.apply(plan)
        self.assertFalse((self.archive / "doctor").exists())

    def test_backup_fingerprinting_does_not_open_any_database_connection(self):
        for i in range(5):
            self.backup(i)
        with mock.patch.object(sqlite3, "connect", side_effect=AssertionError("no SQLite opens during recovery")):
            plan = self.preview()
            result = self.apply(plan)
            self.assertTrue(result["ok"], result)


if __name__ == "__main__":
    unittest.main()
