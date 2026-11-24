"""Eight independent MCP clients seeding one new four-agent mailbox (br-kp1in.33).

Run with python3 tests/e2e/lib/eight_sender.py --bin /path/to/am --out /results.
Each run starts a new server. Receipts distinguish shorter diagnostic runs
from three consecutive 10,000-send-per-client release workloads.
"""

import argparse
from concurrent.futures import ThreadPoolExecutor
import json
from pathlib import Path
import signal
import sqlite3
import tempfile
import threading
import time

from release_smoke import Arm, Session, deliveries_id, sha256


def exercise(arm, per_client):
    def call(tool, args):
        error, result = arm.call(tool, args)
        if error:
            raise RuntimeError(f"{tool}: {result}")
        return result

    call("ensure_project", {"human_key": arm.proj1})
    names = [call("register_agent", {"project_key": arm.proj1, "program": "load-test",
                                     "model": "eight-sender"})["name"] for _ in range(4)]
    if len(set(names)) != 4 or not all(isinstance(n, str) and n for n in names):
        raise RuntimeError(f"registration did not produce four distinct agents: {names}")
    ready = threading.Barrier(8)

    def send(client):
        session = Session(arm.url)
        sender, recipient = names[client % 4], names[(client + 1) % 4]
        result = {"client": client, "attempts": 0, "errors": 0, "error_samples": [], "receipts": []}
        ready.wait(timeout=60)
        for seq in range(per_client):
            if not arm.alive():
                break
            result["attempts"] += 1
            subject = f"eight-sender-{client}-{seq}"
            try:
                error, payload = session.call("send_message", {
                    "project_key": arm.proj1, "sender_name": sender, "to": [recipient],
                    "subject": subject, "body_md": subject, "thread_id": f"seed-{client}",
                })
                mid = None if error else deliveries_id(payload)
                if type(mid) is not int or mid <= 0:
                    raise ValueError(f"missing positive delivery id: {str(payload)[:240]}")
                result["receipts"].append((mid, subject, sender, recipient))
            except Exception as exc:
                result["errors"] += 1
                if len(result["error_samples"]) < 4:
                    result["error_samples"].append(str(exc)[:300])
        return result

    with ThreadPoolExecutor(max_workers=8) as clients:
        return list(clients.map(send, range(8)))


def verify_deliveries(database, project, receipts):
    with sqlite3.connect(database.resolve().as_uri() + "?mode=ro", uri=True) as conn:
        integrity = conn.execute("PRAGMA integrity_check").fetchall()
        rows = conn.execute("""
            SELECT m.id, m.subject, m.body_md, m.thread_id, s.name, d.kind, r.name,
                   (p.human_key IS ? AND s.project_id IS m.project_id AND r.project_id IS m.project_id)
            FROM messages AS m
            LEFT JOIN projects AS p ON p.id=m.project_id
            LEFT JOIN agents AS s ON s.id=m.sender_id
            LEFT JOIN message_recipients AS d ON d.message_id=m.id
            LEFT JOIN agents AS r ON r.id=d.agent_id
        """, (project,)).fetchall()
    # Registration and first contact can emit approval notices in this fresh mailbox.
    # Count only that independently identifiable system mail separately. Scanning
    # every other row also catches seed copies with a damaged subject or body.
    durable = set()
    notices = 0
    for row in rows:
        _, subject, body, thread, sender, kind, recipient, scoped = row
        if (scoped and thread is None and kind == "to"
                and subject == f"Contact approved: {sender} -> {recipient}"
                and isinstance(body, str)
                and body.startswith(f"{sender} requested permission to contact {recipient};")
                and "(auto_accept)" in body):
            notices += 1
        else:
            durable.add(row)
    expected = {(mid, subject, subject, "seed-" + subject.split("-")[2], sender, "to", recipient, 1)
                for mid, subject, sender, recipient in receipts}
    unique_ids = len({mid for mid, *_ in receipts}) == len(receipts)
    return {"integrity": integrity[:8], "durable_deliveries": len(rows) - notices,
            "contact_notices": notices,
            "unique_ids": unique_ids, "missing_sample": sorted(expected - durable, key=repr)[:8],
            "unexpected_sample": sorted(durable - expected, key=repr)[:8],
            "ok": integrity == [("ok",)] and unique_ids and expected == durable
                  and len(rows) - notices == len(receipts)}


def run(binary, out, per_client):
    root = Path(tempfile.mkdtemp(prefix="eight-sender-", dir=out))
    arm = Arm("eight-sender", binary, root)
    receipt = {"binary": str(binary), "binary_sha256": sha256(binary),
               "per_client": per_client, "clients": 8, "ok": False}
    started = time.monotonic()
    workers = []
    try:
        arm.start()
        receipt["running_sha256"] = sha256(Path(f"/proc/{arm.pid()}/exe"))
        workers = exercise(arm, per_client)
    except Exception as exc:
        receipt["error"] = str(exc)
    finally:
        receipt["alive_after_load"] = arm.alive()
        stopped = arm.kill(signal.SIGTERM)
        receipt["graceful_stop"] = receipt["alive_after_load"] and stopped and arm.proc.returncode == 0
        receipt["returncode"] = arm.proc.returncode if arm.proc else None
    receipts = [item for worker in workers for item in worker.pop("receipts")]
    (root / "receipts.json").write_text(json.dumps(receipts))
    receipt.update(workers=workers, acknowledged=len(receipts), elapsed_s=time.monotonic() - started)
    try:
        verification = verify_deliveries(arm.db_path, arm.proj1, receipts)
        receipt.update(verification=verification, ok=(
            verification["ok"] and receipt["alive_after_load"] and receipt["graceful_stop"]
            and receipt["returncode"] == 0 and len(receipts) == 8 * per_client
            and receipt["running_sha256"] == receipt["binary_sha256"]))
    except Exception as exc:
        receipt["verification_error"] = str(exc)
    (root / "run.json").write_text(json.dumps(receipt, indent=2))
    print(f"{root}: {len(receipts)}/{8 * per_client} acknowledged, "
          f"exit={receipt['returncode']}, ok={receipt['ok']}", flush=True)
    return receipt["ok"]


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--bin", required=True, type=Path)
    parser.add_argument("--out", required=True, type=Path)
    parser.add_argument("--per-client", type=int, default=10_000)
    parser.add_argument("--runs", type=int, default=3)
    args = parser.parse_args()
    if args.per_client < 1 or args.runs < 1:
        parser.error("--per-client and --runs must be positive")
    args.out.mkdir(parents=True, exist_ok=True)
    for _ in range(args.runs):
        if not run(args.bin.resolve(), args.out, args.per_client):
            return 1
    original = args.per_client == 10_000 and args.runs >= 3
    shape = "original load completed" if original else "diagnostic load completed"
    print(f"{shape}: {args.runs} runs, 8 clients × {args.per_client} sends each")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
