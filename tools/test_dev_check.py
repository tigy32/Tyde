from __future__ import annotations

import hashlib
import os
import pathlib
import platform
import shutil
import subprocess
import sys
import tempfile
import time
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
        shutil.copy2(
            REPO_ROOT / "tools" / "run-nextest-binary.sh",
            self.root / "tools" / "run-nextest-binary.sh",
        )
        wasm_script = self.root / "tools" / "run-wasm-tests.sh"
        wasm_script.write_text(
            """#!/usr/bin/env bash
set -euo pipefail
identity() {
  printf 'wasm.chrome.path=%s\\n' "${CHROME:-provisioned-chrome}"
  printf 'wasm.chrome.version=%s\\n' "$("${CHROME:-$DEV_CHECK_FAKE_CHROME}" --version)"
  printf 'wasm.chromedriver.path=%s\\n' "${CHROMEDRIVER:-provisioned-driver}"
  printf 'wasm.chromedriver.version=%s\\n' "$("${CHROMEDRIVER:-$DEV_CHECK_FAKE_CHROMEDRIVER}" --version)"
  printf 'wasm.bindgen.required=0.2.118\\n'
  printf 'wasm.bindgen.path=%s\\n' "${WASM_BINDGEN_TEST_RUNNER:-provisioned-runner}"
  printf 'wasm.bindgen.version=%s\\n' "$("${WASM_BINDGEN_TEST_RUNNER:-$DEV_CHECK_FAKE_RUNNER}" --version)"
}
if [[ "${1:-}" == "--identity" ]]; then identity; exit 0; fi
if [[ "${1:-}" == "--prepare" ]]; then
  output="$2"
  identity_file="$output.identity"
  identity > "$identity_file"
  {
    printf 'export TYDE_WASM_TOOLS_PREPARED=1\\n'
    printf 'export CHROME=%q\\n' "${CHROME:-$DEV_CHECK_FAKE_CHROME}"
    printf 'export CHROMEDRIVER=%q\\n' "${CHROMEDRIVER:-$DEV_CHECK_FAKE_CHROMEDRIVER}"
    printf 'export WASM_BINDGEN_TEST_RUNNER=%q\\n' "${WASM_BINDGEN_TEST_RUNNER:-$DEV_CHECK_FAKE_RUNNER}"
    printf 'export WASM_BINDGEN_TEST_WEBDRIVER_JSON=%q\\n' "$output.webdriver.json"
    printf 'export TYDE_WASM_IDENTITY_FILE=%q\\n' "$identity_file"
  } > "$output"
  echo "wasm-prepare" >> "$DEV_CHECK_TEST_LOG"
  exit 0
fi
[[ "${TYDE_WASM_TOOLS_PREPARED:-0}" == 1 ]]
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
        (self.root / "tools" / "test_dev_check.py").write_text(
            """import os
with open(os.environ["DEV_CHECK_TEST_LOG"], "a", encoding="utf-8") as log:
    log.write("contract\\n")
""",
            encoding="utf-8",
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
                "TMPDIR": str(pathlib.Path(self.temp.name) / "tmp"),
                "CHROME": str(self.bin / "google-chrome"),
                "CHROMEDRIVER": str(self.bin / "chromedriver"),
                "WASM_BINDGEN_TEST_RUNNER": str(
                    self.bin / "wasm-bindgen-test-runner"
                ),
                "DEV_CHECK_FAKE_CHROME": str(self.bin / "google-chrome"),
                "DEV_CHECK_FAKE_CHROMEDRIVER": str(self.bin / "chromedriver"),
                "DEV_CHECK_FAKE_RUNNER": str(self.bin / "wasm-bindgen-test-runner"),
                "DEV_CHECK_REAL_PYTHON": sys.executable,
                "TYDE_RUN_REAL_AI_TESTS": "must-be-unset",
                "TYDE_LIVE_CODEX_TEST": "must-be-unset",
                "TYDE_RUN_CLAUDE_INTEGRATION": "must-be-unset",
            }
        )
        pathlib.Path(self.env["TMPDIR"]).mkdir()

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
echo "successful cargo output that must stay in the stage log"
echo "cargo $* toolchain=${RUSTUP_TOOLCHAIN-unset} real-ai=${TYDE_RUN_REAL_AI_TESTS-unset}/${TYDE_LIVE_CODEX_TEST-unset}/${TYDE_RUN_CLAUDE_INTEGRATION-unset}" >> "$DEV_CHECK_TEST_LOG"
if [[ "${DEV_CHECK_NATIVE_MULTI_FAILURE:-0}" == 1 && "cargo $*" == "cargo nextest run" ]]; then
  echo "FAIL [0.010s] tests::native first_independent_failure" >&2
  echo "first independent failure diagnostics" >&2
  echo "FAIL [0.020s] tests::native second_independent_failure" >&2
  echo "second independent failure diagnostics" >&2
  exit 9
fi
if [[ "${DEV_CHECK_FAIL_COMMAND:-}" == "cargo $*" ]]; then
  if [[ -n "${DEV_CHECK_FAIL_ON_RUN:-}" ]]; then
    count_file="$DEV_CHECK_TEST_LOG.fail-count"
    count=0
    [[ -f "$count_file" ]] && count="$(cat "$count_file")"
    count=$((count + 1))
    printf '%s\\n' "$count" > "$count_file"
    printf 'failure-controlled invocation=%s\\n' "$count"
    [[ "$count" == "$DEV_CHECK_FAIL_ON_RUN" ]] || exit 0
  fi
  echo "complete actionable failure from cargo $*" >&2
  exit 9
fi
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
        self._write(
            "sccache",
            """#!/usr/bin/env bash
set -euo pipefail
case "$*" in
  "--version") echo "sccache 0.16.0" ;;
  "--start-server") : ;;
  "--show-stats --stats-format=json")
    cache_dir="$SCCACHE_DIR"
    [[ "${DEV_CHECK_BAD_SCCACHE:-0}" == 1 ]] && cache_dir="/wrong-cache"
    python3 - "$cache_dir" <<'PY'
