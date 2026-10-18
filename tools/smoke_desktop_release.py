#!/usr/bin/env python3
from __future__ import annotations

import argparse
import os
import pathlib
import queue
import signal
import subprocess
import sys
import threading
import time


FRONTEND_READY = "webview.lifecycle event=frontend_ready label=main "
HOST_READY = "proxy router received line from host"


def launch(binary: pathlib.Path, log_directory: pathlib.Path, shell: str | None) -> None:
    log_directory.mkdir(parents=True, exist_ok=True)
    environment = os.environ.copy()
    environment.update(
        RUST_LOG="info", RUST_BACKTRACE="full",
        TYDE_TRACING_DIR_PATH=str(log_directory / "tracing"),
    )
    if shell is not None:
        environment["SHELL"] = shell
    output: queue.Queue[str | None] = queue.Queue()
    with (log_directory / "startup.log").open("w", encoding="utf-8") as log:
        process = subprocess.Popen(
            [str(binary)], cwd=binary.parent, env=environment,
            stdin=subprocess.DEVNULL, stdout=subprocess.PIPE,
            stderr=subprocess.STDOUT, text=True, errors="replace",
            start_new_session=True,
        )

        def read_output() -> None:
            try:
                for line in process.stdout:
                    log.write(line)
                    log.flush()
                    output.put(line)
            finally:
                output.put(None)

        reader = threading.Thread(target=read_output)
        reader.start()
        deadline = time.monotonic() + 600
        frontend_ready = False
        host_ready = False
        try:
            while not (frontend_ready and host_ready):
                try:
                    line = output.get(timeout=max(0, deadline - time.monotonic()))
                except queue.Empty as error:
                    raise RuntimeError("Desktop startup test hung for ten minutes") from error
                if line is None:
                    raise RuntimeError(
                        f"Desktop exited before frontend and host readiness (exit {process.wait()})"
                    )
                frontend_ready |= FRONTEND_READY in line
                host_ready |= HOST_READY in line
            try:
                status = process.wait(timeout=5)
            except subprocess.TimeoutExpired:
                pass
            else:
                raise RuntimeError(f"Desktop exited after readiness (exit {status})")
        finally:
            try:
                os.killpg(process.pid, signal.SIGKILL)
            except ProcessLookupError:
                pass
            process.wait()
            reader.join()
            process.stdout.close()
    print(f"PASS desktop frontend and local host startup ({shell or sys.platform})")


def main() -> int:
    parser = argparse.ArgumentParser(description="Launch the real release desktop and await readiness.")
    parser.add_argument("--binary", type=pathlib.Path, required=True)
    parser.add_argument("--expected-version", required=True)
    parser.add_argument("--log-directory", type=pathlib.Path, required=True)
    args = parser.parse_args()
    binary = args.binary.resolve(strict=True)
    logs = args.log_directory.resolve()
    if sys.platform == "win32":
        return subprocess.run([
            "pwsh", "-NoProfile", "-File",
            str(pathlib.Path(__file__).with_name("smoke_windows_release.ps1")),
            "-Binary", str(binary), "-ExpectedVersion", args.expected_version,
            "-LogDirectory", str(logs),
        ], check=False).returncode
    try:
        actual = subprocess.run(
            [str(binary), "--version"], check=True, capture_output=True, text=True,
        ).stdout.strip()
        if actual != args.expected_version.removeprefix("v"):
            raise RuntimeError(f"Desktop version mismatch: {actual}")
        shells = ["/bin/zsh", "/bin/bash"] if sys.platform == "darwin" else [None]
        for shell in shells:
            launch(binary, logs / pathlib.Path(shell or sys.platform).name, shell)
    except (OSError, RuntimeError, subprocess.CalledProcessError) as error:
        print(f"FAIL {error}", file=sys.stderr)
        for path in logs.glob("*/startup.log"):
            print(f"--- {path} ---\n{path.read_text(encoding='utf-8')}", file=sys.stderr)
        return 1
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
