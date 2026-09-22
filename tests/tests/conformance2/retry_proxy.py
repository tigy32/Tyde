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


class NoRedirect(urllib.request.HTTPRedirectHandler):
    def redirect_request(self, request, response, code, message, headers, url):
        return None


os.umask(0o077)
run = Path(tempfile.mkdtemp(prefix="tyde-real-stream-fault-"))
print("Real stream-disconnect diagnostics:", run, flush=True)
proof = run / "command-finished.txt"
marker = run / "fault.json"
lock = threading.Lock()
state = {
    "requests": 0,
    "cuts": 0,
    "recovery_requests_before_completion": 0,
    "upstream_errors": [],
    "handler_errors": 0,
}
opener = urllib.request.build_opener(NoRedirect())


def record():
    temporary = run / "fault.json.tmp"
    temporary.write_text(json.dumps(state))
    temporary.replace(marker)


class Proxy(http.server.BaseHTTPRequestHandler):
    protocol_version = "HTTP/1.1"

    def log_message(self, *args):
        pass

    def do_GET(self):
        self.forward()

    def do_POST(self):
        self.forward()

    def forward(self):
        payload = self.rfile.read(int(self.headers.get("Content-Length", "0")))
        headers = {
            key: value
            for key, value in self.headers.items()
            if key.lower() not in {
                "host", "connection", "content-length", "accept-encoding",
                "proxy-authorization",
            }
        }
        headers["Accept-Encoding"] = "identity"
        request = urllib.request.Request(
            "https://chatgpt.com/backend-api/codex" + self.path,
            data=payload if self.command == "POST" else None,
            headers=headers,
            method=self.command,
        )
        responses = self.path.split("?")[0].endswith("/responses")
        with lock:
            if responses:
                state["requests"] += 1
                if state["cuts"] and not proof.exists():
                    state["recovery_requests_before_completion"] += 1
            record()
        try:
            response = opener.open(request, timeout=90)
        except urllib.error.HTTPError as error:
            response = error
            with lock:
                state["upstream_errors"].append(error.code)
                record()
        except Exception:
            with lock:
                state["handler_errors"] += 1
                record()
            self.close_connection = True
            return
        with response:
            self.send_response(response.status)
            for key, value in response.headers.items():
                if key.lower() not in {
                    "connection", "transfer-encoding", "content-length", "content-encoding",
                }:
                    self.send_header(key, value)
            self.send_header("Connection", "close")
            self.end_headers()
            self.close_connection = True
            try:
                # Real /responses streams can omit Content-Type. Dispatch by
                # route so a healthy run cannot silently bypass fault injection.
                if not responses or response.status != 200:
                    shutil.copyfileobj(response, self.wfile)
                    return
                frame = bytearray()
                for line in response:
                    frame.extend(line)
                    if line.strip():
                        continue
                    self.wfile.write(frame)
                    self.wfile.flush()
                    cut = False
                    for field in bytes(frame).splitlines():
                        if not field.startswith(b"data:"):
                            continue
                        try:
                            event = json.loads(field[5:])
                        except (ValueError, TypeError):
                            continue
                        if (
                            event.get("type") == "response.output_item.done"
                            and event.get("item", {}).get("type")
                            in {"function_call", "custom_tool_call"}
                        ):
                            with lock:
                                if not state["cuts"]:
                                    state["cuts"] = 1
                                    record()
                                    cut = True
                    frame.clear()
                    if cut:
                        self.connection.shutdown(socket.SHUT_RDWR)
                        return
                if frame:
                    self.wfile.write(frame)
                    self.wfile.flush()
            except (BrokenPipeError, ConnectionResetError):
                pass
            except Exception:
                with lock:
                    state["handler_errors"] += 1
                    record()


class Server(http.server.ThreadingHTTPServer):
    daemon_threads = True


server = Server(("127.0.0.1", 0), Proxy)
thread = threading.Thread(target=server.serve_forever)
thread.start()
try:
    with tempfile.TemporaryDirectory(prefix="tyde-real-retry-home-") as home:
        source = Path(os.environ.get("CODEX_HOME", str(Path.home() / ".codex")))
        for name in ["auth.json", "models_cache.json"]:
            if (source / name).is_file():
                shutil.copyfile(source / name, Path(home) / name)
        (Path(home) / "config.toml").write_text(f'''model_provider = "tyde_retry_probe"
[model_providers.tyde_retry_probe]
name = "Real OpenAI stream fault probe"
base_url = "http://127.0.0.1:{server.server_port}"
wire_api = "responses"
requires_openai_auth = true
supports_websockets = false
''')
        env = dict(
            os.environ,
            CODEX_HOME=home,
            TYDE_REAL_STREAM_FAULT_MARKER=str(marker),
            TYDE_REAL_STREAM_FAULT_PROOF=str(proof),
        )
        with (run / "run.log").open("w") as output:
            result = subprocess.run(
                [sys.argv[1], "--exact", sys.argv[2], "--ignored", "--nocapture"],
                env=env, stdout=output, stderr=subprocess.STDOUT, timeout=240,
            )
finally:
    server.shutdown()
    thread.join()
    server.server_close()
    print("Real stream-disconnect evidence:", json.dumps(state), flush=True)

retries = 0
for line in (run / "run.log").read_text(errors="replace").splitlines():
    if "ROOT NOTIFICATION method=error" in line and "params=" in line:
        event = json.loads(line.split("params=", 1)[1])
        retries += (
            event.get("willRetry") is True
            and "responseStreamDisconnected" in (event.get("error", {}).get("codexErrorInfo") or {})
        )
    if line.startswith(("test result:", "REAL RETRY EVIDENCE")):
        print(line)
print("Real provider retryable stream-disconnect notifications:", retries)
assert state["cuts"] == 1, "fixture did not cut a real tool-bearing stream"
assert retries > 0, "real provider did not report the required retryable disconnect"
assert not state["upstream_errors"], "real upstream rejected a fixture request"
assert state["handler_errors"] == 0, "stream-disconnect fixture encountered a transport error"
sys.exit(result.returncode)
