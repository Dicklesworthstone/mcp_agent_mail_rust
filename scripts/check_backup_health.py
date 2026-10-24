#!/usr/bin/env python3
"""Read-only backup readiness, independent of the live SQLite engine.

Run: python3 scripts/check_backup_health.py --database /mail/storage.sqlite3
Exit 0: the inspected backup signals passed; 2: findings; 3: unavailable.
This is NOT an integrity check, a restore authorization, or `am doctor health`.
No database is opened with SQLite, no exporter is stopped, and no evidence is
changed. The parent-directory lease coordinates with current Unix automatic
exporters, not older or manual writers that ignore that lease. Physical paths
are required on Linux/macOS. Output is one JSON object, including on failure.

Native admission policy reference: integrity_guard_backup_budget.rs (GH#326).
The stage count includes ALL mailboxes sharing the database parent, including
legacy .snapshot-inspect-*.sqlite3 files from interrupted inspections. Logical
lengths, not allocated disk blocks, are used for the staging estimate. Snapshot
metadata is accepted only after a bounded SHA-256 read of its exact candidate;
its recorded integrity verdict is not rerun or promoted into a new verdict.
"""
from __future__ import annotations

import argparse
from contextlib import contextmanager
from datetime import datetime
import hashlib
import json
import os
from pathlib import Path
import re
import stat
import sys
import time
from typing import Any, Iterator

SCHEMA = "am.backup-health.v1"
STAGE_PREFIX = ".mcp-agent-mail-proactive-backup-"
LEGACY_INSPECTION_PREFIX = ".snapshot-inspect-"
LEGACY_INSPECTION_SUFFIX = ".sqlite3"
MAX_ENTRIES = 16_384
MAX_DEPTH = 64
MAX_METADATA_BYTES = 64 * 1024
MAX_U64 = (1 << 64) - 1
MAX_I64 = (1 << 63) - 1
STAGE_LIMIT = 3
STAGE_FLOOR = 512 * 1024 * 1024
DEFAULT_HASH_BYTES = 128 * 1024**3
COMPANIONS = ("-wal", "-shm", "-journal", "-wal-cert", "-wal-cert-head",
              "-fsqlite-ns-gate", "-fsqlite-ns-use", ".lock", ".meta.json")


class Unavailable(RuntimeError):
    pass


class Budget:
    def __init__(self, entries: int, hash_bytes: int):
        self.entries = entries
        self.hash_bytes = hash_bytes

    def visit(self) -> None:
        if self.entries <= 0:
            raise Unavailable("inventory entry budget exhausted; no partial healthy result")
        self.entries -= 1

    def reserve_hash(self, count: int) -> None:
        if not 0 <= count <= self.hash_bytes:
            raise Unavailable("snapshot hash byte budget exhausted")
        self.hash_bytes -= count


def bounded(value: Any, maximum: int = MAX_U64) -> bool:
    return type(value) is int and 0 <= value <= maximum


def stamp(info: os.stat_result) -> tuple[int, ...]:
    return (info.st_dev, info.st_ino, info.st_mode, info.st_nlink, info.st_size,
            info.st_mtime_ns, info.st_ctime_ns)


def flags(directory: bool = False) -> int:
    if not (sys.platform.startswith("linux") or sys.platform == "darwin"):
        raise Unavailable("descriptor-safe backup inspection requires Linux or macOS")
    return os.O_RDONLY | os.O_NOFOLLOW | os.O_NONBLOCK | os.O_CLOEXEC | (os.O_DIRECTORY if directory else 0)


def absolute(value: str | Path) -> Path:
    path = Path(value)
    if ".." in path.parts:
        raise Unavailable("parent traversal is not permitted; use a physical absolute path")
    return path if path.is_absolute() else Path.cwd() / path


@contextmanager
def directory(path: Path) -> Iterator[int]:
    path = absolute(path)
    fd = os.open("/", flags(True))
    try:
        for part in path.parts[1:]:
            child = os.open(part, flags(True), dir_fd=fd)
            os.close(fd)
            fd = child
        yield fd
    finally:
        os.close(fd)


def metadata(parent: int, name: str) -> os.stat_result | None:
    try:
        return os.stat(name, dir_fd=parent, follow_symlinks=False)
    except FileNotFoundError:
        return None


def regular(info: os.stat_result | None, name: str) -> os.stat_result:
    if info is None or not stat.S_ISREG(info.st_mode) or info.st_nlink != 1:
        raise Unavailable(f"{name!r} is not a regular, singly linked file")
    return info


