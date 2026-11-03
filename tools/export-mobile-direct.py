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
    if len(sys.argv) not in (2, 4) or (len(sys.argv) == 4 and sys.argv[2] != "--qa-origin"):
        raise RuntimeError("usage: ./dev.sh mobile-direct-export ABS_NEW_DIRECTORY [--qa-origin HTTPS_ORIGIN]")
    if "TYDE_NATIVE_QA_ORIGIN" in os.environ:
        raise RuntimeError("Select QA publication only through the explicit --qa-origin build option")
    config = json.loads(subprocess.check_output([sys.executable, "tools/native-bootstrap-config.py", *sys.argv[2:]], text=True))
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
    if config["kind"] == "qa":
        command.extend(["-p", "tyde-server", "--bin", "tyde-server"])
    with (out / "build.jsonl").open("wb") as output, (out / "build.log").open("wb") as errors:
        environment = os.environ.copy()
        if config["kind"] == "qa":
            environment["TYDE_NATIVE_QA_ORIGIN"] = config["origin"]
        subprocess.run(command, stdout=output, stderr=errors, check=True, env=environment)
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
    host = None
    if config["kind"] == "qa":
        hosts_built = [item["executable"] for item in artifacts
                      if item.get("reason") == "compiler-artifact"
                      and item.get("target", {}).get("name") == "tyde-server"
                      and item.get("executable")]
        if len(hosts_built) != 1:
            raise RuntimeError("QA export did not emit its actual host executable")
        host = out / "tyde-server"
        shutil.copyfile(hosts_built[0], host)
        host.chmod(0o700)
    if subprocess.check_output(["git", "status", "--porcelain"]) or subprocess.check_output(
            ["git", "rev-parse", "HEAD"], text=True).strip() != commit:
        raise RuntimeError("source changed during export")
    files = subprocess.check_output(["git", "ls-files", "-z"]).decode().split("\0")
    source = {p: hashlib.sha256(Path(p).read_bytes()).hexdigest() for p in files if p}
    receipt = {"commit": commit, "command": "./dev.sh mobile-direct-export ABS_NEW_DIRECTORY",
               "bootstrap_configuration": config, "binary_sha256": hashlib.sha256(binary.read_bytes()).hexdigest(),
               "platform": sys.platform, "machine": os.uname().machine, "source_sha256": source,
               "target": hosts[0], "rustc": compiler.strip()}
    if host is not None:
        receipt["qa_host_binary"] = str(host)
        receipt["qa_host_binary_sha256"] = hashlib.sha256(host.read_bytes()).hexdigest()
    (out / "build-receipt.json").write_text(json.dumps(receipt, indent=2) + "\n")
    print(out)


if __name__ == "__main__":
    try:
        main()
    except (OSError, ValueError, RuntimeError, subprocess.CalledProcessError):
        sys.exit("mobile-direct export failed; inspect private build output")
