#!/usr/bin/env bash
#
# Tyde web/PWA deploy (Phase 6).
#
# Publishes the loader shell (web/loader/ -> /tyde/) and the immutable,
# per-version app bundle (mobile-frontend/dist -> /tyde/v<version>/) to the
# CloudFront-fronted S3 bucket that already serves the tycode.dev marketing
# site, then invalidates only the /tyde/* path.
#
#   tycode.dev  -> CloudFront E3JJ1OF4I8TP6U -> S3 bucket tycode-static
#   our prefix  -> s3://tycode-static/tyde/   (additive; marketing keys at root
#                  are NEVER touched: no --delete, scoped to tyde/ only)
#
# GUARDRAILS (enforced below):
#   * DRY-RUN BY DEFAULT. The real deploy requires an explicit --confirm.
#   * Every S3 destination is built from BUCKET + PREFIX and asserted to live
#     under s3://tycode-static/tyde/. Nothing else can be written.
#   * --delete is NEVER passed to AWS S3 writes and is REFUSED as an input arg.
#   * The version is validated with the host's release-version rules and (when
#     it matches the repo) cross-checked against tools/check_release_version.py.
#   * The marketing site, the tycode.dev bucket beyond tyde/, and tyggs.* are
#     out of scope and untouched.
#
# Usage:
#   web/deploy/deploy.sh [VERSION] [--confirm] [--dist DIR]
#                         [--source-root DIR] [--live-manifest-base]
#
#   VERSION     Release version (e.g. 0.8.19-beta.2). Default: the canonical
#               version printed by tools/check_release_version.py.
#   --confirm   Perform the REAL deploy (build + upload + invalidate). Without
#               it the script runs AWS S3 dry-run upload/sync commands and
#               SKIPS the CloudFront invalidation so you can preview exactly
#               what would be written (all under tyde/, no deletes).
#   --dist DIR  Built Trunk output to publish as the versioned bundle.
#               Default: <source-root>/mobile-frontend/dist.
#   --source-root DIR
#               Source checkout to build/verify. Defaults to the current repo.
#               CI backfills use this to build an exact release tag while running
#               the latest deploy tooling from the default branch.
#   --live-manifest-base
#               Fetch s3://tycode-static/tyde/manifest.json first and merge into
#               that live manifest. This is automatic for --confirm; the flag is
#               still useful for dry-run/validation previews.
#
set -euo pipefail

# --- constants (ground truth from AWS recon) -------------------------------
readonly AWS_REGION="us-west-2"
readonly BUCKET="tycode-static"
readonly PREFIX="tyde"                      # additive target prefix (NEVER root)
readonly DISTRIBUTION_ID="E3JJ1OF4I8TP6U"
readonly INVALIDATION_PATHS="/tyde/*"
readonly LOADER_SHELL_CACHE_CONTROL="public, max-age=60, must-revalidate"
readonly MANIFEST_CACHE_CONTROL="no-store, max-age=0, must-revalidate"
readonly -a LOADER_SHELL_KEYS=(
  "index.html"
  "sw.js"
  "loader.js"
  "loader.css"
  "cbor.js"
  "pairing.js"
  "pairing-ui.js"
  "styles.js"
  "manifest-policy.js"
  "integrity.js"
  "vendor/jsqr.js"
  "manifest.webmanifest"
  "icons/icon.svg"
)

# --- locate repo + tooling -------------------------------------------------
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "${SCRIPT_DIR}/../.." && pwd)"
readonly SCRIPT_DIR REPO_ROOT
readonly LOADER_DIR="${REPO_ROOT}/web/loader"
readonly MANIFEST="${LOADER_DIR}/manifest.json"
readonly GENERATOR="${SCRIPT_DIR}/generate-manifest.mjs"

# --- arg parsing -----------------------------------------------------------
CONFIRM=0
VERSION=""
DIST_DIR=""
SOURCE_ROOT="${REPO_ROOT}"
LIVE_MANIFEST_BASE=0
LIVE_MANIFEST=""
LATEST_MANIFEST=""

cleanup() {
  if [ -n "${LIVE_MANIFEST}" ]; then
    rm -f "${LIVE_MANIFEST}"
  fi
  if [ -n "${LATEST_MANIFEST}" ]; then
    rm -f "${LATEST_MANIFEST}"
  fi
}
trap cleanup EXIT

die() { echo "deploy: $*" >&2; exit 1; }

