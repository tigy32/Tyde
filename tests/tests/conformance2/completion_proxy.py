import asyncio
import json
import os
from pathlib import Path
import shlex
import shutil
import signal
import subprocess
import sys
import tempfile


async def proxy():
    real = os.environ["TYDE_REAL_CODEX_EXECUTABLE"]
    if "app-server" not in sys.argv[2:]:
        os.execv(real, [real, *sys.argv[2:]])
    child = await asyncio.create_subprocess_exec(
        real, *sys.argv[2:], stdin=asyncio.subprocess.PIPE,
        stdout=asyncio.subprocess.PIPE, limit=32 * 1024 * 1024,
    )
    notifications = asyncio.Queue()
    requests = {}
    proof = {
        "completion_held": False,
        "completion_released": False,
        "steer_declined_while_held": False,
        "answer_start_accepted_while_held": False,
        "fabricated_events": 0,
    }
    proof_path = Path(os.environ["TYDE_ANSWER_COMPLETION_PROOF"])

    def save_proof():
        temporary = proof_path.with_suffix(".tmp")
        temporary.write_text(json.dumps(proof))
        temporary.replace(proof_path)

    def forward(data):
        sys.stdout.buffer.write(data)
        sys.stdout.buffer.flush()

    async def forward_input():
        reader = asyncio.StreamReader(limit=32 * 1024 * 1024)
        await asyncio.get_running_loop().connect_read_pipe(
            lambda: asyncio.StreamReaderProtocol(reader), sys.stdin.buffer,
        )
        while data := await reader.readline():
            request = json.loads(data)
            if "id" in request and "method" in request:
                requests[request["id"]] = request["method"]
            child.stdin.write(data)
            await child.stdin.drain()
        child.stdin.close()

    async def read_output():
        completions = 0
        while data := await child.stdout.readline():
            message = json.loads(data)
            if "id" in message and "method" not in message:
                method = requests.pop(message["id"], None)
                if proof["completion_held"] and not proof["completion_released"]:
                    if method == "turn/steer" and message.get("error", {}).get("code") == -32600:
                        proof["steer_declined_while_held"] = True
                    if method == "turn/start" and "result" in message:
                        proof["answer_start_accepted_while_held"] = True
                    save_proof()
                forward(data)
                continue
            delay = 0
            if message.get("method") == "turn/completed":
                completions += 1
                if completions == 2:
                    proof["completion_held"] = True
                    save_proof()
                    delay = 5
            await notifications.put((data, delay))
        await notifications.put(None)

    async def forward_notifications():
        while entry := await notifications.get():
            data, delay = entry
            if delay:
                # Only genuine notifications wait; RPC replies remain independent.
                # FIFO retains the provider's notification ordering.
                await asyncio.sleep(delay)
                proof["completion_released"] = True
                save_proof()
            forward(data)

    tasks = [asyncio.create_task(operation()) for operation in (
        forward_input, read_output, forward_notifications,
    )]

    def fail_transport(task):
        if not task.cancelled() and task.exception() is not None:
            print("Completion proxy transport failed", file=sys.stderr, flush=True)
            if child.returncode is None:
                child.kill()

    for task in tasks:
        task.add_done_callback(fail_transport)
    try:
        status = await child.wait()
        await tasks[1]
        await tasks[2]
        return status
    finally:
        for task in tasks:
            task.cancel()
        await asyncio.gather(*tasks, return_exceptions=True)
        if child.returncode is None:
            child.kill()
        await child.wait()


def run():
    os.umask(0o077)
    real = shutil.which("codex")
    if real is None:
        raise RuntimeError("real Codex CLI is required")
    run_dir = Path(tempfile.mkdtemp(prefix="tyde-real-completion-race-"))
    binary_dir = run_dir / "bin"
    binary_dir.mkdir()
    wrapper = binary_dir / "codex"
    wrapper.write_text(
        "#!/bin/sh\nexec " + shlex.quote(sys.executable) + " "
        + shlex.quote(str(Path(__file__).resolve())) + ' --proxy "$@"\n'
    )
    wrapper.chmod(0o700)
    shell = run_dir / "bash"
    shell.write_text('#!/bin/sh\nexec /bin/bash --noprofile --norc "$@"\n')
    shell.chmod(0o700)
    proof_path = run_dir / "proof.json"
    env = dict(
        os.environ, PATH=str(binary_dir) + os.pathsep + os.environ["PATH"],
        SHELL=str(shell), TYDE_REAL_CODEX_EXECUTABLE=real,
        TYDE_ANSWER_COMPLETION_PROOF=str(proof_path),
    )
    print("Real completion race evidence:", run_dir, flush=True)
    with (run_dir / "run.log").open("w") as output:
        child = subprocess.Popen(
            [sys.argv[1], "--exact", sys.argv[2], "--ignored", "--nocapture"],
            env=env, stdout=output, stderr=subprocess.STDOUT, start_new_session=True,
        )
        try:
            status = child.wait(timeout=240)
        finally:
            try:
                os.killpg(child.pid, signal.SIGKILL)
            except ProcessLookupError:
                pass
            child.wait()
    if not proof_path.exists():
        raise RuntimeError("real provider did not reach the completion-overlap trigger")
    proof = json.loads(proof_path.read_text())
    assert proof == {
        "completion_held": True,
        "completion_released": True,
        "steer_declined_while_held": True,
        "answer_start_accepted_while_held": True,
        "fabricated_events": 0,
    }, "real traffic did not establish the required completion/start ordering"
    print("Transport proof:", json.dumps(proof), flush=True)
    for line in (run_dir / "run.log").read_text(errors="replace").splitlines():
        if line.startswith(("Completion overlap activity:", "answer turn emitted live work", "test result:")):
            print(line, flush=True)
    return status


if __name__ == "__main__":
    sys.exit(asyncio.run(proxy()) if sys.argv[1:2] == ["--proxy"] else run())
