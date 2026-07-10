from __future__ import annotations

import base64
import contextlib
import hashlib
import io
import json
import os
import pathlib
import pty
import select
import shutil
import subprocess
import sys
import tempfile
import time
import unittest


TOOLS_DIR = pathlib.Path(__file__).resolve().parent
REPO_ROOT = TOOLS_DIR.parent
sys.path.insert(0, str(TOOLS_DIR))

import check_release_version as version_check
import release_tool
import set_release_version


TAG = "v1.2.3-beta.4"
SHA = "a" * 40


def release_assets(tag: str) -> list[dict[str, str]]:
    version = tag[1:]
    names = sorted(release_tool.HEADLESS_ASSETS)
    names.extend(
        [
            f"Tyde_{version}_aarch64-apple-darwin.dmg",
            f"Tyde_{version}_x86_64-apple-darwin.dmg",
            f"Tyde_{version}_amd64.AppImage",
            f"Tyde_{version}_aarch64.AppImage",
            f"Tyde_{version}_amd64.AppImage.sha256",
            f"Tyde_{version}_aarch64.AppImage.sha256",
            f"Tyde_{version}_amd64.deb",
            f"Tyde_{version}_arm64.deb",
            f"Tyde_{version}_amd64.deb.sha256",
            f"Tyde_{version}_arm64.deb.sha256",
            f"Tyde-{version}-1.x86_64.rpm",
            f"Tyde-{version}-1.aarch64.rpm",
            f"Tyde_{version}_x64-setup.exe",
        ]
    )
    if "-" not in tag:
        names.append(f"Tyde_{version}_x64_en-US.msi")
    return [{"name": name} for name in names]


def sri(data: bytes) -> str:
    digest = base64.b64encode(hashlib.sha384(data).digest()).decode("ascii")
    return f"sha384-{digest}"


class SetReleaseVersionTests(unittest.TestCase):
    def test_updates_every_checked_version_and_is_idempotent(self) -> None:
        with tempfile.TemporaryDirectory() as raw_dir:
            root = pathlib.Path(raw_dir)
            for source in version_check.release_version_paths(REPO_ROOT):
                relative = source.relative_to(REPO_ROOT)
                target = root / relative
                target.parent.mkdir(parents=True, exist_ok=True)
                shutil.copy2(source, target)

            changed = set_release_version.set_release_version(root, "v1.2.3-beta.4")

            self.assertEqual(set(changed), set(version_check.release_version_paths(root)))
            self.assertEqual(
                set(version_check.collect_versions(root).values()), {"1.2.3-beta.4"}
            )
            self.assertEqual(
                set_release_version.set_release_version(root, "v1.2.3-beta.4"), []
            )

    def test_rejects_non_strict_semver(self) -> None:
        for tag in ("1.2.3", "v01.2.3", "v1.2.3-beta.01", "v1.2.3+build"):
            with self.subTest(tag=tag):
                with self.assertRaises(set_release_version.SetVersionError):
                    set_release_version.normalize_tag(tag)