def unchanged(parent: int, name: str, before: os.stat_result, opened: int) -> None:
    named = metadata(parent, name)
    if named is None or stamp(named) != stamp(before) or stamp(os.fstat(opened)) != stamp(before):
        raise Unavailable(f"{name!r} changed during inspection")


def read_metadata(parent: int, name: str, before: os.stat_result) -> bytes:
    regular(before, name)
    if before.st_size > MAX_METADATA_BYTES:
        raise Unavailable("snapshot metadata exceeds 64 KiB")
    fd = os.open(name, flags(), dir_fd=parent)
    try:
        unchanged(parent, name, before, fd)
        data = bytearray()
        while len(data) <= MAX_METADATA_BYTES:
            block = os.read(fd, min(65536, MAX_METADATA_BYTES + 1 - len(data)))
            if not block:
                break
            data.extend(block)
        if len(data) > MAX_METADATA_BYTES:
            raise Unavailable("snapshot metadata grew beyond 64 KiB")
        unchanged(parent, name, before, fd)
        return bytes(data)
    finally:
        os.close(fd)


def fingerprint(parent: int, name: str, before: os.stat_result, budget: Budget) -> str:
    regular(before, name)
    budget.reserve_hash(before.st_size)
    fd = os.open(name, flags(), dir_fd=parent)
    try:
        unchanged(parent, name, before, fd)
        digest = hashlib.sha256()
        remaining = before.st_size
        while remaining:
            block = os.read(fd, min(1024 * 1024, remaining))
            if not block:
                raise Unavailable("snapshot shrank during hashing")
            digest.update(block)
            remaining -= len(block)
        if os.read(fd, 1):
            raise Unavailable("snapshot grew during hashing")
        unchanged(parent, name, before, fd)
        return digest.hexdigest()
    finally:
        os.close(fd)


def tree_bytes(parent: int, name: str, budget: Budget, depth: int = 0) -> int:
    if depth > MAX_DEPTH:
        raise Unavailable("retained staging tree depth exceeds 64")
    before = metadata(parent, name)
    if before is None or not stat.S_ISDIR(before.st_mode):
        raise Unavailable(f"staging entry {name!r} is not a directory")
    fd = os.open(name, flags(True), dir_fd=parent)
    try:
        unchanged(parent, name, before, fd)
        total = 0
        with os.scandir(fd) as entries:
            for entry in entries:
                budget.visit()
                info = metadata(fd, entry.name)
                if info is not None and stat.S_ISDIR(info.st_mode):
                    total += tree_bytes(fd, entry.name, budget, depth + 1)
                else:
                    total += regular(info, entry.name).st_size
                if total > MAX_U64:
                    raise Unavailable("retained staging byte count overflow")
        unchanged(parent, name, before, fd)
        return total
    finally:
        os.close(fd)


def generation(database_name: str, name: str) -> tuple[str, int] | None:
    prefix = database_name + ".bak."
    if not name.startswith(prefix):
        return None
    match = re.fullmatch(r"([0-9]{8}_[0-9]{6})(?:-([0-9]{2,10}))?", name[len(prefix):])
    if not match:
        return None
    timestamp, suffix = match.groups()
    try:
        date = datetime.strptime(timestamp, "%Y%m%d_%H%M%S")
        if date.strftime("%Y%m%d_%H%M%S") != timestamp:
            return None
        ordinal = int(suffix) if suffix else 0
        if suffix and (not 0 < ordinal <= 2**32 - 1 or f"{ordinal:02}" != suffix):
            return None
    except ValueError:
        return None
    return timestamp, ordinal


def unique_object(pairs: list[tuple[str, Any]]) -> dict[str, Any]:
    out = {}
    for key, value in pairs:
        if key in out:
            raise ValueError("duplicate JSON key")
        out[key] = value
    return out


def valid_claim(value: Any, database: Path) -> bool:
    if type(value) is not dict:
        return False
    counts = value.get("row_counts")
    return (type(value.get("schema")) is int and value["schema"] == 1
            and bounded(value.get("created_us"), MAX_I64) and value["created_us"] > 0
            and value.get("integrity_verified") is True and value.get("integrity_kind") == "integrity_check"
            and bounded(value.get("schema_version"), MAX_I64)
            and type(counts) is dict and all(isinstance(k, str) and bounded(v, MAX_I64) for k, v in counts.items())
            and value.get("source_path") == str(database)
            and value.get("snapshot_path") == str(database) + ".bak"
            and bounded(value.get("snapshot_size_bytes")) and value["snapshot_size_bytes"] > 0
            and isinstance(value.get("snapshot_sha256"), str)
            and re.fullmatch(r"[0-9a-f]{64}", value["snapshot_sha256"]) is not None
            and isinstance(value.get("binary_version"), str) and bool(value["binary_version"]))


