#!/usr/bin/env python3

from __future__ import annotations

import argparse
import json
import os
import pathlib
import shlex
import shutil
import subprocess
import sys
from contextlib import contextmanager


MESON_VERSION = "1.11.2"
NINJA_PACKAGE_VERSION = "1.11.1.4"


def executable(directory: pathlib.Path, name: str) -> pathlib.Path:
    suffix = ".exe" if os.name == "nt" else ""
    return directory / f"{name}{suffix}"


def venv_python(root: pathlib.Path) -> pathlib.Path:
    directory = root / ("Scripts" if os.name == "nt" else "bin")
    return executable(directory, "python")


def scripts_directory(root: pathlib.Path) -> pathlib.Path:
    return root / ("Scripts" if os.name == "nt" else "bin")


def installed_versions(python: pathlib.Path) -> tuple[str, str] | None:
    probe = subprocess.run(
        [
            str(python),
            "-c",
            "import importlib.metadata as m; print(m.version('meson')); print(m.version('ninja'))",
        ],
        text=True,
        capture_output=True,
        check=False,
    )
    if probe.returncode != 0:
        return None
    versions = probe.stdout.splitlines()
    if len(versions) != 2:
        return None
    return versions[0], versions[1]


def tool_version(tool: pathlib.Path) -> str | None:
    try:
        probe = subprocess.run(
            [str(tool), "--version"],
            text=True,
            capture_output=True,
            check=False,
        )
    except OSError:
        return None
    if probe.returncode != 0:
        return None
    return probe.stdout.strip()


def ready(root: pathlib.Path) -> bool:
    python = venv_python(root)
    directory = scripts_directory(root)
    if not python.is_file():
        return False
    meson = executable(directory, "meson")
    ninja = executable(directory, "ninja")
    if not meson.is_file():
        return False
    if not ninja.is_file():
        return False
    if installed_versions(python) != (MESON_VERSION, NINJA_PACKAGE_VERSION):
        return False
    return tool_version(meson) == MESON_VERSION and tool_version(ninja) is not None


@contextmanager
def provisioning_lock(root: pathlib.Path):
    root.parent.mkdir(parents=True, exist_ok=True)
    lock_path = root.with_name(f".{root.name}.lock")
    with lock_path.open("a+b") as lock:
        if os.name == "nt":
            import ctypes
            import msvcrt
            from ctypes import wintypes

            class Overlapped(ctypes.Structure):
                _fields_ = [("internal", ctypes.c_void_p), ("internal_high", ctypes.c_void_p),
                            ("offset", wintypes.DWORD), ("offset_high", wintypes.DWORD),
                            ("event", wintypes.HANDLE)]

            kernel = ctypes.WinDLL("kernel32", use_last_error=True)
            handle = wintypes.HANDLE(msvcrt.get_osfhandle(lock.fileno()))
            overlapped = Overlapped()
            kernel.LockFileEx.argtypes = [wintypes.HANDLE, wintypes.DWORD, wintypes.DWORD,
                                           wintypes.DWORD, wintypes.DWORD, ctypes.POINTER(Overlapped)]
            kernel.UnlockFileEx.argtypes = [wintypes.HANDLE, wintypes.DWORD, wintypes.DWORD,
                                             wintypes.DWORD, ctypes.POINTER(Overlapped)]
            if not kernel.LockFileEx(handle, 2, 0, 1, 0, ctypes.byref(overlapped)):
                raise ctypes.WinError(ctypes.get_last_error())
            try:
                yield
            finally:
                if not kernel.UnlockFileEx(handle, 0, 1, 0, ctypes.byref(overlapped)):
                    raise ctypes.WinError(ctypes.get_last_error())
        else:
            import fcntl

            fcntl.flock(lock.fileno(), fcntl.LOCK_EX)
            try:
                yield
            finally:
                fcntl.flock(lock.fileno(), fcntl.LOCK_UN)



