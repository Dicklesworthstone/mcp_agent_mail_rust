#!/usr/bin/env python3
"""Run explorer SQL regressions against canonical SQLite (not the Rust runtime).

Uses the SQL strings from mail_explorer.rs, rather than a second implementation
of the SELECTs. This supplements, and does not replace, the Rust/RCH test gate.
Run: python3 tests/test_mail_explorer_sql.py
EXPLORER_SOURCE can select a pre-change file for a negative-control run.
"""
from __future__ import annotations

import os
from pathlib import Path
import re
import sqlite3
import unittest

SOURCE = Path(os.environ.get(
    "EXPLORER_SOURCE",
    str(Path(__file__).resolve().parents[1] / "crates/mcp-agent-mail-db/src/mail_explorer.rs"),
))


class ExplorerSQLTests(unittest.TestCase):
    @classmethod
    def setUpClass(cls) -> None:
        source = SOURCE.read_text(encoding="utf-8")
        templates = re.findall(
            r'"(SELECT m\.id, m\.project_id, m\.sender_id, m\.thread_id, m\.subject, m\.body_md, '
            r'.*?LIMIT \{limit\})"', source, re.DOTALL,
        )
        if len(templates) != 2:
            raise AssertionError(f"Expected exactly two explorer SELECTs, got {len(templates)}")
        cls.templates = [re.sub(r"\\\n\s*", "", sql) for sql in templates]
        if "fn sql_order_by(" in source:
            body = source.split("fn sql_order_by(", 1)[1].split("fn candidate_limit(", 1)[0]
            orders = re.findall(r'"([^"\n]*)"', body)
            if len(orders) != 5:
                raise AssertionError(f"Expected five whitelisted sort clauses, got {orders}")
            cls.orders = orders
        else:
            # The original implementation has no sort-aware SQL selector.
            cls.orders = ["m.created_ts DESC"] * 5

    def setUp(self) -> None:
        self.db = sqlite3.connect(":memory:")
        self.addCleanup(self.db.close)
        self.db.executescript("""
            CREATE TABLE projects (id INTEGER PRIMARY KEY, slug TEXT);
            CREATE TABLE agents (id INTEGER PRIMARY KEY, project_id INTEGER, name TEXT);
            CREATE TABLE messages (
                id INTEGER PRIMARY KEY, project_id INTEGER, sender_id INTEGER,
                thread_id TEXT, subject TEXT, body_md TEXT, importance TEXT,
                ack_required INTEGER, created_ts INTEGER
            );
            CREATE TABLE message_recipients (
                message_id INTEGER, agent_id INTEGER, kind TEXT, read_ts INTEGER, ack_ts INTEGER
            );
        """)
        names = ["Viewer", "zulu", "Alpha", "alpha", "Éclair", "HiddenBCC"]
        for project in range(1, 4):
            self.db.execute("INSERT INTO projects VALUES (?, ?)", (project, f"p{project}"))
            for index, name in enumerate(names):
                self.db.execute("INSERT INTO agents VALUES (?, ?, ?)",
                                (100 * project + index, project, name))
            for index in range(36):
                message_id = 1000 * project + index
                sender = 100 * project + (0 if index % 3 == 0 else 1 + index % 4)
                importance = ["urgent", "low", "normal", "high", "URGENT", "unrecognized"][index % 6]
                self.db.execute("INSERT INTO messages VALUES (?, ?, ?, NULL, ?, '', ?, 1, ?)",
                                (message_id, project, sender, f"message {message_id}", importance, index // 3))
                recipients = [(100 * project + 1 + index % 4, "cc"), (100 * project + 5, "bcc")]
                if sender != 100 * project or index % 2 == 0:
                    recipients.append((100 * project, "bcc" if index % 5 == 0 else "to"))
                for recipient, kind in recipients:
                    self.db.execute("INSERT INTO message_recipients VALUES (?, ?, ?, NULL, NULL)",
                                    (message_id, recipient, kind))

    def rows(self, project: int, inbound: bool, mode: str, limit: int) -> list[dict]:
        position = {"date_desc": 0, "date_asc": 1, "importance": 2, "agent": 3 if inbound else 4}[mode]
        sql = self.templates[0 if inbound else 1].format(
            UNKNOWN_SENDER_DISPLAY="[unknown-sender]",
            where_clause="r.agent_id = ?1 AND m.project_id = ?2" if inbound else
                         "m.sender_id = ?1 AND m.project_id = ?2",
            order_by=self.orders[position], limit=limit,
        )
        result = []
        for row in self.db.execute(sql, (100 * project, project)):
            result.append({
                "id": row[0], "project": row[1], "ts": row[8], "importance": row[6],
                "sender": row[12 if inbound else 9], "recipients": row[14 if inbound else 11],
                "inbound": inbound,
            })
        return result

    @staticmethod
    def key(row: dict, mode: str) -> tuple:
        direction = 0 if row["inbound"] else 1
        descending = (-row["ts"], -row["id"], direction)
        if mode == "date_asc":
            return (row["ts"], row["id"], direction)
        if mode == "importance":
            rank = {"urgent": 4, "high": 3, "normal": 2, "low": 1}.get(row["importance"], 0)
            return (-rank, *descending)
        if mode == "agent":
            name = row["sender"] if row["inbound"] else row["recipients"]
            return (name.encode("utf-8").lower(), *descending)
        return descending

    def page(self, mode: str, directions: tuple[bool, ...], offset: int, limit: int) -> list[dict]:
        candidates = [row for project in range(1, 4) for inbound in directions
                      for row in self.rows(project, inbound, mode, offset + limit)]
        return sorted(candidates, key=lambda row: self.key(row, mode))[offset:offset + limit]

    def reference(self, mode: str, directions: tuple[bool, ...]) -> list[dict]:
        all_rows = [row for project in range(1, 4) for inbound in directions
                    for row in self.rows(project, inbound, mode, 1_000_000)]
        return sorted(all_rows, key=lambda row: self.key(row, mode))

    def test_oldest_first_is_not_a_sorted_newest_subset(self) -> None:
        expected = self.reference("date_asc", (True,))[:2]
        self.assertEqual(self.page("date_asc", (True,), 0, 2), expected)

    def test_priority_page_does_not_discard_old_urgent_messages(self) -> None:
        # The only urgent message is older than every ordinary message. A
        # newest-first candidate window cannot recover it by sorting afterward.
        self.db.execute("UPDATE messages SET importance = 'normal'")
        self.db.execute("INSERT INTO messages VALUES (8888, 1, 101, NULL, 'old urgent', '', 'urgent', 0, -100)")
        self.db.execute("INSERT INTO message_recipients VALUES (8888, 100, 'to', NULL, NULL)")
        expected = self.reference("importance", (True, False))[:4]
        self.assertEqual(expected[0]["id"], 8888)
        self.assertEqual(self.page("importance", (True, False), 0, 4), expected)

    def test_cross_project_top_k_matches_unbounded_reference(self) -> None:
        for mode in ("date_desc", "date_asc", "importance", "agent"):
            for directions in ((True,), (False,), (True, False)):
                expected = self.reference(mode, directions)
                for offset in (0, 1, 3, 17, 65, 200):
                    for limit in (0, 1, 2, 7, 50):
                        with self.subTest(mode=mode, directions=directions, offset=offset, limit=limit):
                            self.assertEqual(self.page(mode, directions, offset, limit),
                                             expected[offset:offset + limit])

    def test_inbound_recipient_lists_do_not_expose_bcc(self) -> None:
        rows = self.rows(1, True, "date_desc", 1000)
        self.assertTrue(rows)
        for row in rows:
            self.assertNotIn("HiddenBCC", row["recipients"].split(","))
            # Visible CC names remain present; this is not an empty-list workaround.
            self.assertTrue(row["recipients"])

    def test_outbound_sender_retains_bcc_routing(self) -> None:
        rows = self.rows(1, False, "date_desc", 1000)
        self.assertTrue(rows)
        for row in rows:
            self.assertIn("HiddenBCC", row["recipients"].split(","))

    def test_bcc_only_delivery_is_retained_without_recipient_disclosure(self) -> None:
        self.db.execute("INSERT INTO messages VALUES (9999, 1, 101, NULL, 'bcc only', '', 'normal', 0, 99)")
        self.db.execute("INSERT INTO message_recipients VALUES (9999, 100, 'bcc', NULL, NULL)")
        self.db.execute("INSERT INTO message_recipients VALUES (9999, 105, 'bcc', NULL, NULL)")
        matches = [row for row in self.rows(1, True, "date_desc", 1000) if row["id"] == 9999]
        self.assertEqual(len(matches), 1)
        self.assertEqual(matches[0]["recipients"], "")


if __name__ == "__main__":
    unittest.main(verbosity=2)
