from __future__ import annotations

import json
import os
import pathlib
import shutil
import subprocess
import sys
import tempfile
import unittest

ROOT = pathlib.Path(__file__).resolve().parent.parent
TOOL = ROOT / "tools/release_cadence.py"
VERSION_PATHS = (
    "package.json", "package-lock.json", "Cargo.lock",
    "frontend/tauri-shell/tauri.conf.json", "frontend/tauri-shell/Cargo.toml",
    "tyde-server/Cargo.toml",
)


class ReleaseCadenceGitFlow(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory(prefix="tyde-cadence-e2e-")
        self.addCleanup(self.temp.cleanup)
        self.directory = pathlib.Path(self.temp.name)
        self.root = self.directory / "repo"
        self.root.mkdir()
        self.env = {**os.environ, "GIT_CONFIG_GLOBAL": os.devnull,
                    "GIT_CONFIG_NOSYSTEM": "1", "CADENCE_CHECK_LOG": str(self.directory / "checks")}
        for key in ("GIT_DIR", "GIT_WORK_TREE", "GIT_INDEX_FILE"):
            self.env.pop(key, None)
        self.git("init", "-b", "main")
        self.git("config", "user.name", "Release tests")
        self.git("config", "user.email", "release@example.test")
        for name in VERSION_PATHS:
            target = self.root / name
            target.parent.mkdir(parents=True, exist_ok=True)
            shutil.copyfile(ROOT / name, target)
        (self.root / "tools").mkdir()
        shutil.copyfile(ROOT / "tools/check_release_version.py", self.root / "tools/check_release_version.py")
        (self.root / ".gitignore").write_text("/target\n")
        (self.root / "application.txt").write_text("tested beta source\n")
        # The fixture is a real Git repository with an external check entry point.
        # It exposes gate ordering/failure without recursively running this suite.
        check = self.root / "dev.sh"
        check.write_text('''#!/bin/sh
set -eu
test "$1" = check
test -z "$(git status --porcelain)"
printf '%s %s\\n' "$(git branch --show-current)" "$(git rev-parse HEAD)" >> "$CADENCE_CHECK_LOG"
if [ "${CADENCE_FAIL_BRANCH:-}" = "$(git branch --show-current)" ]; then exit 19; fi
''')
        check.chmod(0o755)
        guard = self.root / "tools/release_check.sh"
        guard.write_text('#!/bin/sh\nset -eu\npython3 tools/check_release_version.py "$1"\n./dev.sh check\n')
        guard.chmod(0o755)
        self.bump("v1.2.3-beta.2")
        self.commit("Seed tested beta")
        self.beta_sha = self.git("rev-parse", "HEAD")
        self.git("tag", "-a", "v1.2.3-beta.2", "-m", "Beta")
        self.remote = self.directory / "origin.git"
        self.command("git", "init", "--bare", str(self.remote))
        self.git("remote", "add", "origin", str(self.remote))
        self.sync()
        self.catalog = self.directory / "catalog.json"
        self.catalog.write_text(json.dumps([
            {"tagName": "v1.2.3-beta.2", "isDraft": False, "isPrerelease": True}
        ]))
        self.plan_path = self.directory / "plan.json"

    def command(self, *args, cwd=None, ok=True, env=None):
        result = subprocess.run(args, cwd=cwd or self.root, env=env or self.env,
                                text=True, capture_output=True)
        if ok:
            self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
        return result

    def git(self, *args, cwd=None):
        return self.command("git", *args, cwd=cwd).stdout.strip()

    def commit(self, message):
        self.git("add", ".")
        self.git("commit", "-m", message)

    def sync(self):
        self.git("push", "origin", "main", "--tags")
        self.git("fetch", "origin", "main")

    def bump(self, tag):
        self.command(sys.executable, str(ROOT / "tools/set_release_version.py"),
                     tag, "--repo-root", str(self.root))

    def tool(self, *args, **kwargs):
        return self.command(sys.executable, str(TOOL), "--repo-root", str(self.root), *args, **kwargs)

    def plan(self, beta=None, ok=True):
        args = ["plan", "--catalog", str(self.catalog), "--output", str(self.plan_path)]
        if beta:
            args += ["--promote", beta]
        result = self.tool(*args, ok=ok)
        return json.loads(self.plan_path.read_text()) if result.returncode == 0 else result

    def stage(self, name="workbenches"):
        directory = self.directory / name
        self.tool("stage", "--plan", str(self.plan_path), "--directory", str(directory), "--confirm")
        path = directory / "state.json"
        return path, json.loads(path.read_text())

    def test_nightly_skips_bookkeeping_but_releases_dependency_changes(self):
        self.assertEqual(self.plan()["action"], "skip")
        self.bump("v1.2.3-beta.9")
        self.commit("Version bookkeeping")
        self.sync()
        self.assertEqual(self.plan()["action"], "skip")
        lock = self.root / "package-lock.json"
        data = json.loads(lock.read_text())
        data["packages"][""]["cadenceDependency"] = "changed"
        lock.write_text(json.dumps(data, indent=2) + "\n")
        self.commit("Change a dependency")
        self.sync()
        before = self.git("rev-parse", "HEAD")
        plan = self.plan()
        self.assertEqual((plan["action"], plan["tag"]), ("beta", "v1.2.3-beta.10"))
        state_path, state = self.stage()
        self.assertEqual(self.git("rev-parse", "HEAD"), before)
        self.assertEqual(self.git("rev-parse", "origin/main"), before)
        self.tool("land", "--state", str(state_path), "--confirm")
        self.assertEqual(self.git("rev-parse", "HEAD"), state["release_sha"])
        self.assertEqual(self.git("status", "--porcelain"), "")
        self.assertFalse(pathlib.Path(state["candidate"]).exists())
        log = (self.directory / "checks").read_text().splitlines()
        self.assertEqual(len(log), 3)
        self.assertTrue(log[0].startswith("release-cadence-v1.2.3-beta.10 "))
        self.assertTrue(log[2].startswith("main "))
        self.assertEqual(self.git("rev-parse", "origin/main"), before, "Landing alone must not push")
        self.git("tag", "-a", plan["tag"], "-m", "Beta")
        self.sync()
        self.assertEqual(self.plan()["action"], "retry", "Interrupted dispatch retries the same tag")
        self.catalog.write_text(json.dumps([
            {"tagName": plan["tag"], "isDraft": True, "isPrerelease": True}
        ]))
        self.assertEqual(self.plan()["tag"], plan["tag"])
        self.catalog.write_text(json.dumps([
            {"tagName": plan["tag"], "isDraft": False, "isPrerelease": True}
        ]))
        self.assertEqual(self.plan()["action"], "skip")

    def test_promote_older_beta_preserves_main_and_advances_future_betas(self):
        (self.root / "application.txt").write_text("newer unapproved main source\n")
        self.commit("Land newer code")
        self.sync()
        before = self.git("rev-parse", "HEAD")
        plan = self.plan("v1.2.3-beta.2")
        self.assertEqual((plan["tag"], plan["next_tag"]), ("v1.2.3", "v1.2.4-beta.0"))
        state_path, state = self.stage()
        candidate = pathlib.Path(state["candidate"])
        landing = pathlib.Path(state["landing"])
        self.assertEqual((candidate / "application.txt").read_text(), "tested beta source\n")
        self.assertEqual((landing / "application.txt").read_text(), "newer unapproved main source\n")
        changed = set(self.git("diff", "--name-only", self.beta_sha, state["release_sha"]).splitlines())
        self.assertEqual(changed, set(VERSION_PATHS))
        self.command(sys.executable, "tools/check_release_version.py", "v1.2.3", cwd=candidate)
        self.command(sys.executable, "tools/check_release_version.py", "v1.2.4-beta.0", cwd=landing)
        self.assertEqual(self.git("rev-parse", "HEAD"), before)
        self.tool("land", "--state", str(state_path), "--confirm")
        self.git("merge-base", "--is-ancestor", state["release_sha"], "main")
        self.git("tag", "-a", "v1.2.3", state["release_sha"], "-m", "Stable")
        self.sync()
        self.assertEqual(self.git("show", "v1.2.3:application.txt"), "tested beta source")
        self.assertEqual((self.root / "application.txt").read_text(), "newer unapproved main source\n")
        self.assertEqual(self.git("status", "--porcelain"), "")
        checks = (self.directory / "checks").read_text().splitlines()
        self.assertEqual([line.split()[0] for line in checks],
                         ["release-cadence-v1.2.3", "release-cadence-v1.2.3",
                          "release-cadence-v1.2.3-landing", "main"])
        self.assertNotEqual(self.plan("v1.2.3-beta.2", ok=False).returncode, 0)
        self.assertEqual(self.plan()["tag"], "v1.2.4-beta.1")
        self.assertEqual(self.git("worktree", "list", "--porcelain").count("worktree "), 1)

    def test_no_nightly_after_promotion_until_source_changes(self):
        self.plan("v1.2.3-beta.2")
        state_path, state = self.stage()
        self.tool("land", "--state", str(state_path), "--confirm")
        self.git("tag", "-a", "v1.2.3", state["release_sha"], "-m", "Stable")
        self.sync()
        self.assertEqual(self.plan()["action"], "skip")
        # Changes to Cargo.lock outside the release package versions still count.
        lock = self.root / "Cargo.lock"
        lock.write_text(lock.read_text() + "\n# New dependency resolution\n")
        self.commit("Change lockfile source")
        self.sync()
        self.assertEqual(self.plan()["tag"], "v1.2.4-beta.1")

    def test_failed_gates_and_moved_main_never_land(self):
        (self.root / "application.txt").write_text("new code\n")
        self.commit("Change source")
        self.sync()
        before = self.git("rev-parse", "HEAD")
        self.plan()
        state_path, state = self.stage()
        result = self.tool("land", "--state", str(state_path), "--confirm", ok=False,
                           env={**self.env, "CADENCE_FAIL_BRANCH": state["branch"]})
        self.assertNotEqual(result.returncode, 0)
        self.assertEqual(self.git("rev-parse", "HEAD"), before)
        self.assertEqual(self.git("tag", "--list", state["tag"]), "")
        (self.root / "application.txt").write_text("concurrent main change\n")
        self.commit("Concurrent change")
        moved = self.git("rev-parse", "HEAD")
        result = self.tool("land", "--state", str(state_path), "--confirm", ok=False)
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("main moved", result.stderr)
        self.assertEqual(self.git("rev-parse", "HEAD"), moved)
        self.assertEqual(self.git("status", "--porcelain"), "")
        self.assertNotEqual(self.plan(ok=False).returncode, 0, "Unpushed main must not be released")

    def test_execute_pushes_validated_main_and_tag_before_dispatch(self):
        self.plan()
        self.tool("execute", "--plan", str(self.plan_path), "--directory",
                  str(self.directory / "unused"), "--confirm")
        self.assertFalse((self.directory / "unused").exists())
        hooks = self.root / ".githooks"
        hooks.mkdir()
        shutil.copy2(ROOT / ".githooks/pre-push", hooks / "pre-push")
        self.git("config", "core.hooksPath", ".githooks")
        self.commit("Install release hook")
        self.sync()
        plan = self.plan()
        commands = self.directory / "dispatch.json"
        binary = self.directory / "bin"
        binary.mkdir()
        gh = binary / "gh"
        gh.write_text(
            "#!/usr/bin/env python3\nimport json, pathlib, sys\n"
            f"pathlib.Path({str(commands)!r}).write_text(json.dumps(sys.argv[1:]))\n"
        )
        gh.chmod(0o755)
        env = {**self.env, "PATH": f"{binary}:{self.env['PATH']}"}
        self.tool("execute", "--plan", str(self.plan_path), "--directory",
                  str(self.directory / "run"), "--confirm", env=env)
        sha = self.git("rev-parse", f"{plan['tag']}^{{commit}}")
        self.assertEqual(self.git("rev-parse", "origin/main"), sha)
        remote = self.git("ls-remote", "origin", f"refs/tags/{plan['tag']}^{{}}")
        self.assertTrue(remote.startswith(sha + "\t"))
        self.assertEqual(self.git("cat-file", "-t", plan["tag"]), "tag")
        self.assertEqual(json.loads(commands.read_text()), [
            "workflow", "run", "release.yml", "--ref", "main", "-f",
            f"version={plan['tag']}", "-f", f"source_sha={sha}", "-f", "publish=true",
        ])
        self.assertEqual(self.git("status", "--porcelain"), "")
        runs = self.directory / "runs.json"
        runs.write_text(json.dumps([
            {"databaseId": 10, "workflowName": "Release", "headBranch": "main",
             "headSha": "unrelated", "createdAt": "2026-09-17",
             "displayTitle": f"Release {plan['tag']} @ {sha}"},
            {"databaseId": 11, "workflowName": "Release", "headBranch": "main",
             "headSha": sha, "createdAt": "2026-09-18",
             "displayTitle": f"Release {plan['tag']} @ wrong-sha"},
        ]))
        selected = self.command(sys.executable, str(ROOT / "tools/release_tool.py"),
                                "select-run", plan["tag"], sha, "--input", str(runs))
        self.assertEqual(selected.stdout.strip(), "10")

    def test_promotion_refuses_drafts_and_off_main_tags(self):
        self.catalog.write_text(json.dumps([
            {"tagName": "v1.2.3-beta.2", "isDraft": True, "isPrerelease": True}
        ]))
        self.assertNotEqual(self.plan("v1.2.3-beta.2", ok=False).returncode, 0)
        self.git("checkout", "-b", "unmerged")
        (self.root / "application.txt").write_text("not on main\n")
        self.commit("Unmerged beta")
        self.git("tag", "v1.2.3-beta.3")
        self.git("checkout", "main")
        self.catalog.write_text(json.dumps([
            {"tagName": "v1.2.3-beta.3", "isDraft": False, "isPrerelease": True}
        ]))
        self.assertNotEqual(self.plan("v1.2.3-beta.3", ok=False).returncode, 0)
        self.assertEqual(self.git("rev-parse", "HEAD"), self.beta_sha)


if __name__ == "__main__":
    unittest.main()