import json
import sys

print(json.dumps({
    "stats": {
        "compile_requests": 0,
        "cache_hits": {"counts": {}},
        "cache_misses": {"counts": {}},
        "cache_errors": {"counts": {}},
        "cache_writes": 0,
    },
    "cache_location": f'Local disk: "{sys.argv[1]}"',
    "cache_size": 0,
    "max_cache_size": 10737418240,
}))
PY
    ;;
  *) exit 2 ;;
esac
""",
        )
        self._write(
            "google-chrome",
            "#!/usr/bin/env bash\necho 'Google Chrome 150.0.7871.102'\n",
        )
        self._write(
            "chromedriver",
            "#!/usr/bin/env bash\necho 'ChromeDriver 150.0.7871.115 test'\n",
        )
        self._write(
            "wasm-bindgen-test-runner",
            "#!/usr/bin/env bash\necho 'wasm-bindgen-test-runner 0.2.118'\n",
        )
        self._write(
            "python3",
            """#!/usr/bin/env bash
if [[ "${1:-}" == "--version" ]]; then
  if [[ "${DEV_CHECK_FAIL_PYTHON_IDENTITY:-0}" == 1 ]]; then
    echo "python identity detail from stderr" >&2
    exit 7
  fi
  echo "Python ${DEV_CHECK_FAKE_PYTHON_VERSION:-3.test}"
  exit 0
