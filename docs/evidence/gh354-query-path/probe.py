#!/usr/bin/env python3
"""Reproduce the complementary GH354 native-CLI query probe on an owned fixture.

Only the standard library is needed. Results are written to --output, never to
the repository. This validates literal SQL and CSV projections, not Rust driver
parameter binding, typed rows, archive publication, or running-server CPU.
"""

import argparse
import ast
import csv
import hashlib
import io
import json
import pathlib
import re
import resource
import sqlite3
import statistics
import subprocess
import time


HERE = pathlib.Path(__file__).resolve().parent
REPO = HERE.parents[2]
RECORDED = json.loads((HERE / "results.json").read_text())
SOURCES = {
    "reservation_legacy": ("crates/mcp-agent-mail-storage/src/recovery.rs", "LEGACY_RESERVATION_SOURCE_SQL", ""),
    "reservation_candidate": ("crates/mcp-agent-mail-storage/src/recovery.rs", "RESERVATION_SOURCE_SQL", ""),
    "message_legacy": ("crates/mcp-agent-mail-storage/src/recovery/message_reconcile/database/source.rs", "LEGACY_SOURCE_SQL", " LIMIT 1026"),
    "message_candidate": ("crates/mcp-agent-mail-db/src/queries.rs", "MESSAGE_ARCHIVE_SOURCE_SQL", " LIMIT 1026"),
}


def sha256(data):
    return hashlib.sha256(data).hexdigest()


def extract_queries():
    queries = {}
    for key, (path, constant, suffix) in SOURCES.items():
        source = (REPO / path).read_text()
        match = re.search(
            r"\b" + constant + r'\s*:\s*&str\s*=\s*("(?:[^"\\]|\\.)*")\s*;',
            source,
            re.S,
        )
        assert match, f"missing SQL constant: {constant}"
        # Retain the original probe's decoder. It preserves extra continuation
        # indentation as SQL whitespace; Rust's decoder strips that indentation.
        sql = ast.literal_eval(match.group(1)) + suffix
        actual = sha256(sql.encode())
        assert actual == RECORDED["sql_sha256"][key], (key, "SQL changed", actual)
        queries[key] = sql
    return queries


def prepare_fixture(output, queries):
    path = output / "mailbox-scale.sqlite3"
    if not path.exists():
        schema = (REPO / "crates/mcp-agent-mail-db/src/schema.rs").read_text()
        schema = schema.split('pub const CREATE_TABLES_SQL: &str = r"', 1)[1]
        schema = schema.split('";', 1)[0].split("-- FTS5 virtual table", 1)[0]
        assert sha256(schema.encode()) == RECORDED["fixture"]["schema_sha256"]
        with sqlite3.connect(path) as conn:
            conn.executescript(schema)
            conn.execute("PRAGMA foreign_keys=ON")
            conn.executemany(
                "INSERT INTO projects(id,slug,human_key,created_at) VALUES(?,?,?,?)",
                ((i, f"project-{i}", f"/mailbox-scale/project-{i}", 1000000) for i in range(1, 6)),
            )
            conn.executemany(
                "INSERT INTO agents(id,project_id,name,program,model,inception_ts,last_active_ts) VALUES(?,?,?,?,?,?,?)",
                ((i, (i - 1) % 5 + 1, f"Agent{i:03}", "test", "test", 1000000, 1000000) for i in range(1, 83)),
            )
            conn.execute("INSERT INTO db_identity VALUES(0,'aabbccdd')")
            reservations = []
            for ident in range(1, 9483):
                agent = (ident - 1) % 82 + 1
                project = (agent - 1) % 5 + 1
                reservations.append((
                    ident, project, agent, f"src/project-{project}/file-{ident}.rs", ident % 2,
                    f"reservation {ident}: retain holder, release authority, and historical metadata",
                    1700000000000000, 1700003600000000,
                    1700000001000000 + ident if ident % 4 == 0 else None,
                ))
            conn.executemany(
                "INSERT INTO file_reservations(id,project_id,agent_id,path_pattern,exclusive,reason,created_ts,expires_ts,released_ts) VALUES(?,?,?,?,?,?,?,?,?)",
                reservations,
            )
            conn.executemany(
                "INSERT INTO file_reservation_releases(reservation_id,released_ts) VALUES(?,?)",
                ((i, 1700000002000000 + i) for i in range(1, 9483) if i % 4 != 3),
            )
            body = RECORDED["fixture"]["body_unit"] * RECORDED["fixture"]["body_repeats"]
            assert len(body.encode()) == RECORDED["fixture"]["body_utf8_bytes"]
            messages, recipients = [], []
            for ident in range(1, 8242):
                agent = (ident - 1) % 82 + 1
                project = (agent - 1) % 5 + 1
                messages.append((
                    ident, project, agent, f"Message {ident}", body, f"thread-{ident % 64}",
                    "lookup", "normal", 1, 1700000000000000 + ident, "{}", "[]", "{}",
                ))
                group = list(range(project, 83, 5))
                start = group.index(agent)
                for offset, kind in [(1, "to"), (2, "cc"), (3, "bcc")]:
                    recipients.append((ident, group[(start + offset) % len(group)], kind, 7, 11))
            conn.executemany(
                "INSERT INTO messages(id,project_id,sender_id,subject,body_md,thread_id,topic,importance,ack_required,created_ts,recipients_json,attachments,archive_metadata_json) VALUES(?,?,?,?,?,?,?,?,?,?,?,?,?)",
                messages,
            )
            conn.executemany(
                "INSERT INTO message_recipients(message_id,agent_id,kind,read_ts,ack_ts) VALUES(?,?,?,?,?)",
                recipients,
            )
            conn.executemany(
                "INSERT INTO project_mailbox_deliveries(message_id,project_id,kind,delivered_ts) VALUES(?,?,?,?)",
                ((i, (((i - 1) % 82) % 5) + 1, "to", 1700000000000000 + i) for i in range(7, 8242, 7)),
            )
        conn.close()
    conn = sqlite3.connect(path)
    try:
        counts = {table: conn.execute(f"SELECT COUNT(*) FROM {table}").fetchone()[0] for table in RECORDED["fixture"]["counts"]}
        assert counts == RECORDED["fixture"]["counts"], counts
        # These are canonical construction checks, outside native timing.
        for prefix, identifiers, limits in [
            ("reservation", [1, 2, 3, 4, 82, 4741, 9482, 9483, -1], [32768]),
            ("message", [1, 4121, 8241, 8242, -1], [4194304, 1024]),
        ]:
            for ident in identifiers:
                old = conn.execute(queries[prefix + "_legacy"], [ident] + limits).fetchall()
                new = conn.execute(queries[prefix + "_candidate"], [ident] + limits).fetchall()
                assert old == new, (prefix, ident)
    finally:
        conn.close()
    return {"path": str(path), "file_bytes": path.stat().st_size, "counts": counts, "sqlite_version": sqlite3.sqlite_version}


