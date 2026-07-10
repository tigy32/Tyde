#!/usr/bin/env python3

from __future__ import annotations

import argparse
import base64
import hashlib
import json
import os
import pathlib
import re
import signal
import subprocess
import sys
from typing import Any

import check_mobile_web_manifest as mobile_manifest
from set_release_version import normalize_tag


EXIT_NOT_FOUND = 3
EXIT_TIMEOUT = 124
HEADLESS_ASSETS = {
    "tyde-server-aarch64-apple-darwin.zip",
    "tyde-server-x86_64-apple-darwin.zip",
    "tyde-server-aarch64-unknown-linux-musl.zip",
    "tyde-server-x86_64-unknown-linux-musl.zip",
    "tyde-server-x86_64-pc-windows-msvc.zip",
}
SECRET_PATTERNS = (
    (re.compile(r"(?i)\bBearer\s+[^\s]+"), "Bearer [REDACTED]"),
    (
        re.compile(r"\b(?:github_pat_[A-Za-z0-9_]+|gh[pousr]_[A-Za-z0-9]+)\b"),
        "[REDACTED]",
    ),
    (re.compile(r"\b(?:AKIA|ASIA)[A-Z0-9]{16}\b"), "[REDACTED]"),
    (
        re.compile(
            r"-----BEGIN [^-]*(?:PRIVATE KEY|CERTIFICATE)-----.*?"
            r"-----END [^-]*(?:PRIVATE KEY|CERTIFICATE)-----",
            re.DOTALL,
        ),
        "[REDACTED PEM]",
    ),
    (
        re.compile(
            r"(?i)\b([A-Za-z0-9_.-]*(?:token|secret|password|authorization|"
            r"cookie|api[_-]?key|private[_-]?key|certificate)[A-Za-z0-9_.-]*)"
            r"([\"']?\s*[:=]\s*)([^\r\n]*)"
        ),
        r"\1\2[REDACTED]",
    ),
)
ANSI_ESCAPE_RE = re.compile(r"\x1b(?:\[[0-?]*[ -/]*[@-~]|\][^\x07]*(?:\x07|\x1b\\))")


class ReleaseToolError(ValueError):
    pass


class RunNotFoundError(ReleaseToolError):
    pass


def load_json(path: pathlib.Path | None) -> Any:
    if path is None:
        return json.load(sys.stdin)
    return json.loads(path.read_text(encoding="utf-8"))


def is_beta(tag: str) -> bool:
    normalize_tag(tag)
    return "-" in tag


def validate_tag_kind(tag: str, beta_only: bool, stable_only: bool) -> str:
    version = normalize_tag(tag)
    if beta_only and not is_beta(tag):
        raise ReleaseToolError(f"{tag} is stable; this command is beta-only")
    if stable_only and is_beta(tag):
        raise ReleaseToolError(f"{tag} is a prerelease; a stable tag is required")
    return version


def select_run(runs: Any, tag: str, sha: str) -> dict[str, Any]:
    if not isinstance(runs, list):
        raise ReleaseToolError("GitHub run list must be a JSON array")
    matches = []
    for run in runs:
        if not isinstance(run, dict):
            continue
        workflow = run.get("workflowName", run.get("name"))
        if (
            workflow == "Release"
            and run.get("headBranch") == tag
            and run.get("headSha") == sha
        ):
            matches.append(run)
    if not matches:
        raise RunNotFoundError(f"no Release workflow run found for {tag} at {sha}")
    return max(
        matches,
        key=lambda run: (str(run.get("createdAt", "")), int(run.get("databaseId", 0))),
    )


def run_outcome(run: Any) -> str:
    if not isinstance(run, dict):
        raise ReleaseToolError("GitHub run view must be a JSON object")
    status = run.get("status")
    conclusion = run.get("conclusion")
    if status != "completed":
        return "running"
    if conclusion == "success":
        return "success"
    return "failure"


def _state(value: Any) -> str:
    if not isinstance(value, dict):
        return "unknown"
    status = value.get("status")
    conclusion = value.get("conclusion")
    if status == "completed" and isinstance(conclusion, str) and conclusion:
        return conclusion
    return str(status or "unknown")


