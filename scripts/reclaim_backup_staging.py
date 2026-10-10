#!/usr/bin/env python3
"""Offline, move-only recovery of retained automatic-backup stages (br-kp1in.36).

Preview by default. Applying requires the preview's digest and an explicit
declaration that ALL exporters sharing the database parent have been stopped.
The parent-directory flock also refuses a concurrent cooperating exporter,
using integrity_guard_backup_budget.rs's lock, not a new lock-file convention.
Older binaries and explicit/manual exporters may not acquire that lease:
--offline is an operator prerequisite, NOT a claim that this tool stopped or
detected every writer. No server is signalled and no SQLite file is opened as a
database. No file is deleted, overwritten, or copied to another filesystem.

Linux / Python 3.10+, standard library only. Atomic no-replace directory moves
require libc renameat2 and filesystem RENAME_NOREPLACE support. Unsupported
platforms fail closed. Quarantine remains resident under STORAGE_ROOT/doctor/
reclaimable; moving stages makes them leave admission's staging namespace, not
the disk. Retry journals, verified backups, metadata, and live state are intact.

--rotated-backups selects surplus standalone <db>.bak.<YYYYMMDD_HHMMSS> files
instead of staging directories. It keeps the newest three generations by
default, every young generation, every exact size/SHA-256 metadata match, and
every generation with SQLite/FrankenSQLite or historical companion state. The
current .bak is never selected. Valid stale metadata does not park the entire
pile; malformed, unreadable, or foreign metadata refuses the operation. Backup
payloads are SHA-256 checked with a bounded read budget (128 GiB per pass by
default); these fingerprints are not new database-integrity verdicts. No native
rotation policy or verification metadata is rewritten by this offline command.

Examples:
  python3 scripts/reclaim_backup_staging.py --database /mail/storage.sqlite3 \
      --storage-root /mail
  # Stop/drain all exporters sharing /mail, then use the returned plan_sha256:
  python3 scripts/reclaim_backup_staging.py --database /mail/storage.sqlite3 \
      --storage-root /mail --apply --offline --expect-plan <plan_sha256>
  # Inspect an interrupted run; no log line alone is authority to move a stage:
  python3 scripts/reclaim_backup_staging.py --database /mail/storage.sqlite3 \
      --storage-root /mail --inspect-run <run_directory>
  # --resume finishes quarantine; --restore returns the same inodes to source.
  # Both require --offline and --expect-plan <the_original_plan_sha256>.
  # Preview obsolete rotated backups, leaving at least the three newest:
  python3 scripts/reclaim_backup_staging.py --database /mail/storage.sqlite3 \
      --storage-root /mail --rotated-backups --keep-backups 3
  # Apply with the SAME scope/policy and preview digest, after stopping exporters.
  # The existing inspect/resume/restore verbs discover the scope from the manifest.

Fixtures are exercised with:
  python3 -m unittest discover -s tests -p test_reclaim_backup_staging.py -v
"""

from __future__ import annotations

import argparse
from contextlib import ExitStack, contextmanager
import ctypes
from datetime import datetime
import errno
import hashlib
import json
import os
from pathlib import Path
import re
import stat
import sys
import time
from typing import Any, Iterator
import uuid

STAGE_PREFIX = ".mcp-agent-mail-proactive-backup-"
RUN_PREFIX = "proactive-staging-"
MAX_ENTRIES = 16_384
MAX_DEPTH = 64
DEFAULT_MIN_AGE_SECONDS = 3600
MAX_BYTES = (1 << 64) - 1
MAX_MANIFEST_BYTES = 8 * 1024 * 1024
SCHEMA = 1
BACKUP_SCHEMA = 2
BACKUP_RUN_PREFIX = "rotated-backups-"
DEFAULT_KEEP_BACKUPS = 3
DEFAULT_MAX_HASH_BYTES = 128 * 1024**3
MAX_SNAPSHOT_METADATA_BYTES = 64 * 1024
SQLITE_COMPANIONS = ("-wal", "-shm", "-journal", "-wal-cert", "-wal-cert-head",
                     "-fsqlite-ns-gate", "-fsqlite-ns-use", ".lock", ".meta.json")


class Refused(RuntimeError):
    """Unavailable or unsafe evidence: nothing authorizes a best-effort move."""


def require_platform() -> None:
    if not sys.platform.startswith("linux"):
        raise Refused("this recovery command requires Linux; no fallback move is permitted")


def canonical_json(value: Any) -> bytes:
    return json.dumps(value, sort_keys=True, ensure_ascii=True, separators=(",", ":")).encode()


def digest(value: Any) -> str:
    return hashlib.sha256(canonical_json(value)).hexdigest()


def absolute_path(value: str | Path) -> Path:
    path = Path(value)
    if ".." in path.parts:
        raise Refused("parent traversal is not permitted")
    # Do not resolve away symlinks; open_directory checks every component.
    return path if path.is_absolute() else Path.cwd() / path


def directory_flags() -> int:
    require_platform()
    return os.O_RDONLY | os.O_DIRECTORY | os.O_NOFOLLOW | os.O_NONBLOCK | os.O_CLOEXEC


def object_id(metadata: os.stat_result) -> list[int]:
    return [metadata.st_dev, metadata.st_ino]


def stamp(metadata: os.stat_result) -> list[int]:
    # A directory's ctime changes on rename, while its contents do not. This is
    # a metadata witness, not a cryptographic assertion about the payload bytes.
    return [metadata.st_dev, metadata.st_ino, metadata.st_mode, metadata.st_nlink,
            metadata.st_size, metadata.st_mtime_ns]


@contextmanager
def open_directory(path: Path) -> Iterator[int]:
    path = absolute_path(path)
    fd = os.open("/", directory_flags())
    try:
        for part in path.parts[1:]:
            child = os.open(part, directory_flags(), dir_fd=fd)
            os.close(fd)
            fd = child
        yield fd
    finally:
        os.close(fd)


