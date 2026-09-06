---
id: cut-stable-release
name: Cut Stable Release
description: Prepare, cut, and verify a Tyde stable release (vX.Y.Z)
coordinator:
  backend: codex
  access_mode: unrestricted
tags: [release, stable]
---

# Cut Stable Release

Cut a Tyde stable release like `v0.9.0`. Stable tags have NO hyphen suffix. Unlike betas, a stable cut publishes directly (draft=false, latest=true, includes the Windows MSI) — there is no `publish` step.

Ask the user for the version tag first if it was not provided. It must match strict semver `vMAJOR.MINOR.PATCH` (e.g. `v0.9.0`).

## Preconditions (fail closed)

1. You must be on branch `main` with a clean tree. Check with `git status --short` and `git branch --show-current`. Stop if dirty or not on `main`.
2. Confirm `core.hooksPath` is `.githooks` (`git config --get core.hooksPath`); if not, run `tools/install-git-hooks.sh` and stop if it cannot be set.
3. Confirm the user explicitly approved this exact version (e.g. `v0.9.0`). Never guess the next version.
4. Confirm `gh auth status` works. Stop if it fails.
5. Confirm the tag does not already exist locally or on origin: `git tag --list vX.Y.Z` and `git ls-remote --tags origin vX.Y.Z`. Stop if it exists.

## Steps

1. Prepare the version bump:
   `./dev.sh release prepare vX.Y.Z --commit`
   - If files already match, prepare reports nothing to commit; continue.
2. Re-verify: clean tree, still on `main`, `python3 tools/check_release_version.py vX.Y.Z` passes.
3. Cut the release (runs `tools/release_check.sh`, pushes `main` then the annotated tag, waits and verifies):
   `./dev.sh release cut vX.Y.Z --confirm`
   - Requires the literal `--confirm` flag. Do not pass `--no-wait` unless the user asked for detached monitoring.
   - On PARTIAL RELEASE errors, stop immediately and report; never delete or recreate remote state.
4. Verify explicitly:
   `./dev.sh release verify vX.Y.Z`
   `./dev.sh release status vX.Y.Z`
   - `verify` checks release assets (DMGs, AppImages, debs, RPMs, checksums, Windows exe + MSI, headless zips), the live `https://tycode.dev/tyde/manifest.json` entry, and downloads every bundle artifact with SRI + content-type checks.
5. If the workflow run is still going, use `./dev.sh release wait vX.Y.Z --timeout 90m` rather than polling in a loop.

## Notes

- Never tag off a feature branch or detached HEAD; releases cut only off `main`.
- Never run underlying Cargo/clippy/nextest/wasm commands directly; `cut` owns validation.
- Do NOT run `./dev.sh release publish` for stable tags — it is beta-only and will refuse.
- Per AGENTS.md: after landing any release-bump commit, clean `main` must pass `./dev.sh check` (the cut already runs the release guard; report its result).