while [ $# -gt 0 ]; do
  case "$1" in
    --confirm) CONFIRM=1 ;;
    --dry-run) CONFIRM=0 ;;                 # explicit default; accepted for clarity
    --dist) shift; [ $# -gt 0 ] || die "--dist needs a directory"; DIST_DIR="$1" ;;
    --source-root) shift; [ $# -gt 0 ] || die "--source-root needs a directory"; SOURCE_ROOT="$1" ;;
    --live-manifest-base) LIVE_MANIFEST_BASE=1 ;;
    --delete)
      die "refusing --delete: this deploy is strictly additive and must never delete keys" ;;
    -h|--help)
      sed -n '2,42p' "${BASH_SOURCE[0]}" | sed 's/^# \{0,1\}//'; exit 0 ;;
    --*) die "unknown flag: $1" ;;
    *)
      [ -z "${VERSION}" ] || die "unexpected extra argument: $1"
      VERSION="$1" ;;
  esac
  shift
done

if [ "${CONFIRM}" -eq 1 ]; then
  LIVE_MANIFEST_BASE=1
fi

# Belt-and-suspenders: refuse a --delete smuggled in via the environment.
case " ${AWS_S3_SYNC_EXTRA_ARGS:-} " in
  *" --delete "*) die "AWS_S3_SYNC_EXTRA_ARGS must not contain --delete" ;;
esac

# Normalize the source checkout after arg parsing so all subsequent checks and
# builds read the release source of truth, not necessarily the deploy-tooling repo.
SOURCE_ROOT="$(cd "${SOURCE_ROOT}" && pwd)" || die "source root not found: ${SOURCE_ROOT}"
if [ -z "${DIST_DIR}" ]; then
  DIST_DIR="${SOURCE_ROOT}/mobile-frontend/dist"
fi

# --- version resolution + validation ---------------------------------------
# Mirror of host-config::validate_release_version / web/loader/pairing.js.
validate_version() {
  local v="$1"
  v="${v#v}"
  [ -n "$v" ] || return 1
  case "$v" in *[/\\]*|*[[:space:]]*) return 1 ;; esac
  # core major.minor.patch, optional [0-9A-Za-z-] dot-separated prerelease
  echo "$v" | grep -Eq '^[0-9]+\.[0-9]+\.[0-9]+(-[0-9A-Za-z-]+(\.[0-9A-Za-z-]+)*)?$'
}

if [ -z "${VERSION}" ]; then
  command -v python3 >/dev/null 2>&1 || die "python3 required to read the canonical version"
  VERSION="$(python3 "${SOURCE_ROOT}/tools/check_release_version.py")" \
    || die "tools/check_release_version.py failed (versions inconsistent?)"
else
  # A version was supplied: assert it matches the repo's canonical version so a
  # typo can't publish a /tyde/vX/ that no host actually advertises.
  if command -v python3 >/dev/null 2>&1; then
    python3 "${SOURCE_ROOT}/tools/check_release_version.py" "${VERSION}" >/dev/null \
      || die "version '${VERSION}' does not match tools/check_release_version.py"
  fi
fi
VERSION="${VERSION#v}"
validate_version "${VERSION}" \
  || die "invalid release version '${VERSION}' (must be major.minor.patch[-prerelease])"

# --- destination guards ----------------------------------------------------
readonly S3_ROOT="s3://${BUCKET}/${PREFIX}/"
readonly S3_LOADER="s3://${BUCKET}/${PREFIX}/"
readonly S3_BUNDLE="s3://${BUCKET}/${PREFIX}/v${VERSION}/"

# Assert every destination lives under s3://tycode-static/tyde/ — a typo in any
# constant (or a version that escaped validation) aborts before any write.
assert_scoped() {
  local dest="$1"
  case "$dest" in
    "s3://${BUCKET}/${PREFIX}/"*) : ;;
    *) die "destination '${dest}' is NOT under ${S3_ROOT} — refusing" ;;
  esac
  case "$dest" in
    *".."*) die "destination '${dest}' contains '..' — refusing" ;;
  esac
}
assert_scoped "${S3_LOADER}"
assert_scoped "${S3_BUNDLE}"

# --- mode banner -----------------------------------------------------------
if [ "${CONFIRM}" -eq 1 ]; then
  MODE="REAL"; DRYFLAG=""
else
  MODE="DRY-RUN"; DRYFLAG="--dryrun"
fi