class RunStatusTests(unittest.TestCase):
    def test_selects_exact_workflow_tag_and_sha(self) -> None:
        runs = [
            {
                "databaseId": 1,
                "workflowName": "Release",
                "headBranch": TAG,
                "headSha": "b" * 40,
                "createdAt": "2026-01-01T00:00:00Z",
            },
            {
                "databaseId": 2,
                "workflowName": "Release",
                "headBranch": TAG,
                "headSha": SHA,
                "createdAt": "2026-01-02T00:00:00Z",
            },
        ]

        self.assertEqual(release_tool.select_run(runs, TAG, SHA)["databaseId"], 2)
        with self.assertRaises(release_tool.RunNotFoundError):
            release_tool.select_run(runs, TAG, "c" * 40)

    def test_normalizes_running_success_and_failure(self) -> None:
        running = {
            "databaseId": 2,
            "status": "in_progress",
            "conclusion": "",
            "url": "https://example/run/2",
            "jobs": [
                {
                    "databaseId": 20,
                    "name": "build",
                    "status": "in_progress",
                    "conclusion": "",
                }
            ],
        }
        success = {**running, "status": "completed", "conclusion": "success"}
        failure = {
            **running,
            "status": "completed",
            "conclusion": "failure",
            "jobs": [
                {
                    "databaseId": 20,
                    "name": "build",
                    "status": "completed",
                    "conclusion": "failure",
                    "url": "https://example/job/20",
                    "steps": [{"name": "Compile", "conclusion": "failure"}],
                }
            ],
        }

        self.assertEqual(release_tool.run_outcome(running), "running")
        self.assertEqual(release_tool.run_outcome(success), "success")
        self.assertEqual(release_tool.run_outcome(failure), "failure")
        self.assertNotEqual(
            release_tool.run_signature(running), release_tool.run_signature(success)
        )
        self.assertIn("build: in_progress", release_tool.run_report(running, TAG))
        self.assertEqual(release_tool.failure_info(failure)["step"], "Compile")

    def test_failed_log_excerpt_is_redacted_and_bounded(self) -> None:
        text = "\n".join(
            [f"line {index}" for index in range(100)]
            + [
                "Authorization: Bearer github_pat_TOPSECRET",
                "AWS_ACCESS_KEY_ID=AKIAABCDEFGHIJKLMNOP",
                "AWS_SECRET_ACCESS_KEY=this-must-not-leak",
                "password=hunter2",
            ]
        )

        excerpt = release_tool.sanitize_log(text, max_lines=8, max_chars=160)

        self.assertLessEqual(len(excerpt), 160)
        self.assertLessEqual(len(excerpt.splitlines()), 8)
        self.assertNotIn("TOPSECRET", excerpt)
        self.assertNotIn("AKIAABCDEFGHIJKLMNOP", excerpt)
        self.assertNotIn("this-must-not-leak", excerpt)
        self.assertNotIn("hunter2", excerpt)
        self.assertIn("[REDACTED]", excerpt)

    def test_bounded_command_terminates_at_deadline(self) -> None:
        started = time.monotonic()
        with contextlib.redirect_stderr(io.StringIO()):
            status = release_tool.run_command_with_timeout(
                [sys.executable, "-c", "import time; time.sleep(5)"], 0.1
            )

        self.assertEqual(status, release_tool.EXIT_TIMEOUT)
        self.assertLess(time.monotonic() - started, 2)


class BytecodeSafetyTests(unittest.TestCase):
    def test_release_entry_points_disable_and_ignore_bytecode(self) -> None:
        for relative in ("dev.sh", "tools/release.sh", "tools/release_check.sh"):
            source = (REPO_ROOT / relative).read_text(encoding="utf-8")
            self.assertIn("export PYTHONDONTWRITEBYTECODE=1", source)
        release_check = (REPO_ROOT / "tools/release_check.sh").read_text(
            encoding="utf-8"
        )
        self.assertNotIn("py_compile", release_check)
        gitignore = (REPO_ROOT / ".gitignore").read_text(encoding="utf-8")
        self.assertIn("__pycache__/", gitignore)
        self.assertIn("*.py[cod]", gitignore)


class ReleaseValidationTests(unittest.TestCase):
    def test_accepts_beta_without_msi_and_stable_with_msi(self) -> None:
        beta = {
            "tagName": TAG,
            "isDraft": True,
            "isPrerelease": True,
            "assets": release_assets(TAG),
        }
        stable_tag = "v1.2.3"
        stable = {
            "tagName": stable_tag,
            "isDraft": False,
            "isPrerelease": False,
            "assets": release_assets(stable_tag),
        }

        self.assertTrue(release_tool.validate_release(beta, TAG)["draft"])
        self.assertFalse(
            release_tool.validate_release(stable, stable_tag, require_published=True)[
                "prerelease"
            ]
        )

    def test_rejects_beta_msi_and_stable_without_msi(self) -> None:
        beta_assets = release_assets(TAG) + [{"name": "Tyde_1.2.3-beta.4_x64.msi"}]
        with self.assertRaisesRegex(release_tool.ReleaseToolError, "must not contain"):
            release_tool.validate_assets({"assets": beta_assets}, TAG)

        stable_assets = [
            asset
            for asset in release_assets("v1.2.3")
            if not asset["name"].endswith(".msi")
        ]
        with self.assertRaisesRegex(release_tool.ReleaseToolError, "must contain"):
            release_tool.validate_assets({"assets": stable_assets}, "v1.2.3")

    def test_rejects_missing_asset_and_wrong_publish_flags(self) -> None:
        assets = release_assets(TAG)
        assets.pop()
        with self.assertRaises(release_tool.ReleaseToolError):
            release_tool.validate_assets({"assets": assets}, TAG)
        release = {
            "tagName": TAG,
            "isDraft": True,
            "isPrerelease": True,
            "assets": release_assets(TAG),
        }
        with self.assertRaisesRegex(release_tool.ReleaseToolError, "still a draft"):
            release_tool.validate_release(release, TAG, require_published=True)


