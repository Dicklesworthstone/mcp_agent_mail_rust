"""Real filesystem/SQLite fixtures for the read-only backup readiness command."""
import hashlib
import importlib.util
from contextlib import closing
import json
import os
from pathlib import Path
import sqlite3
import subprocess
import sys
import tempfile
import time
import unittest
from unittest.mock import patch

SCRIPT = Path(__file__).resolve().parents[1] / "scripts/check_backup_health.py"
spec = importlib.util.spec_from_file_location("backup_health_fixture", SCRIPT)
health = importlib.util.module_from_spec(spec)
spec.loader.exec_module(health)


@unittest.skipUnless(sys.platform.startswith("linux") or sys.platform == "darwin", "Unix descriptor inspection")
class BackupHealthTests(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.addCleanup(self.temp.cleanup)
        self.root = Path(self.temp.name).resolve()
        self.parent = self.root / "mail"
        self.parent.mkdir()
        self.database = self.parent / "mail.sqlite3"
        self.backup = self.parent / "mail.sqlite3.bak"
        with closing(sqlite3.connect(self.database)) as source:
            with source:
                source.execute("CREATE TABLE messages(id INTEGER PRIMARY KEY, body TEXT)")
                source.execute("INSERT INTO messages(body) VALUES ('durable message')")
        with closing(sqlite3.connect(self.database)) as source, closing(sqlite3.connect(self.backup)) as target:
            source.backup(target)
        self.meta = self.parent / "mail.sqlite3.bak.meta.json"
        self.now = time.time_ns() // 1000
        self.claim = {"schema": 1, "created_us": self.now, "integrity_verified": True,
                      "integrity_kind": "integrity_check", "schema_version": 1,
                      "row_counts": {"messages": 1}, "source_path": str(self.database),
                      "snapshot_path": str(self.backup), "snapshot_size_bytes": self.backup.stat().st_size,
                      "snapshot_sha256": hashlib.sha256(self.backup.read_bytes()).hexdigest(),
                      "binary_version": "0.3.39"}
        self.write_claim()

    def write_claim(self):
        self.meta.write_text(json.dumps(self.claim))

    def inspect(self, **options):
        return health.inspect(self.database, now_us=self.now, **options)

    def codes(self, report):
        return {entry["code"] for entry in report["findings"]}

    def stage(self, ordinal, payload=b"failed export"):
        stage = self.parent / f"{health.STAGE_PREFIX}{ordinal}"
        stage.mkdir()
        (stage / "live-export.sqlite3").write_bytes(payload)
        return stage

    def cli(self, *args):
        run = subprocess.run([sys.executable, str(SCRIPT), "--database", str(self.database), *args],
                             capture_output=True, text=True, timeout=10)
        return run.returncode, json.loads(run.stdout)

    def test_healthy_snapshot_does_not_open_sqlite_or_modify_evidence(self):
        paths = [self.database, self.backup, self.meta]
        before = {path: (path.read_bytes(), health.stamp(path.stat())) for path in paths}
        with patch.object(sqlite3, "connect", side_effect=AssertionError("must not open SQLite")):
            report = self.inspect()
        self.assertTrue(report["ok"], report)
        self.assertEqual(report["backups"]["matching_snapshot_images"], [self.backup.name])
        self.assertEqual(report["live_integrity"], "not_probed")
        self.assertFalse(report["backups"]["fresh_integrity_check_performed"])
        self.assertEqual(before, {path: (path.read_bytes(), health.stamp(path.stat())) for path in paths})

    def test_hash_bound_rotated_image_remains_discoverable(self):
        rotated = self.parent / "mail.sqlite3.bak.20260101_000000"
        self.backup.rename(rotated)
        self.backup.write_bytes(b"new unverified proactive snapshot")
        report = self.inspect()
        self.assertTrue(report["ok"], report)
        self.assertEqual(report["backups"]["matching_snapshot_images"], [rotated.name])

    def test_multiple_matching_copies_are_reported_not_overwritten(self):
        rotated = self.parent / "mail.sqlite3.bak.20260101_000000-01"
        rotated.write_bytes(self.backup.read_bytes())
        report = self.inspect()
        self.assertTrue(report["ok"])
        self.assertEqual(report["backups"]["matching_snapshot_images"], [self.backup.name, rotated.name])
        self.assertEqual(report["backups"]["hash_bytes_read"], 2 * self.backup.stat().st_size)

    def test_unmatched_metadata_is_a_finding(self):
        self.claim["snapshot_sha256"] = "0" * 64
        self.write_claim()
        report = self.inspect()
        self.assertFalse(report["ok"])
        self.assertIn("snapshot_bytes_unresolved", self.codes(report))

    def test_same_size_corruption_is_detected(self):
        data = bytearray(self.backup.read_bytes())
        data[-1] ^= 1
        self.backup.write_bytes(data)
        self.assertIn("snapshot_bytes_unresolved", self.codes(self.inspect()))

    def test_metadata_cannot_name_a_foreign_path(self):
        for field in ("source_path", "snapshot_path"):
            with self.subTest(field=field):
                old = self.claim[field]
                self.claim[field] = str(self.root / "other.sqlite3")
                self.write_claim()
                self.assertIn("snapshot_metadata_invalid", self.codes(self.inspect()))
                self.claim[field] = old

    def test_legacy_hashless_metadata_is_preserved_and_not_trusted(self):
        for key in ("schema", "snapshot_sha256", "snapshot_size_bytes"):
            self.claim.pop(key)
        self.claim["binary_version"] = "0.3.30"
        self.write_claim()
        before = self.meta.read_bytes()
        report = self.inspect()
        self.assertIn("snapshot_metadata_invalid", self.codes(report))
        self.assertEqual(self.meta.read_bytes(), before)
        self.assertEqual(report["backups"]["hash_bytes_read"], 0)

    def test_malformed_duplicate_and_nonobject_metadata(self):
        for value in (b"{", b"[]", b"null", b'{"schema":1,"schema":1}', b"\xff"):
            with self.subTest(value=value):
                self.meta.write_bytes(value)
                self.assertIn("snapshot_metadata_invalid", self.codes(self.inspect()))

    def test_incomplete_false_and_wrong_type_metadata(self):
        original = dict(self.claim)
        for field, value in (("schema", True), ("schema", 2), ("created_us", 0),
                             ("integrity_verified", False), ("integrity_kind", "quick_check"),
                             ("row_counts", {"messages": -1}), ("schema_version", True),
                             ("snapshot_size_bytes", True), ("binary_version", "")):
            with self.subTest(field=field, value=value):
                self.claim = {**original, field: value}
                self.write_claim()
                self.assertIn("snapshot_metadata_invalid", self.codes(self.inspect()))

    def test_missing_metadata_is_not_healthy(self):
        self.meta.rename(self.meta.with_suffix(".preserved"))
        self.assertIn("snapshot_metadata_missing", self.codes(self.inspect()))

    def test_missing_backups_are_not_healthy(self):
        self.backup.rename(self.parent / "unclassified-preserved-image")
        self.assertIn("backup_missing", self.codes(self.inspect()))

    def test_snapshot_freshness_boundaries_and_future_clock(self):
        self.claim["created_us"] = self.now - 60_000_000
        self.write_claim()
        self.assertTrue(self.inspect(max_age_seconds=60)["ok"])
        self.assertIn("snapshot_stale", self.codes(self.inspect(max_age_seconds=59)))
        self.claim["created_us"] = self.now + 1
        self.write_claim()
        self.assertIn("snapshot_time_in_future", self.codes(self.inspect()))

    def test_three_retained_stages_block_even_with_healthy_snapshot(self):
        paths = [self.stage(index) for index in range(3)]
        report = self.inspect()
        self.assertIn("staging_count_blocked", self.codes(report))
        self.assertEqual(report["staging"]["count"], 3)
        self.assertEqual(report["staging"]["bytes"], 3 * len(b"failed export"))
        self.assertTrue(all(path.exists() for path in paths))

    def test_nested_stages_and_other_mailboxes_share_one_budget(self):
        stage = self.stage("another-mailbox", b"123")
        (stage / "nested").mkdir()
        (stage / "nested/candidate").write_bytes(b"4567")
        report = self.inspect()
        self.assertTrue(report["ok"])
        self.assertEqual(report["staging"]["bytes"], 7)
        self.assertEqual(report["staging"]["scope"], "database-parent")

    def test_sparse_staging_exhausts_logical_byte_budget(self):
        stage = self.stage(0)
        with (stage / "snapshot.sqlite3").open("wb") as image:
            image.truncate(600 * 1024**2)
        self.assertIn("staging_bytes_blocked", self.codes(self.inspect()))

    def test_wal_is_included_in_next_attempt_estimate(self):
        (self.parent / "mail.sqlite3-wal").write_bytes(b"wal bytes")
        report = self.inspect()
        self.assertEqual(report["staging"]["projected_bytes"], 2 * (self.database.stat().st_size + 9))

    def test_784_backup_accumulation_is_visible(self):
        for ordinal in range(1, 785):
            (self.parent / f"mail.sqlite3.bak.20260101_000000-{ordinal:02}").write_bytes(b"old")
        report = self.inspect()
        self.assertEqual(report["backups"]["count"], 785)
        self.assertIn("retained_backup_count", self.codes(report))
        self.assertFalse(report["ok"])

    def test_rotation_grammar_does_not_count_unrelated_evidence(self):
        for suffix in ("manual", "20261301_000000", "20260101_000000-0", "20260101_000000-001",
                       "20260101_000000-wal", "20260101_000000.meta.json", "20260101_000000-4294967296"):
            (self.parent / ("mail.sqlite3.bak." + suffix)).write_bytes(b"unrelated")
        self.assertEqual(self.inspect()["backups"]["count"], 1)

    def test_current_and_historical_companion_state_prevents_standalone_claim(self):
        for companion in ("mail.sqlite3.bak-wal", "mail.sqlite3-wal.bak"):
            with self.subTest(companion=companion):
                path = self.parent / companion
                path.write_bytes(b"unsettled WAL")
                self.assertIn("snapshot_companion_state", self.codes(self.inspect()))
                path.rename(self.parent / (companion + ".preserved"))

    def test_cooperating_exporter_contention_is_unavailable(self):
        import fcntl
        with health.directory(self.parent) as descriptor:
            fcntl.flock(descriptor, fcntl.LOCK_EX | fcntl.LOCK_NB)
            with self.assertRaisesRegex(health.Unavailable, "lease"):
                self.inspect()
        self.assertTrue(self.inspect()["ok"])

    def test_symlinks_and_special_entries_never_become_empty_healthy_inventory(self):
        stage = self.stage(0)
        (stage / "external").symlink_to(self.backup)
        with self.assertRaises(health.Unavailable):
            self.inspect()
        (stage / "external").rename(self.root / "preserved-link")
        os.mkfifo(stage / "pipe")
        with self.assertRaises(health.Unavailable):
            self.inspect()

    def test_symlinked_database_parent_is_refused(self):
        alias = self.root / "alias"
        alias.symlink_to(self.parent, target_is_directory=True)
        with self.assertRaises(OSError):
            health.inspect(alias / self.database.name)

    def test_hardlinked_backup_is_unavailable(self):
        os.link(self.backup, self.root / "hardlink")
        with self.assertRaises(health.Unavailable):
            self.inspect()

    def test_oversized_metadata_is_unavailable(self):
        self.meta.write_bytes(b" " * (health.MAX_METADATA_BYTES + 1))
        with self.assertRaises(health.Unavailable):
            self.inspect()

    def test_inventory_and_hash_budgets_never_produce_partial_success(self):
        with self.assertRaisesRegex(health.Unavailable, "entry budget"):
            self.inspect(max_entries=1)
        with self.assertRaisesRegex(health.Unavailable, "hash byte budget"):
            self.inspect(max_hash_bytes=self.backup.stat().st_size - 1)
        self.assertTrue(self.inspect(max_hash_bytes=self.backup.stat().st_size)["ok"])

    def test_same_size_edit_with_restored_mtime_during_hash_is_unavailable(self):
        old = self.backup.stat()
        original_read = health.os.read
        changed = False
        def read(descriptor, count):
            nonlocal changed
            data = original_read(descriptor, count)
            if not changed and os.fstat(descriptor).st_ino == old.st_ino:
                changed = True
                with self.backup.open("r+b") as image:
                    image.seek(-1, 2)
                    image.write(b"X")
                os.utime(self.backup, ns=(old.st_atime_ns, old.st_mtime_ns))
            return data
        with patch.object(health.os, "read", side_effect=read):
            with self.assertRaisesRegex(health.Unavailable, "changed"):
                self.inspect()
        self.assertTrue(changed)

    def test_invalid_policy_values_are_unavailable(self):
        for options in ({"keep_backups": 0}, {"max_age_seconds": -1}, {"max_entries": 0},
                        {"max_hash_bytes": 0}, {"max_entries": health.MAX_ENTRIES + 1}, {"keep_backups": True}):
            with self.subTest(options=options), self.assertRaises(health.Unavailable):
                self.inspect(**options)

    def test_cli_machine_readable_exit_contract(self):
        code, report = self.cli()
        self.assertEqual(code, 0, report)
        self.stage(1)
        self.stage(2)
        self.stage(3)
        code, report = self.cli()
        self.assertEqual(code, 2, report)
        self.assertIn("staging_count_blocked", self.codes(report))
        code, report = self.cli("--max-entries", "1")
        self.assertEqual(code, 3, report)
        self.assertEqual(report["status"], "unavailable")
        self.assertFalse(report["ok"])


if __name__ == "__main__":
    unittest.main()
