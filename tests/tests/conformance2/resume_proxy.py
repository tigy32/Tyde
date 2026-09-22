import json
import os
from pathlib import Path
import shlex
import shutil
import subprocess
import sys
import tempfile
import threading
import time


def proxy():
    if "app-server" not in sys.argv[2:]:
        real = os.environ["TYDE_REAL_CODEX_EXECUTABLE"]
        os.execv(real, [real, *sys.argv[2:]])
    child = subprocess.Popen(
        [os.environ["TYDE_REAL_CODEX_EXECUTABLE"], *sys.argv[2:]],
        stdin=subprocess.PIPE, stdout=subprocess.PIPE,
    )
    lock = threading.Lock()
    state = {"request": None, "thread": None, "started": False, "released": False}
    held = None
    deadline = None

    def expire():
        with lock:
            if not state["released"]:
                print("Resume transport trigger timed out", file=sys.stderr, flush=True)
                child.kill()

    def forward_input():
        nonlocal deadline
        try:
            for line in sys.stdin.buffer:
                request = json.loads(line)
                if request.get("method") == "thread/resume":
                    with lock:
                        state["request"] = request["id"]
                        state["thread"] = request["params"]["threadId"]
                    deadline = threading.Timer(30, expire)
                    deadline.start()
                child.stdin.write(line)
                child.stdin.flush()
        except (BrokenPipeError, OSError):
            pass
        finally:
            child.stdin.close()

    def forward(line):
        sys.stdout.buffer.write(line)
        sys.stdout.buffer.flush()

    input_thread = threading.Thread(target=forward_input, daemon=True)
    input_thread.start()
    try:
        for line in child.stdout:
            message = json.loads(line)
            with lock:
                resume_response = (
                    state["request"] is not None
                    and message.get("id") == state["request"]
                    and "result" in message
                )
                resumed_start = (
                    state["request"] is not None
                    and message.get("method") == "turn/started"
                    and message.get("params", {}).get("threadId") == state["thread"]
                )
                if resumed_start:
                    state["started"] = True
                if resume_response:
                    held = line
                else:
                    forward(line)
                if held is not None and state["started"] and not state["released"]:
                    # Let Tyde consume the genuine notification before its
                    # pending resume RPC returns and initializes local state.
                    time.sleep(0.25)
                    forward(held)
                    held = None
                    state["released"] = True
                    if deadline is not None:
                        deadline.cancel()
                    Path(os.environ["TYDE_RESUME_RACE_PROOF"]).write_text(json.dumps({
                        "resume_replies_released": 1,
                        "real_turn_start_forwarded_before_resume_reply": True,
                        "fabricated_events": 0,
                    }))
        return child.wait()
    finally:
        if deadline is not None:
            deadline.cancel()
            deadline.join()
        if child.poll() is None:
            child.kill()
        child.wait()


def run():
    os.umask(0o077)
    real = shutil.which("codex")
    if real is None:
        raise RuntimeError("real Codex CLI is required")
    run_dir = Path(tempfile.mkdtemp(prefix="tyde-real-resume-race-"))
    binary_dir = run_dir / "bin"
    binary_dir.mkdir()
    wrapper = binary_dir / "codex"
    wrapper.write_text(
        "#!/bin/sh\nexec " + shlex.quote(sys.executable) + " "
        + shlex.quote(str(Path(__file__).resolve())) + ' --proxy "$@"\n'
    )
    wrapper.chmod(0o700)
    shell_dir = run_dir / "shell"
    shell_dir.mkdir()
    shell = shell_dir / "bash"
    shell.write_text('#!/bin/sh\nexec /bin/bash --noprofile --norc "$@"\n')
    shell.chmod(0o700)
    proof = run_dir / "proof.json"
    env = dict(
        os.environ,
        PATH=str(binary_dir) + os.pathsep + os.environ["PATH"],
        SHELL=str(shell),
        TYDE_REAL_CODEX_EXECUTABLE=real,
        TYDE_RESUME_RACE_PROOF=str(proof),
    )
    print("Real resume race evidence:", run_dir, flush=True)
    with (run_dir / "run.log").open("w") as output:
        result = subprocess.run(
            [sys.argv[1], "--exact", sys.argv[2], "--ignored", "--nocapture"],
            env=env, stdout=output, stderr=subprocess.STDOUT, timeout=420,
        )
    if not proof.exists():
        raise RuntimeError("real provider did not reach the required resume/start interleaving")
    evidence = json.loads(proof.read_text())
    assert evidence == {
        "resume_replies_released": 1,
        "real_turn_start_forwarded_before_resume_reply": True,
        "fabricated_events": 0,
    }, "resume transport did not establish the required real-event ordering"
    print("Transport proof:", json.dumps(evidence), flush=True)
    for line in (run_dir / "run.log").read_text(errors="replace").splitlines():
        if line.startswith(("Resumed goal activity:", "resumed goal emitted ", "test result:")):
            print(line, flush=True)
    return result.returncode


if __name__ == "__main__":
    sys.exit(proxy() if sys.argv[1:2] == ["--proxy"] else run())