class MobileWebVerificationTests(unittest.TestCase):
    def test_manifest_plan_and_download_validation(self) -> None:
        entry_data = b"console.log('tyde');"
        wasm_data = b"\x00asm\x01\x00\x00\x00"
        version = TAG[1:]
        manifest = {
            "versions": {
                version: {
                    "entry": f"/tyde/v{version}/mobile.js",
                    "integrity": sri(entry_data),
                    "artifacts": {
                        f"/tyde/v{version}/mobile_bg.wasm": sri(wasm_data)
                    },
                }
            }
        }

        plan = release_tool.manifest_plan(manifest, TAG, "https://tycode.dev")

        self.assertEqual(plan["count"], 2)
        self.assertTrue(plan["items"][1]["wasm"])
        with tempfile.TemporaryDirectory() as raw_dir:
            wasm = pathlib.Path(raw_dir) / "mobile.wasm"
            wasm.write_bytes(wasm_data)
            release_tool.validate_download(
                wasm, sri(wasm_data), "application/wasm; charset=binary", True
            )

    def test_rejects_sri_and_wasm_mime_failures(self) -> None:
        with tempfile.TemporaryDirectory() as raw_dir:
            artifact = pathlib.Path(raw_dir) / "mobile.wasm"
            artifact.write_bytes(b"wasm")
            with self.assertRaisesRegex(release_tool.ReleaseToolError, "SRI mismatch"):
                release_tool.validate_download(
                    artifact, sri(b"different"), "application/wasm", True
                )
            with self.assertRaisesRegex(release_tool.ReleaseToolError, "content type"):
                release_tool.validate_download(
                    artifact, sri(b"wasm"), "application/octet-stream", True
                )

    def test_uses_last_redirect_content_type(self) -> None:
        headers = (
            "HTTP/2 302\r\nContent-Type: text/plain\r\n\r\n"
            "HTTP/2 200\r\ncontent-type: application/wasm\r\n"
        )
        self.assertEqual(release_tool.header_content_type(headers), "application/wasm")


