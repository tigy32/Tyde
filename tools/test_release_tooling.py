from __future__ import annotations

import base64
import contextlib
import hashlib
import io
import json
import os
import pathlib
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
import check_transport_protocol_version as transport_version_check
import release_tool
import set_release_version


TAG = "v1.2.3-beta.4"
SHA = "a" * 40


def release_assets(tag: str) -> list[dict[str, str]]:
    version = tag[1:]
    names = sorted(release_tool.HEADLESS_ASSETS | release_tool.update_manifest.expected_assets(tag))
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

    def test_accepts_dash_heavy_prerelease_identifiers(self) -> None:
        for tag in (
            "v1.2.3---.-.-a",
            "v1.2.3-0.--a",
            "v10.20.30-rc.1.alpha-2",
        ):
            with self.subTest(tag=tag):
                self.assertEqual(set_release_version.normalize_tag(tag), tag[1:])

    def test_rejects_adversarial_prerelease(self) -> None:
        tag = "v0.0.0-0." + "--." * 26 + "!"
        start = time.perf_counter()

        with self.assertRaises(set_release_version.SetVersionError):
            set_release_version.normalize_tag(tag)
        self.assertLess(time.perf_counter() - start, 1.0)


class TransportProtocolVersionTests(unittest.TestCase):
    def setUp(self) -> None:
        self.temp_dir = tempfile.TemporaryDirectory()
        self.root = pathlib.Path(self.temp_dir.name)
        subprocess.run(["git", "init", "-q"], cwd=self.root, check=True)
        subprocess.run(
            ["git", "config", "user.email", "test@example.com"],
            cwd=self.root,
            check=True,
        )
        subprocess.run(
            ["git", "config", "user.name", "Release Test"],
            cwd=self.root,
            check=True,
        )
        for path in (
            transport_version_check.TRANSPORT_VERSION_PATH,
            *transport_version_check.WIRE_CONTRACT_PATHS,
        ):
            target = self.root / path
            target.parent.mkdir(parents=True, exist_ok=True)
            if path == transport_version_check.TRANSPORT_VERSION_PATH:
                target.write_text(
                    "pub const MOBILE_RTC_PROTOCOL_VERSION: u32 = 3;\n",
                    encoding="utf-8",
                )
            else:
                target.write_text(f"wire contract: {path.name}\n", encoding="utf-8")
        subprocess.run(["git", "add", "."], cwd=self.root, check=True)
        subprocess.run(["git", "commit", "-qm", "baseline"], cwd=self.root, check=True)
        subprocess.run(["git", "tag", "v1.0.0"], cwd=self.root, check=True)

    def tearDown(self) -> None:
        self.temp_dir.cleanup()

    def test_rejects_wire_change_without_transport_version_bump(self) -> None:
        path = self.root / transport_version_check.WIRE_CONTRACT_PATHS[1]
        path.write_text("changed wire contract\n", encoding="utf-8")

        with self.assertRaisesRegex(
            transport_version_check.TransportVersionError,
            "without increasing MOBILE_RTC_PROTOCOL_VERSION",
        ):
            transport_version_check.check_transport_version(self.root)

    def test_accepts_wire_change_with_transport_version_bump(self) -> None:
        path = self.root / transport_version_check.WIRE_CONTRACT_PATHS[1]
        path.write_text("changed wire contract\n", encoding="utf-8")
        (self.root / transport_version_check.TRANSPORT_VERSION_PATH).write_text(
            "pub const MOBILE_RTC_PROTOCOL_VERSION: u32 = 4;\n",
            encoding="utf-8",
        )

        baseline, old, current, changed = (
            transport_version_check.check_transport_version(self.root)
        )

        self.assertEqual((baseline, old, current), ("v1.0.0", 3, 4))
        self.assertEqual(changed, (transport_version_check.WIRE_CONTRACT_PATHS[1],))

    def test_ignores_non_wire_source_changes(self) -> None:
        unrelated = self.root / "README.md"
        unrelated.write_text("unrelated\n", encoding="utf-8")

        _, old, current, changed = (
            transport_version_check.check_transport_version(self.root)
        )

        self.assertEqual((old, current, changed), (3, 3, ()))

    def test_timer_driver_repair_preserves_wire_guard(self) -> None:
        path = self.root / transport_version_check.WIRE_CONTRACT_PATHS[3]
        path.write_text("wasmtimer::tokio::timeout(Duration::from_secs(20), gather)\n")
        subprocess.run(["git", "add", "."], cwd=self.root, check=True)
        subprocess.run(["git", "commit", "-qm", "timer baseline"], cwd=self.root, check=True)
        path.write_text("tyde_time::timeout(Duration::from_secs(20), gather)\n")
        self.assertEqual(transport_version_check.check_transport_version(self.root, "HEAD")[3], ())
        path.write_text("tyde_time::timeout(Duration::from_secs(25), gather)\n")
        with self.assertRaises(transport_version_check.TransportVersionError):
            transport_version_check.check_transport_version(self.root, "HEAD")

    def test_exported_rtc_type_change_requires_version(self) -> None:
        path = self.root / transport_version_check.TRANSPORT_VERSION_PATH
        path.write_text("pub mod mobile_rtc {\n"
                        "pub const MOBILE_RTC_PROTOCOL_VERSION: u32 = 3;\n"
                        "pub struct Grant { pub token: String }\n}\npub use mobile_rtc::*;\n")
        subprocess.run(["git", "add", "."], cwd=self.root, check=True)
        subprocess.run(["git", "commit", "-qm", "RTC schema baseline"], cwd=self.root, check=True)
        path.write_text(path.read_text().replace("token: String", "token: Vec<u8>"))
        with self.assertRaises(transport_version_check.TransportVersionError):
            transport_version_check.check_transport_version(self.root, "HEAD")

    def test_local_ice_recovery_and_tests_preserve_wire_guard(self) -> None:
        paths = [self.root / path for path in transport_version_check.WIRE_CONTRACT_PATHS[2:4]]
        for path, state in zip(paths, ("RTCPeerConnectionState", "RtcPeerConnectionState")):
            path.write_text(
                f'fn terminal() {{ matches!(state, {state}::Failed | {state}::Closed | {state}::Disconnected) }}\n'
                '#[cfg(test)]\nmod wasm_tests { fn outage() { assert_eq!("}", "}"); } }\n'
                'fn send_ack() { send(&[1]); }\n'
            )
        subprocess.run(["git", "add", "."], cwd=self.root, check=True)
        subprocess.run(["git", "commit", "-qm", "ICE policy baseline"], cwd=self.root, check=True)
        for path, state in zip(paths, ("RTCPeerConnectionState", "RtcPeerConnectionState")):
            path.write_text(path.read_text().replace(f" | {state}::Disconnected", "")
                            .replace('assert_eq!("}", "}");', 'assert!(recovered());'))
        self.assertEqual(transport_version_check.check_transport_version(self.root, "HEAD")[3], ())
        for path in paths:
            unchanged = path.read_text()
            path.write_text(unchanged.replace("send(&[1])", "send(&[2])"))
            with self.assertRaises(transport_version_check.TransportVersionError):
                transport_version_check.check_transport_version(self.root, "HEAD")
            path.write_text(unchanged)

    def test_rejects_transport_version_decrease(self) -> None:
        (self.root / transport_version_check.TRANSPORT_VERSION_PATH).write_text(
            "pub const MOBILE_RTC_PROTOCOL_VERSION: u32 = 2;\n",
            encoding="utf-8",
        )

        with self.assertRaisesRegex(
            transport_version_check.TransportVersionError, "decreased"
        ):
            transport_version_check.check_transport_version(self.root)

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
            if not (
                asset["name"].startswith("Tyde_")
                and asset["name"].endswith(".msi")
            )
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
            "check_transport_protocol_version.py",
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
            r"""#!/usr/bin/env bash
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
  *check_transport_protocol_version.py*) : ;;
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

    def commands(self) -> list[str]:
        return self.log.read_text(encoding="utf-8").splitlines() if self.log.exists() else []

    def test_gate_failure_does_not_mutate(self) -> None:
        result = self.run_shell(
            "cut", TAG, "--confirm", "--no-wait", env={"FAKE_DIRTY": "1"}
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
            "cut", TAG, "--confirm", "--no-wait", env={"FAKE_HOOKS": ".git/hooks"}
        )

        self.assertEqual(result.returncode, 1)
        self.assertIn("core.hooksPath", result.stderr)
        self.assertFalse(any(line.startswith("git tag ") for line in self.commands()))

    def test_missing_confirmation_does_not_run_checks_or_mutate(self) -> None:
        result = self.run_shell("cut", TAG, "--no-wait")
        self.assertEqual(result.returncode, 2)
        self.assertIn("requires --confirm", result.stderr)
        self.assertFalse(any(line.startswith("release_check ") for line in self.commands()))
        self.assertFalse(any(line.startswith("git tag ") for line in self.commands()))

    def test_pushes_main_before_tag(self) -> None:
        result = self.run_shell("cut", TAG, "--confirm", "--no-wait")

        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
        commands = self.commands()
        transport_index = commands.index(
            f"python3 {self.tools / 'check_transport_protocol_version.py'}"
        )
        tag_index = commands.index(f"git tag -a {TAG} -m Release {TAG}")
        main_index = commands.index("git push origin main")
        remote_tag_index = commands.index(f"git push origin {TAG}")
        self.assertLess(transport_index, tag_index)
        self.assertLess(tag_index, main_index)
        self.assertLess(main_index, remote_tag_index)
        self.assertIn("verified both remote refs", result.stdout)

    def test_reports_partial_main_push_when_tag_push_fails(self) -> None:
        result = self.run_shell(
            "cut",
            TAG,
            "--confirm",
            "--no-wait",
            env={"FAKE_TAG_PUSH_FAIL": "1"},
        )

        self.assertEqual(result.returncode, 1)
        self.assertIn("PARTIAL RELEASE", result.stderr)
        self.assertIn("origin/main contains", result.stderr)
        self.assertTrue((self.state / "main-pushed").exists())
        self.assertFalse((self.state / "remote-tag").exists())

    def test_stable_publish_is_refused_before_github_mutation(self) -> None:
        result = self.run_shell("publish", "v1.2.3", "--confirm")

        self.assertEqual(result.returncode, 2)
        self.assertIn("beta-only", result.stderr)
        self.assertFalse(any("release edit" in line for line in self.commands()))

    def test_prepare_commit_includes_version(self) -> None:
        result = self.run_shell("prepare", TAG, "--commit")

        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
        commands = self.commands()
        transport_index = commands.index(
            f"python3 {self.tools / 'check_transport_protocol_version.py'}"
        )
        commit_index = commands.index(
            "git commit -m Bump release to 1.2.3-beta.4 "
            "-m Set all tracked release versions to v1.2.3-beta.4."
        )
        self.assertLess(transport_index, commit_index)
        self.assertIn(
            "git commit -m Bump release to 1.2.3-beta.4 "
            "-m Set all tracked release versions to v1.2.3-beta.4.",
            commands,
        )
        self.assertFalse(any(line.startswith("git push ") for line in commands))

    def test_publish_rejects_remote_tag_outside_origin_main(self) -> None:
        (self.state / "remote-tag").touch()

        result = self.run_shell(
            "publish", TAG, "--confirm", env={"FAKE_PUBLISH_OFF_MAIN": "1"}
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

        result = self.run_shell("publish", TAG, "--confirm")

        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
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




class MacosControllerFlowTests(unittest.TestCase):
    """Real Git, CLI processes, HTTP artifact transport and updater cryptography.

    Apple CLI doubles model network and verification outcomes on non-macOS hosts;
    they do not establish real Apple signing/notarization coverage.
    """

    def setUp(self):
        import http.server
        import threading

        self.temp = tempfile.TemporaryDirectory()
        self.addCleanup(self.temp.cleanup)
        self.root = pathlib.Path(self.temp.name)
        self.controller = self.root / "controller"
        self.source = self.root / "source"
        self.bin = self.root / "bin"
        self.bin.mkdir()
        self.controller.mkdir()
        self.log = self.root / "commands.jsonl"
        self.retained = self.root / "retained"
        self.target = "aarch64-apple-darwin"
        self.repository = "fixture/Tyde"
        self.tag = "v1.2.3"
        self.responses = {}
        self.downloads = {}
        self.uploads = {}
        self.published = False
        self.http_requests = []
        self.http_failures = {}
        self.asset_overrides = {}
        self.release_overrides = {}
        self.extra_releases = []
        self.wait_entered = threading.Event()
        self.wait_release = threading.Event()
        self.block_wait = False
        self.addCleanup(self.wait_release.set)
        owner = self

        class Handler(http.server.BaseHTTPRequestHandler):
            def do_GET(self):
                key = self.path.removeprefix(f"/repos/{owner.repository}/")
                owner.http_requests.append(("GET", key))
                if key in owner.http_failures:
                    self.send_error(owner.http_failures[key], "PRIVATE-ID SECRET Authorization: bearer fixture")
                    return
                release = {"id": 1, "tag_name": owner.tag, "draft": not owner.published,
                           "prerelease": False, **owner.release_overrides}
                assets = [{"id": i, "name": n, "size": len(b), "digest": "sha256:" + hashlib.sha256(b).hexdigest(),
                           **owner.asset_overrides.get(n, {})} for i, (n, b) in enumerate(owner.uploads.items())]
                if key.startswith("releases/tags/"):
                    # GitHub's published-release tag route does not see this draft.
                    self.send_error(404)
                    return
                elif key.startswith("releases?per_page=100&page="):
                    page = int(key.rsplit("=", 1)[1])
                    raw = json.dumps((owner.extra_releases + [release])[(page-1)*100:page*100]).encode()
                elif key == "releases/latest" and owner.published:
                    raw = json.dumps(release).encode()
                elif key == "releases/1":
                    raw = json.dumps(release).encode()
                elif key.startswith("releases/1/assets?per_page=100&page="):
                    page = int(key.rsplit("=", 1)[1])
                    raw = json.dumps(assets[(page-1)*100:page*100]).encode()
                elif key.startswith("tyde/"):
                    path = owner.root / "s3" / key
                    raw = path.read_bytes()
                elif key in owner.responses:
                    raw = json.dumps(owner.responses[key]).encode()
                elif key in owner.downloads:
                    raw = owner.downloads[key]
                elif key.startswith("releases/assets/"):
                    raw = list(owner.uploads.values())[int(key.rsplit("/", 1)[1])]
                elif key == "wait-event":
                    owner.wait_entered.set()
                    owner.wait_release.wait()
                    raw = b"{}"
                else:
                    self.send_error(404)
                    return
                self.send_response(200)
                self.end_headers()
                with contextlib.suppress(BrokenPipeError):
                    self.wfile.write(raw)

            def do_PATCH(self):
                owner.http_requests.append(("PATCH", "publish-stable"))
                body = json.loads(self.rfile.read(int(self.headers["Content-Length"])))
                if self.path != f"/repos/{owner.repository}/releases/1" or body != {"draft": False, "prerelease": False, "make_latest": "true"}:
                    self.send_error(422)
                    return
                owner.published = True
                self.send_response(200)
                self.end_headers()
                self.wfile.write(b"{}")

            def do_POST(self):
                name = self.path.removeprefix("/upload/")
                owner.uploads[name] = self.rfile.read(int(self.headers["Content-Length"]))
                self.send_response(200)
                self.end_headers()
                self.wfile.write(b"{}")

            def log_message(self, *args):
                pass

        self.server = http.server.ThreadingHTTPServer(("127.0.0.1", 0), Handler)
        worker = threading.Thread(target=self.server.serve_forever)
        worker.start()
        def close_server():
            self.wait_release.set()
            self.server.shutdown()
            self.server.server_close()
            worker.join()
        self.addCleanup(close_server)
        self.env = dict(os.environ, PATH=f"{self.bin}:{os.environ['PATH']}",
                        FIXTURE_ROOT=str(self.root), FIXTURE_LOG=str(self.log),
                        FIXTURE_HTTP=f"http://127.0.0.1:{self.server.server_port}",
                        APPLE_SIGNING_IDENTITY="fixture", APPLE_API_ISSUER="fixture",
                        APPLE_API_KEY="fixture", APPLE_API_KEY_PATH=str(self.root / "key.p8"),
                        GITHUB_RUN_ID="10", GITHUB_RUN_ATTEMPT="1", PYTHONDONTWRITEBYTECODE="1")
        self.git(self.controller, "init", "-q")
        self.git(self.controller, "symbolic-ref", "HEAD", "refs/heads/main")
        self.git(self.controller, "config", "user.name", "Release fixture")
        self.git(self.controller, "config", "user.email", "fixture@example.invalid")
        for source in version_check.release_version_paths(REPO_ROOT):
            target = self.controller / source.relative_to(REPO_ROOT)
            target.parent.mkdir(parents=True, exist_ok=True)
            shutil.copyfile(source, target)
        set_release_version.set_release_version(self.controller, self.tag)
        self.crypto = self.root / "crypto.cjs"
        self.crypto.write_text(r"""
