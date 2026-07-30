#!/usr/bin/env python3

from __future__ import annotations

import pathlib
import re
import subprocess
import sys


TRANSPORT_VERSION_PATH = pathlib.Path("mqtt-transport/src/types.rs")
WIRE_CONTRACT_PATHS = (
    pathlib.Path("mqtt-transport/src/chunking.rs"),
    pathlib.Path("mqtt-transport/src/framing.rs"),
    pathlib.Path("mqtt-transport/src/rendezvous.rs"),
    pathlib.Path("mqtt-transport/src/session.rs"),
    pathlib.Path("mqtt-transport/src/topic.rs"),
)
VERSION_PATTERN = re.compile(
    r"pub const MQTT_TRANSPORT_PROTOCOL_VERSION:\s*u32\s*=\s*(\d+)\s*;"
)
SEMVER_TAG_PATTERN = re.compile(
    r"^v(?:0|[1-9]\d*)\.(?:0|[1-9]\d*)\.(?:0|[1-9]\d*)"
    r"(?:-[0-9A-Za-z-]+(?:\.[0-9A-Za-z-]+)*)?$"
)


class TransportVersionError(RuntimeError):
    pass


def git(repo_root: pathlib.Path, *args: str) -> str:
    result = subprocess.run(
        ["git", *args],
        cwd=repo_root,
        check=False,
        capture_output=True,
        text=True,
    )
    if result.returncode != 0:
        detail = result.stderr.strip() or result.stdout.strip()
        raise TransportVersionError(
            f"git {' '.join(args)} failed"
            + (f": {detail}" if detail else "")
        )
    return result.stdout


def read_version(source: str, label: str) -> int:
    match = VERSION_PATTERN.search(source)
    if match is None:
        raise TransportVersionError(
            f"{label} does not define MQTT_TRANSPORT_PROTOCOL_VERSION"
        )
    return int(match.group(1))


def read_ref_file(
    repo_root: pathlib.Path, ref: str, path: pathlib.Path
) -> str:
    return git(repo_root, "show", f"{ref}:{path.as_posix()}")


def latest_release_tag(repo_root: pathlib.Path) -> str:
    tags = git(
        repo_root,
        "tag",
        "--merged",
        "HEAD",
        "--sort=-version:refname",
    ).splitlines()
    for tag in tags:
        if SEMVER_TAG_PATTERN.fullmatch(tag):
            return tag
    raise TransportVersionError(
        "no semver release tag is reachable from HEAD; pass a baseline ref"
    )


def check_transport_version(
    repo_root: pathlib.Path, baseline_ref: str | None = None
) -> tuple[str, int, int, tuple[pathlib.Path, ...]]:
    baseline = baseline_ref or latest_release_tag(repo_root)
    git(repo_root, "rev-parse", "--verify", f"{baseline}^{{commit}}")

    baseline_version = read_version(
        read_ref_file(repo_root, baseline, TRANSPORT_VERSION_PATH),
        f"{baseline}:{TRANSPORT_VERSION_PATH}",
    )
    current_version = read_version(
        (repo_root / TRANSPORT_VERSION_PATH).read_text(encoding="utf-8"),
        str(TRANSPORT_VERSION_PATH),
    )
    if current_version < baseline_version:
        raise TransportVersionError(
            "MQTT_TRANSPORT_PROTOCOL_VERSION decreased "
            f"from {baseline_version} at {baseline} to {current_version}"
        )

    changed = tuple(
        path
        for path in WIRE_CONTRACT_PATHS
        if read_ref_file(repo_root, baseline, path)
        != (repo_root / path).read_text(encoding="utf-8")
    )
    if changed and current_version <= baseline_version:
        paths = ", ".join(str(path) for path in changed)
        raise TransportVersionError(
            "MQTT wire-contract source changed without increasing "
            "MQTT_TRANSPORT_PROTOCOL_VERSION "
            f"(baseline {baseline} uses {baseline_version}, current uses "
            f"{current_version}; changed: {paths})"
        )
    return baseline, baseline_version, current_version, changed


def main() -> int:
    if len(sys.argv) > 2:
        print(
            f"Usage: {pathlib.Path(sys.argv[0]).name} [baseline-git-ref]",
            file=sys.stderr,
        )
        return 2
    repo_root = pathlib.Path(__file__).resolve().parent.parent
    try:
        baseline, old, current, changed = check_transport_version(
            repo_root, sys.argv[1] if len(sys.argv) == 2 else None
        )
    except (OSError, TransportVersionError) as err:
        print(f"ERROR: {err}", file=sys.stderr)
        return 1

    if changed:
        print(
            "MQTT transport protocol guard passed: "
            f"{baseline}={old}, current={current}"
        )
    else:
        print(
            "MQTT transport wire contract unchanged: "
            f"{baseline}={old}, current={current}"
        )
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
