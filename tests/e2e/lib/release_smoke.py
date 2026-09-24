#!/usr/bin/env python3
"""Bounded black-box release smoke for Agent Mail (br-kp1in.14).

Drives a real `am serve-http` binary through the MCP HTTP protocol, as agents
do, in an isolated HOME/STORAGE_ROOT/DATABASE_URL, with the server's soft
RLIMIT_NOFILE started at 1024. Each arm (the candidate and an optional
control, usually the previous release) runs the same phases:

  flow           README coordination loop: register, invalid-name refusal,
                 symmetric-glob reservation conflict, release + re-grant,
                 send/fetch/ack/reply, delivery receipt, search, summarize,
                 resource://inbox, inbox-event cursor, broadcast refusal,
                 idempotent replay + IDEMPOTENCY_KEY_CONFLICT.
  cross_project  a send to a contact-linked agent in another project is
                 delivered there or refused with a structured error, and never
                 creates a same-name placeholder in the sender's project.
  storm          16 clients x 15 sends: every send acknowledged, ids distinct,
                 zero RESOURCE_BUSY.
  crash          SIGKILL mid-storm, restart: every server-acknowledged id is
                 readable, the archive converges to the DB count with NO client
                 reads, and an independent full `PRAGMA integrity_check` (C
                 SQLite, server stopped) is ok.
  soak           >= 5 minutes of mixed load (2 senders, 2 inbox readers, search,
                 health): bounded storage.sqlite3 descriptors, WBQ progress in
                 every 10 s window, fetch_inbox p99 under budget, zero dispatch
                 zombies, zero EMFILE, zero HTTP server restarts, no archive
                 re-root, and archive convergence after the load stops.

It exists because two opposite defects reached artifacts in September 2026
while every in-process suite was green: v0.3.36 leaked descriptors to EMFILE
within a minute of mixed load, and a main build wedged its archive drain.

Usage:  release_smoke.py --bin PATH [--control-bin PATH] --out DIR
Exit 0 only when every phase of the CANDIDATE passes. A phase that raises
before recording a failure, or runs with looser-than-release knobs, is
NO_VERDICT, which also fails. The control arm is recorded, never gating.
"""

from __future__ import annotations

import argparse
import hashlib
import json
import os
import re
import resource
import signal
import socket
import sqlite3
import subprocess
import sys
import threading
import time
import urllib.error
import urllib.request
from pathlib import Path

STORM_THREADS = 16
STORM_PER = 15
FD_BOUND = 64
# Release bounds. The environment may tighten them for development runs; a
# looser value can never produce a PASS (the phase becomes NO_VERDICT).
RELEASE_CONVERGE_SECS = 300
RELEASE_MIN_SOAK_SECS = 300
CONVERGE_SECS = int(os.environ.get("AM_RELEASE_SMOKE_CONVERGE_SECS", str(RELEASE_CONVERGE_SECS)))
SOAK_SECS = int(os.environ.get("AM_RELEASE_SMOKE_SOAK_SECS", str(RELEASE_MIN_SOAK_SECS)))
SOAK_WINDOW_SECS = 10
# Predeclared on 2026-09-23 before any candidate measurement (br-kp1in.14:
# "fetch_inbox p99 is under budget"). Change only through the AGENTS.md
# gate-defect rule, publishing what the change admits.
FETCH_INBOX_P99_BUDGET_S = 2.0
HTTP_RESTART_MARKERS = (
    "HTTP server instance exited unexpectedly; restarting",
    "HTTP server auto-restarted",
    "forcing supervised restart",
    "HTTP server failed to restart",
)
EMFILE_MARKERS = ("No file descriptors available", "os error 24", "Too many open files")


def log(msg: str) -> None:
    print(f"[release-smoke {time.strftime('%H:%M:%S')}] {msg}", flush=True)


def free_port() -> int:
    with socket.socket() as s:
        s.bind(("127.0.0.1", 0))
        return s.getsockname()[1]


def sha256(path: Path) -> str:
    h = hashlib.sha256()
    with path.open("rb") as f:
        for chunk in iter(lambda: f.read(1 << 20), b""):
            h.update(chunk)
    return h.hexdigest()


def has_emfile(text: str) -> bool:
    return any(marker in text for marker in EMFILE_MARKERS)