@contextmanager
def child_directory(parent: int, name: str, *, create: bool = False) -> Iterator[int]:
    if not name or name in (".", "..") or "/" in name or "\0" in name:
        raise Refused("directory name must be one ordinary component")
    if create:
        try:
            os.mkdir(name, 0o700, dir_fd=parent)
            os.fsync(parent)
        except FileExistsError:
            pass
    fd = os.open(name, directory_flags(), dir_fd=parent)
    try:
        yield fd
    finally:
        os.close(fd)


class Budget:
    def __init__(self, entries: int = MAX_ENTRIES, hash_bytes: int = DEFAULT_MAX_HASH_BYTES) -> None:
        self.remaining = entries
        self.hash_remaining = hash_bytes

    def visit(self) -> None:
        if self.remaining <= 0:
            raise Refused("inventory entry limit exceeded; no partial inventory is authoritative")
        self.remaining -= 1

    def hash(self, size: int) -> None:
        if size < 0 or size > self.hash_remaining:
            raise Refused("backup hash byte budget exceeded; no incomplete fingerprint is authoritative")
        self.hash_remaining -= size


def backup_generation(database_name: str, name: str) -> tuple[str, int] | None:
    """The native disk.rs TimestampedBak grammar, not a broad .bak prefix.

    Compare the basename byte-exactly (surrogateescape preserves Unix bytes).
    Private stages, metadata, sidecars and historical .backup-* families are
    deliberately outside this operator's standalone-backup scope.
    """
    prefix = database_name + ".bak."
    if not isinstance(name, str) or not name.startswith(prefix):
        return None
    match = re.fullmatch(r"([0-9]{8}_[0-9]{6})(?:-([0-9]{2,10}))?", name[len(prefix):])
    if match is None:
        return None
    timestamp, collision = match.groups()
    try:
        parsed = datetime.strptime(timestamp, "%Y%m%d_%H%M%S")
        if parsed.strftime("%Y%m%d_%H%M%S") != timestamp:
            return None
        sequence = int(collision) if collision else 0
        if collision and (not 0 < sequence <= 2**32 - 1 or f"{sequence:02}" != collision):
            return None
    except ValueError:
        return None
    return timestamp, sequence


@contextmanager
def regular_file(parent: int, name: str) -> Iterator[int]:
    if not name or name in (".", "..") or "/" in name or "\0" in name:
        raise Refused("file name must be one ordinary component")
    flags = os.O_RDONLY | os.O_NOFOLLOW | os.O_NONBLOCK | os.O_CLOEXEC
    fd = os.open(name, flags, dir_fd=parent)
    try:
        metadata = os.fstat(fd)
        if not stat.S_ISREG(metadata.st_mode) or metadata.st_nlink != 1:
            raise Refused("backup evidence must be a regular, singly linked file")
        yield fd
    finally:
        os.close(fd)


def file_snapshot(fd: int, budget: Budget, *, sync: bool = False) -> dict[str, Any]:
    """Hash the retained file, including same-size edits with restored mtimes.

    This reads bytes, never opens a database connection or grants an integrity
    verdict. Each pass has an explicit byte budget and constant-size buffers.
    """
    before = os.fstat(fd)
    if not stat.S_ISREG(before.st_mode) or before.st_nlink != 1:
        raise Refused("backup evidence is not a singly linked regular file")
    budget.hash(before.st_size)
    os.lseek(fd, 0, os.SEEK_SET)
    remaining = before.st_size
    hasher = hashlib.sha256()
    while remaining:
        chunk = os.read(fd, min(1024 * 1024, remaining))
        if not chunk:
            raise Refused("backup shrank while its fingerprint was read")
        hasher.update(chunk)
        remaining -= len(chunk)
    if os.read(fd, 1):
        raise Refused("backup grew while its fingerprint was read")
    if sync:
        os.fsync(fd)
    after = os.fstat(fd)
    if stamp(before) != stamp(after) or before.st_ctime_ns != after.st_ctime_ns:
        raise Refused("backup changed during fingerprint verification")
    checksum = hasher.hexdigest()
    return {"identity": object_id(before), "entries": 1, "bytes": before.st_size,
            "newest_mtime_ns": before.st_mtime_ns, "file_stamp": stamp(before),
            "content_sha256": checksum, "tree_sha256": digest([stamp(before), checksum])}


def sha256_text(value: Any) -> bool:
    return (isinstance(value, str) and len(value) == 64
            and all(char in "0123456789abcdef" for char in value))


def read_snapshot_claim(mailbox: Mailbox) -> dict[str, Any] | None:
    """Resolve a bounded metadata claim without rewriting stale authority.

    A valid but unmatched old hash is distinct from unreadable/malformed
    authority. Only the former can permit surplus, offline, move-only rotation.
    The current .bak and the metadata file themselves are never candidates.
    """
    name = mailbox.database.name + ".bak.meta.json"
    try:
        os.stat(name, dir_fd=mailbox.parent, follow_symlinks=False)
    except FileNotFoundError:
        return None
    with regular_file(mailbox.parent, name) as fd:
        before = os.fstat(fd)
        if before.st_size > MAX_SNAPSHOT_METADATA_BYTES:
            raise Refused("snapshot metadata exceeds its byte limit")
        data = bytearray()
        while len(data) <= MAX_SNAPSHOT_METADATA_BYTES:
            chunk = os.read(fd, min(65536, MAX_SNAPSHOT_METADATA_BYTES + 1 - len(data)))
            if not chunk:
                break
            data.extend(chunk)
        if len(data) > MAX_SNAPSHOT_METADATA_BYTES:
            raise Refused("snapshot metadata grew beyond its byte limit")
        named = os.stat(name, dir_fd=mailbox.parent, follow_symlinks=False)
        if (stamp(os.fstat(fd)) != stamp(before) or stamp(named) != stamp(before)
                or os.fstat(fd).st_ctime_ns != before.st_ctime_ns):
            raise Refused("snapshot metadata changed while being read")
    try:
        meta = json.loads(data, object_pairs_hook=unique_json_object)
        if (type(meta) is not dict or type(meta["schema"]) is not int or meta["schema"] != 1
                or meta["integrity_verified"] is not True
                or meta["integrity_kind"] != "integrity_check"
                or meta["source_path"] != str(mailbox.database)
                or meta["snapshot_path"] != str(mailbox.database) + ".bak"
                or type(meta["snapshot_size_bytes"]) is not int
                or not 0 < meta["snapshot_size_bytes"] <= MAX_BYTES
                or not sha256_text(meta["snapshot_sha256"])):
            raise Refused("snapshot metadata is not a supported, hash-bound claim for this mailbox")
    except (KeyError, TypeError, ValueError, UnicodeError, RecursionError) as error:
        raise Refused(f"snapshot metadata is unavailable or invalid: {error}") from error
    return {"metadata_stamp": stamp(before), "metadata_sha256": hashlib.sha256(data).hexdigest(),
            "snapshot_size_bytes": meta["snapshot_size_bytes"],
            "snapshot_sha256": meta["snapshot_sha256"]}