const fs = require('node:fs'), crypto = require('node:crypto');
const root = process.env.FIXTURE_ROOT;
if (process.argv[2] === 'generate') {
 const {publicKey, privateKey} = crypto.generateKeyPairSync('ed25519');
 fs.writeFileSync(root + '/signing-key.pem', privateKey.export({format:'pem',type:'pkcs8'}));
 const raw = publicKey.export({format:'der',type:'spki'}).subarray(-32);
 const key = Buffer.concat([Buffer.from('Ed'),Buffer.from('12345678'),raw]);
 console.log(Buffer.from('untrusted comment: fixture\n'+key.toString('base64')+'\n').toString('base64'));
} else {
 const file = process.argv[2], key = fs.readFileSync(root + '/signing-key.pem');
 const sig = crypto.sign(null, crypto.createHash('blake2b512').update(fs.readFileSync(file)).digest(), key), comment = 'fixture';
 const envelope = Buffer.concat([Buffer.from('ED12345678'),sig]);
 const global = crypto.sign(null,Buffer.concat([sig,Buffer.from(comment)]),key);
 const text = 'untrusted comment: fixture\n'+envelope.toString('base64')+'\ntrusted comment: '+comment+'\n'+global.toString('base64')+'\n';
 fs.writeFileSync(file+'.sig',Buffer.from(text).toString('base64'));
}
""")
        key = subprocess.check_output(["node", self.crypto, "generate"], env=self.env).decode().strip()
        config = self.controller / "frontend/tauri-shell/tauri.conf.json"
        value = json.loads(config.read_text())
        value["plugins"]["updater"]["pubkey"] = key
        config.write_text(json.dumps(value))
        (self.controller / "protocol/src").mkdir(parents=True, exist_ok=True)
        (self.controller / "protocol/src/types.rs").write_text("pub const PROTOCOL_VERSION: u32 = 41;\n")
        (self.controller / "mobile-frontend").mkdir()
        (self.controller / "mobile-frontend/index.html").write_text("frozen mobile source")
        (self.controller / "tools").mkdir()
        for name in ("check_release_version.py", "set_release_version.py"):
            shutil.copyfile(TOOLS_DIR / name, self.controller / "tools" / name)
        self.git(self.controller, "add", ".")
        self.git(self.controller, "commit", "-qm", "Frozen application")
        self.source_sha = self.git(self.controller, "rev-parse", "HEAD")
        self.git(self.controller, "tag", self.tag)
        tools = self.controller / "tools"
        for name in ("macos_release.py", "update_manifest.py", "set_release_version.py", "check_release_version.py", "release_tool.py", "check_mobile_web_manifest.py", "merge_mobile_web_manifest.py"):
            shutil.copyfile(TOOLS_DIR / name, tools / name)
        shutil.copytree(REPO_ROOT / "web", self.controller / "web")
        self.git(self.controller, "add", ".")
        self.git(self.controller, "commit", "-qm", "Controller tooling")
        self.controller_sha = self.git(self.controller, "rev-parse", "HEAD")
        self.git(self.controller, "update-ref", "refs/remotes/origin/main", self.controller_sha)
        self.git(self.root, "clone", "--bare", "-q", str(self.controller), str(self.root / "origin.git"))
        self.git(self.controller, "config", f"url.file://{self.root}/origin.git.insteadOf", f"https://github.com/{self.repository}.git")
        self.git(self.controller, "remote", "add", "origin", f"https://github.com/{self.repository}.git")
        self.git(self.root, "clone", "-q", str(self.controller), str(self.source))
        self.git(self.source, "remote", "set-url", "origin", f"https://github.com/{self.repository}.git")
        self.git(self.source, "config", f"url.file://{self.root}/origin.git.insteadOf", f"https://github.com/{self.repository}.git")
        self.git(self.source, "update-ref", "refs/remotes/origin/main", self.controller_sha)
        self.git(self.source, "checkout", "-q", self.tag)
        self.assertFalse((self.source / "tools/macos_release.py").exists())
        self.responses["actions/runs/10"] = self.run_metadata(10)
        self.bundle = self.source / f"target/{self.target}/release"
        app = self.bundle / "bundle/macos/Tyde.app/Contents"
        (app / "MacOS").mkdir(parents=True)
        (app / "MacOS/tyde").write_bytes(b"signed desktop")
        import plistlib
        (app / "Info.plist").write_bytes(plistlib.dumps({"CFBundleShortVersionString": "1.2.3"}))
        (self.bundle / "tyde-server").write_bytes(b"signed server")
        stub = self.bin / "fixture-command"
        stub.write_text("#!" + sys.executable + "\n" + r'''
import io,json,os,pathlib,shutil,subprocess,sys,urllib.request,zipfile
name=pathlib.Path(sys.argv[0]).name; args=sys.argv[1:]; root=pathlib.Path(os.environ['FIXTURE_ROOT'])
with open(os.environ['FIXTURE_LOG'],'a') as f: f.write(json.dumps([name,*[a for a in args if not len(a)==36]])+'\n')
def request(path,data=None,method=None):
 return urllib.request.urlopen(urllib.request.Request(os.environ['FIXTURE_HTTP']+path,data=data,method=method)).read()
if name=='gh':
 if args[0]=='api':
  method=args[args.index('--method')+1] if '--method' in args else 'GET'
  fields=json.load(sys.stdin) if '--input' in args else None
  try:
   raw=request('/'+args[1],json.dumps(fields).encode() if fields else None,method)
   if '--include' in args: sys.stdout.buffer.write(b'HTTP/2.0 200 OK\r\nContent-Type: application/json\r\n\r\n')
   sys.stdout.buffer.write(raw)
  except urllib.error.HTTPError as e:
   if '--include' in args: sys.stdout.buffer.write(f'HTTP/2.0 {e.code} Error\r\n\r\n'.encode())
   print(f'gh: PRIVATE-ID SECRET Authorization: bearer fixture (HTTP {e.code})',file=sys.stderr)
   sys.exit(1)
  sys.exit()
 if args[:2]==['release','upload']:
  path=pathlib.Path(args[3]); request('/upload/'+path.name,path.read_bytes()); sys.exit()
 raise SystemExit(21)
if name=='trunk':
 if (root/'deploy-failure').exists(): raise SystemExit(32)
 source=pathlib.Path.cwd()
 if (source/'index.html').read_text()!='frozen mobile source': raise SystemExit(33)
 dist=pathlib.Path(args[args.index('--dist')+1]); dist.mkdir(parents=True,exist_ok=True)
 (dist/'mobile-fixture.js').write_text('export default "frozen mobile fixture";')
 (dist/'mobile-fixture_bg.wasm').write_bytes(b'\0asm\1\0\0\0')
 (dist/'index.html').write_text('<script type="module">import init from "./mobile-fixture.js";</script><meta name="tyde-follow-selected-host" content="1">')
 sys.exit()
if name=='aws':
 def mapped(value):
  return root/'s3'/value.removeprefix('s3://tycode-static/') if value.startswith('s3://') else pathlib.Path(value)
 if args[0]=='cloudfront': print('{}'); sys.exit()
 if args[:2]==['s3api','head-object']:
  key=args[args.index('--key')+1]; path=root/'s3'/key
  if not path.is_file(): raise SystemExit(34)
  if '--query' in args:
   query=args[args.index('--query')+1]
   print('application/wasm' if query=='ContentType' else 'no-store, max-age=0, must-revalidate' if key=='tyde/manifest.json' else 'public, max-age=60, must-revalidate')
  else: print('{}')
  sys.exit()
 if args[0]=='s3':
  src,dst=mapped(args[2]),mapped(args[3])
  if '--recursive' in args or args[1]=='sync':
   if src!=dst:
    for path in src.rglob('*'):
     if path.is_file() and not (args[2].endswith('web/loader/') and path.name=='manifest.json'):
      target=dst/path.relative_to(src); target.parent.mkdir(parents=True,exist_ok=True); shutil.copyfile(path,target)
  else:
   dst.parent.mkdir(parents=True,exist_ok=True); shutil.copyfile(src,dst)
  sys.exit()
 raise SystemExit(35)
if name=='curl':
 url=args[-1]; raw=request('/repos/fixture/Tyde/'+url.removeprefix('https://tycode.dev/'))
 if (root/'public-corruption').exists() and url.endswith('.wasm'): raw=b'corrupt'
 pathlib.Path(args[args.index('--output')+1]).write_bytes(raw)
 if '--dump-header' in args:
  pathlib.Path(args[args.index('--dump-header')+1]).write_text('HTTP/2 200 OK\nContent-Type: '+('application/wasm' if url.endswith('.wasm') else 'application/javascript')+'\n')
 sys.exit()
if name=='lipo': print('x86_64' if (root/'wrong-arch').exists() else 'arm64')
elif name=='codesign': pass
elif name=='route': print('destination: 17.0.0.1\ngateway: 192.0.2.1\ninterface: en0')
elif name=='dscacheutil': print('ip_address: 17.0.0.1')
elif name=='spctl':
 if (root/'reject-spctl').exists(): raise SystemExit(3)
elif name=='ditto':
 if args[:2]==['-x','-k']:
  with zipfile.ZipFile(args[2]) as z: z.extractall(args[3])
 elif args[:3]==['-c','-k','--keepParent']:
  source=pathlib.Path(args[3])
  with zipfile.ZipFile(args[4],'w') as z:
   for p in source.rglob('*') if source.is_dir() else [source]:
    if p.is_file(): z.write(p,p.relative_to(source.parent))
 else: shutil.copytree(*args)
elif name=='hdiutil':
 if args[0]=='create':
  staging=pathlib.Path(args[args.index('-srcfolder')+1])
  if not (staging/'Tyde.app/stapled').exists(): raise SystemExit(22)
  pathlib.Path(args[-1]).write_bytes(b'signed image containing stapled app')
elif name=='npx':
 if 'build' in args: raise SystemExit(23)
 subprocess.run(['node',root/'crypto.cjs',args[-1]],check=True)
elif name=='xcrun':
 if args[:2]==['notarytool','submit']:
  if '--wait' in args: raise SystemExit(68)
  print(json.dumps({'id':{'server.zip':'1','app.zip':'2','signed.dmg':'3'}[pathlib.Path(args[2]).name]*8+'-1111-1111-1111-111111111111'}))
 elif args[:2]==['notarytool','wait']:
  if (root/'block-wait').exists(): request('/repos/fixture/Tyde/wait-event')
  if (root/'network-failure').exists():
   print('NSURLErrorDomain Code=-1009 https://appstoreconnect.apple.com/notary/v2/submissions/PRIVATE-ID?token=SECRET',file=sys.stderr)
   raise SystemExit(68)
  print(json.dumps({'id':args[2],'status':'Invalid' if (root/'invalid-notary').exists() else 'Accepted'}))
 elif args[0]=='stapler':
  path=pathlib.Path(args[2])
  if args[1]=='staple':
   if path.is_dir(): (path/'stapled').write_text('ticket')
   else: path.write_bytes(path.read_bytes()+b' stapled')
  elif path.is_dir() and not (path/'stapled').exists(): raise SystemExit(25)
 else: raise SystemExit(26)
else: raise SystemExit(27)
''')
        stub.chmod(0o755)
        for name in ("gh", "codesign", "lipo", "ditto", "hdiutil", "npx", "xcrun", "spctl", "route", "dscacheutil", "cargo", "trunk", "aws", "curl"):
            (self.bin / name).symlink_to(stub)
        live = self.root / "s3/tyde/manifest.json"
        live.parent.mkdir(parents=True)
        live.write_text(json.dumps({"schemaVersion": 1, "minSupported": "0.8.19", "blocked": [], "versions": {}}))

    def git(self, root, *args):
        return subprocess.check_output(["git", "-C", str(root), *args], stderr=subprocess.PIPE).decode().strip()

    def run_metadata(self, run_id):
        return {"id": run_id, "repository": {"full_name": self.repository},
                "head_repository": {"full_name": self.repository}, "head_sha": self.controller_sha,
                "head_branch": "main", "path": ".github/workflows/release.yml",
                "display_title": f"Release {self.tag} @ {self.source_sha}"}

    def argv(self, operation, *extra):
        return [sys.executable, str(self.controller / "tools/macos_release.py"), operation,
                "--repository", self.repository, "--tag", self.tag, "--source-sha", self.source_sha,
                "--controller-sha", self.controller_sha, "--source", str(self.source),
                "--target", self.target, "--retained", str(self.retained), *extra]

    def cli(self, operation, *extra, succeeds=True):
        result = subprocess.run(self.argv(operation, *extra), env=self.env, capture_output=True, text=True)
        self.assertEqual(result.returncode == 0, succeeds, result.stdout + result.stderr)
        self.assertNotIn("PRIVATE-ID", result.stdout + result.stderr)
        self.assertNotIn("SECRET", result.stdout + result.stderr)
        return result

    def seal_artifact(self, artifact_id, folder, run_id=10, name=None):
        buffer = io.BytesIO()
        with __import__('zipfile').ZipFile(buffer, "w") as archive:
            for path in folder.rglob("*"):
                if path.is_file(): archive.write(path, path.relative_to(folder))
        raw = buffer.getvalue()
        self.responses[f"actions/artifacts/{artifact_id}"] = {
            "id": artifact_id, "name": name, "expired": False,
            "workflow_run": {"id": run_id, "head_sha": self.controller_sha},
            "digest": "sha256:" + hashlib.sha256(raw).hexdigest()}
        self.downloads[f"actions/artifacts/{artifact_id}/zip"] = raw
        return self.responses[f"actions/artifacts/{artifact_id}"]

    def retained_dmg(self):
        self.cli("prepare")
        self.cli("submit", "--kind", "server")
        self.cli("submit", "--kind", "app")
        self.cli("package")
        self.cli("submit", "--kind", "dmg")

    def retention_state(self, available):
        workflow = (REPO_ROOT / ".github/workflows/macos-release.yml").read_text()
        step = workflow.split("      - name: Inspect retained payload state\n", 1)[1].split("\n      - ", 1)[0]
        script = "\n".join(line[10:] for line in step.split("        run: |\n", 1)[1].splitlines())
        output = self.root / "retention-output"
        output.write_text("")
        result = subprocess.run(["bash", "-euo", "pipefail", "-c", script],
                                env=dict(self.env, RETAINED=str(self.retained), GITHUB_OUTPUT=str(output)),
                                capture_output=True, text=True)
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(output.read_text(), f"available={str(available).lower()}\n")
        return result.stdout

    def test_draft_lookup_pagination_http_failures_and_no_payload(self):
        # Reproduce the real published-only tag route while the same auth can
        # read this exact draft through list/by-ID. Execute both CLI boundaries.
        result = subprocess.run(["gh", "api", f"repos/{self.repository}/releases/tags/{self.tag}"],
                                env=self.env, capture_output=True, text=True)
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("HTTP 404", result.stderr)
        self.http_requests.clear()
        self.extra_releases = [{"id": i+2, "tag_name": f"v0.0.{i}", "draft": False} for i in range(100)]
        self.cli("lookup-draft")
        self.cli("verify")
        self.assertIn(("GET", "releases?per_page=100&page=2"), self.http_requests)
        self.assertIn(("GET", "releases/1"), self.http_requests)
        self.assertFalse(any("releases/tags/" in path for _, path in self.http_requests))
        for status in (401, 403, 404, 429, 500):
            self.http_failures["releases?per_page=100&page=2"] = status
            result = self.cli("verify", succeeds=False)
            self.assertIn(f"release-list: HTTP {status}", result.stderr)
            self.assertFalse(self.retained.exists())
            self.assertIn("No retained payload", self.retention_state(False))
        self.http_failures.clear()
        self.extra_releases = [{"id": 2, "tag_name": self.tag, "draft": True}]
        self.cli("verify", succeeds=False)
        self.extra_releases = []
        self.release_overrides["tag_name"] = "v9.9.9"
        self.cli("verify", succeeds=False)
        self.release_overrides.clear()
        self.http_failures["releases/1"] = 403
        result = self.cli("verify", succeeds=False)
        self.assertIn("release-by-ID: HTTP 403", result.stderr)
        self.http_failures.clear()
        self.cli("prepare")
        self.assertIn("Retained payload receipt present", self.retention_state(True))
        self.assertFalse(self.uploads)
        self.assertFalse(self.published)

    def test_network_failure_explicit_restore_complete_without_compile(self):
        # The pre-fix release.yml coupled submit/wait and uploaded only afterward.
        # Execute that original CLI boundary before the retained flow, including
        # disposal of the failed runner's workspace. No old full gate is needed.
        legacy = self.root / "legacy-runner"
        legacy.mkdir()
        legacy_dmg = legacy / "signed.dmg"
        legacy_dmg.write_bytes(b"signed legacy image")
        before = subprocess.run(["bash", "-e", "-c",
            'codesign --force --timestamp --sign "$APPLE_SIGNING_IDENTITY" "$MACOS_DMG"; '
            'xcrun notarytool submit "$MACOS_DMG" --issuer "$APPLE_API_ISSUER" '
            '--key-id "$APPLE_API_KEY" --key "$APPLE_API_KEY_PATH" --wait; '
            'xcrun stapler staple "$MACOS_DMG"; xcrun stapler validate "$MACOS_DMG"; '
            'spctl -a -t open --context context:primary-signature -v "$MACOS_DMG"; '
            'gh release upload v1.2.3 "$MACOS_DMG" --clobber'],
            env=dict(self.env, MACOS_DMG=str(legacy_dmg)), capture_output=True)
        self.assertEqual(before.returncode, 68)
        self.assertFalse(self.uploads)
        shutil.rmtree(legacy)
        self.cli("complete", succeeds=False)
        self.retained_dmg()
        original = {p.name: p.read_bytes() for p in self.retained.glob("*.zip")}
        original["signed.dmg"] = (self.retained / "signed.dmg").read_bytes()
        (self.root / "network-failure").touch()
        self.cli("complete", succeeds=False)
        receipt = json.loads((self.retained / "receipt.json").read_text())
        self.assertEqual(receipt["phase"], "submitted-dmg")
        self.assertEqual(set(receipt["submissions"]), {"app", "server", "dmg"})
        self.assertEqual(receipt["events"][-1]["network_codes"], ["-1009"])
        (self.retained / "signed.dmg").unlink()
        self.cli("complete", succeeds=False)
        (self.retained / "signed.dmg").write_bytes(original["signed.dmg"])
        self.assertNotIn("PRIVATE-ID", json.dumps(receipt))
        self.assertNotIn("SECRET", json.dumps(receipt))
        self.seal_artifact(20, self.retained)
        shutil.rmtree(self.retained)
        shutil.rmtree(self.source / "target")
        (self.root / "network-failure").unlink()
        self.responses["actions/runs/11"] = self.run_metadata(11)
        self.env["GITHUB_RUN_ID"] = "11"
        self.cli("restore", "--run-id", "10", "--artifact-id", "20")
        self.log.write_text("")
        self.cli("complete")
        self.cli("upload")
        self.cli("checkpoint")
        commands = [json.loads(line) for line in self.log.read_text().splitlines()]
        self.assertFalse(any("submit" in c or "build" in c or c[0] == "cargo" for c in commands))
        self.assertIn(["xcrun", "stapler", "validate", str(self.retained / "work/Tyde_1.2.3_aarch64-apple-darwin.dmg")], commands)
        self.assertEqual(len(self.uploads), 3)
        self.assertTrue(next(v for n, v in self.uploads.items() if n.endswith(".dmg")).endswith(b"stapled"))
        self.assertEqual(original, {name: (self.retained / name).read_bytes() for name in original})
        self.seal_artifact(21, self.retained, run_id=11)
        shutil.rmtree(self.retained)
        self.cli("restore", "--run-id", "11", "--artifact-id", "21")
        self.assertEqual(json.loads((self.retained / "receipt.json").read_text())["phase"], "complete")

    def test_complete_aggregate_requires_same_source_signed_assets_and_stable_msi(self):
        self.retained_dmg()
        self.cli("complete")
        self.cli("upload")
        artifacts = []
        saved = {}
        version = "1.2.3"
        for index, (target, (base, patterns)) in enumerate(release_tool.update_manifest.TARGETS.items()):
            if target == self.target:
                continue
            folder = self.root / target
            folder.mkdir()
            entries = {}
            installers = dict(patterns)
            if base.startswith("windows"):
                installers["msi"] = ""
            for installer in installers:
                if installer == "app":
                    name = f"tyde-update-{version}-{target}.app.tar.gz"
                elif installer == "nsis":
                    name = f"Tyde_{version}_x64-setup.exe"
                elif installer == "msi":
                    name = f"Tyde_{version}_x64_en-US.msi"
                else:
                    arch = "amd64" if "x86_64" in target else "aarch64"
                    name = next(a["name"] for a in release_assets(self.tag) if a["name"].endswith({"appimage": ".AppImage", "deb": ".deb", "rpm": ".rpm"}[installer])
                                and (arch in a["name"] or (arch == "amd64" and "x86_64" in a["name"]) or (arch == "aarch64" and "arm64" in a["name"])))
                payload = self.root / name
                payload.write_bytes(f"signed {target} {installer}".encode())
                subprocess.run(["node", self.crypto, payload], env=self.env, check=True)
                signature = payload.with_name(name + ".sig").read_text()
                entry = {"url": f"https://github.com/{self.repository}/releases/download/{self.tag}/{name}", "signature": signature}
                entries[f"{base}-{installer}"] = entry
                if installer in {"app", "appimage", "nsis"}:
                    entries[base] = entry
                saved[name] = payload.read_bytes()
            (folder / f"update-{target}.json").write_text(json.dumps({"version": version, "platforms": entries}))
            artifacts.append(self.seal_artifact(30 + index, folder, name=f"update-{target}"))
        self.responses["actions/runs/10/artifacts?per_page=100"] = {"total_count": len(artifacts), "artifacts": artifacts}
        for asset in release_assets(self.tag):
            name = asset["name"]
            if name not in self.uploads and name != "tyde-update.json":
                self.uploads[name] = saved.get(name, b"other completed platform asset")
        for name in release_tool.HEADLESS_ASSETS:
            buffer = io.BytesIO()
            with __import__('zipfile').ZipFile(buffer, "w") as archive:
                archive.writestr("tyde-server.exe" if "windows" in name else "tyde-server", b"completed server")
            if self.target not in name:
                self.uploads[name] = buffer.getvalue()
        for name in list(self.uploads):
            if name.endswith(".sha256"):
                package = name.removesuffix(".sha256")
                self.uploads[name] = f"{hashlib.sha256(self.uploads[package]).hexdigest()}  /build/{package}\n".encode()
        msi = f"Tyde_{version}_x64_en-US.msi"
        del self.uploads[msi]
        self.cli("assemble", "--fragments-run-id", "10", succeeds=False)
        self.assertNotIn("tyde-update.json", self.uploads)
        self.uploads[msi] = saved[msi]
        linux = next(name for name in saved if name.endswith(".AppImage"))
        self.uploads[linux] = b"wrong bytes with correct filename"
        self.cli("assemble", "--fragments-run-id", "10", succeeds=False)
        self.assertNotIn("tyde-update.json", self.uploads)
        self.uploads[linux] = saved[linux]
        self.responses["actions/runs/10"]["display_title"] = "Release wrong source"
        self.cli("assemble", "--fragments-run-id", "10", succeeds=False)
        self.responses["actions/runs/10"] = self.run_metadata(10)
        self.cli("assemble", "--fragments-run-id", "10")
        manifest = json.loads(self.uploads["tyde-update.json"])
        self.assertEqual(manifest["version"], version)
        self.assertIn("windows-x86_64-msi", manifest["platforms"])
        self.assertIn("darwin-aarch64", manifest["platforms"])
        self.assertIn("darwin-x86_64", manifest["platforms"])
        self.assertTrue((self.retained / "assembly.json").is_file())
        self.assertFalse(self.published)
        self.log.write_text("")
        self.cli("finalize", "--fragments-run-id", "10", succeeds=False)
        self.cli("finalize", "--fragments-run-id", "10", "--confirm", "--source-sha", "b" * 40, succeeds=False)
        self.asset_overrides[msi] = {"digest": "sha256:" + "0" * 64}
        self.cli("finalize", "--fragments-run-id", "10", "--confirm", succeeds=False)
        self.asset_overrides.clear()
        missing = self.uploads.pop(msi)
        self.cli("finalize", "--fragments-run-id", "10", "--confirm", succeeds=False)
        self.uploads[msi] = missing
        original_manifest = self.uploads["tyde-update.json"]
        self.uploads["tyde-update.json"] = b'{}'
        self.cli("finalize", "--fragments-run-id", "10", "--confirm", succeeds=False)
        self.uploads["tyde-update.json"] = original_manifest
        self.published = True
        self.cli("finalize", "--fragments-run-id", "10", "--confirm", succeeds=False)
        self.published = False
        commands = [json.loads(line) for line in self.log.read_text().splitlines()]
        self.assertFalse(any(c[0] in ("trunk", "aws") for c in commands))
        self.assertFalse(any(method == "PATCH" for method, _ in self.http_requests))
        (self.root / "deploy-failure").touch()
        self.cli("finalize", "--fragments-run-id", "10", "--confirm", succeeds=False)
        self.assertFalse(self.published)
        (self.root / "deploy-failure").unlink()
        (self.root / "public-corruption").touch()
        self.cli("finalize", "--fragments-run-id", "10", "--confirm", succeeds=False)
        self.assertFalse(self.published)
        (self.root / "public-corruption").unlink()
        self.cli("finalize", "--fragments-run-id", "10", "--confirm")
        self.assertTrue(self.published)
        finalization = json.loads((self.retained / "finalization.json").read_text())
        self.assertEqual(finalization["state"], "published")
        self.assertEqual(len(finalization["assets"]), 22)
        self.assertEqual(finalization["source_sha"], self.source_sha)
        live = json.loads((self.root / "s3/tyde/manifest.json").read_text())
        self.assertEqual(live["versions"][version]["protocolVersion"], 41)
        self.assertFalse(self.git(self.source, "status", "--porcelain", "--untracked-files=no"))
        self.assertFalse(self.git(self.controller, "status", "--porcelain", "--untracked-files=no"))
        self.cli("finalize", "--fragments-run-id", "10", "--confirm", succeeds=False)
        self.assertEqual(sum(method == "PATCH" for method, _ in self.http_requests), 1)
        commands = [json.loads(line) for line in self.log.read_text().splitlines()]
        self.assertFalse(any(c[0] == "cargo" or "submit" in c or (c[0] == "npx" and "build" in c) for c in commands))

    def test_rejects_mismatches_security_failures_and_published_mutations(self):
        (self.root / "wrong-arch").touch()
        self.cli("prepare", succeeds=False)
        (self.root / "wrong-arch").unlink()
        self.retained_dmg()
        self.cli("submit", "--kind", "dmg", succeeds=False)
        self.cli("package", succeeds=False)
        self.cli("complete", "--source-sha", "b" * 40, succeeds=False)
        self.cli("complete", "--target", "x86_64-apple-darwin", succeeds=False)
        dmg = self.retained / "signed.dmg"
        payload = dmg.read_bytes()
        dmg.write_bytes(b"corrupt")
        self.cli("complete", succeeds=False)
        dmg.write_bytes(payload)
        self.published = True
        self.cli("complete", succeeds=False)
        self.cli("upload", succeeds=False)
        self.published = False
        (self.root / "invalid-notary").touch()
        self.cli("complete", succeeds=False)
        (self.root / "invalid-notary").unlink()
        (self.root / "reject-spctl").touch()
        self.cli("complete", succeeds=False)
        self.cli("upload", succeeds=False)
        (self.root / "reject-spctl").unlink()
        self.cli("complete")
        updater = next((self.retained / "output/artifacts").iterdir())
        updater.write_bytes(b"corrupt signed updater")
        self.cli("upload", succeeds=False)
        self.assertFalse(self.uploads)
        self.seal_artifact(20, self.retained)
        shutil.rmtree(self.retained)
        self.cli("restore", "--run-id", "99", "--artifact-id", "20", succeeds=False)
        self.responses["actions/runs/10"]["head_repository"]["full_name"] = "wrong/repo"
        self.cli("restore", "--run-id", "10", "--artifact-id", "20", succeeds=False)

    def test_cancellation_retains_submission_and_payload_checkpoint(self):
        import signal
        self.retained_dmg()
        (self.root / "block-wait").touch()
        process = subprocess.Popen(self.argv("complete"), env=self.env, stdout=subprocess.PIPE,
                                   stderr=subprocess.PIPE, start_new_session=True)
        try:
            self.wait_entered.wait()
            os.killpg(process.pid, signal.SIGTERM)
            process.communicate()
        finally:
            self.wait_release.set()
            if process.poll() is None:
                os.killpg(process.pid, signal.SIGKILL)
                process.communicate()
        self.assertNotEqual(process.returncode, 0)
        self.retention_state(True)
        data = json.loads((self.retained / "receipt.json").read_text())
        self.assertEqual(data["events"][-1], {"phase": "wait-dmg", "state": "started"})
        self.seal_artifact(20, self.retained)
        shutil.rmtree(self.retained)
        (self.root / "block-wait").unlink()
        self.cli("restore", "--run-id", "10", "--artifact-id", "20")
        self.cli("complete")


if __name__ == "__main__":
    unittest.main()