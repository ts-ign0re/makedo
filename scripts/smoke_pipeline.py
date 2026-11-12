#!/usr/bin/env python3
"""Offline PTY smoke: progress, long input/feed, visible inbox and Ctrl+Enter steering."""
import fcntl
import http.server
import json
import os
from pathlib import Path
import pty
import select
import signal
import struct
import subprocess
import sys
import tempfile
import termios
import threading
import time

binary = Path(sys.argv[1] if len(sys.argv) > 1 else 'target/dist/makedo').resolve()
permission_yolo = '--permission-yolo' in sys.argv[2:]
root = Path(tempfile.mkdtemp(prefix='makedo-pipeline-'))
project = root / 'project'
project.mkdir()
settings = root / 'settings'
(settings / 'cache').mkdir(parents=True)
(settings / 'cache/models.json').write_text('{}')
state = {'parent': 0, 'child': 0, 'errors': []}
release_model = threading.Event()


def call(ident, name, args):
    return {'tool_calls': [{'index': 0, 'id': ident, 'type': 'function', 'function': {'name': name, 'arguments': json.dumps(args)}}]}


class Model(http.server.BaseHTTPRequestHandler):
    def log_message(self, *_):
        pass

    def do_POST(self):
        try:
            request = json.loads(self.rfile.read(int(self.headers['Content-Length'])))
            child = any(m.get('role') == 'user' and m.get('content') == 'inspect fixture' for m in request['messages'])
            lane = 'child' if child else 'parent'
            step = state[lane]
            state[lane] += 1
            if not child:
                if step == 2:
                    prompt = next(m['content'] for m in reversed(request['messages']) if m.get('role') == 'user')
                    assert prompt == state['draft'] + '!', 'soft wrapping changed the submitted draft'
                if step == 3:
                    # A model response that will not finish until the next request:
                    # ordinary Enter must queue visibly; Ctrl+Enter must interrupt it.
                    self.send_response(200)
                    self.send_header('Content-Type', 'text/event-stream')
                    self.end_headers()
                    self.wfile.write(('data: ' + json.dumps({'choices': [{'index': 0, 'delta': {'content': 'MODEL_HELD'}}]}) + '\n\n').encode())
                    self.wfile.flush()
                    release_model.wait(15)
                    return
                if step == 4:
                    prompts = [m['content'] for m in request['messages'] if m.get('role') == 'user']
                    assert prompts[-4:] == ['hold model', 'QUEUED VISIBLE ONE', 'QUEUED VISIBLE TWO', 'FORCED NOW'], prompts
                    assert sum(m.get('content') == 'MODEL_HELD' for m in request['messages']) == 1
                    release_model.set()
                delta = call('review-task', 'task', {'name': 'code-review', 'kind': 'research', 'prompt': 'inspect fixture'}) if step == 0 else {'content': 'pipeline complete'}
            elif step == 0:
                assert any(t['function']['name'] == 'todo_write' for t in request['tools'])
                delta = call('plan', 'todo_write', {'todos': [{'content': 'Анализ', 'status': 'in_progress'}, {'content': 'Отчёт', 'status': 'pending'}]})
            elif step == 1:
                delta = call('inspect', 'read', {'file_path': 'input.txt'})
            elif step == 2:
                delta = call('done', 'todo_write', {'todos': [{'content': 'Анализ', 'status': 'completed'}, {'content': 'Отчёт', 'status': 'completed'}]})
            elif step == 3:
                delta = {'content': 'fixture reviewed'}
            else:
                raise AssertionError(f'unexpected {lane} step {step}')
            time.sleep(0.15)  # Give the actual terminal event loop time to render running states.
            reason = 'tool_calls' if 'tool_calls' in delta else 'stop'
            payload = 'data: ' + json.dumps({'choices': [{'index': 0, 'delta': delta}]}) + '\n\n'
            payload += 'data: ' + json.dumps({'choices': [{'index': 0, 'delta': {}, 'finish_reason': reason}], 'usage': {'prompt_tokens': 40, 'completion_tokens': 4}}) + '\n\ndata: [DONE]\n\n'
            payload = payload.encode()
            self.send_response(200)
            self.send_header('Content-Type', 'text/event-stream')
            self.send_header('Content-Length', str(len(payload)))
            self.end_headers()
            self.wfile.write(payload)
        except Exception as error:
            state['errors'].append(repr(error))
            self.send_error(500, str(error))


server = http.server.ThreadingHTTPServer(('127.0.0.1', 0), Model)
threading.Thread(target=server.serve_forever, daemon=True).start()
with (root / 'setup.log').open('wb') as log:
    subprocess.run(['git', 'init', '-b', 'main'], cwd=project, stdout=log, stderr=log, check=True)
(project / '.makedo').mkdir()
(project / 'input.txt').write_text('fixture data\n')
(project / '.makedo/config.toml').write_text(f'''[provider]
kind = "openai-compat"
model = "smoke-model"
base_url = "http://127.0.0.1:{server.server_port}/v1"
api_key_env = "MAKEDO_SMOKE_KEY"
''')
raw = bytearray()
pid, fd = pty.fork()
if pid == 0:
    os.chdir(project)
    env = {**os.environ, 'MAKEDO_HOME': str(settings), 'MAKEDO_SMOKE_KEY': 'fixture', 'TERM': 'xterm-256color', 'COLORTERM': 'truecolor'}
    env.pop('NO_COLOR', None)  # The test runner may disable colors; this PTY verifies them.
    os.execve(str(binary), [str(binary)] + ([] if permission_yolo else ['--yolo']), env)


def resize(columns, rows):
    fcntl.ioctl(fd, termios.TIOCSWINSZ, struct.pack('HHHH', rows, columns, 0, 0))


