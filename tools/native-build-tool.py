#!/usr/bin/env python3

from __future__ import annotations

import importlib.util
import os
import pathlib
import subprocess
import sys


def load_provisioner():
    path = pathlib.Path(__file__).with_name("provision-native-build-tools.py")
    spec = importlib.util.spec_from_file_location("tyde_native_build_tools", path)
    if spec is None or spec.loader is None:
        raise RuntimeError(f"could not load native build-tool provisioner at {path}")
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module


def child_exit_status(returncode: int) -> int:
    if returncode < 0:
        return 128 - returncode
    return returncode


def run_tool(executable: pathlib.Path, arguments: list[str]) -> int:
    completed = subprocess.run([str(executable), *arguments], check=False)
    return child_exit_status(completed.returncode)


def main() -> int:
    if len(sys.argv) < 2 or sys.argv[1] not in {"meson", "ninja"}:
        print("usage: native-build-tool.py {meson|ninja} [--] [arguments...]", file=sys.stderr)
        return 2
    tool = sys.argv[1]
    arguments = sys.argv[2:]
    if arguments[:1] == ["--"]:
        arguments = arguments[1:]
    repository = pathlib.Path(__file__).resolve().parent.parent
    root = pathlib.Path(
        os.environ.get(
            "TYDE_NATIVE_BUILD_TOOLS_ROOT",
            repository / "target" / "native-build-tools",
        )
    ).resolve()
    try:
        provisioner = load_provisioner()
        directory = provisioner.provision(root)
        executable = provisioner.executable(directory, tool)
    except (OSError, subprocess.SubprocessError, RuntimeError) as error:
        print(
            f"ERROR: could not provision pinned native build tool {tool}: {error}",
            file=sys.stderr,
        )
        return 1
    current_path = os.environ.get("PATH")
    os.environ["PATH"] = (
        str(directory)
        if not current_path
        else os.pathsep.join((str(directory), current_path))
    )
    return run_tool(executable, arguments)


if __name__ == "__main__":
    raise SystemExit(main())
