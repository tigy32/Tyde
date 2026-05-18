# AGENTS.md

Guidance for AI coding agents working in the Tyde2 repository. The conventions
in this file apply to every agent (Claude, Codex, Gemini, etc.) that touches
this codebase.

## How to commit

### 1. Commit message rules

Every commit message must follow these rules:

- Limit the subject line to 50 characters
- Capitalize the subject/description line
- Do not end the subject line with a period
- Separate the subject from the body with a blank line
- Wrap the body at 72 characters
- Use the body to explain *what* and *why* (not *how* — the diff shows that)
- Use the imperative mood in the subject line, as if giving a command —
  e.g. `feat: Add unit tests for user authentication`. Imperative subjects
  read consistently and make the action obvious.
- Do **not** add any AI attribution (no `Co-authored-by: Claude`, no
  `Generated with ...` trailers, no tool-name footers)

### 2. Pre-commit checks

Before creating any commit, you must verify the following are clean:

- `cargo fmt --all` — formatting
- `cargo check --all-targets` — compiles
- `cargo clippy --all-targets -- -D warnings` — no lint violations
- `cargo test` — native tests pass
- `tools/run-wasm-tests.sh` — wasm UI tests pass

For clippy: **fix the underlying issue**. Do not paper over violations with
`_`-prefixed unused names, `#[allow(...)]` attributes, or other suppressions
just to silence the lint. If a suppression is genuinely warranted, justify
it in a comment and ideally raise it with the user first.

### 3. Tests — backend.rs is special

`backend.rs` tests exercise real AI agents (real API calls, real money).

- **Do not** run `backend.rs` tests unless you are changing a backend.
- If you do change a backend, all `backend.rs` tests for the backend you
  touched must pass before committing.
- All other tests must always pass.

### 4. Local commits only

Always commit locally. Do **not** `git push`, open PRs, force-push, or take
any action that affects the remote without explicit user approval. The same
goes for tags, branches on the remote, or anything else that leaves the
local machine.

### 5. Release pushes

Only push a release after the user explicitly approves the release action and
the exact target version, e.g. `vX.Y.Z`. Never force-push a release.

After approval:

1. Confirm the working tree is clean and you are on `main`:
   `git status --short` and `git branch --show-current`. Stop if the tree is
   dirty or the branch is not `main`.
2. Confirm the release commit contains the target version before tagging.
   Bump the tracked release-version files to `X.Y.Z` (including lockfiles
   and consistency files) before creating any tag, then run
   `python3 tools/check_release_version.py vX.Y.Z`. Stop if it fails. Run
   the full pre-commit sequence and commit the bump locally before
   continuing.
3. Confirm the commit to release: `git log -1 --oneline`.
4. Verify the tag does not already exist locally or on `origin`:
   `git tag --list vX.Y.Z` and `git ls-remote --tags origin vX.Y.Z`. Stop if
   it exists unless the user gives explicit further instructions.
5. Run the full pre-commit sequence above. Stop if any check fails.
6. Re-run the release-version check immediately before tagging:
   `python3 tools/check_release_version.py vX.Y.Z`. Stop if it fails.
7. Create the annotated tag:
   `git tag -a vX.Y.Z -m "Release vX.Y.Z"`.
8. Push `main`, then push the tag:
   `git push origin main` and `git push origin vX.Y.Z`.
9. Verify the remote tag exists:
   `git ls-remote --tags origin vX.Y.Z`.

### 6. Commits don't need to be strictly standalone

Commits don't need to be surgically scoped to your own change. Previous
agents sometimes leave the tree in a slightly broken state — unformatted
files, a test that's flaky or outright broken, a clippy lint that slipped
in. When pre-commit checks surface that kind of collateral:

- If `cargo fmt` rewrites whitespace in files another agent forgot to
  format, include those fmt-only hunks in your commit rather than
  reverting them.
- If `cargo test` or clippy fails on code you didn't touch because of a
  previous agent's mistake, debug and fix it as part of your commit. Do
  not skip the check or stash the failure for someone else.

Mention the collateral fix in the commit body so it's discoverable, but
don't split it into a separate commit just for purity.

## Frontend UI tests are inviolate

Component-level wasm tests live inline in their component file under
`#[cfg(all(test, target_arch = "wasm32"))] mod wasm_tests` and run via
`tools/run-wasm-tests.sh`. They mount real Leptos components into a real
DOM in headless Chrome.

If a UI test fails after a code change, you may **not** weaken or delete
the assertion to make the test pass. Either:

1. Fix the code so the original assertion holds again, or
2. If the assertion is genuinely wrong (testing the wrong thing), explain
   why to the user and ask before changing it.

The whole point of these tests is to be a thing the AI can't silently
route around.

## Debugging discipline

When something is broken, **do not guess at the fix**. The expected loop:

1. Add logs / instrumentation to confirm what's actually happening.
2. Identify the root cause from the evidence.
3. Fix the cause, not the symptom.

Do not try a speculative change just to see if it works. Do not remove
logs you added until the user has signed off on the fix.

## Style and scope

- Prefer editing existing files over creating new ones.
- Keep changes scoped to what was asked. Don't bundle drive-by refactors
  into a fix unless explicitly requested.
- Default to writing no comments. Add one only when the *why* is
  non-obvious (a hidden constraint, a workaround for a specific bug, a
  surprising invariant). Don't restate what the code does.
- Match existing patterns in surrounding code.

## Running the app and tests locally

- Native tests: `cargo test` (no `--target` flag)
- Wasm tests: `tools/run-wasm-tests.sh` (filter with
  `tools/run-wasm-tests.sh wasm_tests::`)
- Build: `./build.sh` or per-crate `cargo build`

The wasm test script handles the fiddly setup (matching chromedriver to
installed Chrome via Chrome for Testing, ad-hoc signing on macOS,
installing `wasm-bindgen-cli` at the lockfile-pinned version, caching
under `target/wasm-test-cache/`). It is the same entry point CI uses —
do not bypass it with raw `cargo test --target wasm32-unknown-unknown`
unless you are debugging the script itself.
