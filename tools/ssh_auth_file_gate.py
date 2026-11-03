import ctypes, os, signal, socket, sys, platform

assert platform.machine() == "x86_64", "Fixture requires x86_64 syscall registers"
libc = ctypes.CDLL(None, use_errno=True)
libc.ptrace.restype = ctypes.c_long


class Regs(ctypes.Structure):
    _fields_ = [
        (n, ctypes.c_ulonglong)
        for n in "r15 r14 r13 r12 rbp rbx r11 r10 r9 r8 rax rcx rdx rsi rdi orig_rax rip cs eflags rsp ss fs_base gs_base ds es fs gs".split()
    ]


def ptrace(request, pid, addr=0, data=0):
    ctypes.set_errno(0)
    r = libc.ptrace(request, pid, ctypes.c_void_p(addr), ctypes.c_void_p(data))
    if r == -1 and ctypes.get_errno():
        raise OSError(ctypes.get_errno(), os.strerror(ctypes.get_errno()))
    return r


def readstr(pid, addr):
    out = b""
    for i in range(0, 1024, 8):
        data = ptrace(2, pid, addr + i) & ((1 << 64) - 1)
        part = data.to_bytes(8, "little")
        out += part
        if b"\0" in part:
            return out.split(b"\0")[0]
    raise RuntimeError("Fixture syscall pathname exceeded bound")


config, keyfile, sock = sys.argv[1:]
pid = os.fork()
if pid == 0:
    ptrace(0, 0)
    os.kill(os.getpid(), signal.SIGSTOP)
    os.execv("/usr/sbin/sshd", ["/usr/sbin/sshd", "-D", "-e", "-f", config])
os.waitpid(pid, 0)
ptrace(0x4200, pid, 0, 1 | 2 | 4 | 8 | 16 | 0x100000)
ptrace(24, pid)
while True:
    try:
        p, status = os.waitpid(-1, 0)
    except ChildProcessError:
        break
    if os.WIFEXITED(status) or os.WIFSIGNALED(status):
        continue
    sig = os.WSTOPSIG(status)
    if sig == signal.SIGTRAP | 0x80:
        regs = Regs()
        ptrace(12, p, 0, ctypes.addressof(regs))
        if (
            regs.orig_rax == 257
            and regs.rax == (1 << 64) - 38
            and readstr(p, regs.rsi) == keyfile.encode()
        ):
            gate = socket.socket(socket.AF_UNIX)
            gate.connect(sock)
            gate.sendall(b"AUTH_DECISION_ENTERED")
            assert (
                gate.recv(1) == b"R"
            ), "Authorization gate released without explicit decision"
            gate.close()
        ptrace(24, p)
    elif sig in (signal.SIGSTOP, signal.SIGTRAP):
        ptrace(24, p)
    else:
        ptrace(24, p, 0, sig)