def make_cases(mode, queries):
    cases = []

    def add(tag, name, ident, byte_limit=None, plan=False):
        values = [ident, 32768 if byte_limit is None else byte_limit] if name.startswith("reservation") else [ident, 4194304 if byte_limit is None else byte_limit, 1024]
        sql = re.sub(r"\?([123])\b", lambda match: str(values[int(match.group(1)) - 1]), queries[name])
        cases.append({"tag": tag, "name": name, "values": values, "sql": ("EXPLAIN QUERY PLAN " if plan else "") + sql + ";"})

    if mode == "plans":
        for prefix, ident in [("reservation", 4741), ("message", 4121)]:
            for variant in ["legacy", "candidate"]:
                add(f"plan:{prefix}_{variant}", f"{prefix}_{variant}", ident, plan=True)
    elif mode.startswith("timing-"):
        prefix = mode.removeprefix("timing-")
        ident = 4741 if prefix == "reservation" else 4121
        for variant in ["legacy", "candidate"]:
            add(f"warm:{variant}", f"{prefix}_{variant}", ident)
        for sample in range(6):
            for variant in (["legacy", "candidate"] if sample % 2 == 0 else ["candidate", "legacy"]):
                add(f"timing:{sample}:{variant}", f"{prefix}_{variant}", ident)
    else:
        prefix = mode.removeprefix("matrix-")
        identifiers = [1, 2, 3, 4, 4741, 9482, 9483, -1] if prefix == "reservation" else [1, 7, 4121, 8241, 8242, -1]
        for ident in identifiers:
            for variant in ["legacy", "candidate"]:
                add(f"matrix:{ident}:{variant}", f"{prefix}_{variant}", ident)
        for variant in ["legacy", "candidate"]:
            add(f"byte_refusal:{variant}", f"{prefix}_{variant}", 1, byte_limit=0)
    return cases


def parse_output(stdout):
    observed, tag, rows, timers = {}, None, [], []
    for row in csv.reader(io.StringIO(stdout)):
        if not row:
            continue
        if row[0] == "GH354_BEGIN":
            tag, rows, timers = row[1], [], []
        elif row[0] == "GH354_END":
            assert tag == row[1] and len(timers) == 2, (tag, timers)
            observed[tag] = {"rows": rows, "timer": timers[1]}
            tag = None
        elif row[0] in ["'GH354_BEGIN'", "'GH354_END'"]:
            continue
        elif tag is not None:
            if row[0].startswith("Run Time:"):
                match = re.fullmatch(r"Run Time: real ([0-9.]+) user ([0-9.]+) sys ([0-9.]+)", row[0])
                assert match, row
                timers.append(dict(zip(["real", "user", "sys"], map(float, match.groups()))))
            else:
                rows.append(row)
    return observed


def child_limits():
    resource.setrlimit(resource.RLIMIT_AS, (1024**3, 1024**3))
    resource.setrlimit(resource.RLIMIT_CPU, (25, 25))