fi
exec "$DEV_CHECK_REAL_PYTHON" "$@"
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

        self.assertIn("CACHE MISS", first.stdout)
        self.assertIn("START cargo fmt --all --check", first.stdout)
        self.assertIn("PASS  cargo fmt --all --check (1/1", first.stdout)
        self.assertNotIn("successful cargo output", first.stdout)
        lines = self._log_lines()
        self.assertEqual(lines[:2], [TOOLCHAIN_UPDATE_LOG, TOOLCHAIN_INSTALL_LOG])
        self.assertEqual(sum(line.startswith("cargo fmt ") for line in lines), 1)
        self.assertEqual(sum(line.startswith("cargo check ") for line in lines), 1)
        self.assertEqual(sum(line.startswith("cargo clippy ") for line in lines), 1)
        self.assertEqual(sum(line.startswith("cargo nextest run ") for line in lines), 1)
        self.assertEqual(lines.count("wasm"), 1)
        self.assertEqual(sum(line.startswith("node --test ") for line in lines), 1)
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
        record = records[0].read_text(encoding="utf-8")
        self.assertIn("schema=4", record)
        self.assertIn("complete=true", record)
        self.assertTrue(record.endswith("record.end=true\n"))
        self.assertEqual(
            list((self.root / "target" / "dev-check-cache").glob(".success.*")), []
        )
        run_dir = max((self.root / "target" / "dev-check-logs").glob("run-*"))
        metadata = (run_dir / "metadata.txt").read_text(encoding="utf-8")
        self.assertIn("disk.start.", metadata)
        self.assertIn("disk.finish.", metadata)
        self.assertIn("cleanup.reclaimed_bytes=", metadata)
        self.assertIn("sccache.delta.requests=0", metadata)
        self.assertIn("overall.cache=miss", metadata)
        fmt_log = next(run_dir.glob("*-cargo-fmt-all-check.log"))
        self.assertIn(
            "successful cargo output that must stay in the stage log",
            fmt_log.read_text(encoding="utf-8"),
        )

        before = list(lines)
        second = self._run()

        self.assertIn("CACHE HIT", second.stdout)
        self.assertIn("PRIOR PASS  cargo nextest run (1/1", second.stdout)
        self.assertEqual(
            self._log_lines(),
            before + [TOOLCHAIN_UPDATE_LOG, TOOLCHAIN_INSTALL_LOG, "wasm-prepare"],
        )

    def test_fingerprint_tracks_commit_and_worktree_content(self) -> None:
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
        self.assertEqual(staged_key, unstaged_key)
        self._git("commit", "-qm", "Update tracked content")
        committed_key = self._explain_key()
        self.assertNotEqual(committed_key, unstaged_key)

        index_after_commit = self._index_digest()
        tracked.unlink()
        deleted_key = self._explain_key()
        self.assertNotEqual(deleted_key, committed_key)
        self.assertEqual(self._index_digest(), index_after_commit)

    def test_fingerprint_ignores_environment_and_tool_identities(self) -> None:
        base_key = self._explain_key()

        chrome = self.bin / "google-chrome"
        chrome.write_text(
            "#!/usr/bin/env bash\necho 'Google Chrome 151.0.8000.1'\n",
            encoding="utf-8",
        )
        chrome.chmod(0o755)
        self.assertEqual(self._explain_key(), base_key)

        chrome.write_text(
            "#!/usr/bin/env bash\necho 'Google Chrome 150.0.7871.102'\n",
            encoding="utf-8",
        )
        chrome.chmod(0o755)
        runner = self.bin / "wasm-bindgen-test-runner"
        runner.write_text(
            "#!/usr/bin/env bash\necho 'wasm-bindgen-test-runner 0.2.119'\n",
            encoding="utf-8",
        )
        runner.chmod(0o755)
        self.assertEqual(self._explain_key(), base_key)

        runner.write_text(
            "#!/usr/bin/env bash\necho 'wasm-bindgen-test-runner 0.2.118'\n",
            encoding="utf-8",
        )
        runner.chmod(0o755)
        changed_config = self.env.copy()
        changed_config["SCCACHE_RECACHE"] = "1"
        changed_config["SCCACHE_BUCKET"] = "must-not-be-used"
        changed_config["SCCACHE_SERVER_PORT"] = "1"
        self.assertEqual(self._explain_key(changed_config), base_key)

        changed_python = self.env.copy()
        changed_python["DEV_CHECK_FAKE_PYTHON_VERSION"] = "3.changed"
        self.assertEqual(self._explain_key(changed_python), base_key)

    def test_environment_and_failures_obey_cache_contract(self) -> None:
        self._run()
        initial_records = list((self.root / "target" / "dev-check-cache").glob("*.success"))
        initial_log_count = len(self._log_lines())

        for removed_option in ("--force", "--no-cache"):
            rejected = self._run(removed_option, check=False)
            self.assertEqual(rejected.returncode, 2)
        self.assertEqual(len(self._log_lines()), initial_log_count)
        self.assertEqual(
            len(list((self.root / "target" / "dev-check-cache").glob("*.success"))),
            len(initial_records),
        )

        env_one = self.env.copy()
        env_one["TYDE_RUN_REAL_LSP_TESTS"] = "one"
        env_two = self.env.copy()
        env_two["TYDE_RUN_REAL_LSP_TESTS"] = "two"
        self.assertEqual(self._explain_key(env_one), self._explain_key(env_two))

        without_real_ai = self.env.copy()
        without_real_ai.pop("TYDE_RUN_REAL_AI_TESTS")
        without_real_ai.pop("TYDE_LIVE_CODEX_TEST")
        without_real_ai.pop("TYDE_RUN_CLAUDE_INTEGRATION")
        self.assertEqual(self._explain_key(), self._explain_key(without_real_ai))

        (self.root / "failure.txt").write_text("new key\n", encoding="utf-8")
        failing_env = self.env.copy()
        failing_env["DEV_CHECK_FAIL_COMMAND"] = "cargo nextest run"
        failing_env["DEV_CHECK_FAIL_ON_RUN"] = "1"
        failed = self._run(env=failing_env, check=False)
        self.assertEqual(failed.returncode, 9)
        self.assertIn("FAIL  cargo nextest run (1/1", failed.stderr)
        self.assertIn(
            "complete actionable failure from cargo nextest run", failed.stderr
        )
        self.assertIn("Failing repetition diagnostics:", failed.stderr)
        self.assertIn("Complete stage log:", failed.stderr)
        self.assertIn("failure-controlled invocation=1", failed.stderr)
        failure_run = max(
            (self.root / "target" / "dev-check-logs").glob("run-*")
        )
        nextest_log = next(failure_run.glob("*-cargo-nextest-run.log"))
        full_log = nextest_log.read_text(encoding="utf-8")
        self.assertIn("failure-controlled invocation=1", full_log)
        failure_metadata = (failure_run / "metadata.txt").read_text(encoding="utf-8")
        self.assertIn("failure_log=", failure_metadata)
        self.assertEqual(
            len(list((self.root / "target" / "dev-check-cache").glob("*.success"))),
            len(initial_records),
        )
        self.assertEqual(
            list((self.root / "target" / "dev-check-cache").glob(".success.*")),
            [],
        )

    def test_native_failure_retains_all_diagnostics_and_gates_later_work(self) -> None:
        env = self.env.copy()
        env["DEV_CHECK_NATIVE_MULTI_FAILURE"] = "1"

        failed = self._run(env=env, check=False)

        self.assertEqual(failed.returncode, 9)
        self.assertIn("FAIL  cargo nextest run (1/1", failed.stderr)
        for diagnostic in (
            "first_independent_failure",
            "first independent failure diagnostics",
            "second_independent_failure",
            "second independent failure diagnostics",
        ):
            self.assertIn(diagnostic, failed.stderr)
        self.assertIn("Complete stage log:", failed.stderr)
        lines = self._log_lines()
        self.assertEqual(sum(line.startswith("cargo nextest run ") for line in lines), 1)
        self.assertNotIn("wasm", lines)
        self.assertFalse(any(line.startswith("node --test ") for line in lines))
        run_dir = max((self.root / "target" / "dev-check-logs").glob("run-*"))
        nextest_log = next(run_dir.glob("*-cargo-nextest-run.log"))
        full_log = nextest_log.read_text(encoding="utf-8")
        self.assertLess(
            full_log.index("first_independent_failure"),
            full_log.index("second_independent_failure"),
        )
        repetition_log = run_dir / ".repetition-10-1.log"
        repetition_output = repetition_log.read_text(encoding="utf-8")
        self.assertIn("first_independent_failure", repetition_output)
        self.assertIn("second_independent_failure", repetition_output)
        metadata = (run_dir / "metadata.txt").read_text(encoding="utf-8")
        self.assertIn(f"failure_log={repetition_log}", metadata)

    def test_toolchain_update_failure_precedes_cache_evaluation_and_checks(self) -> None:
        self._run()
        before = self._log_lines()
        env = self.env.copy()
        env["DEV_CHECK_FAIL_COMMAND"] = "rustup update stable"

        rejected = self._run(env=env, check=False)

        self.assertEqual(rejected.returncode, 9)
        self.assertIn("FAIL  Update stable Rust toolchain", rejected.stderr)
        self.assertNotIn("CACHE HIT", rejected.stdout)
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

    def test_pr_and_local_release_guards_use_canonical_check(self) -> None:
        ci_env = self.env.copy()
        ci_env["CI"] = "true"
        result = self._run(env=ci_env)
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertIn("CACHE MISS", result.stdout)

        release_check = (REPO_ROOT / "tools" / "release_check.sh").read_text(
            encoding="utf-8"
        )
        self.assertIn("./dev.sh check\n", release_check)
        self.assertNotIn("./dev.sh check --", release_check)
        release_workflow = (
            REPO_ROOT / ".github" / "workflows" / "release.yml"
        ).read_text(encoding="utf-8")
        self.assertNotIn("run: ./dev.sh check", release_workflow)
        check_workflow = (
            REPO_ROOT / ".github" / "workflows" / "check.yml"
        ).read_text(encoding="utf-8")
        self.assertIn("pull_request:", check_workflow)
        self.assertNotIn("push:", check_workflow)
        self.assertIn("runs-on: ubuntu-latest", check_workflow)
        self.assertIn("run: ./dev.sh check", check_workflow)
        install = "cargo install sccache --version 0.16.0 --locked --force"
        self.assertIn(install, check_workflow)
        self.assertLess(
            check_workflow.index(install),
            check_workflow.index("run: ./dev.sh check"),
        )

    def test_contract_stage_is_reachable_without_recursive_checks(self) -> None:
        env = self.env.copy()
        env["DEV_CHECK_CONTRACT_CHILD"] = "1"

        result = self._run(env=env)

        self.assertIn("START dev check contract tests", result.stdout)
        self.assertIn("PASS  dev check contract tests (1/1", result.stdout)
        self.assertEqual(self._log_lines().count("contract"), 1)

    def test_lock_contention_fails_immediately_with_owner(self) -> None:
        lock = self.root / "target" / "dev-check.lock"
        lock.mkdir(parents=True)
        (lock / "owner").write_text(
            f"pid={os.getpid()}\nrepository={self.root}\n", encoding="utf-8"
        )

        rejected = self._run(check=False)

        self.assertEqual(rejected.returncode, 1)
        self.assertIn("another ./dev.sh check is already running", rejected.stderr)
        self.assertIn(f"PID {os.getpid()}", rejected.stderr)
        self.assertEqual(self._log_lines(), [])

    def test_invalid_and_partial_cache_records_are_never_hits(self) -> None:
        first = self._run()
        self.assertIn("CACHE MISS", first.stdout)
        record = next(
            (self.root / "target" / "dev-check-cache").glob("*.success")
        )
        original = record.read_text(encoding="utf-8")
        record.write_text(original.removesuffix("record.end=true\n"), encoding="utf-8")
        before = len(self._log_lines())

        rerun = self._run()

        self.assertIn("CACHE MISS", rerun.stdout)
        self.assertGreater(len(self._log_lines()), before)
        self.assertTrue(record.read_text(encoding="utf-8").endswith("record.end=true\n"))
        self.assertEqual(
            list((self.root / "target" / "dev-check-cache").glob(".success.*")), []
        )

    def test_wrong_sccache_version_fails_instead_of_falling_back(self) -> None:
        sccache = self.bin / "sccache"
        contents = sccache.read_text(encoding="utf-8")
        sccache.write_text(
            contents.replace("sccache 0.16.0", "sccache 0.15.0"),
            encoding="utf-8",
        )
        sccache.chmod(0o755)

        rejected = self._run(check=False)

        self.assertEqual(rejected.returncode, 1)
        self.assertIn("required sccache 0.16.0", rejected.stderr)
        self.assertIn("cargo install sccache --version 0.16.0 --locked", rejected.stderr)
        self.assertIn("Failing repetition diagnostics:", rejected.stderr)
        self.assertFalse(
            any(line.startswith("cargo fmt ") for line in self._log_lines())
        )

    def test_identity_failure_preserves_underlying_diagnostics_and_status(self) -> None:
        env = self.env.copy()
        env["DEV_CHECK_FAIL_PYTHON_IDENTITY"] = "1"

        rejected = self._run("--explain-cache", env=env, check=False)

        self.assertEqual(rejected.returncode, 7)
        self.assertIn("could not read python3 identity (exit 7)", rejected.stderr)
        self.assertIn("python identity detail from stderr", rejected.stderr)

    def test_sccache_validation_failure_has_log_and_failure_stats(self) -> None:
        env = self.env.copy()
        env["DEV_CHECK_BAD_SCCACHE"] = "1"

        rejected = self._run(env=env, check=False)

        self.assertEqual(rejected.returncode, 1)
        self.assertIn("sccache is not using the check-local cache", rejected.stderr)
        self.assertIn("Failing repetition diagnostics:", rejected.stderr)
        run_dir = max((self.root / "target" / "dev-check-logs").glob("run-*"))
        metadata = (run_dir / "metadata.txt").read_text(encoding="utf-8")
        self.assertIn("stage.05.log=", metadata)
        self.assertIn("stage.05.failure_log=", metadata)
        self.assertIn("sccache.failure_stats=", metadata)

    def test_explain_cache_is_non_destructive_and_does_not_provision(self) -> None:
        logs = self.root / "target" / "dev-check-logs"
        logs.mkdir(parents=True)
        sentinel = logs / "run-sentinel"
        sentinel.mkdir()
        orphan = self.root / "target" / "dev-check-cache" / ".success.orphan"
        orphan.parent.mkdir(parents=True)
        orphan.write_text("partial\n", encoding="utf-8")

        result = self._run("--explain-cache")

        self.assertIn("cache.key=", result.stdout)
        self.assertTrue(sentinel.exists())
        self.assertTrue(orphan.exists())
        self.assertEqual(self._log_lines(), [])
        self.assertEqual(list(logs.glob("run-20*")), [])

    def test_cold_wasm_tools_provision_before_cache_identity_once(self) -> None:
        env = self.env.copy()
        env.pop("CHROME")
        env.pop("CHROMEDRIVER")
        env.pop("WASM_BINDGEN_TEST_RUNNER")

        result = self._run(env=env)

        self.assertIn("PASS  Provision wasm test tools", result.stdout)
        self.assertEqual(self._log_lines().count("wasm-prepare"), 1)
        self.assertEqual(self._log_lines().count("wasm"), 1)
        self.assertEqual(
            len(list((self.root / "target" / "dev-check-cache").glob("*.success"))),
            1,
        )

    def test_success_retention_uses_mtime_and_removes_orphan_temp(self) -> None:
        cache = self.root / "target" / "dev-check-cache"
        cache.mkdir(parents=True)
        records = []
        for index in range(18):
            record = cache / f"{index:02x}.success"
            record.write_text("old\n", encoding="utf-8")
            os.utime(record, (1000 + index, 1000 + index))
            records.append(record)
        orphan = cache / ".success.interrupted"
        orphan.write_text("partial\n", encoding="utf-8")

        self._run()

        self.assertFalse(records[0].exists())
        self.assertFalse(records[1].exists())
        self.assertTrue(all(record.exists() for record in records[2:]))
        self.assertFalse(orphan.exists())

    def test_empty_cleanup_directories_are_valid_under_nounset(self) -> None:
        target = self.root / "target"
        (target / "dev-check-logs").mkdir(parents=True)
        (target / "dev-check-cache").mkdir()

        result = self._run()

        self.assertIn("RESULT PASS", result.stdout)