def matches_snapshot_claim(item: dict[str, Any], claim: dict[str, Any] | None) -> bool:
    return (claim is not None and item["bytes"] == claim["snapshot_size_bytes"]
            and item["content_sha256"] == claim["snapshot_sha256"])


def backup_companions(mailbox: Mailbox, name: str, *, run: int | None = None) -> list[str]:
    suffix = name[len(mailbox.database.name):]
    names = {name + companion for companion in SQLITE_COMPANIONS}
    # Older copy routines put the companion before .bak.<generation> instead.
    names.update(mailbox.database.name + companion + suffix for companion in SQLITE_COMPANIONS)
    found = []
    for parent in (mailbox.parent,) if run is None else (mailbox.parent, run):
        for candidate in sorted(names):
            try:
                os.stat(candidate, dir_fd=parent, follow_symlinks=False)
            except FileNotFoundError:
                continue
            found.append(candidate)
    return sorted(set(found))


def check_backup_protection(mailbox: Mailbox, plan: dict[str, Any], *, run: int | None = None,
                            selected: dict[str, Any] | None = None) -> None:
    if plan["schema"] != BACKUP_SCHEMA:
        return
    if read_snapshot_claim(mailbox) != plan["snapshot_claim"]:
        raise Refused("snapshot metadata changed since preview; existing authority was not overwritten")
    # Retained backups remain available throughout the offline operation. A
    # writer that ignores the cooperative lock must not remove the keep set
    # while this process continues moving the rest.
    for item in plan["retained"]:
        named = os.stat(item["name"], dir_fd=mailbox.parent, follow_symlinks=False)
        if stamp(named) != item["file_stamp"]:
            raise Refused("retained backup generation changed since preview")
    for item in plan["eligible"] if selected is None else [selected]:
        if matches_snapshot_claim(item, plan["snapshot_claim"]):
            raise Refused("metadata-bound backup must never be selected for quarantine")
        if backup_companions(mailbox, item["name"], run=run):
            raise Refused("selected backup has companion state; family settlement is required")


def stage_name(name: str) -> bool:
    return (isinstance(name, str) and name.startswith(STAGE_PREFIX)
            and len(name) > len(STAGE_PREFIX) and "/" not in name and "\0" not in name)


def tree_snapshot(fd: int, budget: Budget, *, sync: bool = False) -> dict[str, Any]:
    entries: list[Any] = []
    logical_bytes = 0
    newest_ns = 0

    def walk(directory: int, prefix: str, depth: int) -> None:
        nonlocal logical_bytes, newest_ns
        if depth > MAX_DEPTH:
            raise Refused("staging tree depth limit exceeded")
        before = os.fstat(directory)
        entries.append([prefix, stamp(before)])
        newest_ns = max(newest_ns, before.st_mtime_ns)
        with os.scandir(directory) as children:
            for entry in children:
                budget.visit()
                metadata = os.stat(entry.name, dir_fd=directory, follow_symlinks=False)
                relative = f"{prefix}/{entry.name}"
                if stat.S_ISDIR(metadata.st_mode):
                    with child_directory(directory, entry.name) as child:
                        if stamp(os.fstat(child)) != stamp(metadata):
                            raise Refused("staging directory changed during inventory")
                        walk(child, relative, depth + 1)
                elif stat.S_ISREG(metadata.st_mode) and metadata.st_nlink == 1:
                    if sync:
                        flags = os.O_RDONLY | os.O_NOFOLLOW | os.O_NONBLOCK | os.O_CLOEXEC
                        file = os.open(entry.name, flags, dir_fd=directory)
                        try:
                            if stamp(os.fstat(file)) != stamp(metadata):
                                raise Refused("staging file changed before sync")
                            os.fsync(file)
                            if stamp(os.fstat(file)) != stamp(metadata):
                                raise Refused("staging file changed during sync")
                        finally:
                            os.close(file)
                    entries.append([relative, stamp(metadata)])
                    logical_bytes += metadata.st_size
                    if logical_bytes > MAX_BYTES:
                        raise Refused("staging byte count exceeds the supported range")
                    newest_ns = max(newest_ns, metadata.st_mtime_ns)
                else:
                    raise Refused(f"staging contains a symlink, special file, or hard link: {relative!r}")
        if sync:
            os.fsync(directory)
        if stamp(os.fstat(directory)) != stamp(before):
            raise Refused("staging directory changed while its contents were inspected")

    walk(fd, "", 0)
    entries.sort(key=lambda item: item[0])
    return {"identity": object_id(os.fstat(fd)), "entries": len(entries),
            "bytes": logical_bytes, "newest_mtime_ns": newest_ns,
            "tree_sha256": digest(entries)}