def inspect(database: str | Path, *, keep_backups: int = 3, max_age_seconds: int = 86400,
            max_entries: int = MAX_ENTRIES, max_hash_bytes: int = DEFAULT_HASH_BYTES,
            now_us: int | None = None) -> dict[str, Any]:
    if (not bounded(keep_backups, MAX_ENTRIES) or keep_backups < 1
            or not bounded(max_age_seconds, MAX_I64) or max_age_seconds < 1
            or not bounded(max_entries, MAX_ENTRIES) or max_entries < 1
            or not bounded(max_hash_bytes) or max_hash_bytes < 1):
        raise Unavailable("inspection limits must be positive bounded integers")
    database = absolute(database)
    budget = Budget(max_entries, max_hash_bytes)
    now_us = time.time_ns() // 1000 if now_us is None else now_us
    if not bounded(now_us, MAX_I64):
        raise Unavailable("inspection time must be a nonnegative i64 microsecond timestamp")
    findings: list[dict[str, str]] = []

    def finding(code: str, detail: str) -> None:
        findings.append({"code": code, "detail": detail})

    with directory(database.parent) as parent:
        import fcntl
        try:
            fcntl.flock(parent, fcntl.LOCK_EX | fcntl.LOCK_NB)
        except BlockingIOError as error:
            raise Unavailable("an automatic exporter or recovery command holds the database-parent lease") from error
        parent_before = os.fstat(parent)
        primary = regular(metadata(parent, database.name), database.name)
        wal = metadata(parent, database.name + "-wal")
        generation_bytes = primary.st_size + (regular(wal, database.name + "-wal").st_size if wal else 0)
        projected = generation_bytes * 2
        stage_budget = max(STAGE_FLOOR, generation_bytes * 5)
        if stage_budget > MAX_U64 or projected > MAX_U64:
            raise Unavailable("native backup estimate overflows u64")
        candidates: dict[str, os.stat_result] = {}
        legacy_inspections: dict[str, os.stat_result] = {}
        stages: list[dict[str, Any]] = []
        with os.scandir(parent) as entries:
            for entry in entries:
                budget.visit()
                name = entry.name
                if name.startswith(STAGE_PREFIX):
                    stages.append({"name": name, "bytes": tree_bytes(parent, name, budget)})
                elif name.startswith(LEGACY_INSPECTION_PREFIX):
                    if (not name.endswith(LEGACY_INSPECTION_SUFFIX)
                            or len(name) <= len(LEGACY_INSPECTION_PREFIX) + len(LEGACY_INSPECTION_SUFFIX)):
                        raise Unavailable(f"legacy snapshot inspection entry {name!r} is unrecognized; preserve it")
                    info = regular(metadata(parent, name), name)
                    legacy_inspections[name] = info
                    stages.append({"name": name, "bytes": info.st_size})
                elif name == database.name + ".bak" or generation(database.name, name) is not None:
                    candidates[name] = regular(metadata(parent, name), name)
        retained_stage_bytes = sum(stage["bytes"] for stage in stages)
        if retained_stage_bytes > MAX_U64:
            raise Unavailable("staging inventory overflows u64")
        if len(stages) >= STAGE_LIMIT:
            finding("staging_count_blocked", f"{len(stages)} stages reached the parent-wide limit of {STAGE_LIMIT}")
        if retained_stage_bytes + projected > stage_budget:
            finding("staging_bytes_blocked", f"{retained_stage_bytes} retained + {projected} projected bytes exceed {stage_budget}")
        if len(candidates) >= keep_backups + 3:
            finding("retained_backup_count", f"{len(candidates)} direct backup images reached keep_backups + 3 ({keep_backups + 3}); inspect retention")
        if not candidates:
            finding("backup_missing", "no current or strict timestamped backup image is present")
        meta_name = database.name + ".bak.meta.json"
        meta_info = metadata(parent, meta_name)
        claim = None
        verified_names = []
        if meta_info is None:
            finding("snapshot_metadata_missing", "no hash-bound verified-snapshot metadata is present")
        else:
            raw = read_metadata(parent, meta_name, meta_info)
            try:
                value = json.loads(raw, object_pairs_hook=unique_object)
            except (ValueError, UnicodeError, RecursionError):
                value = None
            if not valid_claim(value, database):
                finding("snapshot_metadata_invalid", "metadata is malformed, legacy/unbound, unsupported, or belongs to another mailbox; preserve it")
            else:
                claim = value
                for name, info in sorted(candidates.items()):
                    if info.st_size == claim["snapshot_size_bytes"] and fingerprint(parent, name, info, budget) == claim["snapshot_sha256"]:
                        companions = [name + suffix for suffix in COMPANIONS if suffix != ".meta.json" or name != database.name + ".bak"]
                        companions.extend(database.name + suffix + name[len(database.name):] for suffix in COMPANIONS)
                        if any(metadata(parent, companion) is not None for companion in companions):
                            finding("snapshot_companion_state", f"hash-matching candidate {name!r} has companion state; standalone readiness is not established")
                        else:
                            verified_names.append(name)
                if not verified_names:
                    finding("snapshot_bytes_unresolved", "no standalone current or timestamped backup matches the metadata's exact size and SHA-256")
                age_us = now_us - claim["created_us"]
                if age_us < 0:
                    finding("snapshot_time_in_future", "the metadata timestamp is in the future; clock or metadata requires inspection")
                elif age_us > max_age_seconds * 1_000_000:
                    finding("snapshot_stale", f"the hash-bound snapshot is older than {max_age_seconds} seconds")
                current_meta = metadata(parent, meta_name)
                if current_meta is None or stamp(current_meta) != stamp(meta_info):
                    raise Unavailable("snapshot metadata changed after candidate verification")
        for name, info in candidates.items():
            named = metadata(parent, name)
            if named is None or stamp(named) != stamp(info):
                raise Unavailable("backup inventory changed before completion")
        for name, info in legacy_inspections.items():
            named = metadata(parent, name)
            if named is None or stamp(named) != stamp(info):
                raise Unavailable("legacy snapshot inspection evidence changed before completion")
        with directory(database.parent) as named_parent:
            if stamp(os.fstat(named_parent)) != stamp(parent_before):
                raise Unavailable("database-parent namespace changed during inspection")
        current_primary = regular(metadata(parent, database.name), database.name)
        if (current_primary.st_dev, current_primary.st_ino) != (primary.st_dev, primary.st_ino):
            raise Unavailable("primary database identity changed during inspection")
        return {"schema": SCHEMA, "ok": not findings, "status": "findings" if findings else "ready",
                "scope": "backup-readiness-only", "database": str(database), "live_integrity": "not_probed",
                "findings": findings, "staging": {"scope": "database-parent", "count": len(stages),
                "bytes": retained_stage_bytes, "projected_bytes": projected, "byte_limit": stage_budget,
                "count_limit": STAGE_LIMIT, "entries": sorted(stages, key=lambda item: item["name"])},
                "backups": {"count": len(candidates), "bytes": sum(info.st_size for info in candidates.values()),
                "scope": "current-and-strict-timestamped-siblings", "keep_backups": keep_backups,
                "matching_snapshot_images": verified_names, "hash_bytes_read": max_hash_bytes - budget.hash_bytes,
                "recorded_integrity_verified": claim is not None, "fresh_integrity_check_performed": False}}


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument("--database", required=True, type=Path)
    parser.add_argument("--keep-backups", type=int, default=3)
    parser.add_argument("--max-snapshot-age-seconds", type=int, default=86400)
    parser.add_argument("--max-entries", type=int, default=MAX_ENTRIES)
    parser.add_argument("--max-hash-bytes", type=int, default=DEFAULT_HASH_BYTES)
    args = parser.parse_args(argv)
    try:
        report = inspect(args.database, keep_backups=args.keep_backups,
                         max_age_seconds=args.max_snapshot_age_seconds,
                         max_entries=args.max_entries, max_hash_bytes=args.max_hash_bytes)
        code = 0 if report["ok"] else 2
    except (OSError, Unavailable) as error:
        report = {"schema": SCHEMA, "ok": False, "status": "unavailable",
                  "scope": "backup-readiness-only", "live_integrity": "not_probed", "error": str(error)}
        code = 3
    print(json.dumps(report, sort_keys=True, ensure_ascii=True))
    return code


if __name__ == "__main__":
    raise SystemExit(main())
