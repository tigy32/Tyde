import fcntl
import os
from pathlib import Path
import signal
import subprocess
import sys
import tempfile
import termios


# Give the test a controlling terminal even when the check runs without one.
os.setsid()
master, slave = os.openpty()
fcntl.ioctl(slave, termios.TIOCSCTTY, 0)
assert os.tcgetpgrp(slave) == os.getpgrp()

try:
    with tempfile.TemporaryDirectory(prefix="tyde-path-probe-") as directory:
        root = Path(directory)
        launches = root / "launches"
        shell = root / "login-shell"
        shell.write_text(
            '#!/bin/sh\n'
            'printf "%s\\n" "$$" >> "$TYDE_PATH_PROBE_TEST_LAUNCHES"\n'
            'sleep 0.2\n'
            'exec /bin/bash "$@"\n'
        )
        shell.chmod(0o700)
        for role in ("probe", "failure"):
            environment = dict(os.environ)
            environment.update(
                TYDE_PATH_PROBE_TEST_ROLE=role,
                TYDE_PATH_PROBE_TEST_LAUNCHES=str(launches),
                SHELL=str(shell if role == "probe" else root / "missing-shell"),
            )
            child = subprocess.Popen(
                [sys.argv[1], "--exact", "login_shell_initialization_from_background_terminal", "--nocapture"],
                env=environment,
                stdin=subprocess.DEVNULL,
                stdout=subprocess.PIPE,
                stderr=subprocess.STDOUT,
                preexec_fn=os.setpgrp,
            )
            try:
                assert os.tcgetpgrp(slave) != child.pid
                try:
                    output, _ = child.communicate(timeout=10)
                except subprocess.TimeoutExpired:
                    print(f"{role}: PATH initialization stalled in background process group {child.pid}", flush=True)
                    raise
                print(output.decode(errors="replace"), flush=True)
                assert child.returncode == 0, f"{role}: child exited {child.returncode}"
            finally:
                # The broken probe leaves stopped Bash children in this group.
                try:
                    os.killpg(child.pid, signal.SIGKILL)
                except ProcessLookupError:
                    pass
                child.wait()
            if role == "probe":
                pids = launches.read_text().splitlines()
                assert len(pids) == 1, f"concurrent PATH callers launched {len(pids)} shells: {pids}"
finally:
    signal.signal(signal.SIGHUP, signal.SIG_IGN)
    os.close(slave)
    os.close(master)
