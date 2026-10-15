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
import threading


def strings(value):
    if isinstance(value, str):
        yield value
    elif isinstance(value, dict):
        for member in value.values():
            yield from strings(member)
    elif isinstance(value, list):
        for member in value:
            yield from strings(member)


def proxy():
    real = os.environ["TYDE_READ_COMPLETION_EXECUTABLE"]
    if not any(argument in {"serve", "stream-json"} for argument in sys.argv[2:]):
        os.execv(real, [real, *sys.argv[2:]])
    directory = Path(os.environ["TYDE_READ_COMPLETION_RACE_DIRECTORY"])
    capture = Path(tempfile.mkdtemp(prefix="transport-", dir=directory))
    stderr = (capture / "native-stderr.log").open("wb")
    received = (capture / "native-frames.jsonl").open("wb")
    sent = (capture / "client-frames.jsonl").open("wb")
    child = subprocess.Popen(
        [real, *sys.argv[2:]], stdin=subprocess.PIPE, stdout=subprocess.PIPE,
        stderr=stderr,
    )
    lock = threading.Lock()
    state = {"armed": None, "released": False, "input_failed": False}
    calls = set()
    held = []
    observed_digest = hashlib.sha256()
    forwarded_digest = hashlib.sha256()
    proof = {
        "armed_before_prompt_forwarded": False,
        "native_read_declaration_observed": False,
        "native_successful_read_completion_held": False,
        "native_original_contents_present": False,
        "original_file_present_before_removal": False,
        "file_removed_before_read_turn_output_forwarded": False,
        "read_turn_bytes_and_order_preserved": False,
        "native_successful_read_completions": 0,
        "held_frames": 0,
        "forwarded_frames": 0,
        "original_file_bytes": 0,
        "fabricated_frames": 0,
        "modified_frames": 0,
    }

    def save():
        path = directory / "proof.json"
        temporary = capture / "proof.tmp"
        temporary.write_text(json.dumps(proof))
        temporary.replace(path)

    def forward_input():
        try:
            for data in sys.stdin.buffer:
                sent.write(data)
                sent.flush()
                message = json.loads(data)
                arm = directory / "arm.json"
                with lock:
                    if state["armed"] is None and arm.exists():
                        candidate = json.loads(arm.read_text())
                        if any(candidate["path"] in text for text in strings(message)):
                            state["armed"] = candidate
                            proof["armed_before_prompt_forwarded"] = True
                            save()
                child.stdin.write(data)
                child.stdin.flush()
        except (BrokenPipeError, OSError, ValueError):
            with lock:
                state["input_failed"] = True
            if child.poll() is None:
                child.kill()
        finally:
            child.stdin.close()

    def complete_read(message, target):
        def same_path(value):
            if not isinstance(value, str):
                return False
            path = Path(value)
            if not path.is_absolute():
                path = Path(target["workspace_root"]) / path
            return path.resolve(strict=True) == Path(target["path"]).resolve(strict=True)

        item = message.get("params", {}).get("item", {})
        if item.get("kind") == "toolCall" and item.get("tool") == "read_file":
            arguments = json.loads(item["args"])
            if same_path(arguments.get("path")):
                if message.get("method") == "item/started":
                    calls.add(item["callId"])
                    proof["native_read_declaration_observed"] = True
                if (message.get("method") == "item/completed"
                        and item.get("status") == "completed"
                        and item.get("callId") in calls):
                    return item.get("visibleOutput", "")
        content = message.get("message", {}).get("content", [])
        if isinstance(content, list):
            for block in content:
                if (block.get("type") == "tool_use" and block.get("name") == "Read"
                        and same_path(block.get("input", {}).get("file_path"))):
                    calls.add(block["id"])
                    proof["native_read_declaration_observed"] = True
                if (block.get("type") == "tool_result"
                        and block.get("tool_use_id") in calls
                        and block.get("is_error") is not True):
                    return "\n".join(strings(block.get("content", "")))
        return None

    def forward(data):
        sys.stdout.buffer.write(data)
        sys.stdout.buffer.flush()

    thread = threading.Thread(target=forward_input, daemon=True)
    thread.start()
    try:
        for data in child.stdout:
            received.write(data)
            received.flush()
            with lock:
                target = state["armed"]
                released = state["released"]
            if target is None or released:
                forward(data)
                continue
            # Hold the entire native stream, not just item/completed: Muse's
            # earlier source-range notifications can expose its durable ledger.
            held.append(data)
            observed_digest.update(data)
            proof["held_frames"] += 1
            output = complete_read(json.loads(data), target)
            if output is None:
                continue
            proof["native_successful_read_completions"] += 1
            proof["native_successful_read_completion_held"] = True
            proof["native_original_contents_present"] = target["contents"].strip() in output
            path = Path(target["path"])
            original = path.read_bytes()
            proof["original_file_bytes"] = len(original)
            proof["original_file_present_before_removal"] = original == target["contents"].encode()
            save()
            if not (proof["native_original_contents_present"]
                    and proof["original_file_present_before_removal"]):
                raise RuntimeError("Native read did not return the original unseen file")
            path.unlink()
            proof["file_removed_before_read_turn_output_forwarded"] = not path.exists()
            save()
            for original_frame in held:
                forward(original_frame)
                forwarded_digest.update(original_frame)
                proof["forwarded_frames"] += 1
            proof["read_turn_bytes_and_order_preserved"] = (
                observed_digest.digest() == forwarded_digest.digest())
            held.clear()
            with lock:
                state["released"] = True
                (directory / "arm.json").unlink()
                save()
        status = child.wait()
        if state["input_failed"]:
            raise RuntimeError("Native read transport input failed")
        if state["armed"] is not None and not state["released"]:
            raise RuntimeError("No genuine successful native read crossed the barrier")
        return status
    finally:
        if child.poll() is None:
            child.kill()
        child.wait()
        received.close()
        stderr.close()


