#!/usr/bin/env python3
"""Offline agent-loop smoke: skills, logs, versioned edits/patches, verify, resume.
Usage: python3 scripts/smoke.py target/debug/makedo [--openai] [--output-limit]
All model requests hit this process's local SSE fixture; no paid model is used.
"""
import http.server
import json
import os
import sqlite3
from pathlib import Path
import subprocess
import sys
import tempfile
import threading

binary = Path(sys.argv[1] if len(sys.argv) > 1 else "target/debug/makedo").resolve()
openai = "--openai" in sys.argv[2:]
output_limit = "--output-limit" in sys.argv[2:]
assert not output_limit or openai, "--output-limit requires --openai"
provider = "openai" if openai else "openai-compat"
model = "gpt-6-sol" if openai else "smoke-model"
token_param = "max_output_tokens" if openai else "max_tokens"
other_token_param = "max_tokens" if openai else "max_output_tokens"
root = Path(tempfile.mkdtemp(prefix="makedo smoke "))
project = root / "project"
project.mkdir()
home = root / "settings"
(home / "cache").mkdir(parents=True)
(home / "cache/models.json").write_text("{}")  # Fresh cache suppresses background catalog refresh.
state = {"step": 0, "errors": [], "logs": [], "output": [], "recoveries": 0}


def result(request, call_id):
    return next(m["content"] for m in reversed(request["messages"]) if m.get("tool_call_id") == call_id)


def log_path(text):
    path, _ = json.JSONDecoder().raw_decode(text.split(" | log=", 1)[1])
    return Path(path)


