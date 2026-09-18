#!/usr/bin/env python3
"""Plan nightly betas and preserve a selected beta's source during promotion."""
from __future__ import annotations

import argparse
import hashlib
import json
import os
import pathlib
import re
import subprocess
import sys
import tempfile

import check_release_version as versions
from set_release_version import normalize_tag, set_release_version

ROOT = pathlib.Path(__file__).resolve().parent.parent
BETA = re.compile(r"^v(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)-beta\.(0|[1-9][0-9]*)$")
STABLE = re.compile(r"^v(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)$")


def run(root: pathlib.Path, *args: str) -> str:
    result = subprocess.run(args, cwd=root, text=True, stdout=subprocess.PIPE, check=True)
    return result.stdout.strip()


def git(root: pathlib.Path, *args: str) -> str:
    return run(root, "git", *args)


def clean(root: pathlib.Path) -> None:
    if git(root, "status", "--porcelain"):
        raise ValueError(f"working tree is not clean: {root}")


def ancestor(root: pathlib.Path, older: str, newer: str) -> bool:
    result = subprocess.run(["git", "merge-base", "--is-ancestor", older, newer], cwd=root)
    if result.returncode not in (0, 1):
        raise ValueError(f"cannot compare {older} and {newer}")
    return result.returncode == 0


def source_digest(root: pathlib.Path, ref: str) -> str:
    # Normalize only version values, not whole version-bearing files: dependency
    # changes in Cargo.lock/package-lock.json must still trigger a nightly.
    with tempfile.TemporaryDirectory(prefix="tyde-release-source-") as directory:
        snapshot = pathlib.Path(directory)
        paths = versions.release_version_paths(snapshot)
        normalized = {str(path.relative_to(snapshot)) for path in paths}
        for path in paths:
            path.parent.mkdir(parents=True, exist_ok=True)
            path.write_text(git(root, "show", f"{ref}:{path.relative_to(snapshot)}") + "\n")
        set_release_version(snapshot, "v0.0.0")
        entries = subprocess.check_output(["git", "ls-tree", "-rz", ref], cwd=root).split(b"\0")
        digest = hashlib.sha256()
        for entry in entries:
            if not entry:
                continue
            metadata, raw_path = entry.split(b"\t", 1)
            path = os.fsdecode(raw_path)
            mode, kind, blob = metadata.split()
            if path in normalized:
                blob = hashlib.sha256((snapshot / path).read_bytes()).hexdigest().encode()
            digest.update(b" ".join((mode, kind, blob)) + b"\t" + raw_path + b"\0")
        return digest.hexdigest()


def make_plan(root: pathlib.Path, catalog: list[dict], promote: str | None) -> dict:
    clean(root)
    if git(root, "branch", "--show-current") != "main":
        raise ValueError("release cadence requires clean main")
    head = git(root, "rev-parse", "HEAD")
    if head != git(root, "rev-parse", "origin/main"):
        raise ValueError("main must exactly match origin/main")
    tags = git(root, "tag", "--merged", "main").splitlines()
    releases = {release["tagName"]: release for release in catalog}
    betas = sorted((tag for tag in tags if BETA.fullmatch(tag)),
                   key=lambda tag: tuple(map(int, BETA.fullmatch(tag).groups())))
    current = "v" + versions.read_json_version(root / "package.json")
    normalize_tag(current)
    result = {"head": head, "action": "skip", "tag": "", "source": head}
    if promote:
        match = BETA.fullmatch(promote)
        if not match or promote not in tags:
            raise ValueError("promotion requires a beta tag already contained in main")
        release = releases.get(promote)
        if not release or release["isDraft"] or not release["isPrerelease"]:
            raise ValueError("promotion requires a published beta")
        core = tuple(map(int, match.groups()[:3]))
        tag = "v" + ".".join(map(str, core))
        stable_tags = [tuple(map(int, STABLE.fullmatch(item).groups()))
                       for item in tags if STABLE.fullmatch(item)]
        if stable_tags and max(stable_tags) >= core:
            raise ValueError("promotion must advance the latest stable release")
        if git(root, "tag", "--list", tag):
            raise ValueError(f"{tag} already exists")
        next_core = (*core[:2], core[2] + 1)
        current_core = tuple(map(int, current[1:].split("-")[0].split(".")))
        if current_core != core:
            raise ValueError("main is in another release cycle; inspect before promoting")
        result.update(action="promote", tag=tag, beta=promote,
                      source=git(root, "rev-parse", f"{promote}^{{commit}}"),
                      next_tag="v" + ".".join(map(str, next_core)) + "-beta.0")
        return result
    if betas and source_digest(root, betas[-1]) == source_digest(root, "HEAD"):
        latest = betas[-1]
        release = releases.get(latest)
        if not release or release["isDraft"]:
            result.update(action="retry", tag=latest,
                          source=git(root, "rev-parse", f"{latest}^{{commit}}"))
        return result
    match = BETA.fullmatch(current)
    if match:
        core = tuple(map(int, match.groups()[:3]))
        number = int(match[4])
    elif STABLE.fullmatch(current):
        major, minor, patch = map(int, STABLE.fullmatch(current).groups())
        core, number = (major, minor, patch + 1), 0
    else:
        raise ValueError(f"unsupported release cycle: {current}")
    prefix = "v" + ".".join(map(str, core)) + "-beta."
    number = max([number, *(int(tag.removeprefix(prefix)) for tag in betas if tag.startswith(prefix))])
    tag = prefix + str(number + 1)
    if git(root, "tag", "--list", tag):
        raise ValueError(f"{tag} exists outside main; refusing to overwrite it")
    result.update(action="beta", tag=tag)
    return result


