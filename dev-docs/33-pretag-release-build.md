# Pre-tag release build validation

`.github/workflows/pretag-release-build.yml` is the manual, non-publishing
five-platform release build gate. Use it after the candidate commit is pushed
but before creating a release commit or tag. GitHub can only dispatch a workflow
definition present on the selected workflow ref, so the workflow itself must
already be on `main`; the tool deliberately dispatches that trusted definition.

The source under test must be reachable from GitHub: push `main`, a temporary
branch, or a commit reachable from a pushed ref. `source_ref` may be that branch,
tag, or full SHA. The checkout step resolves it to a commit and rejects any
checked-out `HEAD` mismatch. It does not require a version tag.

Dispatch and boundedly monitor it with the supported release tooling:

```sh
./dev.sh release pretag dispatch <source_ref-or-SHA> --confirm
./dev.sh release pretag status <run-id>
./dev.sh release pretag wait <run-id> --timeout 5400 --interval 30
```

`dispatch` has a bounded 60-second run-discovery window and prints the run ID
and URL. `status` returns 0 for success, 1 for failure, and 4 while running.
`wait` delegates monitoring to `gh run watch`, bounds the whole watch to the
requested seconds, and returns 4 on timeout. All commands verify that the run is
the `Pre-tag release build` workflow rather than accepting an arbitrary run ID.

The matrix runs the release headless build and shared startup smoke plus a
Tauri `--no-bundle` native build on macOS ARM, macOS Intel, Linux x86_64,
Linux ARM64, and Windows MSVC. It has only `contents: read`, does not persist
checkout credentials, and has no secrets, signing, packaging, artifact upload,
release, deploy, or mobile steps. Inputs are passed as action arguments or
environment values rather than interpolated into a shell program.

The vendored WebRTC build keeps its portability behavior in the shared Cargo
build script rather than in workflow-only environment overrides. GNU thin
Abseil archives are materialized into self-contained staged archives before
Rust links them, and short target-local Meson directory components keep the
longest known MSVC object path below `MAX_PATH`. The pre-tag and release
workflows therefore exercise the same archive and path contracts without a
separate Windows `CARGO_TARGET_DIR`.

The workflow creates only Actions logs. It creates no artifacts or release
state, and its Rust cache is restore-only (`save-if: false`), so there is no
release cleanup. If a temporary remote branch supplied
`source_ref`, delete that branch after the run (and only with separate approval):

```sh
git push origin --delete <temporary-branch>
```
