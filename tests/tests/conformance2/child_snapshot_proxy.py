import asyncio
import hashlib
import json
import os
from pathlib import Path
import re
import shlex
import shutil
import signal
import subprocess
import sys
import tempfile


async def proxy():
    real = os.environ["TYDE_CHILD_SNAPSHOT_EXECUTABLE"]
    if "serve" not in sys.argv[2:]:
        os.execv(real, [real, *sys.argv[2:]])
    directory = Path(os.environ["TYDE_CHILD_SNAPSHOT_DIRECTORY"])
    proof_path = directory / "transport.json"
    child = await asyncio.create_subprocess_exec(
        real, *sys.argv[2:], stdin=asyncio.subprocess.PIPE,
        stdout=asyncio.subprocess.PIPE, limit=32 * 1024 * 1024,
    )
    requests = {}
    notifications = asyncio.Queue()
    release = asyncio.Event()
    root_session = None
    snapshot = None
    snapshot_events = []
    armed = False
    observed_digest = hashlib.sha256()
    forwarded_digest = hashlib.sha256()
    gate = os.open(directory / "b-started.fifo", os.O_RDWR | os.O_NONBLOCK)
    proof = {
        "b_gate_reached": False,
        "notifications_held": 0,
        "notifications_forwarded": 0,
        "notification_bytes_and_order_preserved": False,
        "first_child_read_after_b_gate": False,
        "historical_completion_attested": False,
        "distinct_later_turn_active": False,
        "snapshot_reached": False,
        "native_turn_starts": 0,
        "native_turn_completions": 0,
        "first_child_registration_reads": 0,
        "native_b_terminal_seen": False,
        "native_b_completed": False,
    }

    def save():
        temporary = proof_path.with_suffix(".tmp")
        temporary.write_text(json.dumps(proof))
        temporary.replace(proof_path)

    def forward(data):
        sys.stdout.buffer.write(data)
        sys.stdout.buffer.flush()

    def release_b():
        fd = os.open(directory / "b-release.fifo", os.O_RDWR | os.O_NONBLOCK)
        try:
            os.write(fd, b"release\n")
        finally:
            os.close(fd)

    def gate_ready():
        if os.read(gate, 4096):
            proof["b_gate_reached"] = True
            save()
            asyncio.get_running_loop().remove_reader(gate)
            release.set()

    def observe_reply(request, message):
        nonlocal root_session, snapshot
        result = message.get("result")
        if not isinstance(result, dict):
            return
        method, params = request
        if method == "session/start":
            root_session = result["session"]["sessionId"]
        if method == "session/read" and params["sessionId"] != root_session:
            proof["first_child_registration_reads"] += 1
            if snapshot is None:
                snapshot = {
                    "session": params["sessionId"],
                    "cursor": result["viewCursor"],
                    "last_turn": result.get("lastTurn"),
                    "active_turn": None,
                }
                proof["first_child_read_after_b_gate"] = proof["b_gate_reached"]
                save()
        if (snapshot is not None and not proof["snapshot_reached"]
                and method == "view/page" and params["sessionId"] == snapshot["session"]):
            for event in result["events"]:
                snapshot_events.append(event)
                if event["params"]["viewCursor"] == snapshot["cursor"]:
                    proof["snapshot_reached"] = True
                    break
            if proof["snapshot_reached"]:
                starts = [event["params"]["turnId"] for event in snapshot_events
                          if event["method"] == "turn/started"]
                terminals = [event["params"] for event in snapshot_events
                             if event["method"] == "turn/completed"]
                completed_ids = {event["turnId"] for event in terminals}
                active = [turn for turn in starts if turn not in completed_ids]
                last = snapshot["last_turn"]
                proof["native_turn_starts"] = len(starts)
                proof["native_turn_completions"] = len(terminals)
                proof["historical_completion_attested"] = bool(last) and any(
                    event["turnId"] == last["turnId"]
                    and event["terminal"] == last["terminal"] for event in terminals)
                proof["distinct_later_turn_active"] = (
                    bool(last) and len(active) == 1 and active[0] != last["turnId"])
                if proof["distinct_later_turn_active"]:
                    snapshot["active_turn"] = active[0]
                save()
        if (snapshot is not None and method == "view/page"
                and params["sessionId"] == snapshot["session"]):
            for event in result["events"]:
                if (event["method"] == "turn/completed"
                        and event["params"]["turnId"] == snapshot["active_turn"]):
                    proof["native_b_terminal_seen"] = True
                    proof["native_b_completed"] = event["params"]["terminal"] == "completed"
                    save()

    async def forward_input():
        nonlocal armed
        reader = asyncio.StreamReader(limit=32 * 1024 * 1024)
        await asyncio.get_running_loop().connect_read_pipe(
            lambda: asyncio.StreamReaderProtocol(reader), sys.stdin.buffer,
        )
        while data := await reader.readline():
            message = json.loads(data)
            if "id" in message and "method" in message:
                requests[message["id"]] = (message["method"], message.get("params", {}))
                if message["method"] == "turn/start":
                    armed = True
            child.stdin.write(data)
            await child.stdin.drain()
        release_b()
        child.stdin.close()

    async def read_output():
        while data := await child.stdout.readline():
            message = json.loads(data)
            if "id" in message:
                request = requests.pop(message["id"], None)
                if request is not None:
                    observe_reply(request, message)
                forward(data)
            elif armed:
                # Every native notification, including raw-log delivery, waits.
                # RPC replies bypass this FIFO; their original bytes are untouched.
                tracked = not release.is_set()
                if tracked:
                    proof["notifications_held"] += 1
                    observed_digest.update(data)
                    save()
                await notifications.put((data, tracked))
            else:
                forward(data)
        await notifications.put(None)

    async def forward_notifications():
        while entry := await notifications.get():
            await release.wait()
            data, tracked = entry
            forward(data)
            if tracked:
                forwarded_digest.update(data)
                proof["notifications_forwarded"] += 1
                proof["notification_bytes_and_order_preserved"] = (
                    proof["notifications_held"] == proof["notifications_forwarded"]
                    and observed_digest.digest() == forwarded_digest.digest())
                save()

    asyncio.get_running_loop().add_reader(gate, gate_ready)
    tasks = [asyncio.create_task(operation()) for operation in (
        forward_input, read_output, forward_notifications,
    )]
    failures = []

    def fail_transport(task):
        if not task.cancelled() and task.exception() is not None:
            failures.append(type(task.exception()).__name__)
            print("Child snapshot proxy transport failed", file=sys.stderr, flush=True)
            if child.returncode is None:
                child.kill()

    for task in tasks:
        task.add_done_callback(fail_transport)
    try:
        status = await child.wait()
        await tasks[1]
        if not release.is_set():
            raise RuntimeError("Native child never reached its second-task gate")
        await tasks[2]
        if failures:
            raise RuntimeError("Child snapshot proxy transport failed")
        return status
    finally:
        asyncio.get_running_loop().remove_reader(gate)
        os.close(gate)
        release_b()
        for task in tasks:
            task.cancel()
        await asyncio.gather(*tasks, return_exceptions=True)
        if child.returncode is None:
            child.kill()
        await child.wait()


