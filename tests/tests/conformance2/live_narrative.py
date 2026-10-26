"""Forward real inference bytes, holding only one text terminal frame per armed phase."""
import http.server
import json
import os
from pathlib import Path
import shutil
import socket
import subprocess
import sys
import tempfile
import threading
import urllib.error
import urllib.request

os.umask(0o077)
run = Path(tempfile.mkdtemp(prefix="tyde-real-live-narrative-"))
print("Real live narrative evidence:", run, flush=True)
used = set()
lock = threading.Lock()
state = {"held": 0, "released": 0, "transport_errors": 0, "upstream_errors": 0}


class NoRedirect(urllib.request.HTTPRedirectHandler):
    def redirect_request(self, request, response, code, message, headers, url):
        return None


opener = urllib.request.build_opener(NoRedirect())


class Proxy(http.server.BaseHTTPRequestHandler):
    protocol_version = "HTTP/1.1"

    def log_message(self, *args):
        pass

    def do_POST(self):
        self.forward()

    def do_GET(self):
        self.forward()

    def forward(self):
        payload = self.rfile.read(int(self.headers.get("Content-Length", "0")))
        headers = {k: v for k, v in self.headers.items() if k.lower() not in {
            "host", "connection", "content-length", "accept-encoding", "proxy-authorization",
        }}
        headers["Accept-Encoding"] = "identity"
        request = urllib.request.Request(
            "https://chatgpt.com/backend-api/codex" + self.path,
            data=payload if self.command == "POST" else None,
            headers=headers, method=self.command,
        )
        try:
            response = opener.open(request)
        except urllib.error.HTTPError as error:
            response = error
            with lock:
                state["upstream_errors"] += 1
        try:
            with response:
                self.send_response(response.status)
                for key, value in response.headers.items():
                    if key.lower() not in {"connection", "transfer-encoding", "content-length", "content-encoding"}:
                        self.send_header(key, value)
                self.send_header("Connection", "close")
                self.end_headers()
                self.close_connection = True
                if not self.path.split("?")[0].endswith("/responses") or response.status != 200:
                    shutil.copyfileobj(response, self.wfile)
                    return
                frame = bytearray()
                text_seen = False
                for line in response:
                    frame.extend(line)
                    if line.strip():
                        continue
                    events = [json.loads(field[5:]) for field in bytes(frame).splitlines()
                              if field.startswith(b"data:") and field[5:].strip() != b"[DONE]"]
                    text_seen |= any(event.get("type") == "response.output_text.delta" for event in events)
                    terminal = any(event.get("type") == "response.output_text.done" for event in events)
                    if terminal and text_seen and (run / "armed").is_file():
                        phase = (run / "armed").read_text()
                        with lock:
                            hold = phase not in used
                            if hold:
                                used.add(phase)
                                state["held"] += 1
                        if hold:
                            with socket.socket(socket.AF_UNIX) as gate:
                                gate.connect(str(run / "gate.sock"))
                                gate.sendall(b"\x01")
                                if gate.recv(1) != b"\x01":
                                    raise RuntimeError("stream gate closed without release")
                            with lock:
                                state["released"] += 1
                    self.wfile.write(frame)
                    self.wfile.flush()
                    frame.clear()
                if frame:
                    self.wfile.write(frame)
                    self.wfile.flush()
        except (BrokenPipeError, ConnectionResetError):
            if not (run / "armed").is_file() or (run / "armed").read_text() != "interrupted":
                with lock:
                    state["transport_errors"] += 1
            self.close_connection = True
        except Exception:
            with lock:
                state["transport_errors"] += 1
            self.close_connection = True


class Server(http.server.ThreadingHTTPServer):
    daemon_threads = True


if shutil.which("codex") is None:
    raise RuntimeError("installed native Codex is required")
shell_dir = run / "shell"
shell_dir.mkdir()
shell = shell_dir / "bash"
shell.write_text('#!/bin/sh\nexec /bin/bash --noprofile --norc "$@"\n')
shell.chmod(0o700)

server = Server(("127.0.0.1", 0), Proxy)
thread = threading.Thread(target=server.serve_forever)
thread.start()
try:
    with tempfile.TemporaryDirectory(prefix="tyde-live-narrative-home-") as home:
        source = Path(os.environ.get("CODEX_HOME", str(Path.home() / ".codex")))
        for name in ["auth.json", "models_cache.json"]:
            if (source / name).is_file():
                shutil.copyfile(source / name, Path(home) / name)
        (Path(home) / "config.toml").write_text(f'''model_provider = "tyde_live_narrative"
[model_providers.tyde_live_narrative]
name = "Real inference stream gate"
base_url = "http://127.0.0.1:{server.server_port}"
wire_api = "responses"
requires_openai_auth = true
supports_websockets = false
''')
        env = dict(os.environ, CODEX_HOME=home, TYDE_NARRATIVE_GATE_DIR=str(run), SHELL=str(shell))
        with (run / "run.log").open("w") as output:
            result = subprocess.run([sys.argv[1], "--exact", sys.argv[2], "--ignored", "--nocapture"],
                                    env=env, stdout=output, stderr=subprocess.STDOUT)
finally:
    server.shutdown()
    thread.join()
    server.server_close()
    (run / "receipt.json").write_text(json.dumps(state))
    print("Real stream gate receipt:", json.dumps(state), flush=True)
panic = False
for line in (run / "run.log").read_text().splitlines():
    if panic or line.startswith(("LIVE NARRATIVE EVIDENCE", "REQUEST BOUNDARY EVIDENCE", "test result:")):
        print(line)
    panic = "panicked at" in line
assert state["held"] > 0 and state["held"] == state["released"], "no completed real stream gate"
assert not state["transport_errors"] and not state["upstream_errors"], "real stream fixture failed"
sys.exit(result.returncode)