class Mailbox:
    """Retain the exact source parent, archive root, and export lease throughout."""

    def __init__(self, database: str | Path, storage_root: str | Path) -> None:
        self.database = absolute_path(database)
        self.storage_root = absolute_path(storage_root)
        self.stack = ExitStack()

    def __enter__(self) -> Mailbox:
        require_platform()
        import fcntl
        try:
            self.parent = self.stack.enter_context(open_directory(self.database.parent))
            try:
                fcntl.flock(self.parent, fcntl.LOCK_EX | fcntl.LOCK_NB)
            except BlockingIOError as error:
                raise Refused("another exporter or recovery command holds the database-parent lease") from error
            self.archive = self.stack.enter_context(open_directory(self.storage_root))
            metadata = os.stat(self.database.name, dir_fd=self.parent, follow_symlinks=False)
            if not stat.S_ISREG(metadata.st_mode) or metadata.st_nlink != 1:
                raise Refused("primary must be a regular, singly linked file")
            self.binding = {"database": str(self.database), "primary_identity": object_id(metadata),
                            "parent_identity": object_id(os.fstat(self.parent)),
                            "storage_root": str(self.storage_root),
                            "archive_identity": object_id(os.fstat(self.archive))}
            self.validate()
            return self
        except BaseException:
            self.stack.close()
            raise

    def __exit__(self, *args: Any) -> None:
        self.stack.close()

    def validate(self) -> None:
        for path, expected in [(self.database.parent, self.binding["parent_identity"]),
                               (self.storage_root, self.binding["archive_identity"])]:
            with open_directory(path) as current:
                if object_id(os.fstat(current)) != expected:
                    raise Refused("source or archive namespace changed; retained descriptors cannot authorize its replacement")
        primary = os.stat(self.database.name, dir_fd=self.parent, follow_symlinks=False)
        if (not stat.S_ISREG(primary.st_mode) or primary.st_nlink != 1
                or object_id(primary) != self.binding["primary_identity"]):
            raise Refused("primary database identity changed")

    def plan(self, min_age_seconds: int, *, now_ns: int | None = None,
             max_entries: int = MAX_ENTRIES) -> dict[str, Any]:
        if min_age_seconds < 0:
            raise Refused("minimum age must be nonnegative")
        now_ns = time.time_ns() if now_ns is None else now_ns
        budget = Budget(max_entries)
        eligible: list[Any] = []
        retained: list[Any] = []
        self.validate()
        with os.scandir(self.parent) as children:
            for entry in children:
                budget.visit()
                if not entry.name.startswith(STAGE_PREFIX):
                    continue
                if not stage_name(entry.name):
                    raise Refused("staging prefix has no artifact name")
                if self.storage_root.is_relative_to(self.database.parent / entry.name):
                    raise Refused("archive root is inside a staging candidate")
                with child_directory(self.parent, entry.name) as stage:
                    witness = tree_snapshot(stage, budget)
                item = {"name": entry.name, **witness}
                if now_ns - witness["newest_mtime_ns"] >= min_age_seconds * 1_000_000_000:
                    eligible.append(item)
                else:
                    retained.append(item)
        self.validate()
        if sum(item["bytes"] for item in eligible + retained) > MAX_BYTES:
            raise Refused("total staging bytes exceed the supported range")
        plan = {"schema": SCHEMA, "binding": self.binding, "min_age_seconds": min_age_seconds,
                "eligible": sorted(eligible, key=lambda item: item["name"]),
                "retained": sorted(retained, key=lambda item: item["name"])}
        return {**plan, "plan_sha256": digest(plan)}

    def backup_plan(self, min_age_seconds: int, *, keep_backups: int = DEFAULT_KEEP_BACKUPS,
                    max_hash_bytes: int = DEFAULT_MAX_HASH_BYTES,
                    now_ns: int | None = None, max_entries: int = MAX_ENTRIES) -> dict[str, Any]:
        if type(keep_backups) is not int or not 1 <= keep_backups <= MAX_ENTRIES:
            raise Refused("keep-backups must be between 1 and the inventory entry limit")
        if type(max_hash_bytes) is not int or not 0 < max_hash_bytes <= MAX_BYTES:
            raise Refused("max-hash-bytes must be a positive bounded byte count")
        if type(min_age_seconds) is not int or not 0 <= min_age_seconds <= MAX_BYTES:
            raise Refused("minimum age must be a nonnegative bounded integer")
        self.validate()
        claim = read_snapshot_claim(self)
        budget = Budget(max_entries, max_hash_bytes)
        candidates = []
        with os.scandir(self.parent) as children:
            for entry in children:
                budget.visit()
                generation = backup_generation(self.database.name, entry.name)
                if generation is None:
                    continue
                with regular_file(self.parent, entry.name) as file:
                    witness = file_snapshot(file, budget)
                    named = os.stat(entry.name, dir_fd=self.parent, follow_symlinks=False)
                    if stamp(named) != witness["file_stamp"]:
                        raise Refused("backup pathname changed during fingerprinting")
                candidates.append({"name": entry.name, "artifact_kind": "rotated_backup",
                                   "retention_reasons": [], **witness})
        candidates.sort(key=lambda item: backup_generation(self.database.name, item["name"]), reverse=True)
        now_ns = time.time_ns() if now_ns is None else now_ns
        eligible, retained = [], []
        for rank, item in enumerate(candidates):
            reasons = item["retention_reasons"]
            if rank < keep_backups:
                reasons.append("newest_keep")
            if now_ns - item["newest_mtime_ns"] < min_age_seconds * 1_000_000_000:
                reasons.append("young")
            if matches_snapshot_claim(item, claim):
                reasons.append("snapshot_metadata_match")
            if backup_companions(self, item["name"]):
                reasons.append("companion_state")
            (retained if reasons else eligible).append(item)
        plan = {"schema": BACKUP_SCHEMA, "binding": self.binding,
                "min_age_seconds": min_age_seconds, "scope": "rotated_backups",
                "keep_backups": keep_backups, "max_hash_bytes": max_hash_bytes,
                "snapshot_claim": claim,
                "eligible": sorted(eligible, key=lambda item: item["name"]),
                "retained": sorted(retained, key=lambda item: item["name"])}
        self.validate()
        check_backup_protection(self, plan)
        return {**plan, "plan_sha256": digest(plan)}


