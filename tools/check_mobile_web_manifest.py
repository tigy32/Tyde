#!/usr/bin/env python3

from __future__ import annotations

import argparse
import copy
import json
import pathlib
import re
import sys
from functools import cmp_to_key
from functools import lru_cache
from typing import Any


RELEASE_VERSION_RE = re.compile(
    r"^[0-9]+\.[0-9]+\.[0-9]+(?:-[0-9A-Za-z-]+(?:\.[0-9A-Za-z-]+)*)?$"
)
PROTOCOL_VERSION_RE = re.compile(
    r"\bpub\s+const\s+PROTOCOL_VERSION\s*:\s*u32\s*=\s*([0-9]+)\s*;"
)
INTEGRITY_RE = re.compile(r"^sha384-[A-Za-z0-9+/]+={0,2}$")
MOBILE_WEB_POLICY_PATH = pathlib.Path("web/deploy/mobile-web-policy.json")
SELF_HEAL_MIN_SUPPORTED_KEY = "selfHealMinSupported"


class CheckError(ValueError):
    pass


def repo_root() -> pathlib.Path:
    return pathlib.Path(__file__).resolve().parent.parent


@lru_cache(maxsize=None)
def read_mobile_web_policy() -> dict[str, Any]:
    path = repo_root() / MOBILE_WEB_POLICY_PATH
    try:
        policy = json.loads(path.read_text(encoding="utf-8"))
    except json.JSONDecodeError as error:
        raise CheckError(f"mobile web policy is not valid JSON: {path}: {error}") from error
    if not isinstance(policy, dict):
        raise CheckError("mobile web policy root must be a JSON object")
    return policy


def self_heal_min_supported() -> str:
    raw = read_mobile_web_policy().get(SELF_HEAL_MIN_SUPPORTED_KEY)
    if not isinstance(raw, str):
        raise CheckError(f"mobile web policy {SELF_HEAL_MIN_SUPPORTED_KEY} must be a string")
    return normalize_release_version(raw)


def normalize_release_version(raw: str) -> str:
    value = raw.strip()
    if value.startswith("v"):
        value = value[1:]
    if not value:
        raise CheckError("release version must not be empty")
    if "/" in value or "\\" in value or any(ch.isspace() for ch in value):
        raise CheckError(f"invalid release version {raw!r}: path separators/whitespace are forbidden")
    if not RELEASE_VERSION_RE.fullmatch(value):
        raise CheckError(
            f"invalid release version {raw!r}: expected major.minor.patch[-prerelease]"
        )
    return value


def _version_key(version: str) -> tuple[int, int, int, tuple[tuple[int, Any], ...]]:
    core, separator, prerelease = version.partition("-")
    major, minor, patch = (int(part) for part in core.split("."))
    if not separator:
        return (major, minor, patch, ((1, ""),))
    parts: list[tuple[int, Any]] = []
    for part in prerelease.split("."):
        if part.isdigit():
            parts.append((0, int(part)))
        else:
            parts.append((1, part))
    return (major, minor, patch, tuple(parts))


def compare_release_versions(left: str, right: str) -> int:
    left = normalize_release_version(left)
    right = normalize_release_version(right)
    if left == right:
        return 0
    left_key = _version_key(left)
    right_key = _version_key(right)
    left_core = left_key[:3]
    right_core = right_key[:3]
    if left_core != right_core:
        return -1 if left_core < right_core else 1

    left_pre = left_key[3]
    right_pre = right_key[3]
    left_stable = left_pre == ((1, ""),)
    right_stable = right_pre == ((1, ""),)
    if left_stable or right_stable:
        return 1 if left_stable else -1

    for left_part, right_part in zip(left_pre, right_pre):
        if left_part == right_part:
            continue
        left_kind, left_value = left_part
        right_kind, right_value = right_part
        if left_kind != right_kind:
            return -1 if left_kind < right_kind else 1
        return -1 if left_value < right_value else 1
    return -1 if len(left_pre) < len(right_pre) else 1


def read_protocol_version(path: pathlib.Path) -> int:
    source = path.read_text(encoding="utf-8")
    match = PROTOCOL_VERSION_RE.search(source)
    if not match:
        raise CheckError(f"could not find PROTOCOL_VERSION in {path}")
    return int(match.group(1))


