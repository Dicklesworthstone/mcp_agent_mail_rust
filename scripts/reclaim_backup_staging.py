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

Examples:
  python3 scripts/reclaim_backup_staging.py --database /mail/storage.sqlite3 \
      --storage-root /mail
  # Stop/drain all exporters sharing /mail, then use the returned plan_sha256:
  python3 scripts/reclaim_backup_staging.py --database /mail/storage.sqlite3 \
      --storage-root /mail --apply --offline --expect-plan <plan_sha256>

Fixtures are exercised with:
  python3 -m unittest discover -s tests -p test_reclaim_backup_staging.py -v
"""

from __future__ import annotations

import argparse
from contextlib import ExitStack, contextmanager
import ctypes
import errno
import hashlib
import json
import os
from pathlib import Path
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
SCHEMA = 1


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
    def __init__(self, entries: int = MAX_ENTRIES) -> None:
        self.remaining = entries

    def visit(self) -> None:
        if self.remaining <= 0:
            raise Refused("inventory entry limit exceeded; no partial inventory is authoritative")
        self.remaining -= 1


def stage_name(name: str) -> bool:
    return name.startswith(STAGE_PREFIX) and len(name) > len(STAGE_PREFIX)


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
        plan = {"schema": SCHEMA, "binding": self.binding, "min_age_seconds": min_age_seconds,
                "eligible": sorted(eligible, key=lambda item: item["name"]),
                "retained": sorted(retained, key=lambda item: item["name"])}
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
    actual = tree_snapshot(fd, budget, sync=sync)
    if any(actual[key] != expected[key] for key in actual):
        raise Refused(f"staging evidence changed since preview: {expected['name']!r}")


def apply_plan(mailbox: Mailbox, expected_digest: str, min_age_seconds: int, *,
               offline: bool = False) -> dict[str, Any]:
    if not offline:
        raise Refused("--offline is required: first stop ALL exporters sharing the database parent")
    plan = mailbox.plan(min_age_seconds)
    if not expected_digest or plan["plan_sha256"] != expected_digest:
        raise Refused("plan changed or digest is missing; obtain a fresh preview before applying")
    result: dict[str, Any] = {"schema": SCHEMA, "mode": "apply", "ok": True,
                              "plan_sha256": expected_digest, "moved": [], "failures": [],
                              "freed_bytes": 0, "backup_retry_state_unchanged": True}
    if not plan["eligible"]:
        return result
    # A unique private destination is claimed only after all preview checks.
    with ExitStack() as stack:
        doctor = stack.enter_context(child_directory(mailbox.archive, "doctor", create=True))
        reclaim = stack.enter_context(child_directory(doctor, "reclaimable", create=True))
        run_name = RUN_PREFIX + uuid.uuid4().hex
        os.mkdir(run_name, 0o700, dir_fd=reclaim)
        os.fsync(reclaim)
        run = stack.enter_context(child_directory(reclaim, run_name))
        run_path = mailbox.storage_root / "doctor" / "reclaimable" / run_name
        result["run_directory"] = str(run_path)
        manifest = {"schema": SCHEMA, "operation": "quarantine_proactive_backup_stages",
                    "created_at_ns": time.time_ns(), "plan": plan}
        new_record(run, "manifest.json", manifest)
        flags = os.O_WRONLY | os.O_CREAT | os.O_EXCL | os.O_NOFOLLOW | os.O_CLOEXEC
        actions = os.open("actions.jsonl", flags, 0o600, dir_fd=run)
        stack.callback(os.close, actions)
        os.fsync(run)
        budget = Budget()
        for stage in plan["eligible"]:
            renamed = False
            try:
                mailbox.validate()
                with open_directory(run_path) as current:
                    if object_id(os.fstat(current)) != object_id(os.fstat(run)):
                        raise Refused("quarantine namespace changed")
                name = stage["name"]
                with child_directory(mailbox.parent, name) as source:
                    verify_stage(source, stage, budget, sync=True)
                    write_record(actions, {"phase": "intent", "name": name})
                    if object_id(os.stat(name, dir_fd=mailbox.parent, follow_symlinks=False)) != stage["identity"]:
                        raise Refused("source entry changed immediately before rename")
                    rename_noreplace(mailbox.parent, name, run, name)
                    renamed = True
                    os.fsync(mailbox.parent)
                    os.fsync(run)
                    mailbox.validate()
                    with open_directory(run_path) as current:
                        if object_id(os.fstat(current)) != object_id(os.fstat(run)):
                            raise Refused("quarantine namespace changed after rename")
                    if object_id(os.stat(name, dir_fd=run, follow_symlinks=False)) != object_id(os.fstat(source)):
                        raise Refused("destination entry changed after rename")
                    write_record(actions, {"phase": "completed", "name": name, "bytes": stage["bytes"]})
                result["moved"].append({"name": name, "bytes": stage["bytes"]})
            except (OSError, Refused) as error:
                result["ok"] = False
                result["failures"].append({"name": stage["name"], "rename_completed": renamed,
                                           "error": str(error),
                                           "instruction": "preserve the run directory; do not blindly retry or roll back"})
                break
        # This final receipt is supplemental. The synced pre-move manifest and
        # per-move intents survive even if publishing it is interrupted.
        try:
            new_record(run, "result.json", result)
        except OSError as error:
            result["ok"] = False
            result["failures"].append({"error": f"final receipt unavailable: {error}"})
        return result


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument("--database", required=True, type=Path)
    parser.add_argument("--storage-root", required=True, type=Path)
    parser.add_argument("--min-age-seconds", type=int, default=DEFAULT_MIN_AGE_SECONDS)
    parser.add_argument("--apply", action="store_true", help="quarantine the exact previewed stages; never delete")
    parser.add_argument("--offline", action="store_true", help="declare that all exporters sharing this parent are stopped")
    parser.add_argument("--expect-plan", help="plan_sha256 from a preceding preview")
    args = parser.parse_args(argv)
    try:
        if not args.apply and (args.offline or args.expect_plan):
            raise Refused("--offline and --expect-plan require --apply")
        with Mailbox(args.database, args.storage_root) as mailbox:
            if args.apply:
                result = apply_plan(mailbox, args.expect_plan or "", args.min_age_seconds, offline=args.offline)
            else:
                result = {"mode": "dry_run", "ok": True, "freed_bytes": 0,
                          **mailbox.plan(args.min_age_seconds)}
        print(json.dumps(result, sort_keys=True, ensure_ascii=True))
        return 0 if result["ok"] else 2
    except (OSError, Refused) as error:
        print(json.dumps({"schema": SCHEMA, "ok": False, "mode": "refused", "error": str(error)}))
        return 3


if __name__ == "__main__":
    raise SystemExit(main())