cat >&2 <<BANNER
deploy: mode=${MODE}
  region:       ${AWS_REGION}
  bucket:       ${BUCKET}
  loader -> ${S3_LOADER}
  bundle -> ${S3_BUNDLE}
  source:       ${SOURCE_ROOT}
  dist:         ${DIST_DIR}
  manifest:     ${MANIFEST}
  distribution: ${DISTRIBUTION_ID}  (invalidate ${INVALIDATION_PATHS})
  live base:    ${LIVE_MANIFEST_BASE}
BANNER
if [ "${CONFIRM}" -ne 1 ]; then
  echo "deploy: DRY-RUN (default). Re-run with --confirm to actually deploy." >&2
fi

# ===========================================================================
# 0. Live-manifest guard: start from the live manifest for every real deploy.
# ===========================================================================
# Release CI/manual deploys may run from a checkout whose checked-in manifest is
# older than production. Fetching the live manifest as the generator's merge base
# preserves newer entries/policy and fails closed if the authority cannot be read
# or parsed.
MANIFEST_INPUT="${MANIFEST}"
if [ "${LIVE_MANIFEST_BASE}" -eq 1 ]; then
  command -v aws >/dev/null 2>&1 || die "aws CLI required for live manifest merge"
  command -v python3 >/dev/null 2>&1 || die "python3 required for live manifest merge"
  LIVE_MANIFEST="$(mktemp)"
  echo "deploy: fetching live manifest base from ${S3_LOADER}manifest.json…" >&2
  aws s3 cp "${S3_LOADER}manifest.json" "${LIVE_MANIFEST}" \
    --region "${AWS_REGION}" \
    || die "failed to fetch live manifest from ${S3_LOADER}manifest.json"
  python3 -m json.tool "${LIVE_MANIFEST}" >/dev/null \
    || die "live manifest is not valid JSON"
  if ! python3 - "${LIVE_MANIFEST}" <<'PY'
import json
import sys

with open(sys.argv[1], "r", encoding="utf-8") as handle:
    manifest = json.load(handle)
if not isinstance(manifest, dict) or not isinstance(manifest.get("versions"), dict):
    raise SystemExit(1)
PY
  then
    die "live manifest root/versions shape is invalid"
  fi
  MANIFEST_INPUT="${LIVE_MANIFEST}"
fi

# ===========================================================================
# 1. Build the versioned bundle.
# ===========================================================================
# Heavy build — only on a real deploy. (In dry-run we cannot hash/sync a bundle
# that wasn't built; that step is skipped with a note, see below.)
if [ "${CONFIRM}" -eq 1 ]; then
  command -v trunk >/dev/null 2>&1 || die "trunk not found (cargo install trunk; wasm-opt recommended)"
  echo "deploy: building mobile-frontend bundle with trunk (release)…" >&2
  ( cd "${SOURCE_ROOT}/mobile-frontend" \
      && trunk build --release \
           --public-url "/${PREFIX}/v${VERSION}/" \
           --dist "${DIST_DIR}" \
           "${SOURCE_ROOT}/mobile-frontend/index.html" ) \
    || die "trunk build failed"
fi

# ===========================================================================
# 2. Generate manifest.json with REAL sha384 SRI for every executable artifact.
# ===========================================================================
# Needs a built dist. On a real deploy it always exists (step 1). On a dry-run
# we regenerate only if a dist happens to be present; otherwise we keep the
# existing manifest and note that bundle hashing was skipped.
HAVE_DIST=0
if [ -d "${DIST_DIR}" ] \
   && find "${DIST_DIR}" -name '.stage' -prune -o \
        -type f \( -name '*.js' -o -name '*.wasm' \) ! -name '._*' -print 2>/dev/null \
        | head -n1 | grep -q .; then
  HAVE_DIST=1
fi

if [ "${CONFIRM}" -eq 1 ] || [ "${HAVE_DIST}" -eq 1 ]; then
  if [ "${HAVE_DIST}" -ne 1 ]; then
    die "dist '${DIST_DIR}' missing after build — cannot generate manifest"
  fi
  command -v node >/dev/null 2>&1 || die "node required to run the manifest generator"
  echo "deploy: generating manifest (real SRI) for v${VERSION}…" >&2
  node "${GENERATOR}" \
    --dist "${DIST_DIR}" \
    --version "${VERSION}" \
    --manifest "${MANIFEST_INPUT}" \
    --out "${MANIFEST}" \
    --prefix "/${PREFIX}" \
    --protocol-source "${SOURCE_ROOT}/protocol/src/types.rs" \
    || die "manifest generation failed"
  python3 "${REPO_ROOT}/tools/check_mobile_web_manifest.py" \
    --manifest "${MANIFEST}" \
    --protocol-source "${SOURCE_ROOT}/protocol/src/types.rs" \
    "${VERSION}" \
    || die "generated manifest failed release/protocol validation"