class TestingBehaviorContractTests(unittest.TestCase):
    def test_nextest_profiles_preserve_failure_and_output_policy(self) -> None:
        source = (REPO_ROOT / ".config" / "nextest.toml").read_text(
            encoding="utf-8"
        )
        shared = {
            'global-timeout = "5m"',
            'fail-fast = false',
            'status-level = "slow"',
            'final-status-level = "slow"',
            'success-output = "never"',
            'failure-output = "final"',
        }
        profile_specific = {
            "default": {'slow-timeout = "30s"', "retries = 0"},
            "ci": {'slow-timeout = "60s"', "retries = 2"},
        }
        for profile in ("default", "ci"):
            body = source.split(f"[profile.{profile}]\n", 1)[1].split(
                f"\n[[profile.{profile}.scripts]]", 1
            )[0]
            lines = [line.strip() for line in body.splitlines()]
            for setting in shared | profile_specific[profile]:
                self.assertEqual(
                    lines.count(setting),
                    1,
                    f"profile {profile} must contain {setting}",
                )

    def test_fixture_tracing_defaults_to_warn_without_hiding_rust_log(self) -> None:
        source = (REPO_ROOT / "tests" / "tests" / "fixture.rs").read_text(
            encoding="utf-8"
        )
        self.assertIn(
            ".with_default_directive(tracing_subscriber::filter::LevelFilter::WARN.into())",
            source,
        )
        self.assertIn(".from_env_lossy()", source)
        self.assertNotIn("EnvFilter::from_default_env()", source)

    def test_workbench_watcher_exception_is_exact_and_test_local(self) -> None:
        source = (REPO_ROOT / "tests" / "tests" / "workbenches.rs").read_text(
            encoding="utf-8"
        )
        helper = source.split("async fn expect_project_notify", 1)[1].split(
            "async fn expect_command_error", 1
        )[0]
        for diagnostic in (
            "context={context}",
            "envelope_stream={}",
            "request_kind={:?}",
            "operation={}",
            "code={:?}",
            "message={:?}",
            "fatal={}",
        ):
            self.assertIn(diagnostic, helper)

        test_body = source.split(
            "async fn workbench_remove_succeeds_when_worktree_dir_was_deleted_out_of_band()",
            1,
        )[1].split("\n#[tokio::test]", 1)[0]
        for exact_match in (
            'assert_eq!(env.stream, expected_stream',
            'assert_eq!(error.stream, expected_stream',
            "assert_eq!(error.request_kind, FrameKind::ProjectFileList)",
            'assert_eq!(error.operation, "project_watch")',
            "assert_eq!(error.code, CommandErrorCode::Internal)",
            "assert!(error.fatal",
            "error.message.contains(deleted_root.as_ref())",
        ):
            self.assertIn(exact_match, test_body)
        self.assertIn("!tolerated_watcher_error", test_body)