def run_mode(mode, binary, output, fixture, queries):
    cases = make_cases(mode, queries)
    stem = output / f"{mode}-{time.time_ns()}"
    script = ".mode csv\n.headers on\n.timer on\n" + "".join(
        f"SELECT 'GH354_BEGIN', '{item['tag']}';\n{item['sql']}\nSELECT 'GH354_END', '{item['tag']}';\n"
        for item in cases
    )
    stem.with_suffix(".sql").write_text(script)
    started = time.monotonic()
    with stem.with_suffix(".stdout").open("w") as stdout, stem.with_suffix(".stderr").open("w") as stderr:
        try:
            result = subprocess.run([str(binary), "--batch", fixture["path"]], input=script, text=True, stdout=stdout, stderr=stderr, timeout=30, preexec_fn=child_limits)
            status = {"returncode": result.returncode, "timeout": False}
        except subprocess.TimeoutExpired:
            status = {"returncode": None, "timeout": True}
    status.update(mode=mode, wall_s=time.monotonic() - started, requested_source_queries=len(cases), diagnostic_marker_queries=2 * len(cases), artifact_stem=str(stem), fixture=fixture)
    status["stderr"] = stem.with_suffix(".stderr").read_text()
    observed = parse_output(stem.with_suffix(".stdout").read_text())
    status.update(completed_source_queries_captured=len(observed), observations=observed)
    success = not status["timeout"] and status["returncode"] == 0 and not status["stderr"]
    if success:
        assert len(observed) == len(cases), (len(observed), len(cases))
        conn = sqlite3.connect(fixture["path"])
        try:
            for item in cases:
                if item["tag"].startswith("plan:"):
                    continue
                data = observed[item["tag"]]["rows"]
                cursor = conn.execute(queries[item["name"]], item["values"])
                expected = [[str(value) if value is not None else "" for value in row] for row in cursor.fetchall()]
                header = [column[0] for column in cursor.description]
                if expected:
                    assert data[0] == header, (item["tag"], "header")
                    actual = data[1:]
                else:
                    actual = data[1:] if data and data[0] == header else data
                assert actual == expected, (item["tag"], "canonical CSV values")
                if item["name"].endswith("candidate"):
                    legacy_tag = item["tag"].rsplit(":", 1)[0] + ":legacy"
                    assert observed[legacy_tag]["rows"] == data, (item["tag"], "native variants")
        finally:
            conn.close()
        status["comparison"] = "native plans retained verbatim" if mode == "plans" else "complete native CSV headers/values match both variants and canonical SQLite; NULL/empty and scalar-type distinctions are not preserved by CSV"
        if mode.startswith("timing-"):
            status["timing"] = {}
            for variant in ["legacy", "candidate"]:
                samples = [observed[f"timing:{sample}:{variant}"]["timer"] for sample in range(6)]
                cpu = [sample["user"] + sample["sys"] for sample in samples]
                status["timing"][variant] = {"measured_queries": 6, "warmup_queries": 1, "samples": samples, "cpu_s_total": sum(cpu), "wall_s_total": sum(sample["real"] for sample in samples), "median_cpu_ms": statistics.median(cpu) * 1000}
    stem.with_suffix(".json").write_text(json.dumps(status, indent=2) + "\n")
    print(json.dumps({key: value for key, value in status.items() if key != "observations"}, indent=2))
    return success


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--output", required=True, type=pathlib.Path, help="owned scratch directory; the generated database is approximately 18 MB")
    parser.add_argument("--binary", type=pathlib.Path, help="verified official fsqlite 0.4.13 Linux amd64 binary")
    parser.add_argument("--prepare-only", action="store_true", help="extract/hash SQL and construct/check the canonical fixture without running the native CLI")
    parser.add_argument("--mode", choices=["all", "timing-reservation", "timing-message", "plans", "matrix-reservation", "matrix-message"], default="all")
    args = parser.parse_args()
    args.output = args.output.resolve()
    args.output.mkdir(parents=True, exist_ok=True)
    queries = extract_queries()
    for name, sql in queries.items():
        (args.output / (name + ".sql")).write_text(sql + ";\n")
    fixture = prepare_fixture(args.output, queries)
    if args.prepare_only:
        print(json.dumps({"fixture": fixture, "sql_hashes_match": True, "native_cli_executed": False}, indent=2))
        return
    if args.binary is None:
        parser.error("--binary is required unless --prepare-only is used")
    binary = args.binary.resolve()
    assert sha256(binary.read_bytes()) == RECORDED["engine"]["binary_sha256"], "binary hash differs"
    version = subprocess.run([str(binary), "--version"], check=True, capture_output=True, text=True, timeout=10).stdout.strip()
    assert version == RECORDED["engine"]["version"], version
    modes = ["timing-reservation", "timing-message", "plans"] if args.mode == "all" else [args.mode]
    for mode in modes:
        if not run_mode(mode, binary, args.output, fixture, queries):
            raise SystemExit(1)


if __name__ == "__main__":
    main()
