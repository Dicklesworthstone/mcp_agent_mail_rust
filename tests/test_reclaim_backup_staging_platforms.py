"""Portable recovery regressions; real filesystem tests run on Linux and macOS."""
import ctypes
import errno
import importlib.util
import os
from pathlib import Path
from types import SimpleNamespace
import sys
import tempfile
import unittest
from unittest.mock import Mock, patch

SCRIPT = Path(__file__).resolve().parents[1] / "scripts" / "reclaim_backup_staging.py"
spec = importlib.util.spec_from_file_location("reclaim_platform_fixture", SCRIPT)
recovery = importlib.util.module_from_spec(spec)
spec.loader.exec_module(recovery)


class RenameContractTests(unittest.TestCase):
    def test_supported_platforms(self):
        for platform in ("linux", "linux2", "darwin"):
            with self.subTest(platform=platform), patch.object(recovery.sys, "platform", platform):
                recovery.require_platform()

    def test_unsupported_platforms_fail_before_loading_libc(self):
        for platform in ("win32", "freebsd14", "aix", "unknown"):
            with self.subTest(platform=platform), patch.object(recovery.sys, "platform", platform):
                with patch.object(recovery.ctypes, "CDLL") as library:
                    with self.assertRaises(recovery.Refused):
                        recovery.rename_noreplace(17, "stage", 23, "stage")
                    library.assert_not_called()

    def test_platforms_use_exclusive_fd_relative_abi(self):
        for platform, symbol, flag in (("linux", "renameat2", 1), ("darwin", "renameatx_np", 4)):
            rename = Mock(return_value=0)
            libc = SimpleNamespace(**{symbol: rename})
            with self.subTest(platform=platform), patch.object(recovery.sys, "platform", platform):
                with patch.object(recovery.ctypes, "CDLL", return_value=libc) as library:
                    recovery.rename_noreplace(17, "stage-é", 23, "kept-é")
                library.assert_called_once_with(None, use_errno=True)
                rename.assert_called_once_with(17, os.fsencode("stage-é"), 23, os.fsencode("kept-é"), flag)
                self.assertEqual(rename.argtypes, [ctypes.c_int, ctypes.c_char_p, ctypes.c_int,
                                                  ctypes.c_char_p, ctypes.c_uint])
                self.assertIs(rename.restype, ctypes.c_int)

    def test_missing_native_symbol_never_uses_plain_rename(self):
        for platform, symbol in (("linux", "renameat2"), ("darwin", "renameatx_np")):
            with self.subTest(platform=platform), patch.object(recovery.sys, "platform", platform):
                with patch.object(recovery.ctypes, "CDLL", return_value=SimpleNamespace()):
                    with patch.object(recovery.os, "rename") as plain:
                        with self.assertRaisesRegex(recovery.Refused, symbol):
                            recovery.rename_noreplace(17, "stage", 23, "stage")
                        plain.assert_not_called()

    def test_native_errors_are_propagated_without_fallback(self):
        for platform, symbol in (("linux", "renameat2"), ("darwin", "renameatx_np")):
            for code in (errno.EEXIST, errno.EXDEV, errno.ENOSYS, errno.EACCES):
                def refused(*args):
                    ctypes.set_errno(code)
                    return -1
                rename = Mock(side_effect=refused)
                with self.subTest(platform=platform, code=code), patch.object(recovery.sys, "platform", platform):
                    with patch.object(recovery.ctypes, "CDLL", return_value=SimpleNamespace(**{symbol: rename})):
                        with patch.object(recovery.os, "rename") as plain:
                            with self.assertRaises(OSError) as raised:
                                recovery.rename_noreplace(17, "stage", 23, "stage")
                            self.assertEqual(raised.exception.errno, code)
                            plain.assert_not_called()
                            self.assertEqual(rename.call_count, 1)

    def test_stale_errno_is_not_reported_as_the_current_failure(self):
        ctypes.set_errno(errno.EEXIST)
        with patch.object(recovery.sys, "platform", "darwin"):
            with patch.object(recovery.ctypes, "CDLL", return_value=SimpleNamespace(renameatx_np=Mock(return_value=-1))):
                with self.assertRaises(OSError) as raised:
                    recovery.rename_noreplace(17, "stage", 23, "stage")
                self.assertEqual(raised.exception.errno, errno.EIO)

    def test_invalid_components_are_rejected_before_libc(self):
        for name in ("", ".", "..", "../stage", "/stage", "nested/stage", "stage\0other", None):
            for source, target in ((name, "safe"), ("safe", name)):
                with self.subTest(source=source, target=target), patch.object(recovery.sys, "platform", "darwin"):
                    with patch.object(recovery.ctypes, "CDLL") as library:
                        with self.assertRaises(recovery.Refused):
                            recovery.rename_noreplace(17, source, 23, target)
                        library.assert_not_called()

    def test_non_utf8_names_round_trip_without_loss(self):
        name = os.fsdecode(b"stage-\xff")
        rename = Mock(return_value=0)
        with patch.object(recovery.sys, "platform", "darwin"):
            with patch.object(recovery.ctypes, "CDLL", return_value=SimpleNamespace(renameatx_np=rename)):
                recovery.rename_noreplace(17, name, 23, name)
        rename.assert_called_once_with(17, b"stage-\xff", 23, b"stage-\xff", 4)