def rename_noreplace(source_parent: int, source: str, target_parent: int, target: str) -> None:
    require_platform()
    libc = ctypes.CDLL(None, use_errno=True)
    rename = getattr(libc, "renameat2", None)
    if rename is None:
        raise Refused("libc renameat2 is unavailable; no unsafe fallback is allowed")
    rename.argtypes = [ctypes.c_int, ctypes.c_char_p, ctypes.c_int, ctypes.c_char_p, ctypes.c_uint]
    rename.restype = ctypes.c_int
    if rename(source_parent, os.fsencode(source), target_parent, os.fsencode(target), 1) != 0:
        code = ctypes.get_errno()
        raise OSError(code, os.strerror(code), source)


def write_record(fd: int, value: Any) -> None:
    pending = memoryview(canonical_json(value) + b"\n")
    while pending:
        written = os.write(fd, pending)
        if written <= 0:
            raise OSError(errno.EIO, "short recovery-record write")
        pending = pending[written:]
    os.fsync(fd)


def new_record(parent: int, name: str, value: Any) -> None:
    flags = os.O_WRONLY | os.O_CREAT | os.O_EXCL | os.O_NOFOLLOW | os.O_CLOEXEC
    fd = os.open(name, flags, 0o600, dir_fd=parent)
    try:
        write_record(fd, value)
    finally:
        os.close(fd)
    os.fsync(parent)


def verify_stage(fd: int, expected: dict[str, Any], budget: Budget, *, sync: bool = False) -> None:
    actual = (file_snapshot(fd, budget, sync=sync) if expected.get("artifact_kind") == "rotated_backup"
              else tree_snapshot(fd, budget, sync=sync))
    if any(actual[key] != expected[key] for key in actual):
        raise Refused(f"staging evidence changed since preview: {expected['name']!r}")


@contextmanager
def open_artifact(parent: int, item: dict[str, Any]) -> Iterator[int]:
    opener = regular_file if item.get("artifact_kind") == "rotated_backup" else child_directory
    with opener(parent, item["name"]) as file:
        yield file


def validate_run_path(run_path: Path, run: int) -> None:
    with open_directory(run_path) as current:
        if object_id(os.fstat(current)) != object_id(os.fstat(run)):
            raise Refused("quarantine namespace changed")


def stage_location(mailbox: Mailbox, run: int, stage: dict[str, Any]) -> str:
    locations = []
    for label, parent in (("source", mailbox.parent), ("quarantine", run)):
        try:
            metadata = os.stat(stage["name"], dir_fd=parent, follow_symlinks=False)
        except FileNotFoundError:
            continue
        proper_type = (stat.S_ISREG(metadata.st_mode) and metadata.st_nlink == 1
                       if stage.get("artifact_kind") == "rotated_backup" else stat.S_ISDIR(metadata.st_mode))
        if not proper_type or object_id(metadata) != stage["identity"]:
            raise Refused(f"{label} entry is occupied by different evidence: {stage['name']!r}")
        locations.append(label)
    if len(locations) != 1:
        raise Refused(f"stage must exist in exactly one recorded location: {stage['name']!r}")
    return locations[0]


def move_plan(mailbox: Mailbox, run: int, run_path: Path, plan: dict[str, Any],
              actions: int, result: dict[str, Any], *, restore: bool = False,
              check_manifest: Any = None) -> None:
    """Both directions share the same source witness and atomic publication path."""
    desired = "source" if restore else "quarantine"
    budget = Budget(hash_bytes=plan.get("max_hash_bytes", DEFAULT_MAX_HASH_BYTES))
    for stage in plan["eligible"]:
        renamed = False
        try:
            mailbox.validate()
            validate_run_path(run_path, run)
            check_backup_protection(mailbox, plan, run=run, selected=stage)
            if check_manifest is not None:
                check_manifest()
            location = stage_location(mailbox, run, stage)
            source_parent = mailbox.parent if location == "source" else run
            target_parent = mailbox.parent if desired == "source" else run
            name = stage["name"]
            with open_artifact(source_parent, stage) as source:
                verify_stage(source, stage, budget, sync=True)
                if location != desired:
                    write_record(actions, {"phase": "intent", "name": name, "mode": result["mode"]})
                    if object_id(os.stat(name, dir_fd=source_parent, follow_symlinks=False)) != stage["identity"]:
                        raise Refused("source entry changed immediately before rename")
                    check_backup_protection(mailbox, plan, run=run, selected=stage)
                    rename_noreplace(source_parent, name, target_parent, name)
                    renamed = True
                # A crash may have happened after rename but before the old
                # completion receipt. Re-sync both parents even for an already
                # present stage; never infer durability from the receipt alone.
                os.fsync(mailbox.parent)
                os.fsync(run)
                mailbox.validate()
                validate_run_path(run_path, run)
                check_backup_protection(mailbox, plan, run=run, selected=stage)
                if check_manifest is not None:
                    check_manifest()
                if stage_location(mailbox, run, stage) != desired:
                    raise Refused("stage location changed after publication")
                if object_id(os.stat(name, dir_fd=target_parent, follow_symlinks=False)) != object_id(os.fstat(source)):
                    raise Refused("destination entry changed after rename")
                phase = "completed" if renamed else "already_present"
                write_record(actions, {"phase": phase, "name": name,
                                       "bytes": stage["bytes"], "mode": result["mode"]})
            bucket = "moved" if renamed else "unchanged"
            result.setdefault(bucket, []).append({"name": name, "bytes": stage["bytes"]})
        except (OSError, Refused) as error:
            result["ok"] = False
            result["failures"].append({"name": stage["name"], "rename_completed": renamed,
                                       "error": str(error),
                                       "instruction": "preserve the run directory; inspect it before --resume or --restore"})
            break


