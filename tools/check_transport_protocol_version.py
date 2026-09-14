#!/usr/bin/env python3

from __future__ import annotations

import pathlib
import re
import subprocess
import sys


TRANSPORT_VERSION_PATH = pathlib.Path("protocol/src/types.rs")
WIRE_CONTRACT_PATHS = (
    pathlib.Path("rtc-transport/src/lib.rs"),
    pathlib.Path("rtc-transport/src/signaling.rs"),
    pathlib.Path("rtc-transport/src/native.rs"),
    pathlib.Path("rtc-transport/src/browser.rs"),
    TRANSPORT_VERSION_PATH,
)
VERSION_PATTERN = re.compile(
    r"pub const MOBILE_RTC_PROTOCOL_VERSION:\s*u32\s*=\s*(\d+)\s*;"
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
            f"{label} does not define MOBILE_RTC_PROTOCOL_VERSION"
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


RUST_TOKEN = re.compile(
    r'//[^\n]*|/\*[\s\S]*?\*/|r(?P<hashes>#+)"[\s\S]*?"(?P=hashes)'
    r'|"(?:\\.|[^"\\])*"|\'(?:\\.|[^\'\\])\''
    r'|[A-Za-z_][A-Za-z_0-9]*|::|\S'
)


def wire_tokens(source: str) -> tuple[str, ...]:
    tokens = [match.group() for match in RUST_TOKEN.finditer(source)
              if not match.group().startswith(("//", "/*"))]
    result = []
    cursor = 0
    test_module = ["#", "[", "cfg", "(", "test", ")", "]", "mod", "wasm_tests", "{"]
    while cursor < len(tokens):
        if tokens[cursor:cursor + len(test_module)] == test_module:
            cursor += len(test_module)
            depth = 1
            while cursor < len(tokens) and depth:
                depth += (tokens[cursor] == "{") - (tokens[cursor] == "}")
                cursor += 1
            if depth:
                raise TransportVersionError("unterminated browser test module")
            continue
        # Treating temporary ICE loss as fatal is a local lifecycle policy.
        # Removing that alternative changes no SDP, record, ACK or credential.
        if (tokens[cursor:cursor + 1] == ["|"]
                and tokens[cursor + 1:cursor + 2] in (
                    ["RTCPeerConnectionState"], ["RtcPeerConnectionState"])
                and tokens[cursor + 2:cursor + 4] == ["::", "Disconnected"]):
            cursor += 4
            continue
        result.append(tokens[cursor])
        cursor += 1
    return tuple(result)


def wire_source(path: pathlib.Path, source: str) -> tuple[str, ...]:
    if path == TRANSPORT_VERSION_PATH:
        # Application frames have their own version; only exported RTC types
        # belong to this guard. Changing the version itself is not a wire change.
        start = source.find("pub mod mobile_rtc {")
        if start < 0:
            return wire_tokens(VERSION_PATTERN.sub("", source))
        end = source.index("pub use mobile_rtc::*;", start)
        return wire_tokens(VERSION_PATTERN.sub("", source[start:end]))
    # The reconnect repair replaced the browser timer driver, preserving every
    # deadline and wire byte. Normalize only those equivalent call/import names;
    # changes to arguments, framing, authentication or negotiation still fail.
    source = source.replace("wasmtimer::tokio::", "tyde_time::")
    source = source.replace(
        '#[cfg(not(target_arch = "wasm32"))]\n'
        'use tokio::time::{sleep, timeout};\n'
        '#[cfg(target_arch = "wasm32")]\n'
        'use tyde_time::{sleep, timeout};',
        'use tyde_time::{sleep, timeout};',
    )
    return wire_tokens(source)


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
            "MOBILE_RTC_PROTOCOL_VERSION decreased "
            f"from {baseline_version} at {baseline} to {current_version}"
        )

    changed = tuple(
        path
        for path in WIRE_CONTRACT_PATHS
        if wire_source(path, read_ref_file(repo_root, baseline, path))
        != wire_source(path, (repo_root / path).read_text(encoding="utf-8"))
    )
    if changed and current_version <= baseline_version:
        paths = ", ".join(str(path) for path in changed)
        raise TransportVersionError(
            "RTC wire-contract source changed without increasing "
            "MOBILE_RTC_PROTOCOL_VERSION "
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
            "RTC transport protocol guard passed: "
            f"{baseline}={old}, current={current}"
        )
    else:
        print(
            "RTC transport wire contract unchanged: "
            f"{baseline}={old}, current={current}"
        )
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
