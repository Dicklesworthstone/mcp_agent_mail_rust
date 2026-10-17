#!/usr/bin/env python3
"""Prepare a verified, standalone repair COPY for the br-2hpuk v30 incident.

The source must be an offline, checkpointed backup. This program never opens
it through SQLite, never migrates it, and never replaces an existing output.
Publishing the result as a live mailbox is a separate operator decision.
"""

from __future__ import annotations

import argparse
from contextlib import closing
import hashlib
import json
import os
from pathlib import Path
import sqlite3
import stat
import struct
import sys
import tempfile
from typing import Any, BinaryIO


MESSAGE_COLUMNS = frozenset({
    "id", "project_id", "sender_id", "thread_id", "topic", "subject", "body_md",
    "importance", "ack_required", "created_ts", "recipients_json", "attachments",
    "archive_metadata_json",
})
# Never treat a database with companions as a standalone backup. In particular,
# copying just the main file of a WAL database silently loses committed mail.
COMPANIONS = ("-wal", "-shm", "-journal", "-fsqlite-ns-gate", "-fsqlite-ns-use")
CHUNK_BYTES = 1024 * 1024


class RepairError(Exception):
    """The input could not be safely repaired without changing logical data."""


def _quoted(identifier: str) -> str:
    return '"' + identifier.replace('"', '""') + '"'


def _no_companions(path: Path) -> None:
    for suffix in COMPANIONS:
        companion = Path(str(path) + suffix)
        if os.path.lexists(companion):
            raise RepairError(
                f"database companion exists: {companion}; supply a standalone, "
                "offline, checkpointed backup (do not delete live sidecars)"
            )


def _identity(info: os.stat_result) -> tuple[int, int, int, int, int]:
    return info.st_dev, info.st_ino, info.st_size, info.st_mtime_ns, info.st_ctime_ns


def _hash_stream(stream: BinaryIO) -> str:
    digest = hashlib.sha256()
    while chunk := stream.read(CHUNK_BYTES):
        digest.update(chunk)
    return digest.hexdigest()


def _hash_file(path: Path) -> str:
    with path.open("rb") as stream:
        return _hash_stream(stream)


def _snapshot(source: Path, target: Path) -> str:
    """Copy bytes with read-only OS I/O, rejecting observable source changes."""
    _no_companions(source)
    flags = os.O_RDONLY | getattr(os, "O_NOFOLLOW", 0) | getattr(os, "O_NONBLOCK", 0)
    with os.fdopen(os.open(source, flags), "rb") as original:
        before = os.fstat(original.fileno())
        if not stat.S_ISREG(before.st_mode):
            raise RepairError("source must be a regular database file")
        copied = hashlib.sha256()
        with target.open("xb") as output:
            os.chmod(target, 0o600)
            while chunk := original.read(CHUNK_BYTES):
                copied.update(chunk)
                output.write(chunk)
            output.flush()
            os.fsync(output.fileno())
        original.seek(0)
        if copied.hexdigest() != _hash_stream(original):
            raise RepairError("source changed during copying; stop writers and retry")
        if _identity(before) != _identity(os.fstat(original.fileno())):
            raise RepairError("source metadata changed during copying")
        if _identity(before) != _identity(source.stat()):
            raise RepairError("source was replaced during copying")
    _no_companions(source)
    return copied.hexdigest()


def _field(digest: Any, value: Any) -> None:
    """Hash SQLite values without conflating NULL, BLOB, TEXT, INTEGER or REAL."""
    if value is None:
        tag, payload = b"N", b""
    elif isinstance(value, int):
        tag, payload = b"I", struct.pack(">q", value)
    elif isinstance(value, float):
        tag, payload = b"F", struct.pack(">d", value)
    elif isinstance(value, str):
        tag, payload = b"T", value.encode("utf-8")
    elif isinstance(value, bytes):
        tag, payload = b"B", value
    else:
        raise RepairError(f"unsupported SQLite value type: {type(value).__name__}")
    digest.update(tag)
    digest.update(struct.pack(">Q", len(payload)))
    digest.update(payload)


