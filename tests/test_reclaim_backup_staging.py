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


if __name__ == "__main__":
    unittest.main()
