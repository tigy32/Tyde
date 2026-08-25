#!/usr/bin/env python3
from __future__ import annotations

import argparse
import json
import re
import subprocess
import sys
import time
import uuid


WORKFLOW = "pretag-release-build.yml"
WORKFLOW_NAME = "Pre-tag release build"
EXIT_FAILURE = 1
EXIT_USAGE = 2
EXIT_NOT_FOUND = 3
EXIT_RUNNING = 4
EXIT_TOOL = 5


class ToolError(RuntimeError):
    pass


def gh(*args: str, timeout: float = 30) -> str:
    try:
        result = subprocess.run(
            ["gh", *args], capture_output=True, text=True, timeout=timeout, check=False
        )
    except (OSError, subprocess.TimeoutExpired) as error:
        raise ToolError(f"GitHub command failed: {error}") from error
    if result.returncode:
        raise ToolError(result.stderr.strip() or "GitHub command failed")
    return result.stdout


def validate_source_ref(source_ref: str) -> None:
    if not source_ref or len(source_ref) > 256 or any(
        ord(character) < 32 or character.isspace() for character in source_ref
    ):
        raise ValueError("source_ref must be a non-empty ref without whitespace")
    # The run is dispatched *on* the candidate so its Actions cache scope is the
    # candidate branch, never main. GitHub's workflow-dispatch API only accepts a
    # branch or tag name for that ref, so a bare SHA has to be pushed first.
    if re.fullmatch(r"[0-9a-fA-F]{7,40}", source_ref):
        raise ValueError(
            "source_ref must be a pushed branch or tag name, not a commit SHA; "
            "push the commit to a temporary branch and dispatch that"
        )


def run_view(run_id: int) -> dict[str, object]:
    data = json.loads(
        gh(
            "run",
            "view",
            str(run_id),
            "--json",
            "databaseId,workflowName,status,conclusion,url,jobs,displayTitle,headSha",
        )
    )
    if data.get("workflowName") != WORKFLOW_NAME:
        raise ToolError(f"run {run_id} is not a {WORKFLOW_NAME} workflow")
    return data


def outcome(run: dict[str, object]) -> str:
    if run.get("status") != "completed":
        return "running"
    return "success" if run.get("conclusion") == "success" else "failure"


def report(run: dict[str, object]) -> None:
    print(
        f"run {run['databaseId']} ({run.get('headSha', '?')}): {run.get('status')} "
        f"conclusion={run.get('conclusion') or '-'} {run.get('url', '')}"
    )
    for job in run.get("jobs", []):
        print(
            f"  {job.get('name')}: {job.get('status')} "
            f"conclusion={job.get('conclusion') or '-'}"
        )


def dispatch(source_ref: str, timeout: int, confirm: bool) -> int:
    validate_source_ref(source_ref)
    if not confirm:
        raise ValueError("dispatch requires --confirm")
    request_id = f"pretag-{int(time.time())}-{uuid.uuid4().hex[:12]}"
    gh(
        "workflow",
        "run",
        WORKFLOW,
        "--ref",
        source_ref,
        "-f",
        f"request_id={request_id}",
    )
    deadline = time.monotonic() + timeout
    title = f"{WORKFLOW_NAME} {request_id}"
    while time.monotonic() < deadline:
        remaining = max(0.1, deadline - time.monotonic())
        runs = json.loads(
            gh(
                "run",
                "list",
                "--workflow",
                WORKFLOW,
                "--event",
                "workflow_dispatch",
                "--limit",
                "30",
                "--json",
                "databaseId,displayTitle,url,headBranch,headSha",
                timeout=min(30, remaining),
            )
        )
        matches = [run for run in runs if run.get("displayTitle") == title]
        if len(matches) == 1:
            run = matches[0]
            print(
                f"Dispatched run {run['databaseId']} on "
                f"{run.get('headBranch') or source_ref}@{run.get('headSha', '?')}: "
                f"{run.get('url', '')}"
            )
            return 0
        if len(matches) > 1:
            raise ToolError(f"multiple workflow runs matched request {request_id}")
        time.sleep(min(5, max(0, deadline - time.monotonic())))
    print(f"workflow dispatch succeeded but no run appeared within {timeout}s", file=sys.stderr)
    return EXIT_NOT_FOUND


def status(run_id: int) -> int:
    run = run_view(run_id)
    report(run)
    return {"success": 0, "failure": EXIT_FAILURE, "running": EXIT_RUNNING}[
        outcome(run)
    ]


def wait(run_id: int, timeout: int, interval: int) -> int:
    run_view(run_id)
    try:
        watched = subprocess.run(
            ["gh", "run", "watch", str(run_id), "--exit-status", "--interval", str(interval)],
            timeout=timeout,
            check=False,
        )
    except subprocess.TimeoutExpired:
        print(f"workflow is still running after {timeout}s", file=sys.stderr)
        return EXIT_RUNNING
    except OSError as error:
        raise ToolError(f"GitHub command failed: {error}") from error
    final = run_view(run_id)
    report(final)
    if watched.returncode and outcome(final) == "running":
        return EXIT_TOOL
    return {"success": 0, "failure": EXIT_FAILURE, "running": EXIT_RUNNING}[
        outcome(final)
    ]


def positive(value: str) -> int:
    parsed = int(value)
    if parsed < 1:
        raise argparse.ArgumentTypeError("must be positive")
    return parsed


def parser() -> argparse.ArgumentParser:
    root = argparse.ArgumentParser(description="Dispatch and monitor non-publishing pre-tag builds.")
    commands = root.add_subparsers(dest="command", required=True)
    start = commands.add_parser("dispatch")
    start.add_argument("source_ref", help="pushed branch or tag to validate")
    start.add_argument("--discovery-timeout", type=positive, default=60)
    start.add_argument("--confirm", action="store_true")
    inspect = commands.add_parser("status")
    inspect.add_argument("run_id", type=positive)
    block = commands.add_parser("wait")
    block.add_argument("run_id", type=positive)
    block.add_argument("--timeout", type=positive, default=5400)
    block.add_argument("--interval", type=positive, default=30)
    return root


def main(argv: list[str] | None = None) -> int:
    args = parser().parse_args(argv)
    try:
        gh("auth", "status")
        if args.command == "dispatch":
            return dispatch(args.source_ref, args.discovery_timeout, args.confirm)
        if args.command == "status":
            return status(args.run_id)
        return wait(args.run_id, args.timeout, args.interval)
    except ValueError as error:
        print(f"ERROR: {error}", file=sys.stderr)
        return EXIT_USAGE
    except (ToolError, json.JSONDecodeError, KeyError, TypeError) as error:
        print(f"ERROR: {error}", file=sys.stderr)
        return EXIT_TOOL


if __name__ == "__main__":
    raise SystemExit(main())
