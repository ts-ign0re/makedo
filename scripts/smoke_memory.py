#!/usr/bin/env python3
"""Memory integration: native tools, MCP, child journals and ranked semantic retrieval.

Offline by default. --live URL checks real multilingual embeddings at a local
Ollama /api/embed endpoint; all chat generation still uses the local fixture.
"""
import argparse
import http.server
import json
import os
from pathlib import Path
import sqlite3
import subprocess
import tempfile
import threading

parser = argparse.ArgumentParser()
parser.add_argument("binary")
parser.add_argument("--live")
args = parser.parse_args()
binary = str(Path(args.binary).resolve())
root = Path(tempfile.mkdtemp(prefix="makedo-memory-smoke-"))
home = root / "settings"
(home / "cache").mkdir(parents=True)
(home / "cache/models.json").write_text("{}")
project = root / "project"
project.mkdir()
errors = []
embeddings = []


class Fixture(http.server.BaseHTTPRequestHandler):
    def log_message(self, *_):
        pass

    def do_POST(self):
        try:
            body = json.loads(self.rfile.read(int(self.headers["Content-Length"])))
            if self.path == "/api/embed":
                vectors = []
                for text in body["input"]:
                    embeddings.append(text)
                    value = text.lower()
                    if any(word in value for word in ["restore", "recovery", "перезапуска", "диалог"]):
                        vectors.append([1.0, 0.0, 0.0])
                    elif "wal" in value or "concurrent" in value:
                        vectors.append([0.0, 1.0, 0.0])
                    else:
                        vectors.append([0.0, 0.0, 1.0])
                payload = json.dumps({"embeddings": vectors}).encode()
                content_type = "application/json"
            else:
                assert self.path == "/v1/chat/completions", self.path
                names = {t["function"]["name"] for t in body["tools"]}
                assert {"memory_search", "memory_read", "memory_write"} <= names
                messages = body["messages"]
                child = any(m.get("role") == "user" and m.get("content") == "child write memory" for m in messages)
                has_result = any(m.get("role") == "tool" for m in messages)
                if has_result:
                    delta, finish = {"content": "child complete" if child else "parent complete"}, "stop"
                else:
                    name = "memory_write" if child else "task"
                    arguments = ({"title": "Child finding", "text": "The child investigated the code and saved its finding.", "sources": ["code.txt"]}
                                 if child else {"kind": "research", "name": "memory-test", "prompt": "child write memory"})
                    delta = {"tool_calls": [{"index": 0, "id": "child-save" if child else "spawn-child", "type": "function", "function": {"name": name, "arguments": json.dumps(arguments)}}]}
                    finish = "tool_calls"
                chunks = [{"choices": [{"index": 0, "delta": delta}]},
                          {"choices": [{"index": 0, "delta": {}, "finish_reason": finish}], "usage": {"prompt_tokens": 50, "completion_tokens": 10}}]
                payload = ("".join("data: " + json.dumps(c) + "\n\n" for c in chunks) + "data: [DONE]\n\n").encode()
                content_type = "text/event-stream"
            self.send_response(200)
            self.send_header("Content-Type", content_type)
            self.send_header("Content-Length", str(len(payload)))
            self.end_headers()
            self.wfile.write(payload)
        except Exception as error:
            errors.append(str(error))
            self.send_error(500)


server = http.server.ThreadingHTTPServer(("127.0.0.1", 0), Fixture)
threading.Thread(target=server.serve_forever, daemon=True).start()
env = {**os.environ, "MAKEDO_HOME": str(home), "MAKEDO_MEMORY_FIXTURE_KEY": "fixture"}


def run(*arguments, cwd=project):
    result = subprocess.run([binary, *arguments], cwd=cwd, env=env, capture_output=True, text=True, timeout=120)
    with (root / "commands.log").open("a") as log:
        log.write(f"args={arguments}\nexit={result.returncode}\n{result.stderr}\n")
    assert result.returncode == 0, (result.returncode, result.stderr)
    return json.loads(result.stdout)