class Arm:
    """One binary under test with its own isolated mailbox and server process."""

    def __init__(self, name: str, binary: Path, root: Path):
        self.name = name
        self.binary = binary
        self.root = root
        self.port = free_port()
        self.url = f"http://127.0.0.1:{self.port}/mcp/"
        self.proc: subprocess.Popen | None = None
        self.session: str | None = None
        self.lock = threading.Lock()
        self.rid = 0
        for sub in ("home", "storage", "out", "proj1", "proj2"):
            (root / sub).mkdir(parents=True, exist_ok=True)
        self.proj1 = str(root / "proj1")
        self.proj2 = str(root / "proj2")
        self.db_path = root / "storage" / "storage.sqlite3"
        self.server_log = root / "out" / "server.log"

    # -- process lifecycle -------------------------------------------------
    def env(self) -> dict[str, str]:
        home = self.root / "home"
        storage = self.root / "storage"
        return {
            "PATH": "/usr/local/bin:/usr/bin:/bin",
            "HOME": str(home),
            "XDG_CONFIG_HOME": str(home / ".config"),
            "XDG_DATA_HOME": str(home / ".local/share"),
            "XDG_CACHE_HOME": str(home / ".cache"),
            "STORAGE_ROOT": str(storage),
            "DATABASE_URL": f"sqlite:///{self.db_path}",
            "TUI_ENABLED": "false",
            "HTTP_ALLOW_LOCALHOST_UNAUTHENTICATED": "true",
            "AM_INTERFACE_MODE": "cli",
        }

    def start(self) -> None:
        def soft_limit_1024() -> None:
            _, hard = resource.getrlimit(resource.RLIMIT_NOFILE)
            resource.setrlimit(resource.RLIMIT_NOFILE, (min(1024, hard), hard))

        log_file = self.server_log.open("ab")
        self.proc = subprocess.Popen(
            [str(self.binary), "serve-http", "--host", "127.0.0.1", "--port",
             str(self.port), "--no-auth", "--no-tui"],
            cwd=self.root, env=self.env(), stdout=log_file, stderr=log_file,
            preexec_fn=soft_limit_1024,
        )
        deadline = time.time() + 120
        while time.time() < deadline:
            try:
                with urllib.request.urlopen(
                    f"http://127.0.0.1:{self.port}/healthz", timeout=2
                ) as r:
                    if r.status == 200:
                        self.session = None
                        self.initialize()
                        return
            except (urllib.error.URLError, OSError):
                pass
            if self.proc.poll() is not None:
                raise RuntimeError(f"{self.name}: server exited during startup")
            time.sleep(0.5)
        raise RuntimeError(f"{self.name}: server not ready within 120 s")

    def kill(self, sig: int = signal.SIGKILL, grace_s: float = 60) -> bool:
        """Signal the server and wait up to `grace_s` for it to exit.

        Returns whether it exited in time. A server that outlives the grace
        period (a wedged drain can block graceful shutdown) is SIGKILLed so the
        smoke still stops it and writes its receipt."""
        if not (self.proc and self.proc.poll() is None):
            return True
        self.proc.send_signal(sig)
        try:
            self.proc.wait(timeout=grace_s)
            return True
        except subprocess.TimeoutExpired:
            self.proc.kill()
            self.proc.wait(timeout=60)
            return False

    def alive(self) -> bool:
        return self.proc is not None and self.proc.poll() is None

    def pid(self) -> int:
        assert self.proc is not None
        return self.proc.pid

    def threads(self) -> int | None:
        try:
            for line in Path(f"/proc/{self.pid()}/status").read_text().splitlines():
                if line.startswith("Threads:"):
                    return int(line.split()[1])
        except OSError:
            return None
        return None

    # -- MCP over HTTP -----------------------------------------------------
    def rpc(self, method: str, params: dict | None = None, timeout: float = 60) -> dict:
        with self.lock:
            self.rid += 1
            rid = self.rid
        body = json.dumps({"jsonrpc": "2.0", "id": rid, "method": method,
                           "params": params or {}}).encode()
        headers = {"Content-Type": "application/json",
                   "Accept": "application/json, text/event-stream"}
        if self.session:
            headers["Mcp-Session-Id"] = self.session
        req = urllib.request.Request(self.url, data=body, headers=headers, method="POST")
        with urllib.request.urlopen(req, timeout=timeout) as r:
            sid = r.headers.get("Mcp-Session-Id")
            if sid:
                self.session = sid
            raw = r.read().decode()
        if raw.lstrip().startswith("{"):
            return json.loads(raw)
        for line in raw.splitlines():
            if line.startswith("data:"):
                return json.loads(line[5:].strip())
        raise RuntimeError(f"unparseable response: {raw[:300]}")

    def initialize(self) -> None:
        self.rpc("initialize", {"protocolVersion": "2025-06-18", "capabilities": {},
                                "clientInfo": {"name": "release-smoke", "version": "1"}})
        try:
            self.rpc("notifications/initialized")
        except Exception:  # notification replies are optional
            pass

    def call(self, tool: str, args: dict, timeout: float = 60) -> tuple[bool, object]:
        resp = self.rpc("tools/call", {"name": tool, "arguments": args}, timeout=timeout)
        if "error" in resp:
            return True, resp["error"]
        res = resp["result"]
        text = "".join(c.get("text", "") for c in res.get("content", []))
        try:
            payload: object = json.loads(text)
        except ValueError:
            payload = text
        return bool(res.get("isError")), payload

    def read_resource(self, uri: str) -> tuple[bool, object]:
        resp = self.rpc("resources/read", {"uri": uri})
        if "error" in resp:
            return True, resp["error"]
        text = "".join(c.get("text", "") for c in resp["result"].get("contents", []))
        try:
            return False, json.loads(text)
        except ValueError:
            return False, text

    def health(self) -> dict:
        is_err, payload = self.call("health_check", {})
        if is_err or not isinstance(payload, dict):
            raise RuntimeError(f"health_check failed: {payload}")
        return payload

    def archive_db_counts(self) -> tuple[int | None, int | None, str]:
        """(archive, db, detail); counts are None when health cannot report them."""
        try:
            detail = self.health()["semantic_readiness"]["detail"]
        except Exception as ex:  # a sick server must not erase the verdict
            return None, None, f"{type(ex).__name__}: {ex}"[:400]
        counts = [int(n) for n in re.findall(r"messages=(\d+)", detail)]
        if len(counts) != 2:
            return None, None, detail[:400]
        return counts[0], counts[1], detail[:400]

    def wait_converged(self, bound_s: int, poll_s: int) -> tuple[bool, str]:
        deadline = time.time() + bound_s
        while True:
            archive, db, detail = self.archive_db_counts()
            if archive is not None and archive == db:
                return True, f"archive={archive} db={db}"
            if time.time() >= deadline:
                return False, f"archive={archive} db={db} last={detail}"
            time.sleep(poll_s)

    def offline_integrity_check(self) -> tuple[bool, object]:
        """Full PRAGMA integrity_check by C SQLite on the stopped server's file."""
        assert not self.alive(), "offline integrity check needs the server stopped"
        try:
            con = sqlite3.connect(f"file:{self.db_path}?mode=ro", uri=True, timeout=30)
            try:
                rows = [r[0] for r in con.execute("PRAGMA integrity_check").fetchall()]
            finally:
                con.close()
        except sqlite3.Error as ex:
            return False, f"{type(ex).__name__}: {ex}"
        return rows == ["ok"], {"sqlite_version": sqlite3.sqlite_version, "rows": rows[:10]}

    def sqlite_fds(self) -> int:
        n = 0
        fd_dir = Path(f"/proc/{self.pid()}/fd")
        for fd in fd_dir.iterdir():
            try:
                if os.readlink(fd).endswith("storage.sqlite3"):
                    n += 1
            except OSError:
                pass
        return n