def run():
    os.umask(0o077)
    real = shutil.which(sys.argv[3])
    if real is None:
        raise RuntimeError("The selected real provider CLI is required")
    directory = Path(tempfile.mkdtemp(prefix="tyde-real-child-snapshot-"))
    binary_directory = directory / "bin"
    binary_directory.mkdir()
    wrapper = binary_directory / sys.argv[3]
    wrapper.write_text(
        "#!/bin/sh\nexec " + shlex.quote(sys.executable) + " "
        + shlex.quote(str(Path(__file__).resolve())) + ' --proxy "$@"\n'
    )
    wrapper.chmod(0o700)
    shell = directory / "bash"
    shell.write_text('#!/bin/sh\nexec /bin/bash --noprofile --norc "$@"\n')
    shell.chmod(0o700)
    for name in ["b-started.fifo", "b-release.fifo"]:
        os.mkfifo(directory / name, 0o600)
    env = dict(
        os.environ, PATH=str(binary_directory) + os.pathsep + os.environ["PATH"],
        SHELL=str(shell), TYDE_CHILD_SNAPSHOT_EXECUTABLE=real,
        TYDE_CHILD_SNAPSHOT_DIRECTORY=str(directory),
        RUST_LOG="server::backend::muse::actor=debug", NO_COLOR="1",
    )
    print("Real child snapshot evidence:", directory, flush=True)
    with (directory / "run.log").open("w") as output:
        child = subprocess.Popen(
            [sys.argv[1], "--exact", sys.argv[2], "--ignored", "--nocapture"],
            env=env, stdout=output, stderr=subprocess.STDOUT, start_new_session=True,
        )
        try:
            # This is the individual real case's hung guard, not a stage deadline.
            status = child.wait(timeout=600)
        finally:
            try:
                os.killpg(child.pid, signal.SIGKILL)
            except ProcessLookupError:
                pass
            child.wait()
    proof = json.loads((directory / "transport.json").read_text())
    print("Native child snapshot transport proof:", json.dumps(proof), flush=True)
    required = ["b_gate_reached", "notification_bytes_and_order_preserved",
                "first_child_read_after_b_gate", "historical_completion_attested",
                "distinct_later_turn_active", "snapshot_reached"]
    assert all(proof[key] is True for key in required), "Native snapshot interleaving was not established"
    assert proof["notifications_held"] > 0, "No genuine notifications were held"
    assert proof["first_child_registration_reads"] == 1, "Expected exactly one first child registration"
    assert proof["native_turn_starts"] == 2 and proof["native_turn_completions"] == 1, \
        "Snapshot must contain completed A and unfinished B"
    assert proof["native_b_terminal_seen"] is True and proof["native_b_completed"] is True, \
        "The second native child turn did not reach its genuine completed outcome"
    text = re.sub(r"\x1b\[[0-9;]*m", "", (directory / "run.log").read_text(errors="replace"))
    barriers = [line for line in text.splitlines()
                if "Muse native child reached its attested snapshot barrier" in line]
    assert len(barriers) == 1, "Production backend did not cross exactly one child snapshot barrier"
    barrier = barriers[0]
    for fact in ["historical_terminal_present=true", "current_terminal_present=false",
                 "active_turn_present=true"]:
        assert fact in barrier, "Production backend crossed the wrong snapshot state"
    response_open = re.findall(r"\bresponse_open=(true|false)\b", barrier)
    assert len(response_open) == 1, "Production snapshot omitted its actual response handoff state"
    print("Production child snapshot barrier: historical_terminal=true current_terminal=false "
          "active_turn=true response_open=" + response_open[0], flush=True)
    for line in text.splitlines():
        if line.startswith(("Native child followup lifecycle:", "test result:")):
            print(line, flush=True)
    return status


if __name__ == "__main__":
    try:
        sys.exit(asyncio.run(proxy()) if sys.argv[1:2] == ["--proxy"] else run())
    except Exception as error:
        context = str(error) if isinstance(error, (AssertionError, RuntimeError)) else "See private evidence"
        print("Native child snapshot fixture failed:", type(error).__name__, context,
              file=sys.stderr, flush=True)
        sys.exit(1)