class NextestWrapperContractTests(unittest.TestCase):
    def test_lock_release_requires_current_owner(self) -> None:
        source = (REPO_ROOT / "tools" / "run-nextest-binary.sh").read_text(
            encoding="utf-8"
        )
        self.assertIn("LOCK_HELD=false", source)
        self.assertIn('if [[ "$owner_pid" == "$$" ]]', source)
        self.assertIn('if mkdir "$lock_dir"', source)
        self.assertIn('lease_dir="$(mktemp', source)
        self.assertIn("ownerless_grace_seconds=5", source)


@unittest.skipUnless(platform.system() == "Darwin", "macOS clone wrapper")
class NextestCloneTests(unittest.TestCase):
    def test_logical_target_replaces_stale_clone_and_dead_lock(self) -> None:
        with tempfile.TemporaryDirectory() as temp_name:
            temp = pathlib.Path(temp_name)
            root = temp / "repo"
            tools = root / "tools"
            tools.mkdir(parents=True)
            wrapper = tools / "run-nextest-binary.sh"
            shutil.copy2(REPO_ROOT / "tools" / "run-nextest-binary.sh", wrapper)
            tmpdir = temp / "tmp"
            tmpdir.mkdir()
            env = os.environ.copy()
            env["TMPDIR"] = str(tmpdir)

            first = root / "sample-aaaaaaaaaaaaaaaa"
            second = root / "sample-bbbbbbbbbbbbbbbb"
            for binary in (first, second):
                binary.write_text("#!/usr/bin/env bash\nexit 0\n", encoding="utf-8")
                binary.chmod(0o755)

            subprocess.run([str(wrapper), str(first)], env=env, check=True)
            workspace = next((tmpdir / "tyde-nextest").iterdir())
            lock = workspace / "sample.lock"
            lock.mkdir()
            old = time.time() - 10
            os.utime(lock, (old, old))

            subprocess.run([str(wrapper), str(second)], env=env, check=True)

            clones = [path for path in workspace.glob("sample.*") if path.is_file()]
            self.assertEqual(len(clones), 1)
            self.assertFalse(lock.exists())
            partial_lock = workspace / "partial.lock"
            partial_lease = workspace / "sample.partial.use.ownerless"
            recent_lock = workspace / "recent.lock"
            recent_lease = workspace / "sample.recent.use.ownerless"
            partial_lock.mkdir()
            partial_lease.write_text("", encoding="utf-8")
            recent_lock.mkdir()
            recent_lease.write_text("", encoding="utf-8")
            os.utime(partial_lock, (old, old))
            os.utime(partial_lease, (old, old))
            for index in range(70):
                extra = workspace / f"extra.{index:02d}"
                extra.write_text("x", encoding="utf-8")
                extra.chmod(0o755)
                os.utime(extra, (1000 + index, 1000 + index))
            cleanup_env = env.copy()
            cleanup_env["TYDE_DEV_CHECK_LOCK_HELD"] = "1"
            cleanup = subprocess.run(
                [str(wrapper), "--cleanup-stale"],
                env=cleanup_env,
                text=True,
                capture_output=True,
                check=True,
            )
            self.assertGreater(int(cleanup.stdout), 0)
            self.assertTrue(workspace.exists())
            self.assertFalse(partial_lock.exists())
            self.assertFalse(partial_lease.exists())
            self.assertTrue(recent_lock.exists())
            self.assertTrue(recent_lease.exists())
            self.assertLessEqual(
                len(
                    [
                        path
                        for path in workspace.iterdir()
                        if path.is_file() and os.access(path, os.X_OK)
                    ]
                ),
                64,
            )


