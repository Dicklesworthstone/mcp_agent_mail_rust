#!/usr/bin/env python3
"""Prepare exclusively owned, synthetic GH#372 memory fixtures.

The caller supplies an already verified official FrankenSQLite 0.4.10 CLI.
No downloads, live mailbox discovery, or in-place recovery are performed.
The old engine creates the sparse metrics index; stock SQLite VACUUM INTO
creates a separate dense file with exactly the same schema and table data.
"""

import argparse
import hashlib
import json
import os
from pathlib import Path
import sqlite3
import subprocess


BODY = "retained body " * 320
MESSAGE_COUNTS = (("tiny", 1), ("healthy", 4096), ("double", 8192))
METRICS_ROWS = 12_000
TOOL = "send_message_" + "x" * 48
INDEX = "idx_tool_metrics_snapshots_tool_ts"
OLD_CLI_SHA256 = "c8964fdda2c474cdaad60e6c1e2909e2efadb18dfb74cb657ed39111e1ee76ea"
OLD_ARCHIVE_SHA256 = "12b8ec599038c74609a3db2c6ed6fb15fd3e3258693ab7dc62830f5dd3d42fc0"
OLD_RELEASE = "https://github.com/Dicklesworthstone/frankensqlite/releases/tag/v0.4.10"


def file_sha256(path):
    with path.open("rb") as source:
        return hashlib.file_digest(source, "sha256").hexdigest()


def private_root(path):
    root = path.resolve(strict=True)
    assert root == path and root.name.startswith("am-gh372-memory-")
    assert root.is_dir() and os.stat(root).st_mode & 0o077 == 0
    return root


def read_schema(repo):
    schema_path = repo / "crates/mcp-agent-mail-db/src/schema.rs"
    source = schema_path.read_text()
    marker = 'pub const CREATE_TABLES_SQL: &str = r"'
    assert source.count(marker) == 1
    literal = source.split(marker, 1)[1].split('";', 1)[0]
    assert "-- FTS5 virtual table" in literal
    # This is the same literal slice used by init_schema_sql_base(): no
    # runtime PRAGMAs, FTS objects, or unrelated later migrations are added.
    base = literal.split("-- FTS5 virtual table", 1)[0]
    migration_ids = (
        "v9_create_tool_metrics_snapshots",
        "v9_idx_tool_metrics_snapshots_tool_ts",
        "v9_idx_tool_metrics_snapshots_collected_ts",
    )
    migrations = []
    for migration_id in migration_ids:
        marker = f'"{migration_id}".to_string(),'
        assert source.count(marker) == 1
        block = source.split(marker, 1)[1].split("String::new(),", 1)[0]
        sql = block[block.index('"CREATE '):].split('"', 2)[1]
        # The authored v9 statements use only Rust backslash-newline string
        # continuations. Reject an unexpected escape instead of interpreting it.
        sql = "".join(line.rstrip("\\").lstrip() for line in sql.splitlines())
        assert "\\" not in sql
        migrations.append(sql)
    return base, ";\n".join(migrations) + ";\n", {
        "path": str(schema_path),
        "source_sha256": file_sha256(schema_path),
        "base_ddl_sha256": hashlib.sha256(base.encode()).hexdigest(),
        "v9_migration_ids": migration_ids,
        "v9_ddl_sha256": hashlib.sha256(";\n".join(migrations).encode()).hexdigest(),
    }


def seed(path, base_schema, count, metrics_schema=None):
    with path.open("xb"):
        pass
    db = sqlite3.connect(path)
    try:
        db.executescript(base_schema)
        if metrics_schema is not None:
            db.executescript(metrics_schema)
        db.execute(
            "INSERT INTO projects (id,slug,human_key,created_at) "
            "VALUES (1,'gh372','/gh372',1)"
        )
        db.execute(
            "INSERT INTO agents (id,project_id,name,program,model,inception_ts,last_active_ts) "
            "VALUES (1,1,'BlueLake','codex','test',1,1)"
        )
        db.executemany(
            "INSERT INTO messages (id,project_id,sender_id,subject,body_md,created_ts) "
            "VALUES (?,1,1,'before upgrade',?,?)",
            ((index, BODY, index) for index in range(1, count + 1)),
        )
        if metrics_schema is not None:
            db.execute(
                "INSERT INTO tool_metrics_snapshots (tool_name,collected_ts,calls) VALUES (?,0,7)",
                (TOOL,),
            )
        db.commit()
        assert db.execute("PRAGMA integrity_check").fetchall() == [("ok",)]
        assert db.execute("PRAGMA foreign_key_check").fetchall() == []
        assert db.execute("SELECT COUNT(*),SUM(length(body_md)) FROM messages").fetchone() == (
            count, count * len(BODY)
        )
    finally:
        db.close()
    if count >= 4096:
        assert path.stat().st_size > 16 * 1024 * 1024