def load_manifest(path: pathlib.Path) -> dict[str, Any]:
    try:
        manifest = json.loads(path.read_text(encoding="utf-8"))
    except json.JSONDecodeError as error:
        raise CheckError(f"manifest is not valid JSON: {path}: {error}") from error
    if not isinstance(manifest, dict):
        raise CheckError("manifest root must be a JSON object")
    return manifest


def _versions_object(manifest: dict[str, Any]) -> dict[str, Any]:
    versions = manifest.get("versions")
    if not isinstance(versions, dict):
        raise CheckError("manifest.versions must be an object")
    return versions


def _blocked_versions(manifest: dict[str, Any]) -> set[str]:
    blocked = manifest.get("blocked", [])
    if not isinstance(blocked, list):
        raise CheckError("manifest.blocked must be an array when present")
    normalized = set()
    for item in blocked:
        if not isinstance(item, str):
            raise CheckError("manifest.blocked entries must be strings")
        normalized.add(normalize_release_version(item))
    return normalized


def _min_supported(manifest: dict[str, Any]) -> str | None:
    raw = manifest.get("minSupported")
    if raw is None:
        return None
    if not isinstance(raw, str):
        raise CheckError("manifest.minSupported must be a string when present")
    return normalize_release_version(raw)


def _require_string(value: Any, label: str) -> str:
    if not isinstance(value, str) or not value:
        raise CheckError(f"{label} must be a non-empty string")
    return value


def _is_safe_version_path(path: str, version: str) -> bool:
    base = f"/tyde/v{version}/"
    return (
        path.startswith(base)
        and "\\" not in path
        and "/../" not in path
        and not path.endswith("/..")
        and "%2e" not in path.lower()
    )


def validate_manifest_entry(
    manifest: dict[str, Any], version: str, expected_protocol_version: int
) -> None:
    versions = _versions_object(manifest)
    entry = versions.get(version)
    if not isinstance(entry, dict):
        raise CheckError(f"manifest is missing versions[{version!r}]")

    actual_protocol_version = entry.get("protocolVersion")
    if not isinstance(actual_protocol_version, int):
        raise CheckError(f"versions[{version!r}].protocolVersion must be an integer")
    if actual_protocol_version != expected_protocol_version:
        raise CheckError(
            f"versions[{version!r}].protocolVersion is {actual_protocol_version}, "
            f"expected {expected_protocol_version} from protocol/src/types.rs"
        )

    path_value = entry.get("path")
    if path_value is not None:
        path_string = _require_string(path_value, f"versions[{version!r}].path")
        if path_string != f"/tyde/v{version}/":
            raise CheckError(
                f"versions[{version!r}].path must be /tyde/v{version}/, got {path_string!r}"
            )

    entry_path = _require_string(entry.get("entry"), f"versions[{version!r}].entry")
    if not _is_safe_version_path(entry_path, version):
        raise CheckError(f"versions[{version!r}].entry is outside /tyde/v{version}/")

    integrity = _require_string(entry.get("integrity"), f"versions[{version!r}].integrity")
    if not INTEGRITY_RE.fullmatch(integrity):
        raise CheckError(f"versions[{version!r}].integrity is not a sha384 SRI value")

    artifacts = entry.get("artifacts")
    if not isinstance(artifacts, dict) or isinstance(artifacts, list):
        raise CheckError(f"versions[{version!r}].artifacts must be an object")
    if not artifacts:
        raise CheckError(f"versions[{version!r}].artifacts must list executable artifacts")
    has_wasm = False
    for artifact_path, artifact_integrity in artifacts.items():
        if not isinstance(artifact_path, str) or not _is_safe_version_path(artifact_path, version):
            raise CheckError(f"versions[{version!r}].artifacts contains an unsafe path")
        if artifact_path.endswith(".wasm"):
            has_wasm = True
        if not isinstance(artifact_integrity, str) or not INTEGRITY_RE.fullmatch(
            artifact_integrity
        ):
            raise CheckError(
                f"versions[{version!r}].artifacts[{artifact_path!r}] is not a sha384 SRI value"
            )
    if not has_wasm:
        raise CheckError(f"versions[{version!r}].artifacts must include a .wasm artifact")


def _entry_has_protocol_version(entry: Any) -> bool:
    return isinstance(entry, dict) and isinstance(entry.get("protocolVersion"), int)


def is_allowed_by_policy(manifest: dict[str, Any], version: str) -> bool:
    version = normalize_release_version(version)
    min_supported = _min_supported(manifest)
    if min_supported is not None and compare_release_versions(version, min_supported) < 0:
        return False
    return version not in _blocked_versions(manifest)


