#!/usr/bin/env python3

from __future__ import annotations

import json
import pathlib
import sys

try:
    import tomllib
except ModuleNotFoundError:  # pragma: no cover
    print("ERROR: Python 3.11+ is required for tomllib", file=sys.stderr)
    sys.exit(1)


def normalize_expected(raw: str) -> str:
    raw = raw.strip()
    if raw.startswith("v"):
        raw = raw[1:]
    if not raw:
        raise ValueError("expected version must not be empty")
    return raw


def read_json_version(path: pathlib.Path) -> str:
    with path.open("rb") as handle:
        return json.load(handle)["version"]


def read_cargo_package_version(path: pathlib.Path) -> str:
    with path.open("rb") as handle:
        data = tomllib.load(handle)
    return data["package"]["version"]


def main() -> int:
    repo_root = pathlib.Path(__file__).resolve().parent.parent
    versions = {
        "package.json": read_json_version(repo_root / "package.json"),
        "frontend/tauri-shell/Cargo.toml": read_cargo_package_version(
            repo_root / "frontend/tauri-shell/Cargo.toml"
        ),
        "frontend/tauri-shell/tauri.conf.json": read_json_version(
            repo_root / "frontend/tauri-shell/tauri.conf.json"
        ),
    }

    unique_versions = sorted(set(versions.values()))
    if len(unique_versions) != 1:
        print("ERROR: release versions are inconsistent:", file=sys.stderr)
        for path, version in versions.items():
            print(f"  {path}: {version}", file=sys.stderr)
        return 1

    actual = unique_versions[0]

    if len(sys.argv) > 2:
        print(
            f"Usage: {pathlib.Path(sys.argv[0]).name} [expected-version-or-tag]",
            file=sys.stderr,
        )
        return 2

    if len(sys.argv) == 2:
        try:
            expected = normalize_expected(sys.argv[1])
        except ValueError as err:
            print(f"ERROR: {err}", file=sys.stderr)
            return 2
        if actual != expected:
            print(
                "ERROR: release version does not match expected tag/version:",
                file=sys.stderr,
            )
            print(f"  expected: {expected}", file=sys.stderr)
            print(f"  actual:   {actual}", file=sys.stderr)
            return 1

    print(actual)
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