def run_signature(run: Any) -> str:
    if not isinstance(run, dict):
        raise ReleaseToolError("GitHub run view must be a JSON object")
    jobs = run.get("jobs", [])
    if not isinstance(jobs, list):
        raise ReleaseToolError("GitHub run jobs must be a JSON array")
    signature = {
        "run": _state(run),
        "jobs": [
            [job.get("databaseId"), job.get("name"), _state(job)]
            for job in jobs
            if isinstance(job, dict)
        ],
    }
    return json.dumps(signature, separators=(",", ":"), sort_keys=True)


def run_report(run: Any, tag: str) -> str:
    if not isinstance(run, dict):
        raise ReleaseToolError("GitHub run view must be a JSON object")
    jobs = run.get("jobs", [])
    if not isinstance(jobs, list):
        raise ReleaseToolError("GitHub run jobs must be a JSON array")
    run_id = run.get("databaseId", "?")
    lines = [f"{tag} run {run_id}: {_state(run)} {run.get('url', '')}".rstrip()]
    for job in jobs:
        if isinstance(job, dict):
            lines.append(f"  {job.get('name', '?')}: {_state(job)}")
    return "\n".join(lines)


def failure_info(run: Any) -> dict[str, str]:
    if not isinstance(run, dict):
        raise ReleaseToolError("GitHub run view must be a JSON object")
    jobs = run.get("jobs", [])
    if not isinstance(jobs, list):
        raise ReleaseToolError("GitHub run jobs must be a JSON array")
    failed_jobs = [
        job
        for conclusion in (
            "failure",
            "timed_out",
            "startup_failure",
            "action_required",
            "cancelled",
            "stale",
            "skipped",
        )
        for job in jobs
        if isinstance(job, dict) and job.get("conclusion") == conclusion
    ]
    if not failed_jobs:
        return {
            "id": "",
            "job": "unknown",
            "step": "unknown",
            "url": str(run.get("url", "")),
        }
    job = failed_jobs[0]
    steps = job.get("steps", [])
    failed_steps = (
        [
            step
            for conclusion in (
                "failure",
                "timed_out",
                "action_required",
                "cancelled",
                "skipped",
            )
            for step in steps
            if isinstance(step, dict) and step.get("conclusion") == conclusion
        ]
        if isinstance(steps, list)
        else []
    )
    step_name = str(failed_steps[0].get("name", "unknown")) if failed_steps else "unknown"
    return {
        "id": str(job.get("databaseId", "")),
        "job": str(job.get("name", "unknown")),
        "step": step_name,
        "url": str(job.get("url", run.get("url", ""))),
    }


def sanitize_log(text: str, max_lines: int, max_chars: int) -> str:
    sanitized = ANSI_ESCAPE_RE.sub("", text)
    for pattern, replacement in SECRET_PATTERNS:
        sanitized = pattern.sub(replacement, sanitized)
    lines = [line[:500] for line in sanitized.splitlines()]
    lines = lines[-max_lines:]
    excerpt = "\n".join(lines)
    if len(excerpt) > max_chars:
        excerpt = excerpt[-max_chars:]
        first_newline = excerpt.find("\n")
        if first_newline >= 0:
            excerpt = excerpt[first_newline + 1 :]
    return excerpt


def _asset_names(release: Any) -> list[str]:
    if not isinstance(release, dict):
        raise ReleaseToolError("GitHub release data must be a JSON object")
    assets = release.get("assets")
    if not isinstance(assets, list):
        raise ReleaseToolError("GitHub release assets must be a JSON array")
    names = []
    for asset in assets:
        if not isinstance(asset, dict) or not isinstance(asset.get("name"), str):
            raise ReleaseToolError("every GitHub release asset must have a name")
        names.append(asset["name"])
    if len(names) != len(set(names)):
        raise ReleaseToolError("GitHub release contains duplicate asset names")
    return names


def _require_arch_pair(names: list[str], label: str) -> None:
    if len(names) != 2:
        raise ReleaseToolError(f"expected two {label} assets, found {len(names)}")
    lowered = [name.lower() for name in names]
    has_x86 = sum("x86_64" in name or "amd64" in name for name in lowered)
    has_arm = sum("aarch64" in name or "arm64" in name for name in lowered)
    if has_x86 != 1 or has_arm != 1:
        raise ReleaseToolError(f"{label} assets must cover x86_64 and arm64 exactly once")