def _integrity(conn: sqlite3.Connection) -> None:
    # Do not use quick_check: index/table consistency matters for inbox/search.
    if conn.execute("PRAGMA integrity_check").fetchall() != [("ok",)]:
        raise RepairError("canonical SQLite integrity_check failed; no repair published")


def _validate_schema(conn: sqlite3.Connection) -> None:
    tables = {row[0] for row in conn.execute(
        "SELECT name FROM sqlite_schema WHERE type = 'table'"
    )}
    if not {"messages", "message_recipients", "projects", "agents"} <= tables:
        raise RepairError("not an Agent Mail mailbox: required tables are missing")
    columns = conn.execute('PRAGMA table_xinfo("messages")').fetchall()
    if (len(columns) != 13 or {row[1] for row in columns} != MESSAGE_COLUMNS
            or columns[-1][1] != "archive_metadata_json"
            or any(row[6] != 0 for row in columns)):
        raise RepairError("expected the 13-column v30 messages schema; refusing an upgrade")
    primary = [row for row in columns if row[5] != 0]
    if len(primary) != 1 or primary[0][1] != "id" or primary[0][2].upper() != "INTEGER":
        raise RepairError("messages.id must be the INTEGER PRIMARY KEY")


def _varint(data: bytes, position: int, limit: int) -> tuple[int, int]:
    """Decode a bounded SQLite varint, including the eight-bit ninth byte."""
    value = 0
    for index in range(9):
        if position >= limit:
            raise RepairError("truncated SQLite record varint")
        byte = data[position]
        position += 1
        value = (value << (8 if index == 8 else 7)) | (byte if index == 8 else byte & 127)
        if index == 8 or byte < 128:
            return value, position
    raise RepairError("invalid SQLite varint")


