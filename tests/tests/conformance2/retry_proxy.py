import http.server
import json
import os
from pathlib import Path
import shutil
import shlex
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


sequence_only = sys.argv[3:] == ["--sequence"]
os.umask(0o077)
run = Path(tempfile.mkdtemp(prefix="tyde-real-stream-fault-"))
print("Real stream-disconnect diagnostics:", run, flush=True)
proof = run / "command-finished.txt"
marker = run / "fault.json"
arm = run / "arm"
if sequence_only:
    arm.write_text("recover")
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
        if sequence_only and arm.exists() and arm.read_text() == "exhaust":
            self.close_connection = True
            self.connection.shutdown(socket.SHUT_RDWR)
            return
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
                            (sequence_only and arm.exists() and event.get("type") == "response.output_text.delta")
                            or (not sequence_only and event.get("type") == "response.output_item.done"
                                and event.get("item", {}).get("type") in {"function_call", "custom_tool_call"})
                        ):
                            with lock:
                                if sequence_only or not state["cuts"]:
                                    if sequence_only and arm.read_text() != "close":
                                        arm.unlink()
                                    state["cuts"] += 1
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
        if sequence_only:
            env["TYDE_REAL_RETRY_ARM"] = str(arm)
            env["TYDE_REAL_RETRY_FORK"] = str(run / "fork")
            env["TYDE_REAL_RETRY_FORK_PROOF"] = str(run / "fork-events")
            (run / "fork-events").write_text("")
            real_codex = shutil.which("codex")
            if real_codex is None:
                raise RuntimeError("real Codex CLI is required")
            bin_dir = run / "bin"
            bin_dir.mkdir()
            wrapper = bin_dir / "codex"
            wrapper.write_text(r"""#!/usr/bin/env python3
import json
import os
from pathlib import Path
import subprocess
import sys
import threading
real = os.environ["TYDE_REAL_CODEX"]
if "app-server" not in sys.argv:
    os.execv(real, [real, *sys.argv[1:]])
child = subprocess.Popen([real, *sys.argv[1:]], stdin=subprocess.PIPE, stdout=subprocess.PIPE)
def forward_input():
    try:
        for line in sys.stdin.buffer:
            child.stdin.write(line)
            child.stdin.flush()
        child.stdin.close()
    except BrokenPipeError:
        pass
threading.Thread(target=forward_input, daemon=True).start()
arm = Path(os.environ["TYDE_REAL_RETRY_ARM"])
fork = Path(os.environ["TYDE_REAL_RETRY_FORK"])
proof = Path(os.environ["TYDE_REAL_RETRY_FORK_PROOF"])
retry_seen = False
fork_rearmed = False
for line in child.stdout:
    event = json.loads(line)
    retry = event.get("method") == "error" and event.get("params", {}).get("willRetry") is True
    retry_seen = retry_seen or retry
    if fork.exists():
        if event.get("method") == "rawResponse/completed":
            with proof.open("a") as output:
                output.write("raw\n")
        if retry_seen and not fork_rearmed and event.get("method") == "thread/tokenUsage/updated":
            arm.write_text("recover")
            fork_rearmed = True
            with proof.open("a") as output:
                output.write("rearm\n")
    sys.stdout.buffer.write(line)
    sys.stdout.buffer.flush()
    if retry and arm.exists() and arm.read_text() == "close":
        child.kill()
        break
sys.exit(child.wait())
""")
            wrapper.chmod(0o700)
            env["TYDE_REAL_CODEX"] = real_codex
            env["PATH"] = str(bin_dir) + os.pathsep + env["PATH"]
            real_shell = os.environ["SHELL"]
            shell = run / Path(real_shell).name
            shell.write_text("#!" + sys.executable + "\n" + """import os
import shlex
import sys
args = sys.argv[1:]
for index, arg in enumerate(args[:-1]):
    if arg in ("-c", "-lc", "-ilc", "-lic"):
        args[index + 1] = "export PATH=" + shlex.quote(os.environ["TYDE_RETRY_BIN"]) + ':"$PATH"; ' + args[index + 1]
        break
real = os.environ["TYDE_REAL_SHELL"]
os.execv(real, [real, *args])
""")
            shell.chmod(0o700)
            env["TYDE_REAL_SHELL"] = real_shell
            env["TYDE_RETRY_BIN"] = str(bin_dir)
            env["SHELL"] = str(shell)
        with (run / "run.log").open("w") as output:
            result = subprocess.run(
                [sys.argv[1], "--exact", sys.argv[2], "--ignored", "--nocapture"],
                env=env, stdout=output, stderr=subprocess.STDOUT,
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
    if line.startswith(("test result:", "REAL RETRY EVIDENCE", "REAL RETRY SEQUENCE EVIDENCE")):
        print(line)
print("Real provider retryable stream-disconnect notifications:", retries)
if sequence_only:
    assert state["cuts"] == 5, "fixture must cut five separate real response streams"
else:
    assert state["cuts"] == 1, "fixture did not cut a real tool-bearing stream"
    assert retries > 0, "real provider did not report the required retryable disconnect"
assert not state["upstream_errors"], "real upstream rejected a fixture request"
assert state["handler_errors"] == 0, "stream-disconnect fixture encountered a transport error"
sys.exit(result.returncode)