def validate_assets(release: Any, tag: str) -> list[str]:
    version = normalize_tag(tag)
    names = _asset_names(release)
    actual_headless = {name for name in names if name.startswith("tyde-server-")}
    if actual_headless != HEADLESS_ASSETS:
        missing = sorted(HEADLESS_ASSETS - actual_headless)
        extra = sorted(actual_headless - HEADLESS_ASSETS)
        raise ReleaseToolError(
            f"headless asset set mismatch; missing={missing or 'none'} extra={extra or 'none'}"
        )

    dmg = [name for name in names if name.endswith(".dmg")]
    _require_arch_pair(dmg, "DMG")
    if any(version not in name or "apple-darwin" not in name for name in dmg):
        raise ReleaseToolError("DMG assets must contain the version and Apple target")

    appimages = [name for name in names if name.endswith(".AppImage")]
    debs = [name for name in names if name.endswith(".deb")]
    rpms = [name for name in names if name.endswith(".rpm")]
    _require_arch_pair(appimages, "AppImage")
    _require_arch_pair(debs, "Debian")
    _require_arch_pair(rpms, "RPM")
    for label, packages in (("AppImage", appimages), ("Debian", debs), ("RPM", rpms)):
        if any(version not in name for name in packages):
            raise ReleaseToolError(f"{label} assets must contain version {version}")

    checksums = [name for name in names if name.endswith(".sha256")]
    expected_checksums = {f"{name}.sha256" for name in appimages + debs}
    if set(checksums) != expected_checksums:
        raise ReleaseToolError("Linux checksum assets must match every AppImage and Debian asset")

    installers = [name for name in names if name.endswith(".exe")]
    if len(installers) != 1 or version not in installers[0]:
        raise ReleaseToolError("expected one versioned Windows NSIS .exe asset")
    msi = [name for name in names if name.endswith(".msi")]
    if is_beta(tag):
        if msi:
            raise ReleaseToolError("prerelease assets must not contain an MSI")
    elif len(msi) != 1 or version not in msi[0]:
        raise ReleaseToolError("stable release assets must contain one versioned MSI")

    recognized = set(HEADLESS_ASSETS) | set(
        dmg + appimages + debs + rpms + checksums + installers + msi
    )
    unexpected = sorted(set(names) - recognized)
    if unexpected:
        raise ReleaseToolError(f"unexpected release assets: {', '.join(unexpected)}")
    return names


def _release_value(release: dict[str, Any], camel: str, snake: str) -> Any:
    return release[camel] if camel in release else release.get(snake)


def validate_release(release: Any, tag: str, require_published: bool = False) -> dict[str, Any]:
    if not isinstance(release, dict):
        raise ReleaseToolError("GitHub release data must be a JSON object")
    actual_tag = _release_value(release, "tagName", "tag_name")
    if actual_tag != tag:
        raise ReleaseToolError(f"GitHub release tag is {actual_tag!r}, expected {tag}")
    draft = _release_value(release, "isDraft", "draft")
    prerelease = _release_value(release, "isPrerelease", "prerelease")
    if not isinstance(draft, bool) or not isinstance(prerelease, bool):
        raise ReleaseToolError("GitHub release draft/prerelease flags must be booleans")
    if prerelease != is_beta(tag):
        raise ReleaseToolError(
            f"GitHub release prerelease={str(prerelease).lower()} does not match {tag}"
        )
    if require_published and draft:
        raise ReleaseToolError("GitHub release is still a draft")
    assets = validate_assets(release, tag)
    return {
        "assets": len(assets),
        "draft": draft,
        "prerelease": prerelease,
        "url": str(release.get("url", release.get("html_url", ""))),
    }


