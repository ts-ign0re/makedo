#!/usr/bin/env python3
"""Минимальный MCP stdio-сервер для интеграционных тестов: initialize,
tools/list (один тул echo), tools/call. Без зависимостей."""
import json
import sys
from pathlib import Path

pending = {}

TOOLS = [{
    "name": "echo",
    "description": "Echo the input text back",
    "inputSchema": {
        "type": "object",
        "properties": {"text": {"type": "string"}},
        "required": ["text"],
    },
}]


def reply(rid, result):
    sys.stdout.write(json.dumps({"jsonrpc": "2.0", "id": rid, "result": result}) + "\n")
    sys.stdout.flush()


if "--http" in sys.argv:
    import http.server

    class Handler(http.server.BaseHTTPRequestHandler):
        def log_message(self, *_):
            pass

        def do_GET(self):
            self.send_response(405)
            self.send_header("Content-Length", "0")
            self.end_headers()

        def do_POST(self):
            if self.headers.get("Authorization") != "Bearer fixture-token":
                self.send_response(401)
                self.send_header("Content-Length", "0")
                self.end_headers()
                return
            msg = json.loads(self.rfile.read(int(self.headers["Content-Length"])))
            method = msg.get("method")
            if "id" not in msg:
                self.send_response(202)
                self.send_header("Content-Length", "0")
                self.end_headers()
                return
            if method == "initialize":
                result = {"protocolVersion": msg["params"]["protocolVersion"], "capabilities": {"tools": {}}, "serverInfo": {"name": "http-fixture", "version": "1"}}
            elif method == "tools/list":
                result = {"tools": TOOLS}
            else:
                result = {"content": [{"type": "text", "text": "authenticated"}]}
            body = json.dumps({"jsonrpc": "2.0", "id": msg["id"], "result": result}).encode()
            self.send_response(200)
            self.send_header("Content-Type", "application/json")
            self.send_header("Content-Length", str(len(body)))
            self.end_headers()
            self.wfile.write(body)

    server = http.server.ThreadingHTTPServer(("127.0.0.1", 0), Handler)
    print(server.server_port, flush=True)
    server.serve_forever()
    sys.exit(0)


for line in sys.stdin:
    line = line.strip()
    if not line:
        continue
    msg = json.loads(line)
    method = msg.get("method")
    rid = msg.get("id")
    if method == "initialize":
        reply(rid, {
            "protocolVersion": msg["params"]["protocolVersion"],
            "capabilities": {"tools": {}},
            "serverInfo": {"name": "echo-fixture", "version": "0.0.1"},
        })
    elif method == "tools/list":
        reply(rid, {"tools": TOOLS})
    elif method == "notifications/cancelled":
        target = pending.pop(msg.get("params", {}).get("requestId"), None)
        if target:
            Path(target).write_text("cancelled")
    elif method == "tools/call":
        args = msg["params"].get("arguments", {})
        if args.get("wait_for_cancel"):
            pending[rid] = args["wait_for_cancel"]
            continue
        text = msg["params"].get("arguments", {}).get("text", "")
        reply(rid, {"content": [{"type": "text", "text": f"echo: {text}"}]})
    elif rid is not None:
        sys.stdout.write(json.dumps({
            "jsonrpc": "2.0", "id": rid,
            "error": {"code": -32601, "message": f"unknown method {method}"},
        }) + "\n")
        sys.stdout.flush()