class WasmToolScriptTests(unittest.TestCase):
    def test_identity_is_read_only_and_prepare_pins_exact_overrides(self) -> None:
        with tempfile.TemporaryDirectory() as temp_name:
            root = pathlib.Path(temp_name) / "repo"
            tools = root / "tools"
            binaries = root / "bin"
            tools.mkdir(parents=True)
            binaries.mkdir()
            script = tools / "run-wasm-tests.sh"
            shutil.copy2(REPO_ROOT / "tools" / "run-wasm-tests.sh", script)
            (root / "Cargo.lock").write_text(
                'name = "wasm-bindgen"\nversion = "0.2.118"\n',
                encoding="utf-8",
            )

            chrome = binaries / "chrome"
            driver = binaries / "chromedriver"
            runner = binaries / "wasm-bindgen-test-runner"
            chrome.write_text(
                "#!/usr/bin/env bash\necho 'Google Chrome 150.0.7871.102'\n",
                encoding="utf-8",
            )
            driver.write_text(
                "#!/usr/bin/env bash\necho 'ChromeDriver 150.0.7871.115'\n",
                encoding="utf-8",
            )
            runner.write_text(
                "#!/usr/bin/env bash\necho 'wasm-bindgen-test-runner 0.2.118'\n",
                encoding="utf-8",
            )
            for binary in (chrome, driver, runner):
                binary.chmod(0o755)
            env = os.environ.copy()
            env.update(
                {
                    "CHROME": str(chrome),
                    "CHROMEDRIVER": str(driver),
                    "WASM_BINDGEN_TEST_RUNNER": str(runner),
                }
            )
            webdriver = root / "webdriver.json"
            webdriver.write_text('{"capabilities": "external"}\n', encoding="utf-8")
            env["WASM_BINDGEN_TEST_WEBDRIVER_JSON"] = str(webdriver)

            identity = subprocess.run(
                [str(script), "--identity"],
                env=env,
                text=True,
                capture_output=True,
                check=True,
            )

            self.assertIn(f"wasm.chrome.path={chrome}", identity.stdout)
            self.assertIn(f"wasm.chromedriver.path={driver}", identity.stdout)
            webdriver_hash = hashlib.sha256(webdriver.read_bytes()).hexdigest()
            self.assertIn(
                f"wasm.webdriver.identity=sha256:{webdriver_hash}", identity.stdout
            )
            self.assertFalse((root / "target").exists())
            source = script.read_text(encoding="utf-8")
            self.assertIn(
                'if [[ "$mode" == "prepare" && $downloaded_driver -eq 1', source
            )
            self.assertIn("wasm.webdriver.identity=sha256:", source)
            self.assertIn('validate_prepared_identity "$prepared_identity"', source)
            self.assertIn('export PATH="$(dirname "$runner_bin"):$PATH"', source)

            prepared = root / "prepared.env"
            subprocess.run(
                [str(script), "--prepare", str(prepared)],
                env=env,
                text=True,
                capture_output=True,
                check=True,
            )
            prepared_text = prepared.read_text(encoding="utf-8")
            self.assertIn(f"export CHROME={chrome}", prepared_text)
            self.assertIn(f"export CHROMEDRIVER={driver}", prepared_text)
            self.assertTrue(pathlib.Path(f"{prepared}.identity").is_file())

            (root / "Cargo.lock").write_text(
                'name = "wasm-bindgen"\nversion = "0.2.119"\n',
                encoding="utf-8",
            )
            chrome.write_text(
                "#!/usr/bin/env bash\necho 'Google Chrome 151.0.8000.1'\n",
                encoding="utf-8",
            )
            driver.write_text(
                "#!/usr/bin/env bash\necho 'ChromeDriver 151.0.8000.2'\n",
                encoding="utf-8",
            )
            runner.write_text(
                "#!/usr/bin/env bash\necho 'wasm-bindgen-test-runner 0.2.119'\n",
                encoding="utf-8",
            )
            updated = root / "updated.env"
            subprocess.run(
                [str(script), "--prepare", str(updated)],
                env=env,
                text=True,
                capture_output=True,
                check=True,
            )
            updated_identity = pathlib.Path(f"{updated}.identity").read_text(
                encoding="utf-8"
            )
            self.assertIn("wasm.chrome.version=151.0.8000.1", updated_identity)
            self.assertIn("wasm.bindgen.required=0.2.119", updated_identity)

    def test_invalid_or_mismatched_explicit_overrides_fail(self) -> None:
        with tempfile.TemporaryDirectory() as temp_name:
            root = pathlib.Path(temp_name) / "repo"
            tools = root / "tools"
            binaries = root / "bin"
            tools.mkdir(parents=True)
            binaries.mkdir()
            script = tools / "run-wasm-tests.sh"
            shutil.copy2(REPO_ROOT / "tools" / "run-wasm-tests.sh", script)
            (root / "Cargo.lock").write_text(
                'name = "wasm-bindgen"\nversion = "0.2.118"\n',
                encoding="utf-8",
            )
            chrome = binaries / "chrome"
            driver = binaries / "chromedriver"
            runner = binaries / "wasm-bindgen-test-runner"
            chrome.write_text(
                "#!/usr/bin/env bash\necho 'Google Chrome 150.0.7871.102'\n",
                encoding="utf-8",
            )
            driver.write_text(
                "#!/usr/bin/env bash\necho 'ChromeDriver 149.0.7827.155'\n",
                encoding="utf-8",
            )
            runner.write_text(
                "#!/usr/bin/env bash\necho 'wasm-bindgen-test-runner 0.2.118'\n",
                encoding="utf-8",
            )
            for binary in (chrome, driver, runner):
                binary.chmod(0o755)
            env = os.environ.copy()
            env.update(
                {
                    "CHROME": str(chrome),
                    "CHROMEDRIVER": str(driver),
                    "WASM_BINDGEN_TEST_RUNNER": str(runner),
                }
            )

            mismatch = subprocess.run(
                [str(script), "--identity"],
                env=env,
                text=True,
                capture_output=True,
                check=False,
            )
            self.assertNotEqual(mismatch.returncode, 0)
            self.assertIn("different major versions", mismatch.stderr)

            env["CHROME"] = str(binaries / "missing")
            missing = subprocess.run(
                [str(script), "--identity"],
                env=env,
                text=True,
                capture_output=True,
                check=False,
            )
            self.assertNotEqual(missing.returncode, 0)
            self.assertIn("CHROME is not executable", missing.stderr)

            custom_runner = binaries / "custom-runner"
            custom_runner.write_text(
                runner.read_text(encoding="utf-8"), encoding="utf-8"
            )
            custom_runner.chmod(0o755)
            driver.write_text(
                "#!/usr/bin/env bash\necho 'ChromeDriver 150.0.7871.115'\n",
                encoding="utf-8",
            )
            driver.chmod(0o755)
            env["CHROME"] = str(chrome)
            env["WASM_BINDGEN_TEST_RUNNER"] = str(custom_runner)
            wrong_name = subprocess.run(
                [str(script), "--identity"],
                env=env,
                text=True,
                capture_output=True,
                check=False,
            )
            self.assertNotEqual(wrong_name.returncode, 0)
            self.assertIn(
                "must be named wasm-bindgen-test-runner", wrong_name.stderr
            )

    def test_identity_never_signs_an_unusable_explicit_driver(self) -> None:
        with tempfile.TemporaryDirectory() as temp_name:
            root = pathlib.Path(temp_name) / "repo"
            tools = root / "tools"
            binaries = root / "bin"
            tools.mkdir(parents=True)
            binaries.mkdir()
            script = tools / "run-wasm-tests.sh"
            shutil.copy2(REPO_ROOT / "tools" / "run-wasm-tests.sh", script)
            (root / "Cargo.lock").write_text(
                'name = "wasm-bindgen"\nversion = "0.2.118"\n',
                encoding="utf-8",
            )

            marker = root / "codesigned"
            chrome = binaries / "chrome"
            driver = binaries / "chromedriver"
            runner = binaries / "wasm-bindgen-test-runner"
            chrome.write_text(
                "#!/usr/bin/env bash\necho 'Google Chrome 150.0.7871.102'\n",
                encoding="utf-8",
            )
            driver.write_text(
                "#!/usr/bin/env bash\n"
                '[[ -e "$SIGN_MARKER" ]] || exit 9\n'
                "echo 'ChromeDriver 150.0.7871.115'\n",
                encoding="utf-8",
            )
            runner.write_text(
                "#!/usr/bin/env bash\necho 'wasm-bindgen-test-runner 0.2.118'\n",
                encoding="utf-8",
            )
            (binaries / "uname").write_text(
                "#!/usr/bin/env bash\n"
                "case \"${1:-}\" in\n"
                "  -s) echo Darwin ;;\n"
                "  -m) echo x86_64 ;;\n"
                "  *) echo Darwin ;;\n"
                "esac\n",
                encoding="utf-8",
            )
            (binaries / "codesign").write_text(
                '#!/usr/bin/env bash\ntouch "$SIGN_MARKER"\n', encoding="utf-8"
            )
            for binary in (
                chrome,
                driver,
                runner,
                binaries / "uname",
                binaries / "codesign",
            ):
                binary.chmod(0o755)

            env = os.environ.copy()
            env.update(
                {
                    "PATH": f"{binaries}:{env['PATH']}",
                    "CHROME": str(chrome),
                    "CHROMEDRIVER": str(driver),
                    "WASM_BINDGEN_TEST_RUNNER": str(runner),
                    "SIGN_MARKER": str(marker),
                }
            )
            identity = subprocess.run(
                [str(script), "--identity"],
                env=env,
                text=True,
                capture_output=True,
                check=False,
            )

            self.assertNotEqual(identity.returncode, 0)
            self.assertIn("run preparation to provision", identity.stderr)
            self.assertFalse(marker.exists())


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

    def test_check_source_keeps_portable_timing_and_contract_guards(self) -> None:
        source = (REPO_ROOT / "dev.sh").read_text(encoding="utf-8")

        self.assertIn("LC_ALL=C /usr/bin/time", source)
        self.assertIn("GNU [Tt]ime", source)
        self.assertIn("Resource timing parser failure", source)
        self.assertIn("unset DEV_CHECK_CONTRACT_CHILD", source)
        self.assertNotIn('if [[ "${DEV_CHECK_CONTRACT_CHILD', source)
        self.assertIn(
            'run_stage "dev check contract tests" 1 python3 tools/test_dev_check.py',
            source,
        )


if __name__ == "__main__":
    unittest.main()