class Model(http.server.BaseHTTPRequestHandler):
    def log_message(self, *_):
        pass

    def do_POST(self):
        try:
            request = json.loads(self.rfile.read(int(self.headers["Content-Length"])))
            assert request["model"] == model
            assert request.get(token_param, 0) > 0 and other_token_param not in request
            assert "max_completion_tokens" not in request
            assert self.path == ("/v1/responses" if openai else "/v1/chat/completions")
            if openai:
                assert request["store"] is False
                assert "reasoning.encrypted_content" in request["include"]
                assert all(tool["strict"] is False for tool in request["tools"])
                # Exact replay across tool rounds and --resume, with no duplicate text/calls.
                assert [item for item in request["input"] if "id" in item] == state["output"]
                messages = [{"role": "system", "content": request["instructions"]}]
                for item in request["input"]:
                    if item.get("type") == "function_call_output":
                        messages.append({"role": "tool", "tool_call_id": item["call_id"], "content": item["output"]})
                    elif item.get("role") == "user":
                        messages.append(item)
                request["messages"] = messages
                request["tools"] = [{"function": tool} for tool in request["tools"]]
            step = state["step"]
            state["step"] += 1
            names = {t["function"]["name"] for t in request["tools"]}
            assert {"skill_search", "skill_load", "bash", "read", "edit", "apply_patch"} <= names
            if step == 0:
                assert "# Open Code Review" not in request["messages"][0]["content"]
                call = ("skill", "skill_load", {"name": "open-code-review"})
            elif step == 1:
                assert "--audience agent" in result(request, "skill")
                call = ("large", "bash", {"command": "python3 -c 'import sys; sys.stdout.write(\"x\" * 6000001 + \"END\")'"})
            elif step == 2:
                output = result(request, "large")
                assert output.startswith("OK |") and len(output) < 1000
                path = log_path(output)
                assert path.stat().st_size == 6000004
                with path.open("rb") as file:
                    file.seek(-3, 2)
                    assert file.read() == b"END"
                state["logs"].append(path)
                call = ("failed", "bash", {"command": "printf failure-marker >&2; exit 7"})
            elif step == 3:
                output = result(request, "failed")
                assert "FAIL | exit=7" in output and "failure-marker" not in output
                path = log_path(output)
                state["logs"].append(path)
                call = ("diagnose", "grep", {"pattern": "failure-marker", "path": str(path)})
            elif step == 4:
                assert "failure-marker" in result(request, "diagnose")
                if output_limit:
                    assert request[token_param] == 8192 * 2 ** state["recoveries"]
                    if state["recoveries"] == 2:
                        for ident in ("unexecuted-complete", "unexecuted-partial"):
                            assert "not executed" in result(request, ident)
                call = ("edit", "write", {"file_path": "artifact.txt", "content": "ok\n"})
            elif step == 5:
                assert "Wrote" in result(request, "edit")
                call = ("read-file", "read", {"file_path": "artifact.txt", "limit": 1})
            elif step == 6:
                state["version"] = result(request, "read-file").splitlines()[0].removeprefix("version=")
                assert state["version"].startswith("sha256:")
                call = ("versioned-edit", "edit", {"file_path": "artifact.txt", "old_string": "ok", "new_string": "edited", "expected_version": state["version"]})
            elif step == 7:
                assert "Replaced 1" in result(request, "versioned-edit")
                call = ("blind-write", "write", {"file_path": "artifact.txt", "content": "bad"})
            elif step == 8:
                assert "version conflict" in result(request, "blind-write")
                state["patch"] = "*** Begin Patch\n*** Update File: artifact.txt\n@@\n-edited\n+patched\n*** Add File: extra.txt\n+new\n*** End Patch"
                call = ("stale-patch", "apply_patch", {"patch": state["patch"], "expected_versions": {"artifact.txt": state["version"]}})
            elif step == 9:
                assert "version conflict" in result(request, "stale-patch")
                assert (project / "artifact.txt").read_text() == "edited\n"
                assert not (project / "extra.txt").exists()
                call = ("refresh-file", "read", {"file_path": "artifact.txt"})
            elif step == 10:
                version = result(request, "refresh-file").splitlines()[0].removeprefix("version=")
                call = ("patch", "apply_patch", {"patch": state["patch"], "expected_versions": {"artifact.txt": version}})
            elif step == 11:
                output = result(request, "patch")
                assert "Applied patch to 2 file(s)" in output
                version = next(line.split("version=", 1)[1] for line in output.splitlines() if line.startswith("extra.txt:"))
                assert (project / "extra.txt").read_text() == "new\n"
                call = ("delete-file", "apply_patch", {"patch": "*** Begin Patch\n*** Delete File: extra.txt\n*** End Patch", "expected_versions": {"extra.txt": version}})
            elif step == 12:
                assert "extra.txt: version=missing" in result(request, "delete-file")
                call = None
            elif step == 13:
                assert any(m.get("content") == "smoke task" for m in request["messages"])
                assert any(m.get("content") == "resume check" for m in request["messages"])
                assert "FAIL | exit=7" in result(request, "failed")
                call = None
            else:
                raise AssertionError(f"unexpected request {step}")
            if call:
                ident, name, args = call
                delta = {"tool_calls": [{"index": 0, "id": ident, "type": "function", "function": {"name": name, "arguments": json.dumps(args)}}]}
                reason = "tool_calls"
            else:
                delta = {"content": "resumed" if step == 13 else "complete"}
                reason = "stop"
            chunks = [
                {"choices": [{"index": 0, "delta": delta}]},
                {"choices": [{"index": 0, "delta": {}, "finish_reason": reason}], "usage": {"prompt_tokens": 120, "completion_tokens": 5}},
            ]
            if openai:
                output = [{"type": "reasoning", "id": f"rs_{step}", "summary": [], "encrypted_content": f"opaque-{step}"}]
                if call:
                    output.append({"type": "function_call", "id": f"fc_{step}", "call_id": ident, "name": name, "arguments": json.dumps(args), "status": "completed"})
                    chunks = [
                        {"type": "response.output_item.added", "output_index": 1, "item": {**output[-1], "arguments": "", "status": "in_progress"}},
                        {"type": "response.function_call_arguments.delta", "output_index": 1, "delta": json.dumps(args)},
                    ]
                else:
                    output.append({"type": "message", "id": f"msg_{step}", "role": "assistant", "phase": "final_answer", "status": "completed", "content": [{"type": "output_text", "text": delta["content"], "annotations": []}]})
                    chunks = [{"type": "response.output_text.delta", "delta": delta["content"]}]
                terminal = "response.completed"
                details = None
                if output_limit and step == 4 and state["recoveries"] < 2:
                    # First exhaust during reasoning, then mid-call after a complete
                    # call in the same batch. Neither write may execute.
                    attempt = state["recoveries"]
                    output = [{"type": "reasoning", "id": f"rs_limit_{attempt}", "summary": [], "encrypted_content": f"opaque-limit-{attempt}"}]
                    if attempt == 1:
                        output.extend([
                            {"type": "function_call", "id": "fc_limit_complete", "call_id": "unexecuted-complete", "name": "write", "arguments": json.dumps({"file_path": "must-not-exist.txt", "content": "bad"}), "status": "completed"},
                            {"type": "function_call", "id": "fc_limit_partial", "call_id": "unexecuted-partial", "name": "write", "arguments": '{"file_path":"also-must-not-exist.txt","content":"unfinished', "status": "incomplete"},
                        ])
                    chunks = []
                    terminal = "response.incomplete"
                    details = {"reason": "max_output_tokens"}
                    state["recoveries"] += 1
                    state["step"] -= 1
                state["output"].extend(output)
                chunks.append({"type": terminal, "response": {"output": output, "incomplete_details": details, "usage": {"input_tokens": 120, "output_tokens": request[token_param] if details else 5, "input_tokens_details": {"cached_tokens": 100}}}})
            payload = "".join("data: " + json.dumps(chunk) + "\n\n" for chunk in chunks)
            if not openai:
                payload += "data: [DONE]\n\n"
            encoded = payload.encode()
            self.send_response(200)
            self.send_header("Content-Type", "text/event-stream")
            self.send_header("Content-Length", str(len(encoded)))
            self.end_headers()
            self.wfile.write(encoded)
        except Exception as error:
            state["errors"].append(repr(error))
            self.send_error(500, str(error))