else
  echo "deploy: [dry-run] no dist at ${DIST_DIR} — skipping manifest regen + bundle sync." >&2
  echo "deploy: [dry-run] (a real deploy builds the bundle first; bundle sync cannot be" >&2
  echo "deploy:           previewed without a build). Loader-shell scoping IS previewed below." >&2
fi

# ===========================================================================
# 3. Publish the immutable versioned bundle -> /tyde/v<version>/
# ===========================================================================
# Immutable: hash-stamped filenames never change, so cache them for a year.
# NEVER --delete (a re-publish of the same version is byte-identical anyway).
if [ "${CONFIRM}" -eq 1 ] || [ "${HAVE_DIST}" -eq 1 ]; then
  echo "deploy: syncing versioned bundle -> ${S3_BUNDLE} (${MODE})…" >&2
  aws s3 sync "${DIST_DIR}/" "${S3_BUNDLE}" \
    --region "${AWS_REGION}" \
    --exclude '.*' \
    --exclude '._*' \
    --exclude '*/._*' \
    --exclude '.stage/*' \
    --cache-control 'public, max-age=31536000, immutable' \
    ${DRYFLAG} \
    ${AWS_S3_SYNC_EXTRA_ARGS:-}

  # `aws s3 sync` guesses Content-Type from the extension and does NOT know
  # `.wasm` — it would upload it as application/octet-stream, breaking
  # `WebAssembly.instantiateStreaming`. Re-stamp the wasm with application/wasm
  # (metadata-only in-place copy, still immutable). Scoped to the version prefix.
  echo "deploy: fixing Content-Type: application/wasm on bundle .wasm (${MODE})…" >&2
  aws s3 cp "${S3_BUNDLE}" "${S3_BUNDLE}" \
    --region "${AWS_REGION}" \
    --recursive \
    --exclude '*' \
	    --include '*.wasm' \
	    --no-guess-mime-type \
	    --content-type 'application/wasm' \
	    --metadata-directive REPLACE \
	    --cache-control 'public, max-age=31536000, immutable' \
	    ${DRYFLAG}

  if [ "${CONFIRM}" -eq 1 ]; then
    echo "deploy: validating uploaded bundle artifacts before publishing manifest…" >&2
    python3 - "${MANIFEST}" "${VERSION}" <<'PY' | while IFS= read -r key; do
import json
import sys

manifest_path, version = sys.argv[1], sys.argv[2]
with open(manifest_path, "r", encoding="utf-8") as handle:
    manifest = json.load(handle)
entry = manifest["versions"][version]
paths = [entry["entry"], *entry.get("artifacts", {}).keys()]
for path in paths:
    print(path.lstrip("/"))
PY
      aws s3api head-object \
        --bucket "${BUCKET}" \
        --key "${key}" \
        --region "${AWS_REGION}" >/dev/null \
        || die "uploaded bundle artifact missing from S3: s3://${BUCKET}/${key}"
      case "${key}" in
        *.wasm)
          content_type="$(aws s3api head-object \
            --bucket "${BUCKET}" \
            --key "${key}" \
            --region "${AWS_REGION}" \
            --query ContentType \
            --output text)"
          [ "${content_type}" = "application/wasm" ] \
            || die "uploaded wasm has wrong Content-Type (${content_type}): s3://${BUCKET}/${key}"
          ;;
      esac
    done
  fi
fi

# ===========================================================================
# 4. Publish the loader shell -> /tyde/  (short/no-cache so logic + revocations
#    propagate). NEVER --delete; scoped to tyde/. Exclude manifest.json: the
#    manifest is uploaded last, after the versioned bundle is verified.
# ===========================================================================
# Short cache for the un-versioned shell (index.html, sw.js, loader .js modules,
# css, webmanifest, icons) so loader fixes + blocked/minSupported revocations go
# live within ~a minute. Use `cp --recursive`, not `sync`: `sync` skips
# unchanged keys and leaves their old S3 metadata/cache headers in place.
echo "deploy: publishing loader shell -> ${S3_LOADER} (${MODE})…" >&2
aws s3 cp "${LOADER_DIR}/" "${S3_LOADER}" \
  --region "${AWS_REGION}" \
  --recursive \
  --exclude 'manifest.json' \
  --exclude 'test/*' \
  --exclude 'node_modules/*' \
  --exclude '*.test.js' \
  --exclude 'package.json' \
  --exclude '*.md' \
  --exclude '.*' \
  --exclude '._*' \
  --exclude '*/._*' \
  --cache-control "${LOADER_SHELL_CACHE_CONTROL}" \
  ${DRYFLAG} \
  ${AWS_S3_SYNC_EXTRA_ARGS:-}

