from __future__ import annotations

import hashlib
import os
import pathlib
import shutil
import subprocess
import tempfile
import unittest


TOOLS_DIR = pathlib.Path(__file__).resolve().parent
REPO_ROOT = TOOLS_DIR.parent
TOOLCHAIN_UPDATE_LOG = "rustup update stable toolchain=unset"
TOOLCHAIN_INSTALL_LOG = "rustup toolchain install toolchain=unset"


class DevCheckCacheTests(unittest.TestCase):
    def setUp(self) -> None:
        self.temp = tempfile.TemporaryDirectory()
        self.root = pathlib.Path(self.temp.name) / "repo"
        self.bin = pathlib.Path(self.temp.name) / "bin"
        self.log = pathlib.Path(self.temp.name) / "commands.log"
        self.root.mkdir()
        self.bin.mkdir()

        shutil.copy2(REPO_ROOT / "dev.sh", self.root / "dev.sh")
        shutil.copy2(
            REPO_ROOT / "rust-toolchain.toml", self.root / "rust-toolchain.toml"
        )
        (self.root / ".config").mkdir()
        (self.root / ".config" / "nextest.toml").write_text(
            'nextest-version = "0.9.100"\n', encoding="utf-8"
        )
        (self.root / "tools").mkdir()
        (self.root / "tools" / "run-nextest-binary.sh").write_text(
            "#!/usr/bin/env bash\nexec \"$@\"\n", encoding="utf-8"
        )
        wasm_script = self.root / "tools" / "run-wasm-tests.sh"
        wasm_script.write_text(
            """#!/usr/bin/env bash
set -euo pipefail
echo "wasm" >> "$DEV_CHECK_TEST_LOG"
if [[ "${DEV_CHECK_FAIL_COMMAND:-}" == "wasm" ]]; then exit 9; fi
""",
            encoding="utf-8",
        )
        wasm_script.chmod(0o755)
        (self.root / "web" / "loader" / "test").mkdir(parents=True)
        (self.root / "web" / "loader" / "test" / "loader.test.js").write_text(
            "", encoding="utf-8"
        )
        (self.root / ".gitignore").write_text("/target\n", encoding="utf-8")
        (self.root / "tracked.txt").write_text("base\n", encoding="utf-8")

        self._write_fake_commands()
        self._git("init", "-q")
        self._git("config", "user.email", "dev-check@example.com")
        self._git("config", "user.name", "Dev Check Test")
        self._git("add", ".")
        self._git("commit", "-qm", "Initial")

        self.env = os.environ.copy()
        self.env.pop("CI", None)
        self.env.update(
            {
                "PATH": f"{self.bin}:{self.env['PATH']}",
                "DEV_CHECK_TEST_LOG": str(self.log),
                "RUSTUP_TOOLCHAIN": "nightly",
                "TYDE_RUN_REAL_AI_TESTS": "must-be-unset",
                "TYDE_LIVE_CODEX_TEST": "must-be-unset",
                "TYDE_RUN_CLAUDE_INTEGRATION": "must-be-unset",
            }
        )

    def tearDown(self) -> None:
        self.temp.cleanup()

    def _write(self, name: str, content: str) -> None:
        path = self.bin / name
        path.write_text(content, encoding="utf-8")
        path.chmod(0o755)

    def _write_fake_commands(self) -> None:
        self._write(
            "cargo",
            """#!/usr/bin/env bash
set -euo pipefail
case "$*" in
  "-Vv") echo "cargo stable-test (test)"; echo "release: stable-test"; exit 0 ;;
  "nextest --version") echo "cargo-nextest 0.9.100"; exit 0 ;;
esac
echo "cargo $* toolchain=${RUSTUP_TOOLCHAIN-unset} real-ai=${TYDE_RUN_REAL_AI_TESTS-unset}/${TYDE_LIVE_CODEX_TEST-unset}/${TYDE_RUN_CLAUDE_INTEGRATION-unset}" >> "$DEV_CHECK_TEST_LOG"
if [[ "${DEV_CHECK_FAIL_COMMAND:-}" == "cargo $*" ]]; then exit 9; fi
""",
        )
        self._write(
            "cargo-nextest",
            "#!/usr/bin/env bash\necho 'cargo-nextest 0.9.100'\n",
        )
        self._write(
            "rustc",
            """#!/usr/bin/env bash
echo "rustc stable-test (test)"
echo "host: test-host"
""",
        )
        self._write(
            "rustup",
            """#!/usr/bin/env bash
case "$*" in
  "update stable")
    echo "rustup update stable toolchain=${RUSTUP_TOOLCHAIN-unset}" >> "$DEV_CHECK_TEST_LOG"
    [[ "${DEV_CHECK_FAIL_COMMAND:-}" == "rustup update stable" ]] && exit 9
    :
    ;;
  "toolchain install")
    echo "rustup toolchain install toolchain=${RUSTUP_TOOLCHAIN-unset}" >> "$DEV_CHECK_TEST_LOG"
    [[ "${DEV_CHECK_FAIL_COMMAND:-}" == "rustup toolchain install" ]] && exit 9
    :
    ;;
  "show active-toolchain") echo "stable-test-host (environment override by RUSTUP_TOOLCHAIN)" ;;
  "target list --installed") printf 'test-host\\nwasm32-unknown-unknown\\n' ;;
  *) exit 2 ;;
esac
""",
        )
        self._write(
            "node",
            """#!/usr/bin/env bash
set -euo pipefail
if [[ "${1:-}" == "--version" ]]; then echo "v22.0.0"; exit 0; fi
echo "node $*" >> "$DEV_CHECK_TEST_LOG"
if [[ "${DEV_CHECK_FAIL_COMMAND:-}" == "node" ]]; then exit 9; fi
""",
        )

    def _git(self, *args: str) -> subprocess.CompletedProcess[str]:
        return subprocess.run(
            ["git", *args],
            cwd=self.root,
            text=True,
            capture_output=True,
            check=True,
        )

    def _run(
        self, *args: str, env: dict[str, str] | None = None, check: bool = True
    ) -> subprocess.CompletedProcess[str]:
        result = subprocess.run(
            [str(self.root / "dev.sh"), "check", *args],
            cwd=self.root,
            env=env or self.env,
            text=True,
            capture_output=True,
            check=False,
        )
        if check and result.returncode != 0:
            self.fail(
                f"dev.sh failed with {result.returncode}:\n{result.stdout}\n{result.stderr}"
            )
        return result

    def _log_lines(self) -> list[str]:
        if not self.log.exists():
            return []
        return self.log.read_text(encoding="utf-8").splitlines()

    def _explain_key(self, env: dict[str, str] | None = None) -> str:
        result = self._run("--explain-cache", env=env)
        for line in result.stdout.splitlines():
            if line.startswith("cache.key="):
                return line.removeprefix("cache.key=")
        self.fail(f"cache key missing from output:\n{result.stdout}")

    def _index_digest(self) -> str:
        index = pathlib.Path(self._git("rev-parse", "--git-path", "index").stdout.strip())
        if not index.is_absolute():
            index = self.root / index
        return hashlib.sha256(index.read_bytes()).hexdigest()

    def test_miss_runs_required_counts_then_hit_only_updates_toolchain(self) -> None:
        first = self._run()

        self.assertIn("dev check cache miss", first.stdout)
        lines = self._log_lines()
        self.assertEqual(lines[:2], [TOOLCHAIN_UPDATE_LOG, TOOLCHAIN_INSTALL_LOG])
        self.assertEqual(sum(line.startswith("cargo fmt ") for line in lines), 1)
        self.assertEqual(sum(line.startswith("cargo check ") for line in lines), 1)
        self.assertEqual(sum(line.startswith("cargo clippy ") for line in lines), 1)
        self.assertEqual(sum(line.startswith("cargo nextest run ") for line in lines), 3)
        self.assertEqual(lines.count("wasm"), 3)
        self.assertEqual(sum(line.startswith("node --test ") for line in lines), 3)
        self.assertTrue(
            all(
                "real-ai=unset/unset/unset" in line
                for line in lines
                if line.startswith("cargo ")
            )
        )
        self.assertTrue(
            all(
                "toolchain=stable" in line
                for line in lines
                if line.startswith("cargo ")
            )
        )
        records = list((self.root / "target" / "dev-check-cache").glob("*.success"))
        self.assertEqual(len(records), 1)
        self.assertIn("complete=true", records[0].read_text(encoding="utf-8"))

        before = list(lines)
        second = self._run()

        self.assertIn("dev check cache hit", second.stdout)
        self.assertIn("Prior successful stage summary:", second.stdout)
        self.assertIn("cargo nextest run: 3/3 passed", second.stdout)
        self.assertEqual(
            self._log_lines(),
            before + [TOOLCHAIN_UPDATE_LOG, TOOLCHAIN_INSTALL_LOG],
        )

    def test_fingerprint_covers_git_states_without_mutating_real_index(self) -> None:
        base_key = self._explain_key()
        base_index = self._index_digest()
        self.assertEqual(self._index_digest(), base_index)

        (self.root / "untracked.txt").write_text("new\n", encoding="utf-8")
        untracked_key = self._explain_key()
        self.assertNotEqual(untracked_key, base_key)

        ignored = self.root / "target" / "ignored.txt"
        ignored.parent.mkdir(parents=True, exist_ok=True)
        ignored.write_text("ignored\n", encoding="utf-8")
        self.assertEqual(self._explain_key(), untracked_key)
        (self.root / "untracked.txt").unlink()

        tracked = self.root / "tracked.txt"
        tracked.write_text("unstaged\n", encoding="utf-8")
        unstaged_key = self._explain_key()
        self.assertNotEqual(unstaged_key, base_key)
        index_before = self._index_digest()
        cached_diff_before = self._git("diff", "--cached", "--binary").stdout
        self._explain_key()
        self.assertEqual(self._index_digest(), index_before)
        self.assertEqual(
            self._git("diff", "--cached", "--binary").stdout, cached_diff_before
        )

        self._git("add", "tracked.txt")
        staged_key = self._explain_key()
        self.assertNotEqual(staged_key, unstaged_key)
        index_before = self._index_digest()
        tracked.unlink()
        deleted_key = self._explain_key()
        self.assertNotEqual(deleted_key, staged_key)
        self.assertEqual(self._index_digest(), index_before)

    def test_modes_environment_and_failures_obey_cache_contract(self) -> None:
        self._run()
        initial_records = list((self.root / "target" / "dev-check-cache").glob("*.success"))
        initial_log_count = len(self._log_lines())

        forced = self._run("--force")
        self.assertIn("dev check cache bypassed", forced.stdout)
        self.assertEqual(len(self._log_lines()) - initial_log_count, 14)
        self.assertEqual(
            len(list((self.root / "target" / "dev-check-cache").glob("*.success"))),
            len(initial_records),
        )

        before_no_cache = len(self._log_lines())
        no_cache = self._run("--no-cache")
        self.assertIn("dev check cache disabled", no_cache.stdout)
        self.assertEqual(len(self._log_lines()) - before_no_cache, 8)
        self.assertEqual(
            len(list((self.root / "target" / "dev-check-cache").glob("*.success"))),
            len(initial_records),
        )

        env_one = self.env.copy()
        env_one["TYDE_RUN_REAL_LSP_TESTS"] = "one"
        env_two = self.env.copy()
        env_two["TYDE_RUN_REAL_LSP_TESTS"] = "two"
        self.assertNotEqual(self._explain_key(env_one), self._explain_key(env_two))

        without_real_ai = self.env.copy()
        without_real_ai.pop("TYDE_RUN_REAL_AI_TESTS")
        without_real_ai.pop("TYDE_LIVE_CODEX_TEST")
        without_real_ai.pop("TYDE_RUN_CLAUDE_INTEGRATION")
        self.assertEqual(self._explain_key(), self._explain_key(without_real_ai))

        (self.root / "failure.txt").write_text("new key\n", encoding="utf-8")
        failing_env = self.env.copy()
        failing_env["DEV_CHECK_FAIL_COMMAND"] = "cargo nextest run"
        failed = self._run(env=failing_env, check=False)
        self.assertNotEqual(failed.returncode, 0)
        self.assertEqual(
            len(list((self.root / "target" / "dev-check-cache").glob("*.success"))),
            len(initial_records),
        )
        self.assertEqual(list((self.root / "target" / "dev-check-cache").glob(".success.*")), [])

    def test_toolchain_update_failure_precedes_cache_evaluation_and_checks(self) -> None:
        self._run()
        before = self._log_lines()
        env = self.env.copy()
        env["DEV_CHECK_FAIL_COMMAND"] = "rustup update stable"

        rejected = self._run(env=env, check=False)

        self.assertEqual(rejected.returncode, 1)
        self.assertIn("could not update the stable Rust toolchain", rejected.stderr)
        self.assertNotIn("dev check cache hit", rejected.stdout)
        self.assertEqual(self._log_lines(), before + [TOOLCHAIN_UPDATE_LOG])

    def test_workflow_toolchain_entrypoint_uses_the_check_update_path(self) -> None:
        result = subprocess.run(
            [str(self.root / "dev.sh"), "rust-toolchain"],
            cwd=self.root,
            env=self.env,
            text=True,
            capture_output=True,
            check=False,
        )

        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(
            self._log_lines(), [TOOLCHAIN_UPDATE_LOG, TOOLCHAIN_INSTALL_LOG]
        )

    def test_ci_requires_force_and_release_guard_uses_force(self) -> None:
        ci_env = self.env.copy()
        ci_env["CI"] = "true"
        rejected = self._run("--no-cache", env=ci_env, check=False)
        self.assertEqual(rejected.returncode, 1)
        self.assertIn("CI must invoke ./dev.sh check --force", rejected.stderr)
        self.assertEqual(self._log_lines(), [])

        release_check = (REPO_ROOT / "tools" / "release_check.sh").read_text(
            encoding="utf-8"
        )
        self.assertIn("./dev.sh check --force", release_check)
        self.assertNotIn("./dev.sh check\n", release_check)
        release_workflow = (
            REPO_ROOT / ".github" / "workflows" / "release.yml"
        ).read_text(encoding="utf-8")
        self.assertIn("run: ./dev.sh check --force", release_workflow)