# ---------------------------------------------------------------------------
# Phases. Each appends checks to `c` and returns extra receipt fields. The
# runner turns the checks into PASS | FAIL | NO_VERDICT.
# ---------------------------------------------------------------------------

def check(checks: list, name: str, ok: bool, detail: object = "") -> bool:
    checks.append({"name": name, "ok": bool(ok), "detail": str(detail)[:400]})
    log(("PASS " if ok else "FAIL ") + name + (f" {str(detail)[:160]}" if not ok else ""))
    return ok


def verdict(checks: list) -> str:
    if not checks:
        return "NO_VERDICT"
    return "PASS" if all(c["ok"] for c in checks) else "FAIL"


def deliveries_id(payload: object) -> int | None:
    if not isinstance(payload, dict):
        return None
    d = payload.get("deliveries") or []
    return (d[0].get("payload", {}).get("id") if d else None) or payload.get("id")


def contains_id(payload: object, wanted: object) -> bool:
    """True when any nested object carries `"id": wanted`."""
    if isinstance(payload, dict):
        if payload.get("id") == wanted:
            return True
        return any(contains_id(v, wanted) for v in payload.values())
    if isinstance(payload, list):
        return any(contains_id(v, wanted) for v in payload)
    return False


def agent_names(payload: object) -> list:
    rows = payload.get("agents", []) if isinstance(payload, dict) else payload
    return [r.get("name") for r in rows or [] if isinstance(r, dict)]