def provision(root: pathlib.Path) -> pathlib.Path:
    with provisioning_lock(root):
        return provision_locked(root)


def require_cached(root: pathlib.Path) -> pathlib.Path:
    with provisioning_lock(root):
        if not ready(root):
            raise RuntimeError(
                "pinned native build tools are not cached; run ./dev.sh check first"
            )
        return scripts_directory(root)


def provision_locked(root: pathlib.Path) -> pathlib.Path:
    if not ready(root):
        if (root / "tyde-native-build-tools.json").exists():
            raise RuntimeError("published native build tools failed verification; refusing to replace tools another build may be using")
        shutil.rmtree(root, ignore_errors=True)
        root.parent.mkdir(parents=True, exist_ok=True)
        subprocess.run([sys.executable, "-m", "venv", str(root)], check=True)
        python = venv_python(root)
        subprocess.run(
            [
                str(python),
                "-m",
                "pip",
                "install",
                "--disable-pip-version-check",
                "--no-input",
                f"meson=={MESON_VERSION}",
                f"ninja=={NINJA_PACKAGE_VERSION}",
            ],
            check=True,
        )
    if not ready(root):
        raise RuntimeError("the pinned Meson and Ninja executables did not verify")
    marker = root / "tyde-native-build-tools.json"
    marker.write_text(
        json.dumps(
            {
                "meson": MESON_VERSION,
                "ninja_package": NINJA_PACKAGE_VERSION,
                "python": sys.version.split()[0],
            },
            sort_keys=True,
        )
        + "\n",
        encoding="utf-8",
    )
    return scripts_directory(root)


def prepared_environment(root: pathlib.Path, directory: pathlib.Path) -> str:
    path_prefix = shlex.quote(f"{directory}:")
    return (
        f"export TYDE_NATIVE_BUILD_TOOLS_ROOT={shlex.quote(str(root.resolve()))}\n"
        f'export PATH={path_prefix}"${{PATH-}}"\n'
    )


def main() -> int:
    parser = argparse.ArgumentParser(
        description="Provision pinned native audio build tools without system mutation."
    )
    parser.add_argument("--prepare", type=pathlib.Path)
    parser.add_argument("--github-path", type=pathlib.Path)
    parser.add_argument("--github-env", type=pathlib.Path)
    parser.add_argument("--root", type=pathlib.Path)
    parser.add_argument("--offline", action="store_true")
    args = parser.parse_args()
    if args.prepare is None and args.github_path is None:
        parser.error("one of --prepare or --github-path is required")
    repository = pathlib.Path(__file__).resolve().parent.parent
    root = args.root or pathlib.Path(
        os.environ.get(
            "TYDE_NATIVE_BUILD_TOOLS_ROOT",
            repository / "target" / "native-build-tools",
        )
    )
    try:
        directory = require_cached(root.resolve()) if args.offline else provision(root.resolve())
    except (OSError, subprocess.SubprocessError, RuntimeError) as error:
        print(
            "ERROR: automatic native audio build-tool provisioning failed; "
            f"Meson {MESON_VERSION} and Ninja package {NINJA_PACKAGE_VERSION} "
            f"are required by webrtc-audio-processing-sys: {error}",
            file=sys.stderr,
        )
        return 1
    if args.prepare is not None:
        args.prepare.parent.mkdir(parents=True, exist_ok=True)
        args.prepare.write_text(
            prepared_environment(root, directory),
            encoding="utf-8",
        )
    if args.github_path is not None:
        with args.github_path.open("a", encoding="utf-8") as output:
            output.write(f"{directory}\n")
    if args.github_env is not None:
        with args.github_env.open("a", encoding="utf-8") as output:
            output.write(f"TYDE_NATIVE_BUILD_TOOLS_ROOT={root.resolve()}\n")
    print(
        f"native tools ready: meson {MESON_VERSION}, "
        f"ninja package {NINJA_PACKAGE_VERSION}"
    )
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
