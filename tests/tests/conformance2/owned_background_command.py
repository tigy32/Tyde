import os
from pathlib import Path
import select
import socket
import struct
import sys


def worker(address, proof):
    with socket.socket(socket.AF_UNIX, socket.SOCK_STREAM) as connection:
        connection.connect(address)
        connection.sendall(struct.pack("!I", os.getpid()))
        if connection.recv(1) != b"R":
            raise RuntimeError("background command lost its release owner")
        Path(proof).write_bytes(b"completed\n")


def watch(pid):
    if sys.platform == "linux":
        descriptor = os.pidfd_open(pid)
        try:
            sys.stdout.buffer.write(b"R")
            sys.stdout.buffer.flush()
            select.select([descriptor], [], [])
        finally:
            os.close(descriptor)
    elif sys.platform == "darwin":
        with select.kqueue() as queue:
            queue.control(
                [select.kevent(
                    pid,
                    filter=select.KQ_FILTER_PROC,
                    flags=select.KQ_EV_ADD | select.KQ_EV_ONESHOT,
                    fflags=select.KQ_NOTE_EXIT,
                )],
                0,
                None,
            )
            sys.stdout.buffer.write(b"R")
            sys.stdout.buffer.flush()
            events = queue.control(None, 1, None)
            if len(events) != 1 or not events[0].fflags & select.KQ_NOTE_EXIT:
                raise RuntimeError("process observer received no exit event")
    else:
        raise RuntimeError("native process-exit observation is unavailable")


if sys.argv[1] == "worker":
    worker(sys.argv[2], sys.argv[3])
elif sys.argv[1] == "watch":
    watch(int(sys.argv[2]))
else:
    raise RuntimeError("unknown owned-command fixture operation")