def finish_receipt(run: int, name: str, result: dict[str, Any]) -> dict[str, Any]:
    try:
        new_record(run, name, result)
    except OSError as error:
        result["ok"] = False
        result["failures"].append({"error": f"final receipt unavailable: {error}"})
    return result


def apply_plan(mailbox: Mailbox, expected_digest: str, min_age_seconds: int, *,
               offline: bool = False, rotated_backups: bool = False,
               keep_backups: int = DEFAULT_KEEP_BACKUPS,
               max_hash_bytes: int = DEFAULT_MAX_HASH_BYTES) -> dict[str, Any]:
    if not offline:
        raise Refused("--offline is required: first stop ALL exporters sharing the database parent")
    plan = (mailbox.backup_plan(min_age_seconds, keep_backups=keep_backups, max_hash_bytes=max_hash_bytes)
            if rotated_backups else mailbox.plan(min_age_seconds))
    if not expected_digest or plan["plan_sha256"] != expected_digest:
        raise Refused("plan changed or digest is missing; obtain a fresh preview before applying")
    result: dict[str, Any] = {"schema": plan["schema"], "mode": "apply", "ok": True,
                              "plan_sha256": expected_digest, "moved": [], "failures": [],
                              "freed_bytes": 0, "backup_retry_state_unchanged": True}
    if rotated_backups:
        result.update(scope="rotated_backups", retained_backups=len(plan["retained"]),
                      current_backup_preserved=True, snapshot_metadata_unchanged=True)
    if not plan["eligible"]:
        return result
    operation = "quarantine_rotated_backups" if rotated_backups else "quarantine_proactive_backup_stages"
    manifest = {"schema": plan["schema"], "operation": operation,
                "created_at_ns": time.time_ns(), "plan": plan}
    # Never publish a run that the bounded recovery reader cannot reopen.
    if len(canonical_json(manifest)) + 1 > MAX_MANIFEST_BYTES:
        raise Refused("recovery manifest would exceed its byte limit; no quarantine was created")
    validate_manifest(manifest, mailbox)
    # A unique private destination is claimed only after all preview checks.
    with ExitStack() as stack:
        doctor = stack.enter_context(child_directory(mailbox.archive, "doctor", create=True))
        reclaim = stack.enter_context(child_directory(doctor, "reclaimable", create=True))
        run_name = (BACKUP_RUN_PREFIX if rotated_backups else RUN_PREFIX) + uuid.uuid4().hex
        os.mkdir(run_name, 0o700, dir_fd=reclaim)
        os.fsync(reclaim)
        run = stack.enter_context(child_directory(reclaim, run_name))
        run_path = mailbox.storage_root / "doctor" / "reclaimable" / run_name
        result["run_directory"] = str(run_path)
        new_record(run, "manifest.json", manifest)
        flags = os.O_WRONLY | os.O_CREAT | os.O_EXCL | os.O_NOFOLLOW | os.O_CLOEXEC
        actions = os.open("actions.jsonl", flags, 0o600, dir_fd=run)
        stack.callback(os.close, actions)
        os.fsync(run)
        move_plan(mailbox, run, run_path, plan, actions, result)
        # This final receipt is supplemental. The synced pre-move manifest and
        # per-move intents survive even if publishing it is interrupted.
        return finish_receipt(run, "result.json", result)


