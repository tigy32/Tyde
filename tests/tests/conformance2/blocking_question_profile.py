"""Run the same real-provider oracle with Codex's native question tool."""

import json
import os
from pathlib import Path
import shlex
import shutil
import socket
import subprocess
import sys
import tempfile
import threading


def proxy():
    real = os.environ["TYDE_REAL_CODEX_EXECUTABLE"]
    args = sys.argv[2:]
    if "app-server" not in args:
        os.execv(real, [real, *args])
    index = args.index("app-server") + 1
    # Profile defaults precede the adapter's overrides: exclusion must win.
    args[index:index] = [
        "-c", "tools.experimental_request_user_input.enabled=true",
        "-c", "features.default_mode_request_user_input=true",
        "-c", "features.tool_registry.turn_metadata_includes_tool_info=true",
    ]
    if os.environ.get("TYDE_NATIVE_QUESTION_MODE") == "yielded":
        args[index:index] = [
            "-c", "features.code_mode=true",
            "-c", "features.code_mode_host=true",
            "-c", "features.code_mode_only=true",
        ]
    child = subprocess.Popen(
        [real, *args], stdin=subprocess.PIPE, stdout=subprocess.PIPE,
    )

    def forward_input():
        try:
            for line in sys.stdin.buffer:
                request = json.loads(line)
                if request.get("method") == "turn/start":
                    request["params"]["collaborationMode"] = {
                        "mode": "default" if os.environ["TYDE_NATIVE_QUESTION_MODE"] == "yielded" else os.environ["TYDE_NATIVE_QUESTION_MODE"],
                        "settings": {
                            "model": os.environ["TYDE_CODEX_TEST_MODEL"],
                            "reasoning_effort": "low",
                        },
                    }
                    line = (json.dumps(request) + "\n").encode()
                child.stdin.write(line)
                child.stdin.flush()
        except (BrokenPipeError, OSError):
            pass
        finally:
            child.stdin.close()

    threading.Thread(target=forward_input, daemon=True).start()
    try:
        for line in child.stdout:
            event = json.loads(line)
            if event.get("method") == "item/tool/requestUserInput":
                with open(os.environ["TYDE_BLOCKING_QUESTION_PROOF"], "a") as proof:
                    record = {"blocking": event["params"]["isBlocking"]}
                    if os.environ["TYDE_NATIVE_QUESTION_MODE"] == "yielded":
                        yielded = 0
                        for trace in Path(os.environ["CODEX_ROLLOUT_TRACE_ROOT"]).glob("*/trace.jsonl"):
                            lines = trace.read_bytes().split(b"\n")[:-1]
                            for raw in lines:
                                payload = json.loads(raw)["payload"]
                                yielded += payload.get("type") == "code_cell_initial_response" and payload.get("status") == "yielded"
                        record["yielded_cells"] = yielded
                    proof.write(json.dumps(record) + "\n")
            # Never invent or change provider responses or events.
            sys.stdout.buffer.write(line)
            sys.stdout.buffer.flush()
        return child.wait()
    finally:
        if child.poll() is None:
            child.kill()
        child.wait()


def run():
    os.umask(0o077)
    real = shutil.which("codex")
    if real is None:
        raise RuntimeError("real Codex CLI is required")
    root = Path(tempfile.mkdtemp(prefix="tyde-real-blocking-question-"))
    binary_dir = root / "bin"
    binary_dir.mkdir()
    wrapper = binary_dir / "codex"
    wrapper.write_text(
        "#!/bin/sh\nexec " + shlex.quote(sys.executable) + " "
        + shlex.quote(str(Path(__file__).resolve())) + ' --proxy "$@"\n'
    )
    wrapper.chmod(0o700)
    shell_dir = root / "shell"
    shell_dir.mkdir()
    shell = shell_dir / "bash"
    shell.write_text('#!/bin/sh\nexec /bin/bash --noprofile --norc "$@"\n')
    shell.chmod(0o700)
    proof = root / "proof.jsonl"
    mode = sys.argv[3] if len(sys.argv) > 3 else "plan"
    if mode not in ("default", "plan", "yielded"):
        raise ValueError("native question profile must be default or plan")
    model = os.environ.get("TYDE_CODEX_TEST_MODEL", "gpt-6-sol") if mode == "yielded" else ("gpt-6-astra" if mode == "default" else "gpt-5.6-luna")
    env = dict(
        os.environ,
        PATH=str(binary_dir) + os.pathsep + os.environ["PATH"],
        SHELL=str(shell),
        TYDE_REAL_CODEX_EXECUTABLE=real,
        TYDE_BLOCKING_QUESTION_PROOF=str(proof),
        TYDE_CODEX_TEST_MODEL=model,
        TYDE_NATIVE_QUESTION_MODE=mode,
    )
    print("Real native-question evidence:", mode, root, flush=True)
    with (root / "run.log").open("w") as output:
        result = subprocess.run(
            [sys.argv[1], "--exact", sys.argv[2], "--ignored", "--nocapture"],
            env=env, stdout=output, stderr=subprocess.STDOUT,
        )
    if not proof.exists():
        raise RuntimeError("real provider never requested blocking user input")
    records = [json.loads(line) for line in proof.read_text().splitlines()]
    if mode == "yielded" and result.returncode == 0:
        assert len(records) == 2 and all(record.get("yielded_cells", 0) >= i for i, record in enumerate(records, 1)), \
            "each native question must overlap a genuinely yielded cell"
    expected_blocking = mode == "plan"
    assert any(record["blocking"] == expected_blocking for record in records), \
        "native question profile positive control missing"
    print("Real native question count:", len(records), flush=True)
    for line in (root / "run.log").read_text(errors="replace").splitlines():
        if line.startswith("test result:"):
            print(line, flush=True)
    return result.returncode


def held_tool():
    for line in sys.stdin:
        request = json.loads(line)
        if "id" not in request:
            continue
        method = request["method"]
        if method == "initialize":
            result = {"protocolVersion": "2025-06-18", "capabilities": {"tools": {}}, "serverInfo": {"name": "question_gate", "version": "1"}}
        elif method == "tools/list":
            result = {"tools": [{"name": "hold", "description": "Wait for the test controller to release real foreground work.", "inputSchema": {"type": "object", "properties": {}, "additionalProperties": False}}]}
        elif method == "tools/call":
            with socket.socket(socket.AF_UNIX) as gate:
                gate.connect(sys.argv[2])
                if gate.recv(1) != b"R":
                    raise RuntimeError("foreground gate closed without release")
            result = {"content": [{"type": "text", "text": "HELD_WORK_COMPLETED"}]}
        else:
            raise RuntimeError("unsupported MCP request method")
        print(json.dumps({"jsonrpc": "2.0", "id": request["id"], "result": result}), flush=True)
    return 0


if __name__ == "__main__":
    sys.exit(held_tool() if sys.argv[1:2] == ["--held-tool"] else (proxy() if sys.argv[1:2] == ["--proxy"] else run()))
