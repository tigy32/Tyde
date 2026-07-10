#!/usr/bin/env python3

from __future__ import annotations

import argparse
import json
import pathlib
import re
import sys

import check_release_version as version_check


STRICT_TAG_RE = re.compile(
    r"^v(0|[1-9][0-9]*)\."
    r"(0|[1-9][0-9]*)\."
    r"(0|[1-9][0-9]*)"
    r"(?:-(?:0|[1-9][0-9]*|[0-9A-Za-z-]*[A-Za-z-][0-9A-Za-z-]*)"
    r"(?:\.(?:0|[1-9][0-9]*|[0-9A-Za-z-]*[A-Za-z-][0-9A-Za-z-]*))*)?$"
)


class SetVersionError(ValueError):
    pass


def normalize_tag(tag: str) -> str:
    if not STRICT_TAG_RE.fullmatch(tag):
        raise SetVersionError(
            f"invalid release tag {tag!r}: expected strict vMAJOR.MINOR.PATCH"
            "[-PRERELEASE] semver"
        )
    return tag[1:]


def write_json_version(path: pathlib.Path, version: str) -> None:
    data = json.loads(path.read_text(encoding="utf-8"))
    if not isinstance(data, dict) or not isinstance(data.get("version"), str):
        raise SetVersionError(f"{path} must contain a string top-level version")
    data["version"] = version
    path.write_text(json.dumps(data, indent=2) + "\n", encoding="utf-8")


def write_package_lock(path: pathlib.Path, version: str) -> None:
    data = json.loads(path.read_text(encoding="utf-8"))
    root_package = data.get("packages", {}).get("")
    if not isinstance(data.get("version"), str) or not isinstance(root_package, dict):
        raise SetVersionError(f"{path} is missing release version fields")
    if not isinstance(root_package.get("version"), str):
        raise SetVersionError(f'{path} packages[""] must contain a string version')
    data["version"] = version
    root_package["version"] = version
    path.write_text(json.dumps(data, indent=2) + "\n", encoding="utf-8")


def replace_package_version(text: str, version: str, label: str) -> str:
    section_match = re.search(r"(?m)^\[package\]\s*$", text)
    if section_match is None:
        raise SetVersionError(f"{label} is missing [package]")
    section_end = re.search(r"(?m)^\[", text[section_match.end() :])
    end = (
        section_match.end() + section_end.start()
        if section_end is not None
        else len(text)
    )
    section = text[section_match.end() : end]
    replaced, count = re.subn(
        r'(?m)^(version\s*=\s*)"[^"]+"(\s*)$',
        rf'\g<1>"{version}"\g<2>',
        section,
        count=1,
    )
    if count != 1:
        raise SetVersionError(f"{label} [package] must contain one string version")
    return text[: section_match.end()] + replaced + text[end:]


def write_cargo_package(path: pathlib.Path, version: str) -> None:
    text = path.read_text(encoding="utf-8")
    path.write_text(
        replace_package_version(text, version, str(path)), encoding="utf-8"
    )


def write_cargo_lock(
    path: pathlib.Path, package_names: frozenset[str], version: str
) -> None:
    text = path.read_text(encoding="utf-8")
    starts = [match.start() for match in re.finditer(r"(?m)^\[\[package\]\]\s*$", text)]
    starts.append(len(text))
    found: set[str] = set()
    chunks: list[str] = [text[: starts[0]]] if len(starts) > 1 else []
    for index in range(len(starts) - 1):
        chunk = text[starts[index] : starts[index + 1]]
        name_match = re.search(r'(?m)^name\s*=\s*"([^"]+)"\s*$', chunk)
        if name_match is not None and name_match.group(1) in package_names:
            name = name_match.group(1)
            if name in found:
                raise SetVersionError(f"{path} contains duplicate package {name!r}")
            chunk, count = re.subn(
                r'(?m)^(version\s*=\s*)"[^"]+"(\s*)$',
                rf'\g<1>"{version}"\g<2>',
                chunk,
                count=1,
            )
            if count != 1:
                raise SetVersionError(f"{path} package {name!r} is missing version")
            found.add(name)
        chunks.append(chunk)
    missing = package_names - found
    if missing:
        raise SetVersionError(
            f"{path} is missing package(s): {', '.join(sorted(missing))}"
        )
    path.write_text("".join(chunks), encoding="utf-8")


def set_release_version(repo_root: pathlib.Path, tag: str) -> list[pathlib.Path]:
    version = normalize_tag(tag)
    paths = version_check.release_version_paths(repo_root)
    version_check.collect_versions(repo_root)
    before = {path: path.read_bytes() for path in paths}

    for relative in version_check.JSON_VERSION_PATHS:
        write_json_version(repo_root / relative, version)
    write_package_lock(repo_root / version_check.PACKAGE_LOCK_PATH, version)
    for relative in version_check.CARGO_PACKAGE_PATHS:
        write_cargo_package(repo_root / relative, version)
    write_cargo_lock(
        repo_root / version_check.CARGO_LOCK_PATH,
        version_check.CARGO_LOCK_PACKAGE_NAMES,
        version,
    )

    versions = version_check.collect_versions(repo_root)
    mismatches = {label: value for label, value in versions.items() if value != version}
    if mismatches:
        details = ", ".join(f"{label}={value}" for label, value in mismatches.items())
        raise SetVersionError(f"release version update was incomplete: {details}")
    return [path for path in paths if before[path] != path.read_bytes()]


def parse_args(argv: list[str]) -> argparse.Namespace:
    parser = argparse.ArgumentParser(
        description="Set every release version validated by check_release_version.py."
    )
    parser.add_argument("tag", help="strict release tag, for example v0.8.20-beta.1")
    parser.add_argument(
        "--repo-root",
        type=pathlib.Path,
        default=pathlib.Path(__file__).resolve().parent.parent,
        help=argparse.SUPPRESS,
    )
    return parser.parse_args(argv)


def main(argv: list[str] | None = None) -> int:
    args = parse_args(sys.argv[1:] if argv is None else argv)
    try:
        changed = set_release_version(args.repo_root.resolve(), args.tag)
    except (OSError, json.JSONDecodeError, KeyError, SetVersionError) as error:
        print(f"ERROR: {error}", file=sys.stderr)
        return 1
    for path in changed:
        print(path.relative_to(args.repo_root.resolve()))
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
