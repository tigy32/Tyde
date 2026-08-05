#!/usr/bin/env python3
from __future__ import annotations

import argparse
import json
import os
import pathlib
import signal
import stat
import subprocess
import tempfile
import time


REPO_ROOT = pathlib.Path(__file__).resolve().parent.parent


def expected_version(value: str | None) -> str:
    if value:
        return value[1:] if value.startswith("v") else value
    package = json.loads((REPO_ROOT / "package.json").read_text(encoding="utf-8"))
    return str(package["version"])


def binary_path(target: str) -> pathlib.Path:
    suffix = ".exe" if target.endswith("windows-msvc") else ""
    return REPO_ROOT / "target" / target / "release" / f"tyde-server{suffix}"


def smoke(target: str, version: str | None) -> None:
    binary = binary_path(target)
    actual = subprocess.run(
        [str(binary), "--version"],
        check=True,
        capture_output=True,
        text=True,
    ).stdout.strip()
    wanted = expected_version(version)
    print(f"tyde-server --version -> {actual}")
    if actual != wanted:
        raise SystemExit(f"version mismatch: binary={actual} expected={wanted}")
    if target.endswith("windows-msvc"):
        return

    socket_path = pathlib.Path(tempfile.gettempdir()) / f"tyde-smoke-{os.getpid()}.sock"
    socket_path.unlink(missing_ok=True)
    environment = os.environ.copy()
    environment["TYDE_SOCKET_PATH"] = str(socket_path)
    process = subprocess.Popen([str(binary), "host", "--uds"], env=environment)
    ready = False
    try:
        for _ in range(100):
            try:
                ready = stat.S_ISSOCK(socket_path.stat().st_mode)
            except FileNotFoundError:
                ready = False
            if ready or process.poll() is not None:
                break
            time.sleep(0.2)
    finally:
        if process.poll() is None:
            process.send_signal(signal.SIGTERM)
        try:
            process.wait(timeout=5)
        except subprocess.TimeoutExpired:
            process.kill()
            process.wait()
        socket_path.unlink(missing_ok=True)
    if not ready:
        raise SystemExit("tyde-server host --uds never opened its socket")
    print("headless host opened its socket — startup smoke passed")


def main() -> None:
    parser = argparse.ArgumentParser()
    parser.add_argument("--target", required=True)
    parser.add_argument("--expected-version")
    args = parser.parse_args()
    smoke(args.target, args.expected_version)


if __name__ == "__main__":
    main()
