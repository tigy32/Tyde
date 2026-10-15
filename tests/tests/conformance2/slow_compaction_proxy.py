import asyncio
import hashlib
import json
import os
from pathlib import Path
import shlex
import shutil
import signal
import subprocess
import sys
import tempfile
import time


HOLD_SECONDS = 305


async def proxy():
    real = os.environ["TYDE_SLOW_COMPACTION_EXECUTABLE"]
    if "serve" not in sys.argv[2:]:
        os.execv(real, [real, *sys.argv[2:]])
    directory = Path(os.environ["TYDE_SLOW_COMPACTION_DIRECTORY"])
    proof_path = directory / "transport.json"
    child = await asyncio.create_subprocess_exec(
        real, *sys.argv[2:], stdin=asyncio.subprocess.PIPE,
        stdout=asyncio.subprocess.PIPE, limit=32 * 1024 * 1024,
    )
    requests = {}
    notifications = asyncio.Queue()
    admitted = asyncio.Event()
    hold_elapsed = asyncio.Event()
    durable_outcome = asyncio.Event()
    terminal_notification = asyncio.Event()
    release = asyncio.Event()
    armed = False
    session = None
    command = None
    admitted_at = None
    timer = None
    outcome_digest = None
    observed_digest = hashlib.sha256()
    forwarded_digest = hashlib.sha256()
    proof = {
        "compaction_dispatches": 0,
        "admission_ack_passed_before_release": False,
        "durable_outcome_before_release": False,
        "durable_outcome_unchanged": False,
        "durable_target_matches_outcome": False,
        "durable_install_matches_outcome": False,
        "native_completed": False,
        "native_terminal_notification_held": False,
        "notifications_held": 0,
        "notifications_forwarded": 0,
        "notification_bytes_and_order_preserved": False,
        "notifications_released": False,
        "admission_to_release_ms": 0,
        "native_before_tokens": None,
        "native_after_tokens": None,
    }

    def save():
        temporary = proof_path.with_suffix(".tmp")
        temporary.write_text(json.dumps(proof))
        temporary.replace(proof_path)

    def forward(data):
        sys.stdout.buffer.write(data)
        sys.stdout.buffer.flush()

    def observe_durable_outcome():
        nonlocal outcome_digest
        if session is None or command is None:
            return
        records = []
        for line in Path(session["path"]).read_bytes().splitlines(keepends=True):
            if not line.endswith(b"\n"):
                break
            record = json.loads(line)
            if "retained_frame" in record:
                if record["frame_schema_version"] != 1:
                    raise RuntimeError("Native compaction storage frame schema changed")
                records.extend(json.loads(entry["record_json"])
                               for entry in record["children"])
            elif "retained_marker" not in record:
                records.append(record)
        targets = [record for record in records
                   if record.get("payload_type") == "context.compact.manual.target"
                   and record["payload"]["command_id"] == command]
        outcomes = [record for record in records
                    if record.get("payload_type") == "context.compact.manual.outcome"
                    and record["payload"]["command_id"] == command]
        if not outcomes:
            return
        if len(targets) != 1 or len(outcomes) != 1:
            raise RuntimeError("Native compaction did not retain one target and outcome")
        target, outcome = targets[0], outcomes[0]
        if any(record["schema_version"] != 1 or record["payload_schema_version"] != 1
               or record["stream"]["id"] != session["sessionId"]
               or record["stream"]["kind"] != "session"
               for record in [target, outcome]):
            raise RuntimeError("Native compaction durable owner/schema disagreed")
        target_run = target["payload"]["run_stream"]
        proof["durable_target_matches_outcome"] = (
            target_run == outcome["payload"]["run_stream"]
            and target_run["kind"] == "run"
            and target["sequence"] < outcome["sequence"])
        installs = [record for record in records
                    if record.get("payload_type") == "runtime.session"
                    and record["payload"].get("kind") == "run"
                    and record["payload"]["run_id"] == target_run["id"]
                    and record["payload"]["event"].get("kind") == "context_compaction_installed"
                    and record["payload"]["event"]["candidate_id"]
                    == outcome["payload"].get("candidate_id")]
        proof["native_completed"] = outcome["payload"]["terminal_outcome"] == "compacted"
        proof["durable_install_matches_outcome"] = (
            len(installs) == 1 and bool(installs[0]["payload"]["event"]["install_id"])
            and target["sequence"] < installs[0]["sequence"] < outcome["sequence"])
        if len(installs) == 1:
            if (installs[0]["schema_version"] != 1
                    or installs[0]["payload_schema_version"] != 1
                    or installs[0]["stream"] != target["stream"]):
                raise RuntimeError("Native compaction installation owner/schema disagreed")
            install = installs[0]["payload"]["event"]
            proof["native_before_tokens"] = install["budget_before"]["estimated_prompt_tokens"]
            proof["native_after_tokens"] = install["budget_after"]["estimated_prompt_tokens"]
        digest = hashlib.sha256(json.dumps(outcome, sort_keys=True).encode()).digest()
        if outcome_digest is None:
            outcome_digest = digest
            proof["durable_outcome_before_release"] = not release.is_set()
        else:
            proof["durable_outcome_unchanged"] = digest == outcome_digest
        durable_outcome.set()
        save()

    async def forward_input():
        nonlocal armed, command
        reader = asyncio.StreamReader(limit=32 * 1024 * 1024)
        await asyncio.get_running_loop().connect_read_pipe(
            lambda: asyncio.StreamReaderProtocol(reader), sys.stdin.buffer,
        )
        while data := await reader.readline():
            message = json.loads(data)
            if "id" in message and "method" in message:
                method, params = message["method"], message.get("params", {})
                requests[message["id"]] = (method, params)
                if method == "session/compact":
                    proof["compaction_dispatches"] += 1
                    if armed or session is None or params["sessionId"] != session["sessionId"]:
                        raise RuntimeError("Slow compaction fixture lost its sole native owner")
                    command = params["commandId"]
                    armed = True
                    save()
            child.stdin.write(data)
            await child.stdin.drain()
        child.stdin.close()

    async def read_output():
        nonlocal session, admitted_at, timer
        while data := await child.stdout.readline():
            message = json.loads(data)
            if "id" in message:
                request = requests.pop(message["id"], None)
                if request is not None:
                    method, params = request
                    if method == "session/start" and "result" in message:
                        session = message["result"]["session"]
                    if method == "session/compact":
                        result = message.get("result", {})
                        if result.get("commandId") != command or result.get("status") != "accepted":
                            raise RuntimeError("Real native compaction was not admitted")
                        admitted_at = time.monotonic()
                        # This timed hold exercises the removed 300-second product cutoff.
                        timer = asyncio.get_running_loop().call_later(HOLD_SECONDS, hold_elapsed.set)
                        proof["admission_ack_passed_before_release"] = not release.is_set()
                        save()
                        forward(data)
                        admitted.set()
                        continue
                forward(data)
                continue
            tracked = armed and not release.is_set()
            if tracked:
                proof["notifications_held"] += 1
                observed_digest.update(data)
                item = message.get("params", {}).get("item", {})
                if (message.get("method") == "item/completed"
                        and item.get("kind") == "compaction"
                        and item.get("status") == "completed"):
                    proof["native_terminal_notification_held"] = True
                    terminal_notification.set()
                observe_durable_outcome()
                save()
            await notifications.put((data, tracked))
        await notifications.put(None)

    async def release_notifications():
        await admitted.wait()
        await hold_elapsed.wait()
        await terminal_notification.wait()
        observe_durable_outcome()
        await durable_outcome.wait()
        proof["admission_to_release_ms"] = int((time.monotonic() - admitted_at) * 1000)
        proof["notifications_released"] = True
        save()
        release.set()

    async def forward_notifications():
        while entry := await notifications.get():
            data, tracked = entry
            if tracked:
                await release.wait()
            forward(data)
            if tracked:
                forwarded_digest.update(data)
                proof["notifications_forwarded"] += 1
                proof["notification_bytes_and_order_preserved"] = (
                    proof["notifications_held"] == proof["notifications_forwarded"]
                    and observed_digest.digest() == forwarded_digest.digest())
                observe_durable_outcome()
                save()

    tasks = [asyncio.create_task(operation()) for operation in (
        forward_input, read_output, release_notifications, forward_notifications,
    )]
    failures = []

    def fail_transport(task):
        if not task.cancelled() and task.exception() is not None:
            failures.append(type(task.exception()).__name__)
            print("Slow compaction proxy transport failed", file=sys.stderr, flush=True)
            if child.returncode is None:
                child.kill()

    for task in tasks:
        task.add_done_callback(fail_transport)
    try:
        status = await child.wait()
        await tasks[1]
        if not release.is_set():
            raise RuntimeError("Native compaction ended before delayed outcome delivery")
        await tasks[3]
        if failures:
            raise RuntimeError("Slow compaction proxy transport failed")
        return status
    finally:
        if timer is not None:
            timer.cancel()
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
    directory = Path(tempfile.mkdtemp(prefix="tyde-real-slow-compaction-"))
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
    env = dict(
        os.environ, PATH=str(binary_directory) + os.pathsep + os.environ["PATH"],
        SHELL=str(shell), TYDE_SLOW_COMPACTION_EXECUTABLE=real,
        TYDE_SLOW_COMPACTION_DIRECTORY=str(directory),
        RUST_LOG="server::backend::muse::actor=debug", NO_COLOR="1",
    )
    print("Real slow compaction evidence:", directory, flush=True)
    with (directory / "run.log").open("w") as output:
        child = subprocess.Popen(
            [sys.argv[1], "--exact", sys.argv[2], "--ignored", "--nocapture"],
            env=env, stdout=output, stderr=subprocess.STDOUT, start_new_session=True,
        )
        try:
            status = child.wait(timeout=600)
        finally:
            try:
                os.killpg(child.pid, signal.SIGKILL)
            except ProcessLookupError:
                pass
            child.wait()
    proof = json.loads((directory / "transport.json").read_text())
    print("Slow compaction transport proof:", json.dumps(proof), flush=True)
    facts = ["admission_ack_passed_before_release", "durable_outcome_before_release",
             "durable_outcome_unchanged", "durable_target_matches_outcome",
             "durable_install_matches_outcome", "native_completed",
             "native_terminal_notification_held", "notifications_released",
             "notification_bytes_and_order_preserved"]
    assert all(proof[key] is True for key in facts), "Real delayed compaction proof was incomplete"
    assert proof["compaction_dispatches"] == 1, "Slow compaction must retain one native dispatch"
    assert proof["admission_to_release_ms"] >= HOLD_SECONDS * 1000, \
        "Native terminal delivery did not cross the removed 300-second cutoff"
    assert proof["notifications_held"] == proof["notifications_forwarded"] > 0, \
        "Native compaction notification bytes/order were not all preserved"
    for line in (directory / "run.log").read_text(errors="replace").splitlines():
        if line.startswith(("Slow native compaction terminal:",
                            "Slow native compaction lifecycle:", "test result:")):
            print(line, flush=True)
    return status


if __name__ == "__main__":
    try:
        sys.exit(asyncio.run(proxy()) if sys.argv[1:2] == ["--proxy"] else run())
    except Exception as error:
        context = str(error) if isinstance(error, (AssertionError, RuntimeError)) else "See private evidence"
        print("Slow compaction fixture failed:", type(error).__name__, context,
              file=sys.stderr, flush=True)
        sys.exit(1)