@unittest.skipUnless(sys.platform.startswith("linux") or sys.platform == "darwin", "native Unix recovery")
class NativeRecoveryTests(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.addCleanup(self.temp.cleanup)
        self.root = Path(self.temp.name).resolve()
        self.source = self.root / "source"
        self.target = self.root / "target"
        self.source.mkdir()
        self.target.mkdir()
        self.database = self.source / "mail.sqlite3"
        self.database.write_bytes(b"live database evidence")

    def rename(self, source, target):
        with recovery.open_directory(self.source) as src, recovery.open_directory(self.target) as dst:
            recovery.rename_noreplace(src, source, dst, target)

    def stage(self, index):
        stage = self.source / (recovery.STAGE_PREFIX + str(index))
        stage.mkdir()
        (stage / "live-export.sqlite3").write_bytes(b"failed export evidence")
        return stage

    def test_native_file_move_preserves_identity_and_bytes(self):
        path = self.source / "backup-é"
        path.write_bytes(b"snapshot")
        identity = recovery.object_id(path.stat())
        self.rename(path.name, path.name)
        moved = self.target / path.name
        self.assertFalse(path.exists())
        self.assertEqual(moved.read_bytes(), b"snapshot")
        self.assertEqual(recovery.object_id(moved.stat()), identity)

    def test_native_file_collision_preserves_both_files(self):
        (self.source / "backup").write_bytes(b"source")
        (self.target / "backup").write_bytes(b"target")
        with self.assertRaises(FileExistsError):
            self.rename("backup", "backup")
        self.assertEqual((self.source / "backup").read_bytes(), b"source")
        self.assertEqual((self.target / "backup").read_bytes(), b"target")

    def test_native_empty_directory_collision_is_not_replaced(self):
        (self.source / "stage").mkdir()
        (self.target / "stage").mkdir()
        before = [recovery.object_id((parent / "stage").stat()) for parent in (self.source, self.target)]
        with self.assertRaises(FileExistsError):
            self.rename("stage", "stage")
        self.assertEqual(before, [recovery.object_id((parent / "stage").stat())
                                  for parent in (self.source, self.target)])

    def test_native_destination_symlink_is_not_followed_or_replaced(self):
        (self.source / "backup").write_bytes(b"source")
        (self.target / "victim").write_bytes(b"victim")
        (self.target / "backup").symlink_to("victim")
        with self.assertRaises(FileExistsError):
            self.rename("backup", "backup")
        self.assertTrue((self.target / "backup").is_symlink())
        self.assertEqual((self.source / "backup").read_bytes(), b"source")
        self.assertEqual((self.target / "victim").read_bytes(), b"victim")

    def test_native_missing_source_does_not_create_target(self):
        with self.assertRaises(FileNotFoundError):
            self.rename("missing", "missing")
        self.assertFalse((self.target / "missing").exists())

    def test_staging_apply_inspect_restore_and_resume(self):
        stages = [self.stage(index) for index in range(3)]
        identities = {stage.name: recovery.object_id(stage.stat()) for stage in stages}
        with recovery.Mailbox(self.database, self.target) as mailbox:
            plan = mailbox.plan(0)
            self.assertEqual(len(plan["eligible"]), 3)
            self.assertTrue(all(stage.exists() for stage in stages))
            with self.assertRaises(recovery.Refused):
                recovery.apply_plan(mailbox, plan["plan_sha256"], 0)
            result = recovery.apply_plan(mailbox, plan["plan_sha256"], 0, offline=True)
            self.assertTrue(result["ok"], result)
            self.assertEqual(result["freed_bytes"], 0)
            run = result["run_directory"]
            inspected = recovery.recover_run(mailbox, run)
            self.assertEqual({item["location"] for item in inspected["locations"]}, {"quarantine"})
            restored = recovery.recover_run(mailbox, run, mode="restore", offline=True,
                                            expected_digest=plan["plan_sha256"])
            self.assertTrue(restored["ok"], restored)
            self.assertEqual(identities, {stage.name: recovery.object_id(stage.stat()) for stage in stages})
            resumed = recovery.recover_run(mailbox, run, mode="resume", offline=True,
                                           expected_digest=plan["plan_sha256"])
            self.assertTrue(resumed["ok"], resumed)
            repeated = recovery.recover_run(mailbox, run, mode="resume", offline=True,
                                            expected_digest=plan["plan_sha256"])
            self.assertTrue(repeated["ok"], repeated)
            self.assertEqual(repeated["moved"], [])
            self.assertEqual(len(repeated["unchanged"]), 3)
        self.assertEqual(self.database.read_bytes(), b"live database evidence")

    def test_rotated_backups_keep_newest_and_round_trip(self):
        backups = []
        for day in range(1, 6):
            path = self.source / f"mail.sqlite3.bak.202601{day:02}_000000"
            path.write_bytes(f"backup-{day}".encode())
            backups.append(path)
        current = self.source / "mail.sqlite3.bak"
        current.write_bytes(b"current snapshot")
        identities = {path.name: recovery.object_id(path.stat()) for path in backups}
        with recovery.Mailbox(self.database, self.target) as mailbox:
            plan = mailbox.backup_plan(0, keep_backups=3)
            self.assertEqual(len(plan["eligible"]), 2)
            self.assertEqual(len(plan["retained"]), 3)
            result = recovery.apply_plan(mailbox, plan["plan_sha256"], 0, offline=True,
                                         rotated_backups=True, keep_backups=3)
            self.assertTrue(result["ok"], result)
            self.assertTrue(all(path.exists() for path in backups[2:]))
            restored = recovery.recover_run(mailbox, result["run_directory"], mode="restore", offline=True,
                                            expected_digest=plan["plan_sha256"])
            self.assertTrue(restored["ok"], restored)
            self.assertEqual(identities, {path.name: recovery.object_id(path.stat()) for path in backups})
        self.assertEqual(current.read_bytes(), b"current snapshot")

    def test_interruption_after_rename_recovers_from_actual_locations(self):
        stages = [self.stage(index) for index in range(2)]
        native = recovery.rename_noreplace
        def interrupted(*args):
            native(*args)
            raise OSError(errno.EIO, "simulated interruption after native rename")
        with recovery.Mailbox(self.database, self.target) as mailbox:
            plan = mailbox.plan(0)
            with patch.object(recovery, "rename_noreplace", side_effect=interrupted):
                result = recovery.apply_plan(mailbox, plan["plan_sha256"], 0, offline=True)
            self.assertFalse(result["ok"])
            inspected = recovery.recover_run(mailbox, result["run_directory"])
            self.assertEqual({item["location"] for item in inspected["locations"]}, {"source", "quarantine"})
            resumed = recovery.recover_run(mailbox, result["run_directory"], mode="resume", offline=True,
                                           expected_digest=plan["plan_sha256"])
            self.assertTrue(resumed["ok"], resumed)
            self.assertEqual(len(resumed["moved"]), 1)
            self.assertEqual(len(resumed["unchanged"]), 1)
            self.assertTrue(all(not stage.exists() for stage in stages))

    def test_parent_exporter_lock_is_still_enforced(self):
        self.stage(0)
        with recovery.Mailbox(self.database, self.target):
            with self.assertRaises(recovery.Refused):
                with recovery.Mailbox(self.database, self.target):
                    self.fail("second parent lock unexpectedly succeeded")
        self.assertFalse((self.target / "doctor").exists())

    def test_changed_preview_never_creates_quarantine(self):
        stage = self.stage(0)
        with recovery.Mailbox(self.database, self.target) as mailbox:
            plan = mailbox.plan(0)
            (stage / "new-evidence").write_bytes(b"late write")
            with self.assertRaises(recovery.Refused):
                recovery.apply_plan(mailbox, plan["plan_sha256"], 0, offline=True)
        self.assertFalse((self.target / "doctor").exists())
        self.assertEqual((stage / "new-evidence").read_bytes(), b"late write")


if __name__ == "__main__":
    unittest.main()