def manifest_plan(manifest: Any, tag: str, base_url: str) -> dict[str, Any]:
    version = normalize_tag(tag)
    if not isinstance(manifest, dict):
        raise ReleaseToolError("manifest root must be a JSON object")
    versions = manifest.get("versions")
    entry = versions.get(version) if isinstance(versions, dict) else None
    if not isinstance(entry, dict):
        raise ReleaseToolError(f"manifest is missing versions[{version!r}]")
    items = [
        {
            "kind": "entry",
            "url": base_url.rstrip("/") + str(entry.get("entry", "")),
            "integrity": entry.get("integrity"),
            "wasm": False,
        }
    ]
    artifacts = entry.get("artifacts")
    if not isinstance(artifacts, dict):
        raise ReleaseToolError("manifest entry artifacts must be an object")
    for path, integrity in sorted(artifacts.items()):
        items.append(
            {
                "kind": "artifact",
                "url": base_url.rstrip("/") + str(path),
                "integrity": integrity,
                "wasm": str(path).endswith(".wasm"),
            }
        )
    for item in items:
        if not isinstance(
            item["integrity"], str
        ) or not mobile_manifest.INTEGRITY_RE.fullmatch(item["integrity"]):
            raise ReleaseToolError(f"{item['url']} has invalid sha384 integrity")
        if not str(item["url"]).startswith(base_url.rstrip("/") + f"/tyde/v{version}/"):
            raise ReleaseToolError(
                f"manifest download is outside the release prefix: {item['url']}"
            )
    if not any(item["wasm"] for item in items):
        raise ReleaseToolError("manifest download plan has no WASM artifact")
    return {"count": len(items), "items": items}


def validate_download(
    path: pathlib.Path, expected_integrity: str, content_type: str, wasm: bool
) -> None:
    if not mobile_manifest.INTEGRITY_RE.fullmatch(expected_integrity):
        raise ReleaseToolError("expected integrity is not a sha384 SRI value")
    digest = base64.b64encode(hashlib.sha384(path.read_bytes()).digest()).decode("ascii")
    actual = f"sha384-{digest}"
    if actual != expected_integrity:
        raise ReleaseToolError(
            f"SRI mismatch for {path}: expected {expected_integrity}, got {actual}"
        )
    mime = content_type.partition(";")[0].strip().lower()
    if wasm and mime != "application/wasm":
        raise ReleaseToolError(
            f"WASM content type must be application/wasm, got {content_type!r}"
        )


def parse_duration(raw: str) -> int:
    match = re.fullmatch(r"([1-9][0-9]*)([smh]?)", raw)
    if match is None:
        raise ReleaseToolError(f"invalid duration {raw!r}; use seconds or an s/m/h suffix")
    multipliers = {"": 1, "s": 1, "m": 60, "h": 3600}
    return int(match.group(1)) * multipliers[match.group(2)]


def run_command_with_timeout(command: list[str], timeout: float) -> int:
    if not command:
        raise ReleaseToolError("bounded command must not be empty")
    if timeout <= 0:
        raise ReleaseToolError("bounded command timeout must be positive")
    process = subprocess.Popen(command, start_new_session=True)
    try:
        return process.wait(timeout=timeout)
    except subprocess.TimeoutExpired:
        try:
            os.killpg(process.pid, signal.SIGKILL)
        except ProcessLookupError:
            return process.wait()
        process.wait()
        print(
            f"ERROR: command timed out after {timeout:g}s: {command[0]}",
            file=sys.stderr,
        )
        return EXIT_TIMEOUT
    except KeyboardInterrupt:
        try:
            os.killpg(process.pid, signal.SIGTERM)
        except ProcessLookupError:
            pass
        process.wait()
        return 130


def header_content_type(text: str) -> str:
    values = []
    for line in text.splitlines():
        name, separator, value = line.partition(":")
        if separator and name.strip().lower() == "content-type":
            values.append(value.strip())
    if not values:
        raise ReleaseToolError("HTTP response omitted Content-Type")
    return values[-1]


def json_field(data: Any, field: str) -> str:
    value = data
    for part in field.split("."):
        if isinstance(value, list):
            value = value[int(part)]
        elif isinstance(value, dict):
            value = value[part]
        else:
            raise ReleaseToolError(f"cannot read {field!r} from JSON value")
    if isinstance(value, bool):
        return "true" if value else "false"
    if value is None:
        return ""
    if isinstance(value, (dict, list)):
        return json.dumps(value, separators=(",", ":"), sort_keys=True)
    return str(value)