def phase_flow(arm: Arm, state: dict, c: list) -> dict:
    e, _ = arm.call("ensure_project", {"human_key": arm.proj1})
    check(c, "ensure_project", not e)
    names = []
    for program in ("claude-code", "codex-cli"):
        e, a = arm.call("register_agent", {"project_key": arm.proj1, "program": program,
                                           "model": "smoke"})
        names.append(a.get("name") if isinstance(a, dict) else None)
    a_name, b_name = names
    state.update(A=a_name, B=b_name)
    check(c, "register two agents", all(names), names)
    e, bad = arm.call("register_agent", {"project_key": arm.proj1, "program": "smoke",
                                         "model": "smoke", "name": "not a valid name!"})
    check(c, "invalid agent name refused", e, bad)
    e, g = arm.call("file_reservation_paths", {"project_key": arm.proj1, "agent_name": a_name,
                                               "paths": ["src/auth/**"], "ttl_seconds": 3600,
                                               "exclusive": True})
    check(c, "reserve src/auth/**", not e and len(g.get("granted", [])) == 1, g)
    e, conflict = arm.call("file_reservation_paths", {"project_key": arm.proj1,
                                                      "agent_name": b_name,
                                                      "paths": ["src/auth/login.rs"],
                                                      "ttl_seconds": 3600, "exclusive": True})
    check(c, "symmetric glob conflict",
          not e and len(conflict.get("conflicts", [])) >= 1, conflict)
    e, s = arm.call("send_message", {"project_key": arm.proj1, "sender_name": a_name,
                                     "to": [b_name], "subject": "smoke", "body_md": "hi",
                                     "thread_id": "smoke-1", "ack_required": True})
    mid = deliveries_id(s)
    check(c, "send_message", not e and mid is not None, s)
    e, inbox = arm.call("fetch_inbox", {"project_key": arm.proj1, "agent_name": b_name})
    check(c, "fetch_inbox sees message",
          isinstance(inbox, list) and any(m.get("id") == mid for m in inbox), inbox)
    e, _ = arm.call("acknowledge_message", {"project_key": arm.proj1, "agent_name": b_name,
                                            "message_id": mid})
    check(c, "acknowledge_message", not e)
    e, _ = arm.call("reply_message", {"project_key": arm.proj1, "message_id": mid,
                                      "sender_name": b_name, "body_md": "ack"})
    check(c, "reply_message", not e)
    e, r = arm.call("get_message_delivery_receipt", {"project_key": arm.proj1,
                                                     "message_id": mid})
    check(c, "delivery receipt persisted", not e and r.get("persisted") is True, r)
    e, sr = arm.call("search_messages", {"project_key": arm.proj1, "query": "smoke",
                                         "limit": 20})
    check(c, "search_messages finds the message", not e and contains_id(sr, mid), sr)
    e, summ = arm.call("summarize_thread", {"project_key": arm.proj1, "thread_id": "smoke-1"})
    check(c, "summarize_thread", not e and isinstance(summ, dict) and "summary" in summ, summ)
    e, res = arm.read_resource(f"resource://inbox/{b_name}?project={arm.proj1}&limit=20")
    check(c, "resource://inbox lists the message", not e and contains_id(res, mid), res)
    e, bc = arm.call("send_message", {"project_key": arm.proj1, "sender_name": a_name,
                                      "to": [b_name], "subject": "x", "body_md": "x",
                                      "broadcast": True})
    check(c, "broadcast refused", e, bc)
    args = {"project_key": arm.proj1, "agent_name": a_name, "paths": ["docs/**"],
            "ttl_seconds": 600, "exclusive": True, "idempotency_key": "smoke-k1"}
    e1, i1 = arm.call("file_reservation_paths", args)
    e2, i2 = arm.call("file_reservation_paths", args)
    check(c, "idempotent replay", not e1 and not e2 and isinstance(i2, dict)
          and i2.get("idempotent_replay") is True, i2)
    e3, i3 = arm.call("file_reservation_paths", dict(args, paths=["other/**"]))
    check(c, "idempotency key conflict",
          e3 and "IDEMPOTENCY_KEY_CONFLICT" in json.dumps(i3), i3)
    e, rel = arm.call("release_file_reservations", {"project_key": arm.proj1,
                                                    "agent_name": a_name,
                                                    "paths": ["src/auth/**"]})
    check(c, "release", not e and isinstance(rel, dict) and rel.get("released", 0) >= 1, rel)
    e, regrant = arm.call("file_reservation_paths", {"project_key": arm.proj1,
                                                     "agent_name": b_name,
                                                     "paths": ["src/auth/login.rs"],
                                                     "ttl_seconds": 600, "exclusive": True})
    check(c, "re-grant after release",
          not e and len(regrant.get("granted", [])) == 1 and not regrant.get("conflicts"),
          regrant)
    e, ev = arm.call("fetch_inbox_events", {"project_key": arm.proj1, "agent_name": b_name})
    check(c, "inbox events cursor", not e and isinstance(ev, dict) and "next_cursor" in ev, ev)
    return {}


