#!/usr/bin/env python3

from __future__ import annotations

import argparse
import os
import math
import queue
import re
import runpy
import signal
import subprocess
import sys
import threading
import time
import unittest


def stop_process_tree(child: subprocess.Popen) -> None:
    if os.name == "posix":
        try:
            os.killpg(child.pid, signal.SIGKILL)
        except ProcessLookupError:
            pass
    else:
        subprocess.run(
            ["taskkill", "/pid", str(child.pid), "/t", "/f"],
            stdout=subprocess.DEVNULL,
            stderr=subprocess.DEVNULL,
            check=False,
        )


def run(command: list[str], hang_seconds: float, browser: bool = True) -> int:
    env = os.environ.copy()
    # The upstream browser timeout covers the whole suite, not one test. Its
    # parser has no disabled value; u64::MAX removes that practical cutoff.
    env["TYDE_TEST_HANG_SECONDS"] = str(hang_seconds)
    if browser:
        env["WASM_BINDGEN_TEST_TIMEOUT"] = str(2**64 - 1)
        env["WASM_BINDGEN_TEST_DRIVER_TIMEOUT"] = "600"
        env.pop("WASM_BINDGEN_TEST_NO_STREAM", None)
    child = subprocess.Popen(
        command,
        env=env,
        stdout=subprocess.PIPE,
        start_new_session=os.name == "posix",
    )
    output: queue.Queue[bytes | None] = queue.Queue()

    def read_output() -> None:
        try:
            for line in child.stdout:
                output.put(line)
        finally:
            output.put(None)

    reader = threading.Thread(target=read_output)
    reader.start()
    deadline = None
    remaining = 0
    completed = 0

    def hung() -> int:
        print(
            f"Hung browser test: test {completed + 1} has not finished "
            f"after {hang_seconds:g}s; {completed} completed tests retained",
            file=sys.stderr,
            flush=True,
        )
        stop_process_tree(child)
        child.wait()
        return 124

    try:
        while True:
            if deadline is not None and time.monotonic() >= deadline:
                return hung()
            seconds = None if deadline is None else max(0, deadline - time.monotonic())
            try:
                line = output.get(timeout=seconds)
            except queue.Empty:
                return hung()
            if line is None:
                if deadline is None:
                    return child.wait()
                try:
                    return child.wait(timeout=max(0, deadline - time.monotonic()))
                except subprocess.TimeoutExpired:
                    print("Hung browser test runner after output closed", file=sys.stderr)
                    stop_process_tree(child)
                    child.wait()
                    return 124
            sys.stdout.buffer.write(line)
            sys.stdout.buffer.flush()
            if not browser:
                continue
            text = line.decode("utf-8", errors="replace").strip()
            start = re.fullmatch(r"running (\d+) tests?", text)
            if start:
                remaining = int(start.group(1))
                deadline = time.monotonic() + hang_seconds if remaining else None
            elif remaining and re.fullmatch(r"test \S+ \.\.\. (?:ok|FAIL|ignored.*)", text):
                completed += 1
                remaining -= 1
                deadline = time.monotonic() + hang_seconds if remaining else None
            elif text.startswith("test result:"):
                deadline = None
    finally:
        stop_process_tree(child)
        child.wait()
        reader.join()
        child.stdout.close()


def run_python_tests(command: list[str], hang_seconds: float) -> int:
    if os.name == "posix" and os.getpgrp() != os.getpid():
        os.setsid()
    lock = threading.Lock()
    active = 0
    timer = None
    original_start = unittest.TestResult.startTest
    original_stop = unittest.TestResult.stopTest

    def start_test(result, test):
        nonlocal active, timer
        original_start(result, test)
        with lock:
            active += 1
            identity = active

            def terminate():
                with lock:
                    if active != identity:
                        return
                    print(f"Hung Python test: {test.id()} exceeded {hang_seconds:g}s", file=sys.stderr, flush=True)
                    if os.name == "posix":
                        os.killpg(os.getpgrp(), signal.SIGKILL)
                    else:
                        subprocess.run(["taskkill", "/pid", str(os.getpid()), "/t", "/f"], check=False)
                    os._exit(124)

            timer = threading.Timer(hang_seconds, terminate)
            timer.daemon = True
            timer.start()

    def stop_test(result, test):
        nonlocal active
        with lock:
            active += 1
            timer.cancel()
        original_stop(result, test)

    unittest.TestResult.startTest = start_test
    unittest.TestResult.stopTest = stop_test
    if command[:2] == ["-m", "unittest"]:
        # Script launch puts this file's directory on sys.path, replacing the
        # working directory `python -m unittest` uses as the only module root.
        sys.path[0] = ""
        sys.argv = command[1:]
        runpy.run_module("unittest", run_name="__main__")
    else:
        sys.argv = command
        sys.path.insert(0, os.path.dirname(os.path.abspath(command[0])))
        runpy.run_path(command[0], run_name="__main__")
    return 0


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("--hang-seconds", type=float, default=600)
    modes = parser.add_mutually_exclusive_group()
    modes.add_argument("--python-tests", action="store_true")
    modes.add_argument("--node-tests", action="store_true")
    parser.add_argument("command", nargs=argparse.REMAINDER)
    args = parser.parse_args()
    command = args.command[1:] if args.command[:1] == ["--"] else args.command
    if not math.isfinite(args.hang_seconds) or args.hang_seconds <= 0 or not command:
        parser.error("a positive hung-test limit and runner command are required")
    if args.python_tests:
        return run_python_tests(command, args.hang_seconds)
    if args.node_tests:
        watchdog = os.path.join(os.path.dirname(os.path.abspath(__file__)), "node-test-hang-guard.mjs")
        return run(["node", "--test", f"--import={watchdog}", "--test-reporter=spec", *command], args.hang_seconds, browser=False)
    return run(command, args.hang_seconds)


if __name__ == "__main__":
    sys.exit(main())