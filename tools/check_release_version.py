#!/usr/bin/env python3

from __future__ import annotations

import json
import pathlib
import sys

try:
    import tomllib
except ModuleNotFoundError:  # pragma: no cover
    tomllib = None


JSON_VERSION_PATHS = (
    pathlib.Path("package.json"),
    pathlib.Path("frontend/tauri-shell/tauri.conf.json"),
    pathlib.Path("mobile/src-tauri/tauri.conf.json"),
)
PACKAGE_LOCK_PATH = pathlib.Path("package-lock.json")
CARGO_PACKAGE_PATHS = (
    pathlib.Path("frontend/tauri-shell/Cargo.toml"),
    pathlib.Path("tyde-server/Cargo.toml"),
    pathlib.Path("mobile/src-tauri/Cargo.toml"),
)
CARGO_LOCK_PATH = pathlib.Path("Cargo.lock")
CARGO_LOCK_PACKAGE_NAMES = frozenset(
    {"tauri-shell", "tyde-server", "tyde-mobile-shell"}
)


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


def read_package_lock_versions(path: pathlib.Path) -> dict[str, str]:
    with path.open("rb") as handle:
        data = json.load(handle)
    versions = {
        "package-lock.json": data["version"],
    }
    root_package = data.get("packages", {}).get("", {})
    if "version" in root_package:
        versions['package-lock.json packages[""]'] = root_package["version"]
    return versions


def parse_toml_string_value(line: str) -> str | None:
    _, _, value = line.partition("=")
    value = value.strip()
    if not value.startswith('"'):
        return None
    end = value.find('"', 1)
    if end == -1:
        return None
    return value[1:end]


def read_cargo_package_version(path: pathlib.Path) -> str:
    if tomllib is not None:
        with path.open("rb") as handle:
            data = tomllib.load(handle)
        return data["package"]["version"]

    in_package = False
    for line in path.read_text().splitlines():
        stripped = line.strip()
        if stripped == "[package]":
            in_package = True
            continue
        if in_package and stripped.startswith("["):
            break
        if in_package and stripped.startswith("version"):
            value = parse_toml_string_value(stripped)
            if value is not None:
                return value
    raise KeyError(f"{path} missing [package].version")


def read_cargo_lock_versions(
    path: pathlib.Path, package_names: set[str]
) -> dict[str, str]:
    if tomllib is not None:
        with path.open("rb") as handle:
            data = tomllib.load(handle)
        packages = data.get("package", [])
    else:
        packages = []
        current: dict[str, str] | None = None
        for line in path.read_text().splitlines():
            stripped = line.strip()
            if stripped == "[[package]]":
                if current is not None:
                    packages.append(current)
                current = {}
                continue
            if current is None:
                continue
            if stripped.startswith("name"):
                value = parse_toml_string_value(stripped)
                if value is not None:
                    current["name"] = value
            elif stripped.startswith("version"):
                value = parse_toml_string_value(stripped)
                if value is not None:
                    current["version"] = value
        if current is not None:
            packages.append(current)

    versions = {}
    for package in packages:
        name = package.get("name")
        if name in package_names:
            versions[f"Cargo.lock {name}"] = package["version"]
    missing = package_names - {
        key.removeprefix("Cargo.lock ") for key in versions.keys()
    }
    if missing:
        raise KeyError(f"Cargo.lock missing package(s): {', '.join(sorted(missing))}")
    return versions


def release_version_paths(repo_root: pathlib.Path) -> tuple[pathlib.Path, ...]:
    return tuple(
        repo_root / path
        for path in (
            *JSON_VERSION_PATHS,
            PACKAGE_LOCK_PATH,
            *CARGO_PACKAGE_PATHS,
            CARGO_LOCK_PATH,
        )
    )


def collect_versions(repo_root: pathlib.Path) -> dict[str, str]:
    versions = {
        str(path): read_json_version(repo_root / path)
        for path in JSON_VERSION_PATHS
    }
    versions.update(read_package_lock_versions(repo_root / PACKAGE_LOCK_PATH))
    versions.update(
        {
            str(path): read_cargo_package_version(repo_root / path)
            for path in CARGO_PACKAGE_PATHS
        }
    )
    versions.update(
        read_cargo_lock_versions(
            repo_root / CARGO_LOCK_PATH,
            set(CARGO_LOCK_PACKAGE_NAMES),
        )
    )
    return versions


def main() -> int:
    repo_root = pathlib.Path(__file__).resolve().parent.parent
    versions = collect_versions(repo_root)

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