def phase_cross_project(arm: Arm, state: dict, c: list) -> dict:
    e, _ = arm.call("ensure_project", {"human_key": arm.proj2})
    e, peer = arm.call("register_agent", {"project_key": arm.proj2, "program": "gemini-cli",
                                          "model": "smoke"})
    peer_name = peer.get("name") if isinstance(peer, dict) else None
    check(c, "register peer in the second project", not e and peer_name, peer)
    e, hs = arm.call("macro_contact_handshake", {
        "project_key": arm.proj1, "requester": state["A"], "target": peer_name,
        "to_project": arm.proj2, "reason": "release smoke", "auto_accept": True,
        "welcome_subject": "hello", "welcome_body": "welcome across repos"})
    check(c, "cross-project contact handshake", not e, hs)
    e, sent = arm.call("send_message", {"project_key": arm.proj1, "sender_name": state["A"],
                                        "to": [peer_name], "subject": "xp-smoke",
                                        "body_md": "cross-project"})
    _, agents = arm.call("list_agents", {"project_key": arm.proj1})
    names = agent_names(agents)
    check(c, "no same-name placeholder in the sender's project",
          isinstance(names, list) and peer_name not in names, names)
    _, peer_inbox = arm.call("fetch_inbox", {"project_key": arm.proj2, "agent_name": peer_name,
                                             "limit": 50, "mark_read": False})
    delivered = isinstance(peer_inbox, list) and any(
        m.get("subject") == "xp-smoke" for m in peer_inbox)
    refused = e and "CROSS_PROJECT_RECIPIENT" in json.dumps(sent)
    check(c, "delivered to the peer's project or refused with CROSS_PROJECT_RECIPIENT",
          delivered or refused, {"send": sent, "peer_inbox": peer_inbox})
    return {"outcome": "delivered" if delivered else ("refused" if refused else "misdelivered")}


def storm(arm: Arm, state: dict, threads: int, per: int, deadline_s: float = 90) -> dict:
    acked: list = []
    errors: list = []
    lat: list = []
    lock = threading.Lock()

    def worker(i: int) -> None:
        for j in range(per):
            t0 = time.time()
            try:
                e, s = arm.call("send_message", {
                    "project_key": arm.proj1,
                    "sender_name": state["A"] if i % 2 else state["B"],
                    "to": [state["B"] if i % 2 else state["A"]],
                    "subject": f"storm {i}-{j}", "body_md": "storm",
                    "thread_id": f"storm-{i}"}, timeout=deadline_s)
                with lock:
                    if e:
                        errors.append(str(s)[:400])
                    else:
                        acked.append(deliveries_id(s))
                        lat.append(time.time() - t0)
            except Exception as ex:  # connection refused after SIGKILL etc.
                with lock:
                    errors.append(f"{type(ex).__name__}: {ex}"[:200])

    ts = [threading.Thread(target=worker, args=(i,)) for i in range(threads)]
    for t in ts:
        t.start()
    return {"threads": ts, "acked": acked, "errors": errors, "lat": lat}


