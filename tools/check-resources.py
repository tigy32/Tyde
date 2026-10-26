#!/usr/bin/env python3
"""Synchronous ownership for check artifacts, never whole-check admission."""
from __future__ import annotations

import fcntl
import os
import pathlib
import shutil
import selectors
import socket
import threading
import subprocess
import sys
import tempfile
from contextlib import contextmanager


@contextmanager
def resource_lock(path: pathlib.Path, shared: bool = False):
    path.parent.mkdir(parents=True, exist_ok=True)
    with path.open("a+b") as lock:
        fcntl.flock(lock, fcntl.LOCK_SH if shared else fcntl.LOCK_EX)
        yield


def cleanup(logs: pathlib.Path, cache: pathlib.Path) -> int:
    reclaimed = 0
    for directory, pattern, keep in ((logs, "run-*", 7), (cache, "*.success", 64)):
        entries = sorted(directory.glob(pattern), key=lambda p: p.stat().st_mtime_ns, reverse=True)
        completed = []
        for path in entries:
            if directory == logs:
                lease = path / ".lease"
                if lease.is_file():
                    with lease.open("rb") as handle:
                        try:
                            fcntl.flock(handle, fcntl.LOCK_EX | fcntl.LOCK_NB)
                        except BlockingIOError:
                            continue
                        # The shared records lock excludes other cleaners;
                        # completed runners never reopen their private lease.
                owner = path / "owner"
                if not owner.is_file():
                    # Older runners have no lease: only terminal records are safe.
                    metadata = path / "metadata.txt"
                    if not metadata.is_file() or not any(
                        line in ("overall.result=PASS", "overall.result=FAIL")
                        for line in metadata.read_text().splitlines()
                    ):
                        continue
                elif not lease.is_file():
                    try:
                        os.kill(int(owner.read_text()), 0)
                        continue
                    except ProcessLookupError:
                        pass
            completed.append(path)
        for path in completed[keep:]:
            if path.is_dir():
                reclaimed += sum(p.stat().st_size for p in path.rglob("*") if p.is_file())
                shutil.rmtree(path)
            else:
                reclaimed += path.stat().st_size
                path.unlink()
    # Temporary publications are written under this same short resource lock.
    for path in cache.glob(".success-owned.*"):
        reclaimed += path.stat().st_size
        path.unlink()
    return reclaimed


def rtc_fixture(executable: str, command: list[str]) -> int:
    # Readiness and child exit are independent events; neither needs a timer.
    with tempfile.TemporaryDirectory(prefix="tyde-rtc-") as temporary:
        root = pathlib.Path(temporary)
        ready = root / "ready"
        os.mkfifo(ready)
        fifo = os.open(ready, os.O_RDWR | os.O_NONBLOCK)
        exited, notify = socket.socketpair()
        with (root / "fixture.log").open("w+b") as log:
            child = subprocess.Popen([executable, str(ready)], stdout=log, stderr=log)
            def wait_child():
                child.wait()
                notify.sendall(b"x")
            waiter = threading.Thread(target=wait_child)
            waiter.start()
            try:
                with selectors.DefaultSelector() as selector:
                    selector.register(fifo, selectors.EVENT_READ, "ready")
                    selector.register(exited, selectors.EVENT_READ, "exit")
                    events = selector.select()
                    if any(key.data == "exit" for key, _ in events):
                        raise RuntimeError("RTC fixture exited before readiness")
                    url = os.read(fifo, 4096).decode().strip()
                    if not url.startswith("http://127.0.0.1:"):
                        raise RuntimeError("RTC fixture emitted invalid readiness")
                result = subprocess.run(command, env=dict(os.environ, TYDE_RTC_TEST_URL=url), check=False)
                if result.returncode or child.poll() is not None:
                    if child.poll() is not None:
                        print("RTC fixture exited while browser tests were running", file=sys.stderr)
                    log.seek(0)
                    sys.stderr.write(log.read().decode(errors="replace"))
                    return result.returncode if result.returncode else 1
                return 0
            except BaseException:
                log.seek(0)
                sys.stderr.write(log.read().decode(errors="replace"))
                raise
            finally:
                if child.poll() is None:
                    child.terminate()
                waiter.join()
                exited.close()
                notify.close()
                os.close(fifo)


def main() -> int:
    action, *args = sys.argv[1:]
    if action == "rtc":
        executable, separator, *command = args
        if separator != "--" or not command:
            raise ValueError("rtc requires EXECUTABLE -- COMMAND")
        return rtc_fixture(executable, command)
    if action == "lock-fd":
        fcntl.flock(int(args[0]), fcntl.LOCK_SH if args[1:] == ["shared"] else fcntl.LOCK_EX)
    elif action == "lock":
        path, access, separator, *command = args
        if separator != "--" or access not in ("shared", "exclusive") or not command:
            raise ValueError("lock requires PATH shared|exclusive -- COMMAND")
        with resource_lock(pathlib.Path(path), access == "shared"):
            result = subprocess.run(command, check=False)
            return result.returncode if result.returncode >= 0 else 128 - result.returncode
    elif action in ("cleanup", "cache-read", "cache-publish"):
        cache = pathlib.Path(args[0])
        with resource_lock(cache / ".records.lock", action == "cache-read"):
            if action == "cleanup":
                print(cleanup(pathlib.Path(args[1]), cache))
            elif action == "cache-read":
                source, destination = map(pathlib.Path, args[1:])
                if source.is_file():
                    shutil.copyfile(source, destination)
            else:
                source, destination = map(pathlib.Path, args[1:])
                fd, name = tempfile.mkstemp(prefix=".success-owned.", dir=cache)
                temporary = pathlib.Path(name)
                try:
                    with os.fdopen(fd, "wb") as output, source.open("rb") as contents:
                        shutil.copyfileobj(contents, output)
                    os.replace(temporary, destination)
                finally:
                    temporary.unlink(missing_ok=True)
    else:
        raise ValueError("unknown check resource operation")
    return 0


if __name__ == "__main__":
    try:
        sys.exit(main())
    except (OSError, ValueError, RuntimeError) as error:
        print(f"Check resource operation failed: {error}", file=sys.stderr)
        sys.exit(1)