if [ "${CONFIRM}" -eq 1 ]; then
  echo "deploy: validating loader shell cache metadata…" >&2
  for shell_key in "${LOADER_SHELL_KEYS[@]}"; do
    cache_control="$(aws s3api head-object \
      --bucket "${BUCKET}" \
      --key "${PREFIX}/${shell_key}" \
      --region "${AWS_REGION}" \
      --query CacheControl \
      --output text)"
    [ "${cache_control}" = "${LOADER_SHELL_CACHE_CONTROL}" ] \
      || die "loader shell cache metadata drift for s3://${BUCKET}/${PREFIX}/${shell_key}: ${cache_control}"
  done
fi

if [ "${CONFIRM}" -eq 1 ] || [ "${HAVE_DIST}" -eq 1 ]; then
  # Re-fetch the live global manifest immediately before upload and merge just
  # this generated version entry. GitHub also serializes mobile-web manifest
  # writers, but this protects against stale local/CI state and preserves newer
  # live entries.
  if [ "${CONFIRM}" -eq 1 ] && [ "${LIVE_MANIFEST_BASE}" -eq 1 ]; then
    LATEST_MANIFEST="$(mktemp)"
    echo "deploy: re-fetching live manifest before final manifest upload…" >&2
    aws s3 cp "${S3_LOADER}manifest.json" "${LATEST_MANIFEST}" \
      --region "${AWS_REGION}" \
      || die "failed to re-fetch live manifest from ${S3_LOADER}manifest.json"
    python3 "${REPO_ROOT}/tools/merge_mobile_web_manifest.py" \
      "${VERSION}" \
      --base "${LATEST_MANIFEST}" \
      --entry-source "${MANIFEST}" \
      --out "${MANIFEST}" \
      --protocol-source "${SOURCE_ROOT}/protocol/src/types.rs" \
      || die "failed to merge generated entry into latest live manifest"
  fi

  python3 "${REPO_ROOT}/tools/check_mobile_web_manifest.py" \
    --manifest "${MANIFEST}" \
    --protocol-source "${SOURCE_ROOT}/protocol/src/types.rs" \
    "${VERSION}" \
    || die "final manifest failed release/protocol validation"

  # The manifest is the security/revocation authority and is uploaded LAST so the
  # loader never advertises a bundle until its files and wasm metadata are present.
  echo "deploy: publishing manifest.json last (${MODE})…" >&2
  aws s3 cp "${MANIFEST}" "${S3_LOADER}manifest.json" \
    --region "${AWS_REGION}" \
    --content-type 'application/json' \
    --cache-control "${MANIFEST_CACHE_CONTROL}" \
    ${DRYFLAG}
  if [ "${CONFIRM}" -eq 1 ]; then
    manifest_cache_control="$(aws s3api head-object \
      --bucket "${BUCKET}" \
      --key "${PREFIX}/manifest.json" \
      --region "${AWS_REGION}" \
      --query CacheControl \
      --output text)"
    [ "${manifest_cache_control}" = "${MANIFEST_CACHE_CONTROL}" ] \
      || die "manifest cache metadata drift for s3://${BUCKET}/${PREFIX}/manifest.json: ${manifest_cache_control}"
  fi
else
  echo "deploy: [dry-run] SKIPPING manifest upload because no bundle manifest was generated." >&2
fi

# ===========================================================================
# 5. Invalidate CloudFront — only /tyde/*  (REAL deploy only).
# ===========================================================================
if [ "${CONFIRM}" -eq 1 ]; then
  echo "deploy: creating CloudFront invalidation ${INVALIDATION_PATHS} on ${DISTRIBUTION_ID}…" >&2
  aws cloudfront create-invalidation \
    --distribution-id "${DISTRIBUTION_ID}" \
    --paths "${INVALIDATION_PATHS}"
  echo "deploy: DONE. Published v${VERSION} and invalidated ${INVALIDATION_PATHS}." >&2
else
  echo "deploy: [dry-run] SKIPPING CloudFront invalidation (${INVALIDATION_PATHS})." >&2
  echo "deploy: dry-run complete. Verify above that every line is under ${S3_ROOT} with no (delete) ops." >&2
fi
