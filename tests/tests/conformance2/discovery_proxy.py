#!/usr/bin/env python3
import asyncio
import json
from pathlib import Path
import shutil
import sys
import time


async def run():
    config = json.loads(Path(__file__).with_suffix(".json").read_text())
    real = shutil.which(config["program"])
    if real is None:
        raise RuntimeError("the selected real provider CLI is required")
    child = await asyncio.create_subprocess_exec(
        real, *sys.argv[1:], stdin=asyncio.subprocess.PIPE,
        stdout=asyncio.subprocess.PIPE, stderr=asyncio.subprocess.PIPE,
        limit=32 * 1024 * 1024,
    )
    requests = {}

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

    async def forward_output():
        delayed = False
        while data := await child.stdout.readline():
            response = json.loads(data)
            method = None
            if "id" in response and "method" not in response:
                method = requests.pop(response["id"], None)
            if not delayed and method is not None and method != "initialize" and "result" in response:
                started = time.monotonic()
                await asyncio.sleep(config["delay_seconds"])
                Path(config["proof"]).write_text(json.dumps({
                    "held_seconds": time.monotonic() - started,
                    "native_result_bytes": len(data),
                }))
                delayed = True
            # Parsing selects a real pending reply; the forwarded bytes are never rebuilt.
            sys.stdout.buffer.write(data)
            sys.stdout.buffer.flush()

    async def drain_errors():
        while await child.stderr.read(65536):
            pass

    tasks = [asyncio.create_task(operation()) for operation in (
        forward_input, forward_output, drain_errors,
    )]

    def fail_transport(task):
        if not task.cancelled() and task.exception() is not None and child.returncode is None:
            child.kill()

    for task in tasks:
        task.add_done_callback(fail_transport)
    try:
        status = await child.wait()
        for task in tasks:
            if task.done() and not task.cancelled():
                task.result()
        return status
    finally:
        for task in tasks:
            task.cancel()
        await asyncio.gather(*tasks, return_exceptions=True)
        if child.returncode is None:
            child.kill()
        await child.wait()


if __name__ == "__main__":
    try:
        sys.exit(asyncio.run(run()))
    except Exception:
        print("Real discovery reply proxy failed", file=sys.stderr, flush=True)
        sys.exit(1)