def verify_plan(root: pathlib.Path, plan: dict) -> None:
    clean(root)
    if git(root, "branch", "--show-current") != "main" or git(root, "rev-parse", "HEAD") != plan["head"]:
        raise ValueError("main moved since planning; start a new run")
    if plan["action"] != "skip":
        normalize_tag(plan["tag"])
    if plan["action"] == "promote" and git(root, "rev-parse", f"{plan['beta']}^{{commit}}") != plan["source"]:
        raise ValueError("selected beta moved since planning")
    if not ancestor(root, plan["source"], "main"):
        raise ValueError("release source is not contained in main")


def bump(root: pathlib.Path, tag: str, subject: str, body: str) -> None:
    paths = set_release_version(root, tag)
    if not paths:
        raise ValueError(f"nothing to prepare for {tag}")
    git(root, "add", "--", *(str(path) for path in paths))
    git(root, "commit", "-m", subject, "-m", body)


def stage(root: pathlib.Path, plan: dict, directory: pathlib.Path) -> dict:
    verify_plan(root, plan)
    if plan["action"] not in ("beta", "promote"):
        raise ValueError("only a new beta or promotion needs a workbench")
    directory.mkdir(parents=True, exist_ok=False)
    candidate = directory / "candidate"
    branch = f"release-cadence-{plan['tag']}"
    git(root, "worktree", "add", "-b", branch, str(candidate), plan["source"])
    bump(candidate, plan["tag"], f"Prepare {plan['tag']}",
         f"Prepare the release candidate for {plan['tag']}.")
    sha = git(candidate, "rev-parse", "HEAD")
    if source_digest(root, sha) != source_digest(root, plan["source"]):
        raise ValueError("release candidate changed more than version metadata")
    landing = candidate
    landing_branch = branch
    if plan["action"] == "promote":
        landing = directory / "landing"
        landing_branch = branch + "-landing"
        git(root, "worktree", "add", "-b", landing_branch, str(landing), plan["head"])
        # Keep newer main source intact while making the exact stable snapshot
        # an ancestor of main. The stable tag points at the snapshot, not this merge.
        git(landing, "merge", "--no-ff", "--no-commit", "-s", "ours", sha)
        bump(landing, plan["next_tag"], f"Advance betas after {plan['tag']}",
             f"Preserve main development while landing the {plan['beta']}\n"
             f"source snapshot as {plan['tag']}. Start the next beta cycle.")
        if source_digest(root, "HEAD") != source_digest(landing, "HEAD"):
            raise ValueError("promotion changed main source beyond version metadata")
    return {**plan, "candidate": str(candidate), "branch": branch,
            "landing": str(landing), "landing_branch": landing_branch, "release_sha": sha}


def validate(root: pathlib.Path, workbench: pathlib.Path) -> None:
    shared = root / "target"
    shared.mkdir(exist_ok=True)
    if workbench != root and not (workbench / "target").exists():
        (workbench / "target").symlink_to(shared, target_is_directory=True)
    subprocess.run(["./dev.sh", "check"], cwd=workbench, check=True)
    clean(workbench)


def land(root: pathlib.Path, state: dict) -> None:
    verify_plan(root, state)
    candidate = pathlib.Path(state["candidate"])
    landing = pathlib.Path(state["landing"])
    if git(candidate, "rev-parse", "HEAD") != state["release_sha"]:
        raise ValueError("release candidate moved after staging")
    if state["action"] != "beta":
        validate(root, candidate)
        subprocess.run(["tools/release_check.sh", state["tag"]], cwd=candidate, check=True)
    clean(candidate)
    if landing != candidate:
        validate(root, landing)
    landing_sha = git(landing, "rev-parse", "HEAD")
    if not ancestor(root, state["release_sha"], landing_sha):
        raise ValueError("landing does not contain the release candidate")
    if source_digest(root, state["source"]) != source_digest(root, state["release_sha"]):
        raise ValueError("release source changed")
    expected_main = state["head"] if state["action"] == "promote" else state["source"]
    if source_digest(root, expected_main) != source_digest(root, landing_sha):
        raise ValueError("landing changed main source")
    # The checker resolves its own checkout, so execute the candidate's copy.
    run(candidate, sys.executable, "tools/check_release_version.py", state["tag"])
    verify_plan(root, state)
    git(root, "merge", "--ff-only", landing_sha)
    if state["action"] != "beta":
        validate(root, root)
    clean(root)
    for path, branch in dict([(str(candidate), state["branch"]),
                              (str(landing), state["landing_branch"])]).items():
        git(root, "worktree", "remove", path)
        git(root, "branch", "-d", branch)