class ReleaseShellTests(unittest.TestCase):
    def setUp(self) -> None:
        self.temp = tempfile.TemporaryDirectory()
        self.root = pathlib.Path(self.temp.name)
        self.tools = self.root / "tools"
        self.bin = self.root / "bin"
        self.state = self.root / "state"
        self.tools.mkdir()
        self.bin.mkdir()
        self.state.mkdir()
        shutil.copy2(TOOLS_DIR / "release.sh", self.tools / "release.sh")
        (self.tools / "release.sh").chmod(0o755)
        (self.tools / "release_check.sh").write_text(
            '#!/usr/bin/env bash\necho "release_check $*" >> "$FAKE_LOG"\n',
            encoding="utf-8",
        )
        (self.tools / "release_check.sh").chmod(0o755)
        for name in (
            "release_tool.py",
            "set_release_version.py",
            "check_release_version.py",
        ):
            (self.tools / name).write_text("", encoding="utf-8")
        hooks = self.root / ".githooks"
        hooks.mkdir()
        (hooks / "pre-push").write_text("#!/usr/bin/env bash\n", encoding="utf-8")
        (hooks / "pre-push").chmod(0o755)
        self.log = self.root / "commands.log"
        self._write_fake_commands()
        self.env = os.environ.copy()
        self.env.update(
            {
                "PATH": f"{self.bin}:{self.env['PATH']}",
                "FAKE_ROOT": str(self.root),
                "FAKE_STATE": str(self.state),
                "FAKE_LOG": str(self.log),
                "FAKE_SHA": SHA,
                "REAL_PYTHON": sys.executable,
                "REAL_RELEASE_HELPER": str(TOOLS_DIR / "release_tool.py"),
            }
        )
        (self.root / "dev.sh").write_text(
            '#!/usr/bin/env bash\necho "dev.sh $*" >> "$FAKE_LOG"\n',
            encoding="utf-8",
        )
        (self.root / "dev.sh").chmod(0o755)

    def tearDown(self) -> None:
        self.temp.cleanup()

    def _write(self, name: str, content: str) -> None:
        path = self.bin / name
        path.write_text(content, encoding="utf-8")
        path.chmod(0o755)

    def _write_fake_commands(self) -> None:
        self._write(
            "python3",
            """#!/usr/bin/env bash
echo "python3 $*" >> "$FAKE_LOG"
if [[ "$*" == *"release_tool.py run-command"* ]]; then
  while [[ "$1" != "run-command" ]]; do shift; done
  exec "$REAL_PYTHON" -B "$REAL_RELEASE_HELPER" "$@"
fi
case "$*" in
  *release_tool.py\ validate-tag*) echo "1.2.3-beta.4" ;;
  *release_tool.py\ select-run*)
    [[ "${FAKE_RUN_NOT_FOUND:-0}" == "1" ]] && exit 3
    echo "99"
    ;;
  *release_tool.py\ run-report*) echo "v1.2.3-beta.4 run 99: ${FAKE_RUN_OUTCOME:-running}" ;;
  *release_tool.py\ run-signature*) echo "${FAKE_RUN_OUTCOME:-running}" ;;
  *release_tool.py\ run-outcome*) echo "${FAKE_RUN_OUTCOME:-running}" ;;
  *release_tool.py\ failure-field\ id*) echo "900" ;;
  *release_tool.py\ failure-field\ job*) echo "build" ;;
  *release_tool.py\ failure-field\ step*) echo "Compile" ;;
  *release_tool.py\ failure-field\ url*) echo "https://example/job/900" ;;
  *release_tool.py\ sanitize-log*) cat >/dev/null; echo "token=[REDACTED]" ;;
  *release_tool.py\ parse-duration*) echo "1" ;;
  *release_tool.py\ validate-release*) echo "release assets OK: count=18 draft=true prerelease=true" ;;
  *release_tool.py\ manifest-plan*)
    while [[ $# -gt 0 ]]; do
      if [[ "$1" == "--output" ]]; then
        shift
        printf '%s\n' '{"count":1,"items":[{"url":"https://tycode.dev/tyde/v1.2.3-beta.4/mobile_bg.wasm","integrity":"sha384-AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA","wasm":true}]}' > "$1"
        exit 0
      fi
      shift
    done
    exit 1
    ;;
  *release_tool.py\ json-field\ count*) echo "1" ;;
  *release_tool.py\ json-field\ items.0.url*) echo "https://tycode.dev/tyde/v1.2.3-beta.4/mobile_bg.wasm" ;;
  *release_tool.py\ json-field\ items.0.integrity*) echo "sha384-AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA" ;;
  *release_tool.py\ json-field\ items.0.wasm*) echo "true" ;;
  *release_tool.py\ json-field\ isDraft*)
    [[ -f "$FAKE_STATE/published" ]] && echo "false" || echo "true"
    ;;
  *release_tool.py\ header-content-type*) echo "application/wasm" ;;
  *release_tool.py\ validate-download*) : ;;
  *set_release_version.py*) echo "package.json" ;;
  *check_mobile_web_manifest.py*) echo "mobile web manifest OK" ;;
  *check_release_version.py*) echo "1.2.3-beta.4" ;;
  *) echo "unexpected fake python3 call: $*" >&2; exit 1 ;;
esac
""",
        )
        self._write(
            "gh",
            """#!/usr/bin/env bash
echo "gh $*" >> "$FAKE_LOG"
[[ "$1 $2" == "auth status" ]] && exit 0
if [[ "$1 $2" == "release view" ]]; then
  if [[ -f "$FAKE_STATE/published" ]]; then draft=false; else draft=true; fi
  printf '{"tagName":"v1.2.3-beta.4","isDraft":%s,"isPrerelease":true,"url":"https://example/release","assets":[]}\n' "$draft"
  exit 0
fi
if [[ "$1 $2" == "release edit" ]]; then
  touch "$FAKE_STATE/published"
  exit 0
fi
if [[ "$1 $2" == "run list" ]]; then
  [[ -n "${FAKE_RUN_LIST_SLEEP:-}" ]] && sleep "$FAKE_RUN_LIST_SLEEP"
  echo '[]'
  exit 0
fi
if [[ "$1 $2" == "run view" ]]; then
  if [[ "$*" == *--log-failed* ]]; then
    echo 'token=super-secret-value'
  else
    echo '{}'
  fi
  exit 0
fi
if [[ "$1 $2" == "api repos/{owner}/{repo}/releases/latest" ]]; then
  echo 'v1.2.2'
  exit 0
fi
if [[ "$1" == "api" ]]; then
  echo 'pub const PROTOCOL_VERSION: u32 = 42;'
  exit 0
fi
echo "unexpected fake gh call: $*" >&2
exit 1
""",
        )
        self._write(
            "curl",
            """#!/usr/bin/env bash
echo "curl $*" >> "$FAKE_LOG"
output=""
headers=""
while [[ $# -gt 0 ]]; do
  case "$1" in
    --output) shift; output="$1" ;;
    --dump-header) shift; headers="$1" ;;
  esac
  shift
done
[[ -n "$output" ]] && printf 'asset' > "$output"
[[ -n "$headers" ]] && printf 'HTTP/2 200\r\nContent-Type: application/wasm\r\n\r\n' > "$headers"
exit 0
""",
        )
        self._write(
            "git",
            """#!/usr/bin/env bash
echo "git $*" >> "$FAKE_LOG"
case "$1" in
  rev-parse)
    if [[ "$2" == "--show-toplevel" ]]; then echo "$FAKE_ROOT"; else echo "$FAKE_SHA"; fi
    ;;
  status)
    [[ "${FAKE_DIRTY:-0}" == "1" ]] && echo " M dirty.txt"
    exit 0
    ;;
  branch) echo "${FAKE_BRANCH:-main}" ;;
  symbolic-ref) [[ "${FAKE_DETACHED:-0}" != "1" ]] ;;
  config) echo "${FAKE_HOOKS:-.githooks}" ;;
  fetch) [[ "${FAKE_FETCH_FAIL:-0}" != "1" ]] ;;
  merge-base)
    if [[ "${FAKE_PUBLISH_OFF_MAIN:-0}" == "1" && "$3" == "$FAKE_SHA" && "$4" == "origin/main" ]]; then
      exit 1
    fi
    if [[ "${FAKE_RELATION:-ok}" == "ok" ]]; then exit 0; fi
    if [[ "${FAKE_RELATION:-ok}" == "behind" && "$4 $5" == "HEAD origin/main" ]]; then exit 0; fi
    exit 1
    ;;
  show-ref) [[ -f "$FAKE_STATE/local-tag" ]] ;;
  ls-remote)
    if [[ -f "$FAKE_STATE/remote-tag" ]]; then
      printf '%s\trefs/tags/v1.2.3-beta.4\n' "$(printf 'b%.0s' {1..40})"
      printf '%s\trefs/tags/v1.2.3-beta.4^{}\n' "$FAKE_SHA"
    fi
    ;;
  log) echo "Prepare release tooling" ;;
  tag) touch "$FAKE_STATE/local-tag" ;;
  push)
    if [[ "$3" == "main" ]]; then
      [[ "${FAKE_MAIN_PUSH_FAIL:-0}" != "1" ]] || exit 1
      touch "$FAKE_STATE/main-pushed"
    else
      [[ "${FAKE_TAG_PUSH_FAIL:-0}" != "1" ]] || exit 1
      touch "$FAKE_STATE/remote-tag"
    fi
    ;;
  add) : ;;
  diff) exit 1 ;;
  commit) : ;;
  *) echo "unexpected fake git call: $*" >&2; exit 1 ;;
esac
""",
        )

    def run_shell(
        self, *args: str, env: dict[str, str] | None = None
    ) -> subprocess.CompletedProcess[str]:
        merged = self.env | (env or {})
        return subprocess.run(
            [str(self.tools / "release.sh"), *args],
            cwd=self.root,
            env=merged,
            text=True,
            capture_output=True,
            check=False,
        )

    def run_tty(
        self, response: str, *args: str, env: dict[str, str] | None = None
    ) -> tuple[int, str, str]:
        merged = self.env | (env or {})
        master, slave = pty.openpty()
        process = subprocess.Popen(
            [str(self.tools / "release.sh"), *args],
            cwd=self.root,
            env=merged,
            stdin=slave,
            stdout=slave,
            stderr=subprocess.PIPE,
        )
        os.close(slave)
        os.write(master, response.encode("utf-8"))
        output = bytearray()
        while process.poll() is None:
            ready, _, _ = select.select([master], [], [], 0.1)
            if ready:
                try:
                    output.extend(os.read(master, 4096))
                except OSError:
                    break
        deadline = time.time() + 1
        while time.time() < deadline:
            ready, _, _ = select.select([master], [], [], 0.05)
            if not ready:
                break
            try:
                output.extend(os.read(master, 4096))
            except OSError:
                break
        os.close(master)
        stderr = process.stderr.read().decode("utf-8") if process.stderr else ""
        if process.stderr:
            process.stderr.close()
        return process.wait(), output.decode("utf-8", errors="replace"), stderr

    def commands(self) -> list[str]:
        return self.log.read_text(encoding="utf-8").splitlines() if self.log.exists() else []

    def test_gate_failure_does_not_mutate(self) -> None:
        result = self.run_shell(
            "cut", TAG, "--no-wait", env={"FAKE_DIRTY": "1"}
        )

        self.assertEqual(result.returncode, 1)
        self.assertIn("not clean", result.stderr)
        self.assertFalse(any(line.startswith("git tag ") for line in self.commands()))
        self.assertFalse(any(line.startswith("git push ") for line in self.commands()))

    def test_invalid_semver_is_rejected_before_tools(self) -> None:
        result = self.run_shell("cut", "v1.2.3-beta.", "--no-wait")

        self.assertEqual(result.returncode, 2)
        self.assertIn("invalid release tag", result.stderr)
        self.assertEqual(self.commands(), [])

    def test_hook_gate_failure_does_not_mutate(self) -> None:
        result = self.run_shell(
            "cut", TAG, "--no-wait", env={"FAKE_HOOKS": ".git/hooks"}
        )

        self.assertEqual(result.returncode, 1)
        self.assertIn("core.hooksPath", result.stderr)
        self.assertFalse(any(line.startswith("git tag ") for line in self.commands()))

    def test_non_tty_and_wrong_confirmation_do_not_mutate(self) -> None:
        result = self.run_shell("cut", TAG, "--no-wait")
        self.assertEqual(result.returncode, 1)
        self.assertIn("interactive TTY", result.stderr)
        self.assertFalse(any(line.startswith("git tag ") for line in self.commands()))

        self.log.unlink()
        code, _, stderr = self.run_tty("wrong\n", "cut", TAG, "--no-wait")
        self.assertEqual(code, 1)
        self.assertIn("did not match", stderr)
        self.assertFalse(any(line.startswith("git tag ") for line in self.commands()))

    def test_pushes_main_before_tag(self) -> None:
        code, stdout, stderr = self.run_tty(f"{TAG}\n", "cut", TAG, "--no-wait")

        self.assertEqual(code, 0, stdout + stderr)
        commands = self.commands()
        tag_index = commands.index(f"git tag -a {TAG} -m Release {TAG}")
        main_index = commands.index("git push origin main")
        remote_tag_index = commands.index(f"git push origin {TAG}")
        self.assertLess(tag_index, main_index)
        self.assertLess(main_index, remote_tag_index)
        self.assertIn("verified both remote refs", stdout)

    def test_reports_partial_main_push_when_tag_push_fails(self) -> None:
        code, _, stderr = self.run_tty(
            f"{TAG}\n",
            "cut",
            TAG,
            "--no-wait",
            env={"FAKE_TAG_PUSH_FAIL": "1"},
        )

        self.assertEqual(code, 1)
        self.assertIn("PARTIAL RELEASE", stderr)
        self.assertIn("origin/main contains", stderr)
        self.assertTrue((self.state / "main-pushed").exists())
        self.assertFalse((self.state / "remote-tag").exists())

    def test_stable_publish_is_refused_before_github_mutation(self) -> None:
        result = self.run_shell("publish", "v1.2.3")

        self.assertEqual(result.returncode, 2)
        self.assertIn("beta-only", result.stderr)
        self.assertFalse(any("release edit" in line for line in self.commands()))

    def test_prepare_commit_includes_version(self) -> None:
        result = self.run_shell("prepare", TAG, "--commit")

        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
        self.assertIn(
            "git commit -m Bump release to 1.2.3-beta.4 "
            "-m Set all tracked release versions to v1.2.3-beta.4.",
            self.commands(),
        )
        self.assertFalse(any(line.startswith("git push ") for line in self.commands()))

    def test_publish_rejects_remote_tag_outside_origin_main(self) -> None:
        (self.state / "remote-tag").touch()

        result = self.run_shell(
            "publish", TAG, env={"FAKE_PUBLISH_OFF_MAIN": "1"}
        )

        self.assertEqual(result.returncode, 1)
        self.assertIn("not contained in origin/main", result.stderr)
        self.assertFalse(any("release edit" in line for line in self.commands()))

    def test_status_exit_codes_distinguish_running_and_not_found(self) -> None:
        (self.state / "remote-tag").touch()

        running = self.run_shell(
            "status", TAG, env={"FAKE_RUN_OUTCOME": "running"}
        )
        self.assertEqual(running.returncode, 4)
        self.assertIn("run 99: running", running.stdout)

        self.log.unlink()
        missing = self.run_shell(
            "status", TAG, env={"FAKE_RUN_NOT_FOUND": "1"}
        )
        self.assertEqual(missing.returncode, 3)
        self.assertIn("workflow", missing.stderr.lower())

    def test_wait_failure_prints_bounded_sanitized_context(self) -> None:
        (self.state / "remote-tag").touch()

        result = self.run_shell(
            "wait",
            TAG,
            "--timeout",
            "1",
            "--interval",
            "1",
            env={"FAKE_RUN_OUTCOME": "failure"},
        )

        self.assertEqual(result.returncode, 1)
        self.assertIn("Failed step: Compile", result.stderr)
        self.assertIn("https://example/job/900", result.stderr)
        self.assertIn("[REDACTED]", result.stderr)
        self.assertNotIn("super-secret-value", result.stderr)

    def test_wait_timeout_bounds_hung_github_call(self) -> None:
        (self.state / "remote-tag").touch()
        started = time.monotonic()

        result = self.run_shell(
            "wait",
            TAG,
            "--timeout",
            "1",
            "--interval",
            "1",
            env={"FAKE_RUN_LIST_SLEEP": "5", "FAKE_RUN_NOT_FOUND": "1"},
        )

        self.assertEqual(result.returncode, 3)
        self.assertLess(time.monotonic() - started, 3)
        self.assertIn("not found within 1", result.stderr)

    def test_wait_timeout_reports_known_running_workflow(self) -> None:
        (self.state / "remote-tag").touch()
        started = time.monotonic()

        result = self.run_shell(
            "wait",
            TAG,
            "--timeout",
            "1",
            "--interval",
            "5",
            env={"FAKE_RUN_OUTCOME": "running"},
        )

        self.assertEqual(result.returncode, 4)
        self.assertLess(time.monotonic() - started, 3)
        self.assertIn("still running after 1", result.stderr)

    def test_beta_publish_uses_required_flags_and_rereads_release(self) -> None:
        (self.state / "remote-tag").touch()

        code, stdout, stderr = self.run_tty(f"{TAG}\n", "publish", TAG)

        self.assertEqual(code, 0, stdout + stderr)
        commands = self.commands()
        edit = f"gh release edit {TAG} --draft=false --prerelease --latest=false"
        self.assertIn(edit, commands)
        edit_index = commands.index(edit)
        release_views = [
            index
            for index, command in enumerate(commands)
            if command.startswith(f"gh release view {TAG} ")
        ]
        self.assertTrue(any(index > edit_index for index in release_views))
        self.assertIn(
            "gh api repos/{owner}/{repo}/releases/latest --jq .tag_name", commands
        )
        self.assertTrue((self.state / "published").exists())


if __name__ == "__main__":
    unittest.main()
