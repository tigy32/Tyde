---
id: cut-beta-release
name: Cut Beta Release
description: Prepare, cut, verify, and publish a Tyde beta release (vX.Y.Z-beta.N)
coordinator:
  backend: codex
  access_mode: unrestricted
tags: [release, beta]
---

# Cut Beta Release

Cut a Tyde beta release like `v0.8.20-beta.1`. Beta tags contain a hyphen prerelease suffix. The release workflow builds artifacts, deploys the mobile web bundle, and leaves a verified DRAFT; publishing is a separate beta-only step.

Ask the user for the version tag first if it was not provided. It must match strict semver `vMAJOR.MINOR.PATCH-PRERELEASE` (e.g. `v0.8.20-beta.1`).

## Preconditions (fail closed)

1. You must be on branch `main` with a clean tree. Check with `git status --short` and `git branch --show-current`. Stop if dirty or not on `main`.
2. Confirm `core.hooksPath` is `.githooks` (`git config --get core.hooksPath`); if not, run `tools/install-git-hooks.sh` and stop if it cannot be set.
3. Confirm the user explicitly approved this exact version (e.g. `v0.8.20-beta.1`). Never guess the next beta number.
4. Confirm `gh auth status` works. Stop if it fails.

## Steps

1. Prepare the version bump (updates tracked version files, verifies them):
   `./dev.sh release prepare vX.Y.Z-beta.N --commit`
   - If files already match, prepare reports nothing to commit; continue.
2. Re-verify preconditions: clean tree, still on `main`, `python3 tools/check_release_version.py vX.Y.Z-beta.N` passes.
3. Cut the release (runs the canonical guard `tools/release_check.sh`, pushes `main` then the annotated tag, waits and verifies by default):
   `./dev.sh release cut vX.Y.Z-beta.N --confirm`
   - Requires the literal `--confirm` flag. Do not pass `--no-wait` unless the user asked for detached monitoring.
   - On PARTIAL RELEASE errors, stop immediately and report; never delete or recreate remote state.
4. The cut ends with: `BETA RELEASE IS VERIFIED BUT REMAINS A DRAFT`, plus the publish command. Verify explicitly if needed:
   `./dev.sh release verify vX.Y.Z-beta.N`
   `./dev.sh release status vX.Y.Z-beta.N`
5. Publish the beta (beta-only, requires `--confirm`; re-verifies, flips draft=false with prerelease=true and latest=false):
   `./dev.sh release publish vX.Y.Z-beta.N --confirm`
6. Final check: `./dev.sh release verify vX.Y.Z-beta.N` passes and `gh release view vX.Y.Z-beta.N --json tagName,isDraft,isPrerelease` shows `isDraft=false`, `isPrerelease=true`.

## Notes

- Never tag off a feature branch or detached HEAD; releases cut only off `main`.
- Never run underlying Cargo/clippy/nextest/wasm commands directly; `./dev.sh release cut` owns validation via `./dev.sh check` plus `tools/release_check.sh`.
- `publish` refuses stable tags; it is beta-only.
- If the workflow run is still going, use `./dev.sh release wait vX.Y.Z-beta.N --timeout 90m` or `status` rather than polling in a loop.