server = http.server.ThreadingHTTPServer(("127.0.0.1", 0), Model)
threading.Thread(target=server.serve_forever, daemon=True).start()
try:
    with (root / "setup.log").open("wb") as log:
        subprocess.run(["git", "init", "-b", "main"], cwd=project, stdout=log, stderr=log, check=True)
    (project / ".makedo").mkdir()
    (project / ".makedo/config.toml").write_text(f'''[provider]
kind = "{provider}"
model = "{model}"
base_url = "http://127.0.0.1:{server.server_port}/v1"
api_key_env = "MAKEDO_SMOKE_KEY"
[verify]
cmds = ["test -f artifact.txt && printf verify-output"]
''')
    env = {**os.environ, "MAKEDO_HOME": str(home), "MAKEDO_SMOKE_KEY": "local-fixture"}
    for number, args in enumerate([["-p", "smoke task", "--yolo"], ["-p", "resume check", "--resume", "latest", "--yolo"]], 1):
        db = project / ".git/makedo/memory.sqlite"
        before = sqlite3.connect(db).execute("SELECT id,payload FROM events ORDER BY id").fetchall() if db.exists() else []
        with (root / f"run-{number}.out").open("wb") as stdout, (root / f"run-{number}.log").open("wb") as stderr:
            completed = subprocess.run([str(binary), *args], cwd=project, env=env, stdout=stdout, stderr=stderr, timeout=40)
        assert completed.returncode == 0, f"exit={completed.returncode}; see run-{number}.log; errors={state['errors']}"
        assert not state["errors"], state["errors"]
        with sqlite3.connect(db) as connection:
            after = dict(connection.execute("SELECT id,payload FROM events"))
            assert all(after[ident] == data for ident, data in before), "resume overwrote the original session"
    assert state["step"] == 14
    assert (project / "artifact.txt").read_text() == "patched\n"
    assert not (project / "extra.txt").exists()
    assert not (project / "must-not-exist.txt").exists()
    assert not (project / "also-must-not-exist.txt").exists()
    with sqlite3.connect(db) as connection:
        assert connection.execute("SELECT count(*) FROM sessions WHERE agent='main'").fetchone()[0] == 2
    stderr = (root / "run-1.log").read_text()
    assert "[log]" in stderr and "verify:" in stderr
    assert "failure-marker" not in stderr and "verify-output" not in (root / "run-1.out").read_text()
    assert (root / "run-2.out").read_text().strip() == "resumed"
    if output_limit:
        assert state["recoveries"] == 2
        assert "out=24641" in stderr, "usage must include both truncated responses"
    print(f"OK: {provider} request contract, skill, 6 MB log, grep diagnosis, versioned edit/patch/conflict, verify, resume; output-limit recovery={output_limit}. Artifacts: {root}")
except Exception:
    print(f"FAIL: artifacts={root}, fixture_errors={state['errors']}", file=sys.stderr)
    raise
finally:
    server.shutdown()
