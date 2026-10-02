#!/usr/bin/env python3
"""Hermetic fake MCP server for pantheon-mcp tests.

Speaks JSON-RPC 2.0 over stdio (newline-delimited). No network, no
dependencies beyond the stdlib.

Usage: fake_mcp_server.py <mode> [method-log-path]

Modes:
  normal   - answers initialize/tools/list/tools/call correctly
  hang     - never answers tools/call (tests the client's timeout kill)
  oversize - answers tools/list with an 11MiB Content-Length-framed body
  garbage  - answers tools/list with a non-JSON line
  badversion- answers initialize with an unknown protocol version
"""

import json
import sys
import time

MODE = sys.argv[1] if len(sys.argv) > 1 else "normal"
LOG = sys.argv[2] if len(sys.argv) > 2 else None


def record(method: str) -> None:
    if LOG:
        with open(LOG, "a", encoding="utf-8") as f:
            f.write(method + "\n")


def send(obj) -> None:
    sys.stdout.write(json.dumps(obj) + "\n")
    sys.stdout.flush()


def send_framed(obj) -> None:
    body = json.dumps(obj)
    sys.stdout.write(f"Content-Length: {len(body)}\r\n\r\n{body}")
    sys.stdout.flush()


def read_msg():
    line = sys.stdin.readline()
    if not line:
        return None
    if line.startswith("Content-Length:"):
        length = int(line.split(":", 1)[1])
        sys.stdin.readline()  # blank line
        return json.loads(sys.stdin.read(length))
    return json.loads(line)


def respond(req_id, result) -> None:
    send({"jsonrpc": "2.0", "id": req_id, "result": result})


TOOLS = [
    {
        "name": "echo",
        "description": "Echoes its arguments back",
        "inputSchema": {"type": "object"},
    },
    {
        "name": "fail",
        "description": "Always reports a tool error",
        "inputSchema": {"type": "object"},
    },
]


def main() -> None:
    while True:
        try:
            msg = read_msg()
        except Exception:
            return
        if msg is None:
            return
        method = msg.get("method")
        if method is None:
            continue
        record(method)
        if method == "initialize":
            version = "1999-01-01" if MODE == "badversion" else "2025-03-26"
            respond(
                msg.get("id"),
                {
                    "protocolVersion": version,
                    "capabilities": {},
                    "serverInfo": {"name": "fake", "version": "0"},
                },
            )
        elif method == "notifications/initialized":
            pass  # notification: no reply
        elif method == "tools/list":
            if MODE == "oversize":
                big = "x" * (11 * 1024 * 1024)
                send_framed(
                    {
                        "jsonrpc": "2.0",
                        "id": msg.get("id"),
                        "result": {
                            "tools": [
                                {
                                    "name": "big",
                                    "description": big,
                                    "inputSchema": {},
                                }
                            ]
                        },
                    }
                )
            elif MODE == "garbage":
                sys.stdout.write("this is not json\n")
                sys.stdout.flush()
            else:
                respond(msg.get("id"), {"tools": TOOLS})
        elif method == "tools/call":
            if MODE == "hang":
                time.sleep(3600)
                return
            name = msg["params"]["name"]
            args = msg["params"].get("arguments", {})
            if name == "fail":
                respond(
                    msg.get("id"),
                    {
                        "content": [{"type": "text", "text": "boom"}],
                        "isError": True,
                    },
                )
            else:
                respond(
                    msg.get("id"),
                    {"content": [{"type": "text", "text": json.dumps(args)}]},
                )
        else:
            send(
                {
                    "jsonrpc": "2.0",
                    "id": msg.get("id"),
                    "error": {"code": -32601, "message": "method not found"},
                }
            )


if __name__ == "__main__":
    main()