try:
    with (root / "git.log").open("w") as log:
        subprocess.run(["git", "init", "-b", "main"], cwd=project, check=True, stdout=log, stderr=log)
    (project / ".makedo").mkdir()
    endpoint = args.live or f"http://127.0.0.1:{server.server_port}/api/embed"
    model = "embeddinggemma" if args.live else "fixture-embeddings"
    (project / ".makedo/config.toml").write_text(f'''[provider]
kind = "openai-compat"
model = "memory-fixture"
base_url = "http://127.0.0.1:{server.server_port}/v1"
api_key_env = "MAKEDO_MEMORY_FIXTURE_KEY"
[memory]
embedding_url = "{endpoint}"
embedding_model = "{model}"
''')
    notes = [
        ("Recovery", "Persist conversation history and pending messages in SQLite, then restore the previous session when restarting the agent."),
        ("Concurrency", "Use WAL mode to allow concurrent database readers while serializing short writes."),
        ("Colors", "Generate a dark color palette for the website header."),
    ]
    saved = [run("memory", "write", json.dumps({"title": title, "text": text, "sources": ["code.txt"]})) for title, text in notes]
    question = "После перезапуска агент забывает предыдущий диалог. Как восстановить историю?"
    hits = run("memory", "search", question)
    assert hits["mode"] == "hybrid" and hits["notice"] is None, hits
    assert hits["results"][0]["id"] == saved[0]["id"], hits
    assert len(hits["results"]) == 3
    previous_calls = len(embeddings)
    assert run("memory", "search", question)["results"][0]["id"] == saved[0]["id"]
    if not args.live:
        assert len(embeddings) == previous_calls + 1, "cached document vectors were recomputed"
    status = run("memory", "status")
    (project / "nested").mkdir()
    assert run("memory", "status", cwd=project / "nested")["path"] == status["path"]
    assert run("memory", "read", str(saved[0]["id"]))["entry"]["text"] == notes[0][1]

    with (root / "mcp.log").open("w") as stderr:
        process = subprocess.Popen([binary, "mcp-serve"], cwd=project, env=env, stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=stderr, text=True)
        requests = [
            {"jsonrpc": "2.0", "id": 1, "method": "tools/list"},
            {"jsonrpc": "2.0", "id": 2, "method": "tools/call", "params": {"name": "memory_search", "arguments": {"task": question, "kind": "note"}}},
        ]
        output, _ = process.communicate("".join(json.dumps(r) + "\n" for r in requests), timeout=120)
        assert process.returncode == 0
        replies = [json.loads(line) for line in output.splitlines()]
        assert {"memory_search", "memory_read", "memory_write", "wiki_trace"} <= {t["name"] for t in replies[0]["result"]["tools"]}
        result = json.loads(replies[1]["result"]["content"][0]["text"])
        assert result["results"][0]["id"] == saved[0]["id"]

    result = subprocess.run([binary, "-p", "delegate a memory task", "--yolo"], cwd=project, env=env, capture_output=True, text=True, timeout=120)
    (root / "native.log").write_text(result.stderr)
    assert result.returncode == 0 and "parent complete" in result.stdout, result.stderr
    with sqlite3.connect(status["path"]) as connection:
        for agent in ("main", "subagent"):
            assert connection.execute("SELECT count(*) FROM events JOIN sessions ON sessions.id=events.session WHERE agent=?", (agent,)).fetchone()[0] >= 3, agent
        assert connection.execute("SELECT count(*) FROM entries WHERE title='Child finding'").fetchone()[0] == 1
        assert connection.execute("PRAGMA journal_mode").fetchone()[0] == "wal"
    assert not errors, errors
    # Publish one selected note through Git; a teammate's clone gets knowledge, not the database.
    exported = subprocess.run([binary, "memory", "export", str(saved[0]["id"])], cwd=project, env=env, capture_output=True, text=True, check=True)
    published = Path(exported.stdout.strip())
    assert published.name == f"memory-{saved[0]['uid']}.md"
    teammate = root / "teammate"
    with (root / "git.log").open("a") as log:
        subprocess.run(["git", "add", str(published)], cwd=project, stdout=log, stderr=log, check=True)
        subprocess.run(["git", "-c", "user.name=Memory Test", "-c", "user.email=test@example.invalid", "commit", "-m", "Share recovery knowledge"], cwd=project, stdout=log, stderr=log, check=True)
        subprocess.run(["git", "clone", "--local", str(project), str(teammate)], stdout=log, stderr=log, check=True)
    assert not (teammate / ".git/makedo/memory.sqlite").exists()
    (teammate / ".makedo/config.toml").write_text((project / ".makedo/config.toml").read_text())
    shared = run("memory", "search", question, cwd=teammate)
    assert shared["results"] and "Persist conversation history" in shared["results"][0]["summary"], shared
    assert run("memory", "status", cwd=teammate)["path"] != status["path"]
    print(f"OK: {'live multilingual' if args.live else 'offline'} ranking, cached vectors, MCP, child journals and Git sharing between clones. Artifacts: {root}")
finally:
    server.shutdown()