def validate_supported_entries_have_protocol_versions(manifest: dict[str, Any]) -> None:
    versions = _versions_object(manifest)
    _min_supported(manifest)
    _blocked_versions(manifest)
    for raw_version, entry in versions.items():
        version = normalize_release_version(raw_version)
        if is_allowed_by_policy(manifest, version) and not _entry_has_protocol_version(entry):
            raise CheckError(
                f"versions[{version!r}] is allowed by policy but lacks protocolVersion"
            )


def _manifest_min_supported_floor(manifest: dict[str, Any]) -> str:
    versions = _versions_object(manifest)
    protocol_versions = [
        normalize_release_version(version)
        for version, entry in versions.items()
        if _entry_has_protocol_version(entry)
    ]
    floor = self_heal_min_supported()
    if protocol_versions:
        protocol_floor = min(protocol_versions, key=cmp_to_key(compare_release_versions))
        if compare_release_versions(protocol_floor, floor) > 0:
            floor = protocol_floor
    return floor


def validate_min_supported_floor(manifest: dict[str, Any]) -> None:
    floor = _manifest_min_supported_floor(manifest)
    current = _min_supported(manifest)
    if current is None or compare_release_versions(current, floor) < 0:
        raise CheckError(
            f"manifest.minSupported must be at least {floor} "
            f"(mobile web self-heal floor is {self_heal_min_supported()})"
        )


def enforce_min_supported_floor(manifest: dict[str, Any]) -> bool:
    floor = _manifest_min_supported_floor(manifest)
    current = _min_supported(manifest)
    if current is None or compare_release_versions(current, floor) < 0:
        manifest["minSupported"] = floor
        return True
    return False


def enforce_protocol_floor(manifest: dict[str, Any]) -> bool:
    return enforce_min_supported_floor(manifest)


def merge_target_entry(
    base_manifest: dict[str, Any],
    entry_source_manifest: dict[str, Any],
    version: str,
    expected_protocol_version: int,
) -> dict[str, Any]:
    version = normalize_release_version(version)
    validate_manifest_entry(entry_source_manifest, version, expected_protocol_version)
    merged = copy.deepcopy(base_manifest)
    versions = _versions_object(merged)
    source_versions = _versions_object(entry_source_manifest)
    versions[version] = copy.deepcopy(source_versions[version])
    enforce_min_supported_floor(merged)
    validate_supported_entries_have_protocol_versions(merged)
    validate_min_supported_floor(merged)
    validate_manifest_entry(merged, version, expected_protocol_version)
    if not is_allowed_by_policy(merged, version):
        raise CheckError(f"versions[{version!r}] is not allowed by manifest policy")
    return merged


def parse_args(argv: list[str]) -> argparse.Namespace:
    root = repo_root()
    parser = argparse.ArgumentParser(
        description=(
            "Verify that the mobile web release manifest contains the target "
            "version and that its protocolVersion matches Rust PROTOCOL_VERSION."
        )
    )
    parser.add_argument("version", help="release version or tag, e.g. v0.8.19-beta.9")
    parser.add_argument(
        "--manifest",
        type=pathlib.Path,
        default=root / "web" / "loader" / "manifest.json",
        help="manifest path to verify (default: web/loader/manifest.json)",
    )
    parser.add_argument(
        "--protocol-source",
        type=pathlib.Path,
        default=root / "protocol" / "src" / "types.rs",
        help="Rust source containing PROTOCOL_VERSION (default: protocol/src/types.rs)",
    )
    return parser.parse_args(argv)


def main(argv: list[str] | None = None) -> int:
    args = parse_args(sys.argv[1:] if argv is None else argv)
    try:
        version = normalize_release_version(args.version)
        protocol_version = read_protocol_version(args.protocol_source)
        manifest = load_manifest(args.manifest)
        validate_manifest_entry(manifest, version, protocol_version)
        validate_supported_entries_have_protocol_versions(manifest)
        validate_min_supported_floor(manifest)
        if not is_allowed_by_policy(manifest, version):
            raise CheckError(f"versions[{version!r}] is not allowed by manifest policy")
    except (OSError, CheckError) as error:
        print(f"ERROR: {error}", file=sys.stderr)
        return 1

    print(
        f"mobile web manifest OK: version={version} protocolVersion={protocol_version} "
        f"manifest={args.manifest}"
    )
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