def run():
    os.umask(0o077)
    real = shutil.which(sys.argv[3])
    if real is None:
        raise RuntimeError("The selected real provider CLI is required")
    directory = Path(tempfile.mkdtemp(prefix="tyde-real-read-completion-race-"))
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
        SHELL=str(shell), TYDE_READ_COMPLETION_EXECUTABLE=real,
        TYDE_READ_COMPLETION_RACE_DIRECTORY=str(directory),
    )
    print("Real native read completion evidence:", directory, flush=True)
    with (directory / "run.log").open("w") as output:
        child = subprocess.Popen(
            [sys.argv[1], "--exact", sys.argv[2], "--ignored", "--nocapture"],
            env=env, stdout=output, stderr=subprocess.STDOUT, start_new_session=True,
        )
        try:
            status = child.wait()
        finally:
            try:
                os.killpg(child.pid, signal.SIGKILL)
            except ProcessLookupError:
                pass
            child.wait()
    proof = json.loads((directory / "proof.json").read_text())
    print("Native read completion transport proof:", json.dumps(proof), flush=True)
    facts = [
        "armed_before_prompt_forwarded", "native_read_declaration_observed",
        "native_successful_read_completion_held", "native_original_contents_present",
        "original_file_present_before_removal",
        "file_removed_before_read_turn_output_forwarded", "read_turn_bytes_and_order_preserved",
    ]
    if (not all(proof[fact] is True for fact in facts)
            or proof["native_successful_read_completions"] != 1
            or proof["held_frames"] == 0
            or proof["held_frames"] != proof["forwarded_frames"]
            or proof["fabricated_frames"] != 0 or proof["modified_frames"] != 0):
        raise RuntimeError("The native read transport did not establish the actual filesystem race")
    print("Native read completion case passed:", status == 0, flush=True)
    return status


if __name__ == "__main__":
    try:
        sys.exit(proxy() if sys.argv[1:2] == ["--proxy"] else run())
    except Exception as error:
        print("Native read completion fixture failed:", type(error).__name__,
              "See private evidence", file=sys.stderr, flush=True)
        sys.exit(1)