def join(st: dict) -> dict:
    for t in st["threads"]:
        t.join()
    q = sorted(st["lat"])
    return {"acked": [a for a in st["acked"] if a is not None], "errors": st["errors"],
            "p50_s": q[len(q) // 2] if q else None, "p99_s": q[int(len(q) * 0.99)] if q else None}


def phase_storm(arm: Arm, state: dict, c: list) -> dict:
    res = join(storm(arm, state, STORM_THREADS, STORM_PER))
    offered = STORM_THREADS * STORM_PER
    busy = [err for err in res["errors"] if "RESOURCE_BUSY" in err]
    check(c, "all storm sends acknowledged", len(res["acked"]) == offered,
          f"{len(res['acked'])}/{offered} errors={res['errors'][:2]}")
    check(c, "acknowledged ids distinct", len(set(res["acked"])) == len(res["acked"]))
    check(c, "zero RESOURCE_BUSY", not busy, f"{len(busy)}/{offered}")
    return {"offered": offered, "p50_s": res["p50_s"], "p99_s": res["p99_s"],
            "errors": res["errors"][:5]}


def phase_crash(arm: Arm, state: dict, c: list) -> dict:
    st = storm(arm, state, STORM_THREADS, 40)
    time.sleep(6)
    arm.kill(signal.SIGKILL)
    res = join(st)
    arm.start()
    seen: set = set()
    for agent in (state["A"], state["B"]):
        e, inbox = arm.call("fetch_inbox", {"project_key": arm.proj1, "agent_name": agent,
                                            "limit": 10000, "mark_read": False})
        if isinstance(inbox, list):
            seen |= {m.get("id") for m in inbox}
    missing = sorted(set(res["acked"]) - seen)
    check(c, f"all {len(res['acked'])} acknowledged ids survive SIGKILL", not missing,
          f"missing={missing[:10]}")
    # Informational: right after a restart the server honestly reports that no
    # full check has run in this process yet; the gate uses an independent one.
    server_integrity = arm.health()["verdicts"]["integrity_check"]
    ok, detail = arm.wait_converged(CONVERGE_SECS, 10)
    check(c, f"archive converges to DB without client reads (<= {CONVERGE_SECS}s)", ok, detail)
    check(c, "server exits within 60 s of SIGTERM", arm.kill(signal.SIGTERM))
    ok, detail = arm.offline_integrity_check()
    check(c, "full PRAGMA integrity_check ok (independent C SQLite, server stopped)", ok, detail)
    arm.start()
    extra: dict = {"acked": len(res["acked"]),
                   "server_integrity_verdict_after_restart": server_integrity}
    if CONVERGE_SECS > RELEASE_CONVERGE_SECS:
        extra["cannot_pass"] = f"convergence bound {CONVERGE_SECS}s is looser than release"
    return extra


def phase_soak(arm: Arm, state: dict, c: list) -> dict:
    stop = time.time() + SOAK_SECS
    counts = {"send": 0, "read": 0, "search": 0, "health": 0, "err": 0}
    read_lat: list = []
    client_emfile: list = []
    lock = threading.Lock()

    def loop(kind: str) -> None:
        while time.time() < stop:
            t0 = time.time()
            try:
                if kind == "send":
                    e, p = arm.call("send_message", {"project_key": arm.proj1,
                                                     "sender_name": state["A"],
                                                     "to": [state["B"]], "subject": "soak",
                                                     "body_md": "soak", "thread_id": "soak"})
                elif kind == "read":
                    e, p = arm.call("fetch_inbox", {"project_key": arm.proj1,
                                                    "agent_name": state["B"], "limit": 20,
                                                    "mark_read": False})
                elif kind == "search":
                    e, p = arm.call("search_messages", {"project_key": arm.proj1,
                                                        "query": "soak", "limit": 10})
                else:
                    e, p = arm.call("health_check", {})
                    time.sleep(1)
                with lock:
                    counts["err" if e else kind] += 1
                    if kind == "read" and not e:
                        read_lat.append(time.time() - t0)
                    if e and has_emfile(json.dumps(p)):
                        client_emfile.append(f"{kind}: {str(p)[:200]}")
            except Exception as ex:
                with lock:
                    counts["err"] += 1
                    if has_emfile(str(ex)):
                        client_emfile.append(f"{kind}: {ex}"[:200])
                time.sleep(0.5)

    ts = [threading.Thread(target=loop, args=(k,))
          for k in ("send", "send", "read", "read", "search", "health")]
    for t in ts:
        t.start()
    fds_max = 0
    zombies_max = 0
    samples: list = []  # (t, drained_total, depth) or (t, None, error)
    lag: dict = {}  # t -> (drain_stalled, critical_threshold_ms, archive_db_parity status)
    while time.time() < stop:
        time.sleep(SOAK_WINDOW_SECS)
        try:
            fds_max = max(fds_max, arm.sqlite_fds())
            h = arm.health()
            wbq = h["queues"]["wbq"]
            zombies_max = max(zombies_max,
                              h["timeout_diagnostics"]["blocking_dispatch_zombies"])
            t = time.time()
            samples.append((t, wbq["drained_total"], wbq["depth"]))
            # An older control binary may lack these fields; it then simply
            # cannot pass the agreement check, without disturbing the others.
            al = h["queues"].get("archive_lag", {})
            if "drain_stalled" in al and "critical_threshold_ms" in al:
                lag[t] = (al["drain_stalled"], al["critical_threshold_ms"],
                          h.get("verdicts", {}).get("archive_db_parity", {}).get("status"))
        except Exception as ex:
            samples.append((time.time(), None, f"{type(ex).__name__}: {ex}"[:200]))
    for t in ts:
        t.join()
    threads_end = arm.threads()
    # A window is judged from consecutive samples: work already queued when the
    # window opened must see drained_total advance before it closes. An
    # unobservable window cannot pass.
    bad_windows = []
    for prev, cur in zip(samples, samples[1:]):
        if prev[1] is None or cur[1] is None:
            bad_windows.append({"t": round(cur[0]), "unobserved": cur[2] if cur[1] is None
                                else prev[2]})
        elif prev[2] > 0 and cur[1] <= prev[1]:
            bad_windows.append({"t": round(cur[0]), "drained_total": cur[1],
                                "depth_at_open": prev[2], "depth_at_close": cur[2]})
    # br-kp1in.23: health's stall verdict must agree with the WBQ counters in
    # both directions. Progress inside a window shorter than the critical bound
    # means "not stalled"; queued work with no progress for longer than the
    # bound (plus one window of sampling slack) means "stalled" and red parity.
    lag_disagree = []
    last_progress_t = samples[0][0] if samples else 0.0
    for prev, cur in zip(samples, samples[1:]):
        if prev[1] is None or cur[1] is None or cur[0] not in lag:
            continue
        stalled, critical_ms, parity = lag[cur[0]]
        window_s = cur[0] - prev[0]
        if cur[1] > prev[1]:
            last_progress_t = cur[0]
            if stalled and window_s * 1000 < critical_ms:
                lag_disagree.append({"t": round(cur[0]), "want": "not stalled",
                                     "drained": [prev[1], cur[1]]})
        elif cur[2] > 0 and (cur[0] - last_progress_t) * 1000 > critical_ms + window_s * 1000:
            if not stalled or parity != "red":
                lag_disagree.append({"t": round(cur[0]), "want": "stalled+red parity",
                                     "got": [stalled, parity], "depth": cur[2]})
    q = sorted(read_lat)
    read_p99 = q[min(len(q) - 1, int(len(q) * 0.99))] if q else None
    log_text = arm.server_log.read_text(errors="replace")
    restarts = [m for m in HTTP_RESTART_MARKERS if m in log_text]
    check(c, f"storage.sqlite3 descriptors bounded (<= {FD_BOUND})", fds_max <= FD_BOUND,
          f"max={fds_max}")
    check(c, "no EMFILE (server log and client responses)",
          not has_emfile(log_text) and not client_emfile, client_emfile[:3])
    check(c, f"WBQ drained_total advances in every {SOAK_WINDOW_SECS}s window with queued work",
          len(samples) >= 2 and not bad_windows,
          f"samples={len(samples)} bad={bad_windows[:5]}")
    check(c, "health archive_lag stall verdict agrees with the WBQ counters",
          len(lag) >= 2 and not lag_disagree,
          f"lag_samples={len(lag)} disagree={lag_disagree[:5]}")
    check(c, f"fetch_inbox p99 <= {FETCH_INBOX_P99_BUDGET_S}s under write load",
          read_p99 is not None and read_p99 <= FETCH_INBOX_P99_BUDGET_S,
          f"p99={read_p99} n={len(q)}")
    check(c, "no dispatch zombies", zombies_max == 0, f"max={zombies_max}")
    check(c, "zero HTTP server restarts", arm.alive() and not restarts,
          {"alive": arm.alive(), "markers": restarts})
    check(c, "no archive re-root", "re-root" not in log_text)
    ok, detail = arm.wait_converged(CONVERGE_SECS, 5)
    check(c, "archive converges after load stops", ok, detail)
    extra: dict = {"counts": counts, "sqlite_fds_max": fds_max, "fetch_inbox_p99_s": read_p99,
                   "fetch_inbox_samples": len(q), "wbq_samples": len(samples),
                   "server_threads_end": threads_end}
    if SOAK_SECS < RELEASE_MIN_SOAK_SECS:
        extra["cannot_pass"] = f"soak {SOAK_SECS}s is shorter than the {RELEASE_MIN_SOAK_SECS}s minimum"
    if CONVERGE_SECS > RELEASE_CONVERGE_SECS:
        extra["cannot_pass"] = f"convergence bound {CONVERGE_SECS}s is looser than release"
    return extra


PHASES = (("flow", phase_flow), ("cross_project", phase_cross_project),
          ("storm", phase_storm), ("crash", phase_crash), ("soak", phase_soak))


def run_arm(name: str, binary: Path, out: Path) -> dict:
    # A fresh directory per run: never delete earlier evidence.
    root = out / f"{name}-{time.strftime('%Y%m%dT%H%M%S')}-{os.getpid()}"
    arm = Arm(name, binary, root)
    result: dict = {"arm": name, "binary": str(binary), "binary_sha256": sha256(binary),
                    "phases": {}}
    try:
        version = subprocess.run([str(binary), "--version"], capture_output=True, text=True,
                                 timeout=60, env=arm.env())
        result["binary_version"] = (version.stdout or version.stderr).strip()[:200]
        arm.start()
        result["server_exe_sha256"] = sha256(Path(f"/proc/{arm.pid()}/exe"))
        soft, hard = open(f"/proc/{arm.pid()}/limits").read().split("Max open files")[1].split()[:2]
        result["server_nofile_soft_hard"] = [soft, hard]
        result["server_threads_start"] = arm.threads()
        state: dict = {}
        for phase_name, phase in PHASES:
            log(f"{name}: phase {phase_name}")
            c: list = []
            extra: dict = {}
            try:
                extra = phase(arm, state, c) or {}
                v = verdict(c)
            except Exception as ex:
                extra = {"error": f"{type(ex).__name__}: {ex}"[:600]}
                v = "FAIL" if any(not x["ok"] for x in c) else "NO_VERDICT"
                log(f"{name}: phase {phase_name} raised {extra['error'][:200]}")
            if v == "PASS" and extra.get("cannot_pass"):
                v = "NO_VERDICT"
            result["phases"][phase_name] = {"verdict": v, "checks": c, **extra}
            if not (state.get("A") and state.get("B")):
                break  # without the flow's agents no later phase can run
            if not arm.alive():
                arm.start()  # a crashed server still gets judged by the next phase
    except Exception as ex:
        result["error"] = f"{type(ex).__name__}: {ex}"
    finally:
        result["clean_shutdown"] = arm.kill(signal.SIGTERM)
        if not result["clean_shutdown"]:
            log(f"{name}: server ignored SIGTERM for 60 s; SIGKILLed")
    try:
        result["server_log_walfec_lines"] = sum(
            1 for line in arm.server_log.read_text(errors="replace").splitlines()
            if "WAL-FEC" in line)
    except OSError:
        pass
    verdicts = [p.get("verdict") for p in result["phases"].values()]
    result["verdict"] = ("PASS" if len(verdicts) == len(PHASES)
                         and all(v == "PASS" for v in verdicts)
                         and result["clean_shutdown"] else "FAIL")
    return result


def main() -> int:
    ap = argparse.ArgumentParser(description=__doc__.split("\n")[0])
    ap.add_argument("--bin", required=True, type=Path, help="candidate `am` binary")
    ap.add_argument("--control-bin", type=Path, help="control binary (e.g. previous release)")
    ap.add_argument("--out", required=True, type=Path, help="output directory for the receipt")
    args = ap.parse_args()
    args.out.mkdir(parents=True, exist_ok=True)
    receipt = {"schema_version": 2,
               "started_at": time.strftime("%Y-%m-%dT%H:%M:%SZ", time.gmtime()),
               "host": socket.gethostname(), "soak_secs": SOAK_SECS,
               "converge_secs": CONVERGE_SECS,
               "fetch_inbox_p99_budget_s": FETCH_INBOX_P99_BUDGET_S, "arms": []}
    receipt["arms"].append(run_arm("candidate", args.bin.resolve(), args.out))
    if args.control_bin:
        receipt["arms"].append(run_arm("control", args.control_bin.resolve(), args.out))
    receipt["finished_at"] = time.strftime("%Y-%m-%dT%H:%M:%SZ", time.gmtime())
    receipt["candidate_verdict"] = receipt["arms"][0]["verdict"]
    (args.out / "release_smoke_receipt.json").write_text(json.dumps(receipt, indent=2))
    log(f"receipt: {args.out / 'release_smoke_receipt.json'}")
    for arm in receipt["arms"]:
        log(f"{arm['arm']}: {arm['verdict']} "
            + " ".join(f"{k}={v.get('verdict')}" for k, v in arm["phases"].items()))
    return 0 if receipt["candidate_verdict"] == "PASS" else 1


if __name__ == "__main__":
    sys.exit(main())