def collect(seconds):
    end = time.monotonic() + seconds
    while time.monotonic() < end:
        if not select.select([fd], [], [], min(0.1, end - time.monotonic()))[0]:
            continue
        try:
            data = os.read(fd, 65536)
        except OSError:
            return
        if not data:
            return
        raw.extend(data)
        if b'\x1b[6n' in data:
            os.write(fd, b'\x1b[1;1R')
        if b'\x1b[?u' in data:
            os.write(fd, b'\x1b[?0u')


try:
    resize(140, 36)
    collect(1.5)
    os.write(fd, b'show pipeline\r')
    if permission_yolo:
        deadline = time.monotonic() + 10
        while b'permission required' not in raw and time.monotonic() < deadline:
            collect(0.2)
        assert b'permission required' in raw, 'expected initial permission prompt'
        os.write(fd, b'YOLO\r')
    deadline = time.monotonic() + 25
    while state['parent'] < 2 and time.monotonic() < deadline:
        collect(0.3)
    collect(0.5)
    assert not state['errors'], state['errors']
    if permission_yolo:
        assert b'YOLO enabled' in raw, 'YOLO did not enable during approval'
    assert state['parent'] == 2 and state['child'] == 4, state
    for text in ['AGENTS', 'code-review', 'Анализ', 'Отчёт', 'pipeline complete']:
        assert text.encode() in raw, f'missing terminal output: {text}'
    before_selection = len(raw)
    os.write(fd, b'\x1b[17~')  # F6 details
    collect(0.2)
    selected_output = raw[before_selection:]
    assert '❯'.encode() in selected_output, 'selected agent has no visible marker'
    assert b'48;2;240;235;250m' in selected_output, 'selected agent has no contrasting background'
    os.write(fd, b'\x1b[18~')  # F7 collapse
    collect(0.2)
    assert b'Agents:' in raw, 'panel did not collapse'
    resize(80, 24)
    collect(0.2)
    # A single long logical line must grow vertically; keep the tail visible
    # when narrowed, allow continued typing, and submit the exact original text.
    state['draft'] = 'HEAD ' + 'сезонные вакансии по месяцам ' * 12 + 'TAIL'
    before_input = len(raw)
    os.write(fd, b'\x1b[200~' + state['draft'].encode() + b'\x1b[201~')
    collect(0.3)
    assert b'TAIL' in raw[before_input:], 'long input is clipped instead of wrapping'
    resize(40, 24)
    collect(0.2)
    before_typing = len(raw)
    os.write(fd, b'!')
    collect(0.2)
    assert b'!' in raw[before_typing:], 'continued typing is outside the visible input'
    resize(120, 24)
    collect(0.2)
    before_submit = len(raw)
    os.write(fd, b'\r')
    deadline = time.monotonic() + 10
    while state['parent'] < 3 and time.monotonic() < deadline:
        collect(0.2)
    collect(0.3)
    assert state['parent'] == 3 and not state['errors'], state
    # 120 columns minus outer margins (4), prompt indent (4), right padding (2).
    # Continuations in the submitted message must keep the same text column.
    continuation = ('    ' + state['draft'][110:220]).encode()
    assert continuation in raw[before_submit:], 'feed continuation lost its left indent'
    os.write(fd, b'hold model\r')
    deadline = time.monotonic() + 5
    while b'MODEL_HELD' not in raw and time.monotonic() < deadline:
        collect(0.1)
    assert b'MODEL_HELD' in raw and state['parent'] == 4, 'model did not enter held response'
    for text in ['QUEUED VISIBLE ONE', 'QUEUED VISIBLE TWO']:
        os.write(fd, b'\x1b[200~' + text.encode() + b'\x1b[201~')
        collect(0.15)
        before_queue = len(raw)
        os.write(fd, b'\r')
        collect(0.25)
        # Request a fresh frame: the differential renderer can otherwise reuse
        # unchanged letters/spaces from the previous bubble without emitting them.
        resize(121, 24)
        collect(0.1)
        resize(120, 24)
        collect(0.15)
        assert text.encode() in raw[before_queue:], 'queued text is missing from the feed'
        assert b'queued' in raw[before_queue:], 'queued message has no status'
        assert state['parent'] == 4, 'ordinary Enter interrupted the current response'
    assert b'\x1b[>1u' in raw, 'enhanced keyboard protocol was not enabled'
    os.write(fd, b'\x1b[200~FORCED NOW\x1b[201~')
    collect(0.15)
    os.write(fd, b'\x1b[13;5u')  # Ctrl+Enter in the enhanced keyboard protocol.
    deadline = time.monotonic() + 5
    while state['parent'] < 5 and time.monotonic() < deadline:
        collect(0.1)
    collect(0.3)
    assert state['parent'] == 5 and not state['errors'], state
    os.write(fd, b'\x03')
    deadline = time.monotonic() + 8
    while True:
        waited, status = os.waitpid(pid, os.WNOHANG)
        if waited == pid:
            pid = 0
            assert os.waitstatus_to_exitcode(status) == 0, f'terminal exit={os.waitstatus_to_exitcode(status)}'
            break
        assert time.monotonic() < deadline, 'terminal shutdown exceeded 8 seconds'
        collect(0.1)
    print(f'OK: child progress, F6/F7, long input/feed, visible inbox, Ctrl+Enter steering, clean exit; chat YOLO={permission_yolo}. Artifacts: {root}')
except Exception:
    print(f'FAIL: artifacts={root}; state={state}', file=sys.stderr)
    raise
finally:
    release_model.set()
    (root / 'terminal.log').write_bytes(raw)
    if pid:
        os.kill(pid, signal.SIGTERM)
        os.waitpid(pid, 0)
    os.close(fd)
    server.shutdown()
