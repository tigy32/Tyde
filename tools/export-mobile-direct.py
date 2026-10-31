#!/usr/bin/env python3
"""Export the production-linked bounded adapter; invoked only by dev.sh."""
import hashlib
import json
import os
from pathlib import Path
import shutil
import subprocess
import sys


def main():
    if len(sys.argv) != 2:
        raise RuntimeError("usage: ./dev.sh mobile-direct-export ABS_NEW_DIRECTORY")
    out = Path(sys.argv[1])
    if not out.is_absolute() or out.parent.resolve() != out.parent:
        raise RuntimeError("export parent must be canonical and absolute")
    if subprocess.check_output(["git", "status", "--porcelain"]):
        raise RuntimeError("commit the completed workbench before export")
    commit = subprocess.check_output(["git", "rev-parse", "HEAD"], text=True).strip()
    os.umask(0o077)
    out.mkdir(mode=0o700)
    compiler = subprocess.check_output(["rustc", "-vV"], text=True)
    hosts = [line.removeprefix("host: ") for line in compiler.splitlines()
             if line.startswith("host: ")]
    if len(hosts) != 1:
        raise RuntimeError("compiler did not report exactly one native target")
    command = ["cargo", "build", "--locked", "--target", hosts[0], "--message-format=json", "-p",
               "tyde-dev-driver", "--bin", "tyde-dev-driver"]
    with (out / "build.jsonl").open("wb") as output, (out / "build.log").open("wb") as errors:
        subprocess.run(command, stdout=output, stderr=errors, check=True)
    artifacts = [json.loads(line) for line in (out / "build.jsonl").read_text().splitlines()]
    binaries = [item["executable"] for item in artifacts
                if item.get("reason") == "compiler-artifact"
                and item.get("target", {}).get("name") == "tyde-dev-driver"
                and item.get("executable")]
    if len(binaries) != 1:
        raise RuntimeError("build did not emit exactly one adapter executable")
    binary = out / "tyde-dev-driver"
    shutil.copyfile(binaries[0], binary)
    binary.chmod(0o700)
    if subprocess.check_output(["git", "status", "--porcelain"]) or subprocess.check_output(
            ["git", "rev-parse", "HEAD"], text=True).strip() != commit:
        raise RuntimeError("source changed during export")
    files = subprocess.check_output(["git", "ls-files", "-z"]).decode().split("\0")
    source = {p: hashlib.sha256(Path(p).read_bytes()).hexdigest() for p in files if p}
    receipt = {"commit": commit, "command": "./dev.sh mobile-direct-export ABS_NEW_DIRECTORY",
               "cargo_command": command, "binary_sha256": hashlib.sha256(binary.read_bytes()).hexdigest(),
               "platform": sys.platform, "machine": os.uname().machine, "source_sha256": source,
               "target": hosts[0], "rustc": compiler.strip()}
    (out / "build-receipt.json").write_text(json.dumps(receipt, indent=2) + "\n")
    print(out)


if __name__ == "__main__":
    try:
        main()
    except (OSError, ValueError, RuntimeError, subprocess.CalledProcessError):
        sys.exit("mobile-direct export failed; inspect private build output")