def _physical_layout(path: Path, root_page: int, expected_rows: int) -> dict[str, Any]:
    """Count stored fields, not SQL's default-filled projection of a record.

    Format: https://www.sqlite.org/fileformat2.html, sections 1.6, 1.7, 2.1.
    Read one page at a time and retain only each record's bounded header. Visit
    each reachable table/overflow page once, rejecting cycles and bad pointers.
    This is a targeted v30 layout probe, not a replacement for integrity_check.
    """
    widths: dict[str, int] = {}
    samples: list[int] = []
    rows = short = table_pages = overflow_pages = 0
    previous_rowid: int | None = None
    with path.open("rb") as stream:
        header = stream.read(100)
        if len(header) != 100 or header[:16] != b"SQLite format 3\x00":
            raise RepairError("invalid SQLite database header")
        size = int.from_bytes(header[16:18], "big")
        size = 65536 if size == 1 else size
        length = os.fstat(stream.fileno()).st_size
        if size < 512 or size > 65536 or size & (size - 1) or length % size:
            raise RepairError("invalid SQLite page size or truncated file")
        usable = size - header[20]
        if usable < 480:
            raise RepairError("invalid reserved space in SQLite pages")
        page_count = length // size
        visited: set[int] = set()

        def read_page(number: int) -> bytes:
            if not 1 <= number <= page_count:
                raise RepairError("SQLite page pointer is outside the database")
            if number in visited:
                raise RepairError("cycle or shared page in messages b-tree")
            visited.add(number)
            stream.seek((number - 1) * size)
            data = stream.read(usable)
            if len(data) != usable:
                raise RepairError("truncated SQLite page")
            return data

        pending = [root_page]
        while pending:
            number = pending.pop()
            page = read_page(number)
            table_pages += 1
            start = 100 if number == 1 else 0
            kind = page[start]
            if kind not in (5, 13):
                raise RepairError("messages must use a rowid table b-tree")
            count = int.from_bytes(page[start + 3:start + 5], "big")
            pointers = start + (12 if kind == 5 else 8)
            pointer_end = pointers + 2 * count
            content = int.from_bytes(page[start + 5:start + 7], "big") or 65536
            if not pointer_end <= content <= usable:
                raise RepairError("invalid SQLite cell pointer array")
            children = []
            for index in range(count):
                cell = int.from_bytes(page[pointers + 2 * index:pointers + 2 * index + 2], "big")
                if not content <= cell < usable:
                    raise RepairError("SQLite cell pointer is outside the content area")
                if kind == 5:
                    if cell + 4 >= usable:
                        raise RepairError("truncated SQLite interior cell")
                    children.append(int.from_bytes(page[cell:cell + 4], "big"))
                    _varint(page, cell + 4, usable)
                    continue
                payload, position = _varint(page, cell, usable)
                rowid, position = _varint(page, position, usable)
                rowid = rowid - (1 << 64) if rowid >= (1 << 63) else rowid
                if previous_rowid is not None and rowid <= previous_rowid:
                    raise RepairError("messages rowids are not strictly ordered")
                previous_rowid = rowid
                maximum = usable - 35
                minimum = ((usable - 12) * 32 // 255) - 23
                candidate = minimum + (payload - minimum) % (usable - 4)
                local = payload if payload <= maximum else (
                    candidate if candidate <= maximum else minimum
                )
                end = position + local
                if end + (4 if local < payload else 0) > usable:
                    raise RepairError("SQLite record extends beyond its page")
                header_size, _ = _varint(page, position, end)
                if not 1 <= header_size <= min(payload, 9 * (len(MESSAGE_COLUMNS) + 1)):
                    raise RepairError("invalid or oversized v30 record header")
                record = page[position:min(end, position + header_size)]
                remaining = payload - local
                next_page = int.from_bytes(page[end:end + 4], "big") if remaining else 0
                while remaining:
                    block = read_page(next_page)
                    overflow_pages += 1
                    take = min(remaining, usable - 4)
                    needed = max(0, header_size - len(record))
                    record += block[4:4 + min(take, needed)]
                    remaining -= take
                    next_page = int.from_bytes(block[:4], "big")
                if next_page:
                    raise RepairError("overflow chain exceeds the record payload")
                _, position = _varint(record, 0, len(record))
                fields = body_size = 0
                while position < header_size:
                    serial, position = _varint(record, position, header_size)
                    if serial in (10, 11):
                        raise RepairError("reserved SQLite serial type in message record")
                    body_size += (
                        (0, 1, 2, 3, 4, 6, 8, 8, 0, 0)[serial]
                        if serial < 10 else (serial - 12) // 2
                    )
                    fields += 1
                if not 1 <= fields <= len(MESSAGE_COLUMNS) or header_size + body_size != payload:
                    raise RepairError("message field count or payload length is inconsistent")
                rows += 1
                widths[str(fields)] = widths.get(str(fields), 0) + 1
                if fields < len(MESSAGE_COLUMNS):
                    short += 1
                    if len(samples) < 8:
                        samples.append(rowid)
            if kind == 5:
                children.append(int.from_bytes(page[start + 8:start + 12], "big"))
                # Bound corrupt fan-out before it can grow an unbounded worklist.
                if len(children) + len(pending) > page_count:
                    raise RepairError("messages b-tree fan-out exceeds database size")
                pending.extend(reversed(children))
    if rows != expected_rows:
        raise RepairError("physical message count differs from canonical SQLite")
    return {
        "rows": rows, "short_records": short, "full_records": rows - short,
        "field_counts": widths, "short_record_sample_ids": samples,
        "table_pages": table_pages, "overflow_pages": overflow_pages,
    }


def inspect_mailbox(source: Path) -> dict[str, Any]:
    """Diagnose an offline mailbox via a private copy; never mutate the source."""
    source = source.resolve(strict=True)
    with tempfile.TemporaryDirectory(prefix=".am-v30-inspect-") as directory:
        staged = Path(directory) / "mailbox.sqlite3"
        source_hash = _snapshot(source, staged)
        with closing(sqlite3.connect(staged.as_uri() + "?mode=ro&immutable=1", uri=True)) as conn:
            _validate_schema(conn)
            _integrity(conn)
            root = conn.execute("SELECT rootpage FROM sqlite_schema WHERE name = 'messages'").fetchone()[0]
            count = conn.execute("SELECT COUNT(*) FROM messages").fetchone()[0]
        layout = _physical_layout(staged, root, count)
        _no_companions(source)
        if _hash_file(source) != source_hash:
            raise RepairError("source changed during inspection")
    return {
        "incident": "br-2hpuk", "source": str(source), "source_sha256": source_hash,
        "status": "requires_materialization" if layout["short_records"] else "full_records",
        "physical_layout": layout, "live_mailbox_replaced": False,
    }


def _logical_state(conn: sqlite3.Connection) -> dict[str, Any]:
    """Stream hashes of the entire logical database, including row identities.

    All ordinary tables are covered, not only messages: an UPDATE trigger that
    changes recipients, metrics, an audit table, or a migration ledger must fail
    the equality check and roll back. No message content goes into the report.
    """
    schema = conn.execute(
        "SELECT type, name, tbl_name, sql FROM sqlite_schema ORDER BY type, name"
    ).fetchall()
    schema_hash = hashlib.sha256()
    for row in schema:
        for value in row:
            _field(schema_hash, value)
    table_info = {row[1]: (row[2], row[4]) for row in conn.execute("PRAGMA table_list")
                  if row[0] == "main" and row[1] != "sqlite_schema"}
    state: dict[str, Any] = {
        "schema_sha256": schema_hash.hexdigest(),
        "user_version": conn.execute("PRAGMA user_version").fetchone()[0],
        "application_id": conn.execute("PRAGMA application_id").fetchone()[0],
        "encoding": conn.execute("PRAGMA encoding").fetchone()[0],
        "tables": {},
    }
    for name, (kind, without_rowid) in sorted(table_info.items()):
        if kind != "table":
            raise RepairError(f"unsupported virtual/shadow table: {name}")
        columns = conn.execute(f"PRAGMA table_xinfo({_quoted(name)})").fetchall()
        if any(row[6] != 0 for row in columns):
            raise RepairError(f"unsupported generated/hidden columns in table: {name}")
        names = {row[1].casefold() for row in columns}
        if without_rowid:
            keys = sorted((row[5], row[1]) for row in columns if row[5] != 0)
            if not keys:
                raise RepairError(f"no primary key for WITHOUT ROWID table: {name}")
            order = ", ".join(_quoted(key) + " COLLATE BINARY" for _, key in keys)
            projection = "*"
        else:
            alias = next((key for key in ("_rowid_", "rowid", "oid") if key not in names), None)
            if alias is None:
                raise RepairError(f"all rowid aliases are shadowed in table: {name}")
            order, projection = _quoted(alias), _quoted(alias) + ", *"
        digest = hashlib.sha256()
        count = 0
        for row in conn.execute(f"SELECT {projection} FROM {_quoted(name)} ORDER BY {order}"):
            digest.update(b"R" + struct.pack(">Q", len(row)))
            for value in row:
                _field(digest, value)
            count += 1
        state["tables"][name] = {"rows": count, "sha256": digest.hexdigest()}
    return state


def prepare_repair(source: Path, destination: Path) -> dict[str, Any]:
    """Validate and publish a new repair copy, never an in-place replacement."""
    if sqlite3.sqlite_version_info < (3, 37, 0):
        raise RepairError("SQLite 3.37 or later is required for complete table enumeration")
    source = source.resolve(strict=True)
    parent = destination.parent.resolve(strict=True)
    destination = parent / destination.name
    if os.path.lexists(destination):
        raise RepairError(f"output already exists (never overwritten): {destination}")
    _no_companions(destination)
    with tempfile.TemporaryDirectory(prefix=".am-v30-repair-", dir=parent) as directory:
        staged = Path(directory) / "mailbox.sqlite3"
        source_hash = _snapshot(source, staged)
        with closing(sqlite3.connect(staged, isolation_level=None, timeout=5)) as conn:
            # This is the private COPY, never the source. Ensure the published
            # result has no uncheckpointed WAL dependency.
            mode = conn.execute("PRAGMA journal_mode = DELETE").fetchone()[0]
            if mode != "delete":
                raise RepairError("could not make the private copy standalone")
            conn.execute("PRAGMA synchronous = FULL")
            _validate_schema(conn)
            _integrity(conn)
            before = _logical_state(conn)
            root = conn.execute("SELECT rootpage FROM sqlite_schema WHERE name = 'messages'").fetchone()[0]
            layout_before = _physical_layout(staged, root, before["tables"]["messages"]["rows"])
            conn.execute("BEGIN IMMEDIATE")
            try:
                changed = conn.execute(
                    "UPDATE messages SET archive_metadata_json = archive_metadata_json"
                ).rowcount
                if changed != before["tables"]["messages"]["rows"]:
                    raise RepairError("not every message was materialized")
                if _logical_state(conn) != before:
                    raise RepairError("materialization changed logical data or schema; rolled back")
                _integrity(conn)
                conn.execute("COMMIT")
            except BaseException:
                if conn.in_transaction:
                    conn.execute("ROLLBACK")
                raise
            # Check committed state as well as the in-transaction state.
            if _logical_state(conn) != before:
                raise RepairError("committed logical state differs; no repair published")
            _integrity(conn)
        _no_companions(staged)
        layout_after = _physical_layout(staged, root, layout_before["rows"])
        if layout_after["short_records"]:
            raise RepairError("short records remain after materialization; no repair published")
        _no_companions(source)
        if _hash_file(source) != source_hash:
            raise RepairError("source changed while preparing repair; no repair published")
        result_hash = _hash_file(staged)
        _no_companions(destination)
        # Atomic no-clobber publication on the same filesystem. Unlike replace
        # or rename, link cannot overwrite a file/symlink created by a race.
        os.link(staged, destination)
        with destination.open("rb") as output:
            os.fsync(output.fileno())
        if os.name == "posix":
            descriptor = os.open(parent, os.O_RDONLY | getattr(os, "O_DIRECTORY", 0))
            try:
                os.fsync(descriptor)
            finally:
                os.close(descriptor)
    return {
        "status": "verified_repair_copy",
        "incident": "br-2hpuk",
        "source": str(source),
        "destination": str(destination),
        "source_sha256": source_hash,
        "destination_sha256": result_hash,
        "messages_materialized": changed,
        "physical_layout_before": layout_before,
        "physical_layout_after": layout_after,
        "logical_state": before,
        "live_mailbox_replaced": False,
    }


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--check", action="store_true", help="diagnose physical records without preparing a repair output")
    parser.add_argument("source", type=Path, help="standalone, offline v30 mailbox backup")
    parser.add_argument("destination", type=Path, nargs="?", help="new repair copy; must not already exist")
    args = parser.parse_args(argv)
    if args.check and args.destination is not None:
        parser.error("--check does not accept a destination")
    if not args.check and args.destination is None:
        parser.error("a destination is required unless --check is used")
    try:
        report = inspect_mailbox(args.source) if args.check else prepare_repair(args.source, args.destination)
    except (RepairError, OSError, sqlite3.Error, UnicodeError) as error:
        print(f"v30 repair refused: {error}", file=sys.stderr)
        return 1
    print(json.dumps(report, indent=2, sort_keys=True))
    return 2 if report["status"] == "requires_materialization" else 0


if __name__ == "__main__":
    raise SystemExit(main())