def build_parser() -> argparse.ArgumentParser:
    parser = argparse.ArgumentParser(
        description="Release-tooling validation and bounded execution helpers."
    )
    subparsers = parser.add_subparsers(dest="command", required=True)

    validate = subparsers.add_parser("validate-tag")
    validate.add_argument("tag")
    validate.add_argument("--beta-only", action="store_true")
    validate.add_argument("--stable-only", action="store_true")

    select = subparsers.add_parser("select-run")
    select.add_argument("tag")
    select.add_argument("sha")
    select.add_argument("--input", type=pathlib.Path)

    for name in ("run-report", "run-signature", "run-outcome"):
        command = subparsers.add_parser(name)
        command.add_argument("--input", type=pathlib.Path)
        if name == "run-report":
            command.add_argument("--tag", required=True)

    failure = subparsers.add_parser("failure-field")
    failure.add_argument("field", choices=("id", "job", "step", "url"))
    failure.add_argument("--input", type=pathlib.Path)

    sanitize = subparsers.add_parser("sanitize-log")
    sanitize.add_argument("--max-lines", type=int, default=80)
    sanitize.add_argument("--max-chars", type=int, default=12000)

    release = subparsers.add_parser("validate-release")
    release.add_argument("tag")
    release.add_argument("--input", type=pathlib.Path)
    release.add_argument("--require-published", action="store_true")

    plan = subparsers.add_parser("manifest-plan")
    plan.add_argument("tag")
    plan.add_argument("--manifest", type=pathlib.Path, required=True)
    plan.add_argument("--base-url", default="https://tycode.dev")
    plan.add_argument("--output", type=pathlib.Path, required=True)

    field = subparsers.add_parser("json-field")
    field.add_argument("field")
    field.add_argument("--input", type=pathlib.Path, required=True)

    download = subparsers.add_parser("validate-download")
    download.add_argument("--path", type=pathlib.Path, required=True)
    download.add_argument("--integrity", required=True)
    download.add_argument("--content-type", default="")
    download.add_argument("--wasm", action="store_true")

    duration = subparsers.add_parser("parse-duration")
    duration.add_argument("duration")

    headers = subparsers.add_parser("header-content-type")
    headers.add_argument("--input", type=pathlib.Path, required=True)

    runner = subparsers.add_parser("run-command")
    runner.add_argument("--timeout", type=float, required=True)
    runner.add_argument("command_args", nargs=argparse.REMAINDER)
    return parser


def main(argv: list[str] | None = None) -> int:
    args = build_parser().parse_args(sys.argv[1:] if argv is None else argv)
    try:
        if args.command == "validate-tag":
            print(validate_tag_kind(args.tag, args.beta_only, args.stable_only))
        elif args.command == "select-run":
            print(select_run(load_json(args.input), args.tag, args.sha)["databaseId"])
        elif args.command == "run-report":
            print(run_report(load_json(args.input), args.tag))
        elif args.command == "run-signature":
            print(run_signature(load_json(args.input)))
        elif args.command == "run-outcome":
            print(run_outcome(load_json(args.input)))
        elif args.command == "failure-field":
            print(failure_info(load_json(args.input))[args.field])
        elif args.command == "sanitize-log":
            if args.max_lines < 1 or args.max_chars < 1:
                raise ReleaseToolError("log bounds must be positive")
            print(sanitize_log(sys.stdin.read(), args.max_lines, args.max_chars))
        elif args.command == "validate-release":
            report = validate_release(load_json(args.input), args.tag, args.require_published)
            print(
                f"release assets OK: count={report['assets']} "
                f"draft={str(report['draft']).lower()} "
                f"prerelease={str(report['prerelease']).lower()} {report['url']}".rstrip()
            )
        elif args.command == "manifest-plan":
            plan = manifest_plan(load_json(args.manifest), args.tag, args.base_url)
            args.output.write_text(json.dumps(plan, indent=2) + "\n", encoding="utf-8")
        elif args.command == "json-field":
            print(json_field(load_json(args.input), args.field))
        elif args.command == "validate-download":
            validate_download(args.path, args.integrity, args.content_type, args.wasm)
        elif args.command == "parse-duration":
            print(parse_duration(args.duration))
        elif args.command == "header-content-type":
            print(header_content_type(args.input.read_text(encoding="utf-8")))
        elif args.command == "run-command":
            command = args.command_args
            if command and command[0] == "--":
                command = command[1:]
            return run_command_with_timeout(command, args.timeout)
        else:  # pragma: no cover
            raise ReleaseToolError(f"unsupported command {args.command}")
    except RunNotFoundError as error:
        print(f"ERROR: {error}", file=sys.stderr)
        return EXIT_NOT_FOUND
    except (
        IndexError,
        KeyError,
        OSError,
        ValueError,
        json.JSONDecodeError,
    ) as error:
        print(f"ERROR: {error}", file=sys.stderr)
        return 1
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