class RustToolchainParityTests(unittest.TestCase):
    def test_repository_pin_declares_every_required_rust_tool(self) -> None:
        self.assertEqual(
            (REPO_ROOT / "rust-toolchain.toml").read_text(encoding="utf-8"),
            """[toolchain]
channel = "stable"
components = ["clippy", "rustfmt"]
targets = ["wasm32-unknown-unknown"]
profile = "minimal"
""",
        )

    def test_release_workflows_install_the_repository_pin(self) -> None:
        release_workflow = (
            REPO_ROOT / ".github" / "workflows" / "release.yml"
        ).read_text(encoding="utf-8")
        mobile_workflow = (
            REPO_ROOT / ".github" / "workflows" / "mobile-web-release.yml"
        ).read_text(encoding="utf-8")

        root_install = """      - name: Install repository Rust toolchain
        run: ./dev.sh rust-toolchain
"""
        nested_install = """      - name: Install repository Rust toolchain
        working-directory: deploy-tools
        shell: bash
        run: |
          ./dev.sh rust-toolchain
"""

        self.assertEqual(release_workflow.count(root_install), 3)
        self.assertEqual(mobile_workflow.count(nested_install), 1)
        self.assertNotIn("dtolnay/rust-toolchain@stable", release_workflow)
        self.assertNotIn("dtolnay/rust-toolchain@stable", mobile_workflow)
        self.assertNotIn("rustup update stable", release_workflow)
        self.assertNotIn("rustup update stable", mobile_workflow)
        self.assertIn(
            "RUSTUP_TOOLCHAIN=$(rustup show active-toolchain", mobile_workflow
        )


if __name__ == "__main__":
    unittest.main()