def dispatch(root: pathlib.Path, tag: str, sha: str) -> None:
    run(root, "gh", "workflow", "run", "release.yml", "--ref", "main",
        "-f", f"version={tag}", "-f", f"source_sha={sha}", "-f", "publish=true")
    print(f"Dispatched Release {tag} @ {sha}")


def execute(root: pathlib.Path, plan: dict, directory: pathlib.Path) -> None:
    verify_plan(root, plan)
    if plan["action"] == "skip":
        print("No source changes since the last beta")
        return
    if git(root, "config", "--get", "core.hooksPath") != ".githooks":
        raise ValueError("install the release pre-push hook before executing")
    if plan["action"] == "retry":
        runs = json.loads(run(root, "gh", "run", "list", "--workflow", "release.yml",
                              "--limit", "100", "--json", "displayTitle,status,headBranch,headSha"))
        title = f"Release {plan['tag']} @ {plan['source']}"
        if any(
            item["status"] != "completed" and (
                item["displayTitle"] == title or
                (item["headBranch"] == plan["tag"] and item["headSha"] == plan["source"])
            ) for item in runs
        ):
            print("The beta build is already running")
            return
        dispatch(root, plan["tag"], plan["source"])
        return
    if plan["action"] == "promote":
        subprocess.run(["./dev.sh", "release", "verify", plan["beta"]], cwd=root, check=True)
    state = stage(root, plan, directory)
    (directory / "state.json").write_text(json.dumps(state, indent=2) + "\n")
    land(root, state)
    git(root, "fetch", "--no-tags", "origin", "main")
    if git(root, "rev-parse", "origin/main") != plan["head"]:
        raise ValueError("origin/main advanced; nothing pushed, start a new run")
    if git(root, "ls-remote", "--tags", "origin", f"refs/tags/{plan['tag']}"):
        raise ValueError("remote release tag already exists")
    git(root, "tag", "-a", plan["tag"], state["release_sha"], "-m", f"Release {plan['tag']}")
    git(root, "push", "--atomic", "origin", "main", f"refs/tags/{plan['tag']}")
    # GITHUB_TOKEN tag pushes do not trigger other workflows. Dispatch explicitly
    # from main's current workflow while binding it to the immutable source SHA.
    dispatch(root, plan["tag"], state["release_sha"])


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--repo-root", type=pathlib.Path, default=ROOT, help=argparse.SUPPRESS)
    commands = parser.add_subparsers(dest="command", required=True)
    planning = commands.add_parser("plan")
    planning.add_argument("--catalog", type=pathlib.Path, required=True)
    planning.add_argument("--promote")
    planning.add_argument("--output", type=pathlib.Path, required=True)
    for name in ("stage", "execute"):
        command = commands.add_parser(name)
        command.add_argument("--plan", type=pathlib.Path, required=True)
        command.add_argument("--directory", type=pathlib.Path, required=True)
        command.add_argument("--confirm", action="store_true", required=True)
    landing = commands.add_parser("land")
    landing.add_argument("--state", type=pathlib.Path, required=True)
    landing.add_argument("--confirm", action="store_true", required=True)
    args = parser.parse_args()
    root = args.repo_root.resolve()
    try:
        if args.command == "plan":
            plan = make_plan(root, json.loads(args.catalog.read_text()), args.promote)
            args.output.write_text(json.dumps(plan, indent=2) + "\n")
            print(json.dumps(plan))
            if output := os.environ.get("GITHUB_OUTPUT"):
                with open(output, "a") as handle:
                    handle.write(f"action={plan['action']}\ntag={plan['tag']}\n")
        elif args.command == "land":
            land(root, json.loads(args.state.read_text()))
        else:
            plan = json.loads(args.plan.read_text())
            if args.command == "stage":
                state = stage(root, plan, args.directory.resolve())
                (args.directory / "state.json").write_text(json.dumps(state, indent=2) + "\n")
            else:
                execute(root, plan, args.directory.resolve())
    except (ValueError, OSError, subprocess.CalledProcessError) as error:
        print(f"ERROR: {error}", file=sys.stderr)
        return 1
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
