#!/usr/bin/env python3
"""Black-box reproduction/regression probe for Agent Mail GH332.

Run against a disposable, local, no-auth server built from the revision under
investigation (this script does not start, stop, or reconfigure that server):

    am serve-http --no-tui --host 127.0.0.1 --port 8765 --path /mcp/ --no-auth
    python3 scripts/repro_http_empty_headers.py --expect-bug
    python3 scripts/repro_http_empty_headers.py  # require the fixed behavior

Uses raw HTTP/1.1 so empty and whitespace-only headers really reach the server;
client-side header validation must not turn a negative server test into a pass.
No Tailscale installation, third-party Python packages, or credentials needed.
A successful run is evidence only for the server actually listening at --url.
"""

from __future__ import annotations

import argparse
import http.client
import json
import math
import socket
import ssl
import sys
from urllib.parse import urlsplit

BODY = json.dumps({
    "jsonrpc": "2.0",
    "id": 1,
    "method": "initialize",
    "params": {
        "protocolVersion": "2025-03-26",
        "capabilities": {},
        "clientInfo": {"name": "gh332-header-probe", "version": "1.0"},
    },
}, separators=(",", ":")).encode("ascii")
MAX_RESPONSE_BYTES = 262144


def request_bytes(host: bytes, path: bytes, additions: list[tuple[bytes, bytes]]) -> bytes:
    """Replace same-named defaults without normalizing the supplied field bytes."""
    defaults = [
        (b"Host", host),
        (b"Content-Type", b"application/json"),
        (b"Accept", b"application/json, text/event-stream"),
        (b"Content-Length", str(len(BODY)).encode("ascii")),
        (b"Connection", b"close"),
    ]
    replaced = {name.lower() for name, _ in additions}
    # Isolate empty Transfer-Encoding itself, not the independently forbidden
    # combination of Content-Length and Transfer-Encoding.
    if b"transfer-encoding" in replaced:
        replaced.add(b"content-length")
    headers = [(name, value) for name, value in defaults if name.lower() not in replaced]
    headers.extend(additions)
    head = b"POST " + path + b" HTTP/1.1\r\n"
    head += b"".join(name + b":" + value + b"\r\n" for name, value in headers)
    return head + b"\r\n" + BODY


def exchange(url: str, timeout: float, additions: list[tuple[bytes, bytes]]) -> tuple[int, bytes]:
    target = urlsplit(url)
    if (target.scheme not in ("http", "https") or not target.hostname
            or target.username is not None or target.password is not None
            or target.query or target.fragment):
        raise ValueError("--url must be an HTTP(S) endpoint without credentials, query, or fragment")
    path = (target.path or "/").encode("ascii")
    host = target.netloc.encode("ascii")
    if any(byte <= 32 or byte == 127 for byte in path + host):
        raise ValueError("--url contains invalid request-target or authority characters")
    port = target.port or (443 if target.scheme == "https" else 80)
    with socket.create_connection((target.hostname, port), timeout=timeout) as connection:
        if target.scheme == "https":
            connection = ssl.create_default_context().wrap_socket(
                connection, server_hostname=target.hostname,
            )
        with connection:
            connection.sendall(request_bytes(host, path, additions))
            with http.client.HTTPResponse(connection) as response:
                response.begin()
                body = response.read(MAX_RESPONSE_BYTES + 1)
                if len(body) > MAX_RESPONSE_BYTES:
                    raise ValueError("response exceeds probe byte limit")
                return response.status, body


def initialized(status: int, body: bytes) -> dict | None:
    try:
        message = json.loads(body)
        if (status == 200 and isinstance(message, dict)
                and message.get("jsonrpc") == "2.0" and message.get("id") == 1
                and "error" not in message and isinstance(message.get("result"), dict)):
            result = message["result"]
            if (isinstance(result.get("serverInfo"), dict)
                    and isinstance(result.get("protocolVersion"), str)):
                return result
    except (ValueError, UnicodeDecodeError):
        pass
    return None


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument("--url", default="http://127.0.0.1:8765/mcp/")
    parser.add_argument("--timeout", type=float, default=5.0)
    parser.add_argument("--expect-bug", action="store_true", help="require baseline success and HTTP 400 for empty extension headers")
    args = parser.parse_args()
    if not math.isfinite(args.timeout) or args.timeout <= 0:
        parser.error("--timeout must be a positive finite number")
    try:
        status, body = exchange(args.url, args.timeout, [])
        baseline = initialized(status, body)
        if baseline is None:
            print(f"FAIL baseline: HTTP {status}, no initialize result; use a disposable no-auth server", file=sys.stderr)
            return 1
        print("PASS baseline:", json.dumps({"serverInfo": baseline["serverInfo"], "protocolVersion": baseline["protocolVersion"]}, sort_keys=True))
        cases = [
            ("populated profile picture", [(b"Tailscale-User-Profile-Pic", b"https://example.invalid/avatar.png")], "success"),
            ("empty profile picture", [(b"Tailscale-User-Profile-Pic", b"")], "extension"),
            ("OWS-only profile picture", [(b"Tailscale-User-Profile-Pic", b" \t ")], "extension"),
            ("mixed-case empty profile picture", [(b"tAiLsCaLe-UsEr-PrOfIlE-pIc", b"")], "extension"),
            ("unrelated empty extension", [(b"X-Optional-Metadata", b"")], "extension"),
            ("unrelated OWS-only extension", [(b"X-Optional-Metadata", b"\t")], "extension"),
        ]
        for name in (b"Authorization", b"Content-Type", b"Content-Length", b"Transfer-Encoding", b"Host", b"Origin"):
            for label, value in (("empty", b""), ("OWS-only", b" \t ")):
                cases.append((f"{label} {name.decode('ascii')}", [(name, value)], "reject"))
        cases.extend([
            ("invalid field name", [(b"Bad Header", b"value")], "reject"),
            ("NUL in extension value", [(b"X-Optional-Metadata", b"a\x00b")], "reject"),
            ("DEL in extension value", [(b"X-Optional-Metadata", b"a\x7fb")], "reject"),
            ("bare CR in extension value", [(b"X-Optional-Metadata", b"a\rb")], "reject"),
        ])
        failures = 0
        for label, headers, expected in cases:
            try:
                status, body = exchange(args.url, args.timeout, headers)
                if expected == "reject":
                    passed = 400 <= status < 500
                elif expected == "extension" and args.expect_bug:
                    passed = status == 400
                else:
                    result = initialized(status, body)
                    passed = result is not None and all(
                        result[key] == baseline[key] for key in ("serverInfo", "protocolVersion")
                    )
                print(f"{'PASS' if passed else 'FAIL'} {label}: HTTP {status}")
            except (OSError, ValueError, http.client.HTTPException) as error:
                # A client exception, timeout, or silent close is not evidence
                # that the server returned the required rejection response.
                passed = False
                print(f"FAIL {label}: {type(error).__name__}")
            failures += not passed
        print(f"{len(cases) - failures}/{len(cases)} cases passed; mode={'reported-bug' if args.expect_bug else 'fixed-regression'}")
        return int(failures != 0)
    except (OSError, ValueError, http.client.HTTPException) as error:
        print(f"FAIL baseline: {type(error).__name__}: {error}", file=sys.stderr)
        return 1


if __name__ == "__main__":
    raise SystemExit(main())