def image_witness(db, count, metrics=False):
    assert db.execute("PRAGMA integrity_check").fetchall() == [("ok",)]
    assert db.execute("PRAGMA foreign_key_check").fetchall() == []
    schema = db.execute(
        "SELECT type,name,tbl_name,sql FROM sqlite_master ORDER BY type,name"
    ).fetchall()
    tables = {}
    for (name,) in db.execute(
        "SELECT name FROM sqlite_master WHERE type='table' ORDER BY name"
    ).fetchall():
        identifier = '"' + name.replace('"', '""') + '"'
        digest = hashlib.sha256()
        rows = 0
        for row in db.execute(f"SELECT * FROM {identifier} ORDER BY rowid"):
            encoded = [
                {"sqlite_blob_hex": value.hex()} if isinstance(value, bytes) else value
                for value in row
            ]
            digest.update(json.dumps(encoded, ensure_ascii=False, separators=(",", ":")).encode())
            digest.update(b"\n")
            rows += 1
        tables[name] = {"rows": rows, "sha256": digest.hexdigest()}
    messages = db.execute("SELECT COUNT(*),SUM(length(body_md)) FROM messages").fetchone()
    assert messages == (count, count * len(BODY))
    witness = {"schema": schema, "tables": tables, "messages": messages}
    if metrics:
        counts = db.execute("SELECT COUNT(*),SUM(calls) FROM tool_metrics_snapshots").fetchone()
        assert counts == (METRICS_ROWS + 1, 7 + 3 * METRICS_ROWS * (METRICS_ROWS + 1) // 2)
        ordered = db.execute(
            "SELECT collected_ts,calls FROM tool_metrics_snapshots INDEXED BY " + INDEX
            + " WHERE tool_name=? ORDER BY collected_ts DESC", (TOOL,)
        )
        rows_seen = 0
        for index, row in enumerate(ordered):
            timestamp = METRICS_ROWS - index
            assert row == (timestamp, timestamp * 3 if timestamp else 7)
            rows_seen += 1
        assert rows_seen == METRICS_ROWS + 1
        witness["metrics"] = counts
    return witness


def index_shape(path, root_page):
    size = path.stat().st_size
    with path.open("rb") as source:
        header = source.read(100)
        assert header.startswith(b"SQLite format 3\0")
        page_size = int.from_bytes(header[16:18], "big")
        if page_size == 1:
            page_size = 65536
        assert size % page_size == 0
        pending = [(root_page, 1)]
        visited = set()
        result = {"page_size": page_size, "pages": 0, "leaves": 0,
                  "single_cell_leaves": 0, "leaf_cells": 0, "depth": 0}
        while pending:
            number, depth = pending.pop()
            assert 0 < number <= size // page_size and number not in visited
            visited.add(number)
            source.seek((number - 1) * page_size)
            page = source.read(page_size)
            offset = 100 if number == 1 else 0
            cells = int.from_bytes(page[offset + 3:offset + 5], "big")
            result["pages"] += 1
            result["depth"] = max(result["depth"], depth)
            if page[offset] == 0x0A:
                result["leaves"] += 1
                result["single_cell_leaves"] += int(cells == 1)
                result["leaf_cells"] += cells
            elif page[offset] == 0x02:
                pending.append((int.from_bytes(page[offset + 8:offset + 12], "big"), depth + 1))
                for cell in range(cells):
                    pointer = offset + 12 + cell * 2
                    start = int.from_bytes(page[pointer:pointer + 2], "big")
                    assert offset + 12 + 2 * cells <= start <= page_size - 4
                    pending.append((int.from_bytes(page[start:start + 4], "big"), depth + 1))
            else:
                raise AssertionError(f"non-index page {number}: {page[offset]:#x}")
    return result


def populate_sparse(root, old_cli):
    script = root / "metrics-insert-0.4.10.sql"
    source = root / "sparse.sqlite3"
    tool = TOOL.replace("'", "''")
    with script.open("x") as sql:
        sql.write("BEGIN;\n")
        for timestamp in range(1, METRICS_ROWS + 1):
            sql.write("INSERT INTO tool_metrics_snapshots (tool_name,collected_ts,calls) "
                      f"VALUES ('{tool}',{timestamp},{timestamp * 3});\n")
            if timestamp % 100 == 0:
                sql.write("COMMIT;\n")
                if timestamp != METRICS_ROWS:
                    sql.write("BEGIN;\n")
        sql.write("SELECT COUNT(*) FROM tool_metrics_snapshots;\n")
    with script.open("rb") as sql:
        run = subprocess.run([str(old_cli), "--batch", str(source)], stdin=sql,
                             capture_output=True, timeout=600)
    (root / "old-cli.stdout").write_bytes(run.stdout)
    (root / "old-cli.stderr").write_bytes(run.stderr)
    assert run.returncode == 0, f"old CLI failed: {run.returncode}; inspect old-cli.stderr"
    assert run.stdout.strip() == str(METRICS_ROWS + 1).encode(), run.stdout
    return {"command": [str(old_cli), "--batch", str(source)], "sql_sha256": file_sha256(script),
            "stdout": str(root / "old-cli.stdout"), "stderr": str(root / "old-cli.stderr")}


def compact_and_verify(root):
    source = root / "sparse.sqlite3"
    dense = root / "dense.sqlite3"
    assert source.is_file() and not dense.exists()
    db = sqlite3.connect(source, isolation_level=None)
    try:
        # The old CLI has exited. All following operations concern our private
        # synthetic fixture, with no live owner and no operational mailbox.
        assert db.execute("PRAGMA wal_checkpoint(TRUNCATE)").fetchone()[0] == 0
        source_before = file_sha256(source)
        before = image_witness(db, 4096, metrics=True)
        before_root = db.execute("SELECT rootpage FROM sqlite_master WHERE name=?", (INDEX,)).fetchone()[0]
        db.execute("VACUUM INTO ?", (str(dense),))
    finally:
        db.close()
    assert file_sha256(source) == source_before
    verify = sqlite3.connect(dense.as_uri() + "?mode=ro", uri=True)
    try:
        after = image_witness(verify, 4096, metrics=True)
        after_root = verify.execute("SELECT rootpage FROM sqlite_master WHERE name=?", (INDEX,)).fetchone()[0]
    finally:
        verify.close()
    assert before == after
    sparse_shape = index_shape(source, before_root)
    dense_shape = index_shape(dense, after_root)
    assert sparse_shape["single_cell_leaves"] * 2 > sparse_shape["leaves"]
    assert dense_shape["single_cell_leaves"] * 10 < dense_shape["leaves"]
    assert sparse_shape["pages"] > 5 * dense_shape["pages"]
    return {
        "schema_and_all_table_witnesses_equal": True,
        "sparse_source_sha256_unchanged": source_before,
        "sparse": {"path": str(source), "bytes": source.stat().st_size,
                   "sha256": source_before, "index": sparse_shape, "witness": before},
        "dense": {"path": str(dense), "bytes": dense.stat().st_size,
                  "sha256": file_sha256(dense), "index": dense_shape, "witness": after},
    }


def main():
    if not __debug__:
        raise RuntimeError("run with python3 -I so assertions remain enabled")
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--repo", type=Path, required=True)
    parser.add_argument("--old-cli", type=Path, required=True)
    parser.add_argument("--output", type=Path, required=True)
    args = parser.parse_args()
    repo = args.repo.resolve(strict=True)
    old_cli = args.old_cli.resolve(strict=True)
    assert old_cli.is_file() and file_sha256(old_cli) == OLD_CLI_SHA256
    version = subprocess.run([str(old_cli), "--version"], check=True, capture_output=True,
                             text=True, timeout=30).stdout.strip()
    assert version == "fsqlite 0.4.10", version
    base, metrics_schema, schema_provenance = read_schema(repo)
    root = args.output.absolute()
    assert root.name.startswith("am-gh372-memory-")
    root.mkdir(mode=0o700, parents=False, exist_ok=False)
    root = private_root(root)
    report = {
        "sqlite_version": sqlite3.sqlite_version,
        "schema_provenance": schema_provenance,
        "old_cli": {"path": str(old_cli), "version": version, "sha256": OLD_CLI_SHA256,
                    "official_archive_sha256": OLD_ARCHIVE_SHA256, "release": OLD_RELEASE},
        "message_body_bytes_per_row": len(BODY),
        "healthy": {},
    }
    for label, count in MESSAGE_COUNTS:
        path = root / f"{label}.sqlite3"
        seed(path, base, count)
        db = sqlite3.connect(path.as_uri() + "?mode=ro", uri=True)
        try:
            witness = image_witness(db, count)
        finally:
            db.close()
        report["healthy"][label] = {"path": str(path), "bytes": path.stat().st_size,
                                    "sha256": file_sha256(path), "witness": witness}
    seed(root / "sparse.sqlite3", base, 4096, metrics_schema)
    report["old_cli_run"] = populate_sparse(root, old_cli)
    report["density_pair"] = compact_and_verify(root)
    report_path = root / "fixture-verification.json"
    with report_path.open("x") as output:
        json.dump(report, output, indent=2, sort_keys=True)
        output.write("\n")
    print(json.dumps({"report": str(report_path), "root": str(root),
                      "seeds": {label: str(root / f"{label}.sqlite3")
                                for label in ("tiny", "healthy", "double", "sparse", "dense")}},
                     sort_keys=True), flush=True)


if __name__ == "__main__":
    main()
