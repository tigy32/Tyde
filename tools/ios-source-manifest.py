#!/usr/bin/env python3
"""Bind the native executable bundle to the exact reviewed source inputs."""
import hashlib
import json
from pathlib import Path
import subprocess
import sys

ROOT = Path(__file__).resolve().parent.parent
MANIFEST = ROOT / "target/ios-client/source-manifest.json"


def digest(data):
    return hashlib.sha256(data).hexdigest()


def source_files():
    result = subprocess.run(
        ["git", "ls-files", "--cached", "--others", "--exclude-standard", "-z"],
        cwd=ROOT, check=True, capture_output=True,
    )
    paths = sorted(set(result.stdout.decode().rstrip("\0").split("\0")))
    return {
        path: digest((ROOT / path).read_bytes()) for path in paths
        if (ROOT / path).is_file()
    }


def main():
    if sys.argv[1:] == ["write"]:
        files = source_files()
        fingerprint = digest(json.dumps(files, sort_keys=True).encode())
        MANIFEST.parent.mkdir(parents=True, exist_ok=True)
        MANIFEST.write_text(json.dumps({"sha256": fingerprint, "files": files}, indent=2) + "\n")
    elif sys.argv[1:] == ["verify"]:
        manifest = json.loads(MANIFEST.read_text())
        files = manifest["files"]
        if digest(json.dumps(files, sort_keys=True).encode()) != manifest["sha256"]:
            raise SystemExit("Native source manifest fingerprint is invalid")
        for path, expected in files.items():
            file = ROOT / path
            if not file.is_file() or digest(file.read_bytes()) != expected:
                raise SystemExit("Native source changed after asset packaging: " + path)
        print("Native source SHA-256: " + manifest["sha256"])
    else:
        raise SystemExit("Usage: ios-source-manifest.py write|verify")


if __name__ == "__main__":
    main()