def validate_manifest(manifest: Any, mailbox: Mailbox) -> dict[str, Any]:
    try:
        if (type(manifest) is not dict or type(manifest["schema"]) is not int
                or manifest["schema"] not in (SCHEMA, BACKUP_SCHEMA)):
            raise Refused("unsupported recovery manifest")
        backups = manifest["schema"] == BACKUP_SCHEMA
        operation = "quarantine_rotated_backups" if backups else "quarantine_proactive_backup_stages"
        if manifest["operation"] != operation:
            raise Refused("unsupported recovery manifest operation")
        plan = manifest["plan"]
        keys = {"schema", "binding", "min_age_seconds", "eligible", "retained", "plan_sha256"}
        if backups:
            keys |= {"scope", "keep_backups", "max_hash_bytes", "snapshot_claim"}
        if type(plan) is not dict or set(plan) != keys:
            raise Refused("invalid recovery plan shape")
        if (type(plan["schema"]) is not int or plan["schema"] != manifest["schema"]
                or plan["binding"] != mailbox.binding):
            raise Refused("manifest belongs to another mailbox, archive, or file generation")
        unsigned = lambda value: type(value) is int and 0 <= value <= MAX_BYTES
        if not unsigned(plan["min_age_seconds"]):
            raise Refused("invalid manifest age")
        if backups and (plan["scope"] != "rotated_backups"
                        or not unsigned(plan["keep_backups"]) or not 1 <= plan["keep_backups"] <= MAX_ENTRIES
                        or not unsigned(plan["max_hash_bytes"]) or plan["max_hash_bytes"] == 0):
            raise Refused("invalid backup retention policy")
        if any(type(plan[key]) is not list for key in ("eligible", "retained")):
            raise Refused("invalid manifest stage lists")
        stages = plan["eligible"] + plan["retained"]
        if len(stages) > MAX_ENTRIES:
            raise Refused("manifest stage limit exceeded")
        names: set[str] = set()
        total_entries = 0
        total_bytes = 0
        for stage in stages:
            item_keys = {"name", "identity", "entries", "bytes", "newest_mtime_ns", "tree_sha256"}
            if backups:
                item_keys |= {"artifact_kind", "retention_reasons", "file_stamp", "content_sha256"}
            if (type(stage) is not dict
                    or set(stage) != item_keys or not isinstance(stage["name"], str)
                    or (backup_generation(mailbox.database.name, stage["name"]) is None if backups
                        else not stage_name(stage["name"])) or stage["name"] in names):
                raise Refused("invalid or duplicate manifest stage")
            names.add(stage["name"])
            identity = stage["identity"]
            if type(identity) is not list or len(identity) != 2 or not all(unsigned(part) for part in identity):
                raise Refused("invalid stage identity")
            if (not all(unsigned(stage[key]) for key in ("entries", "bytes", "newest_mtime_ns"))
                    or stage["entries"] == 0):
                raise Refused("invalid stage witness bounds")
            if (not isinstance(stage["tree_sha256"], str) or len(stage["tree_sha256"]) != 64
                    or any(char not in "0123456789abcdef" for char in stage["tree_sha256"])):
                raise Refused("invalid stage witness digest")
            total_entries += stage["entries"]
            total_bytes += stage["bytes"]
            if total_entries > MAX_ENTRIES or total_bytes > MAX_BYTES:
                raise Refused("manifest inventory bounds exceeded")
            if backups:
                file_stamp = stage["file_stamp"]
                if (stage["artifact_kind"] != "rotated_backup" or stage["entries"] != 1
                        or type(file_stamp) is not list or len(file_stamp) != 6
                        or not all(unsigned(part) for part in file_stamp)
                        or not stat.S_ISREG(file_stamp[2]) or file_stamp[3] != 1
                        or file_stamp[:2] != stage["identity"] or file_stamp[4] != stage["bytes"]
                        or file_stamp[5] != stage["newest_mtime_ns"]
                        or not sha256_text(stage["content_sha256"])
                        or digest([file_stamp, stage["content_sha256"]]) != stage["tree_sha256"]
                        or total_bytes > plan["max_hash_bytes"]):
                    raise Refused("invalid backup fingerprint")
                reasons = stage["retention_reasons"]
                allowed = {"newest_keep", "young", "snapshot_metadata_match", "companion_state"}
                if (type(reasons) is not list or any(not isinstance(reason, str) or reason not in allowed for reason in reasons)
                        or len(set(reasons)) != len(reasons)):
                    raise Refused("invalid backup retention reasons")
            if mailbox.storage_root.is_relative_to(mailbox.database.parent / stage["name"]):
                raise Refused("archive root is inside a recorded stage")
        unsigned_plan = {key: value for key, value in plan.items() if key != "plan_sha256"}
        if digest(unsigned_plan) != plan["plan_sha256"]:
            raise Refused("recovery manifest checksum mismatch")
        if backups:
            if (type(manifest["created_at_ns"]) is not int or manifest["created_at_ns"] < 0
                    or read_snapshot_claim(mailbox) != plan["snapshot_claim"]):
                raise Refused("snapshot metadata changed or the manifest timestamp is invalid")
            newest = sorted(stages, key=lambda item: backup_generation(mailbox.database.name, item["name"]), reverse=True)
            must_keep = {item["name"] for item in newest[:plan["keep_backups"]]}
            for item in plan["eligible"]:
                if (item["name"] in must_keep or item["retention_reasons"]
                        or matches_snapshot_claim(item, plan["snapshot_claim"])
                        or manifest["created_at_ns"] - item["newest_mtime_ns"] < plan["min_age_seconds"] * 1_000_000_000):
                    raise Refused("backup plan selects a protected generation")
        return plan
    except (KeyError, TypeError, ValueError, RecursionError) as error:
        raise Refused(f"invalid recovery manifest: {error}") from error


def unique_json_object(pairs: list[Any]) -> dict[str, Any]:
    result = {}
    for key, value in pairs:
        if key in result:
            raise Refused("duplicate key in recovery manifest")
        result[key] = value
    return result


@contextmanager
def recorded_run(mailbox: Mailbox, requested_path: str | Path) -> Iterator[Any]:
    run_path = absolute_path(requested_path)
    if (run_path.parent != mailbox.storage_root / "doctor/reclaimable"
            or not run_path.name.startswith((RUN_PREFIX, BACKUP_RUN_PREFIX))):
        raise Refused("run must be a recorded backup quarantine under this archive")
    with open_directory(run_path) as run:
        if os.fstat(run).st_mode & 0o077 or os.fstat(run).st_uid != os.geteuid():
            raise Refused("recovery run must be private and owned by this operator")
        flags = os.O_RDONLY | os.O_NOFOLLOW | os.O_NONBLOCK | os.O_CLOEXEC
        file = os.open("manifest.json", flags, dir_fd=run)
        try:
            before = os.fstat(file)
            if (not stat.S_ISREG(before.st_mode) or before.st_nlink != 1
                    or before.st_mode & 0o077 or before.st_uid != os.geteuid()
                    or before.st_size > MAX_MANIFEST_BYTES):
                raise Refused("manifest is not a bounded, private, singly linked regular file")
            data = bytearray()
            while len(data) <= MAX_MANIFEST_BYTES:
                chunk = os.read(file, min(65536, MAX_MANIFEST_BYTES + 1 - len(data)))
                if not chunk:
                    break
                data.extend(chunk)
            if len(data) > MAX_MANIFEST_BYTES:
                raise Refused("manifest byte limit exceeded")

            def check_manifest():
                mailbox.validate()
                validate_run_path(run_path, run)
                named = os.stat("manifest.json", dir_fd=run, follow_symlinks=False)
                if stamp(os.fstat(file)) != stamp(before) or stamp(named) != stamp(before):
                    raise Refused("manifest identity or contents changed while the run was open")

            check_manifest()
            try:
                manifest = json.loads(data, object_pairs_hook=unique_json_object)
            except (ValueError, UnicodeError, RecursionError) as error:
                raise Refused(f"manifest cannot be decoded: {error}") from error
            plan = validate_manifest(manifest, mailbox)
            yield run, run_path, plan, check_manifest
        finally:
            os.close(file)


def inspect_recorded_stages(mailbox: Mailbox, run: int, plan: dict[str, Any]) -> list[Any]:
    locations = []
    budget = Budget(hash_bytes=plan.get("max_hash_bytes", DEFAULT_MAX_HASH_BYTES))
    check_backup_protection(mailbox, plan, run=run)
    for stage in plan["eligible"]:
        location = stage_location(mailbox, run, stage)
        parent = mailbox.parent if location == "source" else run
        with open_artifact(parent, stage) as opened:
            verify_stage(opened, stage, budget)
        locations.append({"name": stage["name"], "location": location, "bytes": stage["bytes"]})
    return locations


def recover_run(mailbox: Mailbox, run_path: str | Path, *, mode: str = "inspect",
                offline: bool = False, expected_digest: str = "") -> dict[str, Any]:
    if mode not in ("inspect", "resume", "restore"):
        raise Refused("unknown recovery mode")
    if mode != "inspect" and not offline:
        raise Refused("--offline is required before resuming or restoring a run")
    with recorded_run(mailbox, run_path) as (run, run_path, plan, check_manifest):
        # Validate EVERY stage before opening a new action log. Unknown, moved,
        # edited, ambiguous, or colliding evidence never triggers partial undo.
        locations = inspect_recorded_stages(mailbox, run, plan)
        check_manifest()
        result = {"schema": plan["schema"], "mode": mode, "ok": True,
                  "run_directory": str(run_path), "plan_sha256": plan["plan_sha256"],
                  "locations": locations, "freed_bytes": 0,
                  "backup_retry_state_unchanged": True, "moved": [], "unchanged": [], "failures": []}
        if mode == "inspect":
            return result
        if expected_digest != plan["plan_sha256"]:
            raise Refused("recovery plan digest is missing or different; inspect the run first")
        result["locations_before"] = result.pop("locations")
        attempt = mode + "-" + uuid.uuid4().hex
        flags = os.O_WRONLY | os.O_CREAT | os.O_EXCL | os.O_NOFOLLOW | os.O_CLOEXEC
        actions = os.open(attempt + ".actions.jsonl", flags, 0o600, dir_fd=run)
        try:
            os.fsync(run)
            move_plan(mailbox, run, run_path, plan, actions, result,
                      restore=mode == "restore", check_manifest=check_manifest)
        finally:
            os.close(actions)
        if result["ok"]:
            try:
                result["locations_after"] = inspect_recorded_stages(mailbox, run, plan)
                check_manifest()
            except (OSError, Refused) as error:
                result["ok"] = False
                result["failures"].append({"error": f"post-operation observation unavailable: {error}"})
        return finish_receipt(run, attempt + ".result.json", result)


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument("--database", required=True, type=Path)
    parser.add_argument("--storage-root", required=True, type=Path)
    parser.add_argument("--min-age-seconds", type=int, default=DEFAULT_MIN_AGE_SECONDS)
    parser.add_argument("--rotated-backups", action="store_true",
                        help="preview/apply surplus standalone dated .bak files instead of staging directories")
    parser.add_argument("--keep-backups", type=int,
                        help="newest dated generations to keep (default 3, minimum 1; requires --rotated-backups)")
    parser.add_argument("--max-hash-bytes", type=int,
                        help="backup payload read budget per verification pass (default 128 GiB; requires --rotated-backups)")
    mode = parser.add_mutually_exclusive_group()
    mode.add_argument("--apply", action="store_true", help="quarantine the exact previewed stages; never delete")
    mode.add_argument("--inspect-run", type=Path, help="read-only inspection of recorded stages and their actual locations")
    mode.add_argument("--resume", type=Path, help="finish an interrupted quarantine using its original manifest")
    mode.add_argument("--restore", type=Path, help="return the recorded stages to their original names, without overwriting")
    parser.add_argument("--offline", action="store_true", help="declare that all exporters sharing this parent are stopped")
    parser.add_argument("--expect-plan", help="plan_sha256 from a preceding preview")
    args = parser.parse_args(argv)
    try:
        mutating = args.apply or args.resume or args.restore
        if not mutating and (args.offline or args.expect_plan):
            raise Refused("--offline and --expect-plan require --apply, --resume, or --restore")
        if not args.rotated_backups and (args.keep_backups is not None or args.max_hash_bytes is not None):
            raise Refused("backup retention/hash options require --rotated-backups")
        if args.rotated_backups and (args.inspect_run or args.resume or args.restore):
            raise Refused("recorded runs use their original scope and policy; omit --rotated-backups")
        keep_backups = DEFAULT_KEEP_BACKUPS if args.keep_backups is None else args.keep_backups
        max_hash_bytes = DEFAULT_MAX_HASH_BYTES if args.max_hash_bytes is None else args.max_hash_bytes
        with Mailbox(args.database, args.storage_root) as mailbox:
            if args.apply:
                result = apply_plan(mailbox, args.expect_plan or "", args.min_age_seconds, offline=args.offline,
                                    rotated_backups=args.rotated_backups, keep_backups=keep_backups,
                                    max_hash_bytes=max_hash_bytes)
            elif args.inspect_run or args.resume or args.restore:
                selected_mode = "resume" if args.resume else "restore" if args.restore else "inspect"
                result = recover_run(mailbox, args.inspect_run or args.resume or args.restore,
                                     mode=selected_mode, offline=args.offline,
                                     expected_digest=args.expect_plan or "")
            else:
                plan = (mailbox.backup_plan(args.min_age_seconds, keep_backups=keep_backups, max_hash_bytes=max_hash_bytes)
                        if args.rotated_backups else mailbox.plan(args.min_age_seconds))
                result = {"mode": "dry_run", "ok": True, "freed_bytes": 0,
                          **plan}
        print(json.dumps(result, sort_keys=True, ensure_ascii=True))
        return 0 if result["ok"] else 2
    except (OSError, Refused) as error:
        print(json.dumps({"schema": SCHEMA, "ok": False, "mode": "refused", "error": str(error)}))
        return 3


if __name__ == "__main__":
    raise SystemExit(main())
