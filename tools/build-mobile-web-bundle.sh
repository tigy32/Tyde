#!/usr/bin/env bash
#
# Assemble the mobile web bundle that tyde-server's direct hosting serves.
#
# Direct hosting serves a directory laid out exactly like the deployed
# tycode.dev /tyde/ prefix, because it is the same loader shell booting the
# same versioned bundle through the same manifest:
#
#   <out>/index.html, sw.js, loader.js, …   the un-versioned loader shell
#   <out>/manifest.json                     the SRI + revocation authority
#   <out>/v<version>/…                      the immutable app bundle
#
# Producing that by hand means running trunk with the right --public-url,
# copying the right subset of web/loader/, and regenerating the manifest so its
# hashes match the bundle. Getting any of the three wrong yields a directory
# that loads and then fails integrity checks on the phone. This script is the
# one supported way to build it.
#
# Usage:
#   tools/build-mobile-web-bundle.sh [--out DIR] [--version X.Y.Z]
#
#   --out DIR       Where to write the bundle. Default: target/mobile-web.
#                   Updated additively; retains prior published assets.
#   --version V     Release version to stamp. Default: the canonical version
#                   from tools/check_release_version.py.
#
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "${SCRIPT_DIR}/.." && pwd)"
readonly SCRIPT_DIR REPO_ROOT
readonly LOADER_DIR="${REPO_ROOT}/web/loader"
readonly PREFIX="tyde"
# Git Bash must leave served URL arguments as URLs while still translating the
# filesystem arguments passed to native Windows programs.
MSYS2_URL_ARG_EXCLUSION="${MSYS2_ARG_CONV_EXCL:+${MSYS2_ARG_CONV_EXCL};}"
MSYS2_URL_ARG_EXCLUSION+="/${PREFIX}"
readonly MSYS2_URL_ARG_EXCLUSION

die() { echo "build-mobile-web-bundle: $*" >&2; exit 1; }
log() { echo "build-mobile-web-bundle: $*" >&2; }

OUT_DIR=""
VERSION=""
while [ $# -gt 0 ]; do
  case "$1" in
    --out) shift; [ $# -gt 0 ] || die "--out needs a directory"; OUT_DIR="$1" ;;
    --version) shift; [ $# -gt 0 ] || die "--version needs a version"; VERSION="$1" ;;
    -h|--help) sed -n '2,26p' "${BASH_SOURCE[0]}" | sed 's/^# \{0,1\}//'; exit 0 ;;
    *) die "unexpected argument: $1" ;;
  esac
  shift
done

[ -n "${OUT_DIR}" ] || OUT_DIR="${REPO_ROOT}/target/mobile-web"

command -v trunk >/dev/null 2>&1 || die "trunk not found (cargo install trunk)"
command -v node >/dev/null 2>&1 || die "node required to generate the manifest"
# Windows runners ship `python`, not `python3`, and the release matrix builds
# there too.
if command -v python3 >/dev/null 2>&1; then
  PYTHON=python3
elif command -v python >/dev/null 2>&1; then
  PYTHON=python
else
  die "python3 (or python) required to resolve and validate the version"
fi
readonly PYTHON

if [ -z "${VERSION}" ]; then
  VERSION="$("${PYTHON}" "${REPO_ROOT}/tools/check_release_version.py")" \
    || die "tools/check_release_version.py failed (versions inconsistent?)"
fi
VERSION="${VERSION#v}"
# Mirror of web/deploy/deploy.sh: the version becomes a served path segment, so
# anything with a separator or space in it must never reach the layout.
case "${VERSION}" in
  *[/\\]*|*[[:space:]]*|"") die "invalid release version '${VERSION}'" ;;
esac
echo "${VERSION}" | grep -Eq '^[0-9]+\.[0-9]+\.[0-9]+(-[0-9A-Za-z-]+(\.[0-9A-Za-z-]+)*)?$' \
  || die "invalid release version '${VERSION}' (must be major.minor.patch[-prerelease])"

# Refuse unsafe output roots. Publication is additive: retained documents may
# still import their old SDK or legacy root modules.
case "${OUT_DIR}" in
  ""|"/"|"."|"..") die "refusing to build into '${OUT_DIR}'" ;;
esac
mkdir -p "${OUT_DIR}"
OUT_DIR="$(cd "${OUT_DIR}" && pwd)"
[ "${OUT_DIR}" != "/" ] || die "refusing to build into /"
[ "${OUT_DIR}" != "${REPO_ROOT}" ] || die "refusing to build into the repository root"

readonly BUNDLE_DIR="${OUT_DIR}/v${VERSION}"

log "building v${VERSION} into ${OUT_DIR}"


# Refuse drift between the typed shell contract and its generated loader consumer.
native_contract="$(mktemp)"
trap 'rm -f "$native_contract"' EXIT
cargo run --quiet -p mobile-shell-types --bin native-bindings -- --javascript > "$native_contract"
cmp "$native_contract" "$LOADER_DIR/native-contract.js" || die "generated native loader contract is stale"
"$PYTHON" - "$native_contract" "$REPO_ROOT/mobile-frontend/index.html" <<'NATIVE_ABI'
from pathlib import Path
import re
import sys
native = re.search(r'nativeBridgeVersion = ([0-9]+);', Path(sys.argv[1]).read_text())
client = re.search(r'name="tyde-native-bridge" content="([0-9]+)"', Path(sys.argv[2]).read_text())
if not native or not client or native[1] != client[1]:
    raise SystemExit("Native client manifest and generated shell ABI differ")
NATIVE_ABI

# 1. The immutable app bundle. --public-url has to match where it is served
#    from, or the loader's absolute asset URLs miss.
(
  cd "${REPO_ROOT}/mobile-frontend"
  env -u NO_COLOR MSYS2_ARG_CONV_EXCL="${MSYS2_URL_ARG_EXCLUSION}" \
    trunk build --release \
    --public-url "/${PREFIX}/v${VERSION}/" \
    --dist "${BUNDLE_DIR}" \
    "${REPO_ROOT}/mobile-frontend/index.html"
) || die "trunk build failed"

# 2. The canonical generation-bound shell, identical to the static publisher.
log "assembling SDK generation"
node "${REPO_ROOT}/web/deploy/assemble-shell.mjs" "${LOADER_DIR}" "${OUT_DIR}" \
  || die "SDK shell assembly failed"

# 3. The manifest, with real sha384 SRI over what was just built. This is the
#    authority the loader checks every executable artifact against, so it must
#    be generated from this bundle and not copied from the checkout.
log "generating manifest"
MSYS2_ARG_CONV_EXCL="${MSYS2_URL_ARG_EXCLUSION}" \
node "${REPO_ROOT}/web/deploy/generate-manifest.mjs" \
  --dist "${BUNDLE_DIR}" \
  --version "${VERSION}" \
  --manifest "${LOADER_DIR}/manifest.json" \
  --out "${OUT_DIR}/manifest.json" \
  --prefix "/${PREFIX}" \
  --protocol-source "${REPO_ROOT}/protocol/src/types.rs" \
  || die "manifest generation failed"

# The generator merges into the checked-in manifest, which lists every version
# ever published to tycode.dev. This host serves exactly one, and every other
# entry is a path that 404s here — including, if this is an older release than
# the checkout's manifest knows about, one the loader would prefer to boot.
log "pruning the manifest to v${VERSION}"
"${PYTHON}" - "${OUT_DIR}/manifest.json" "${VERSION}" <<'PRUNE' || die "manifest pruning failed"
import json
import sys

path, version = sys.argv[1], sys.argv[2]
with open(path, encoding="utf-8") as handle:
    manifest = json.load(handle)
versions = manifest.get("versions", {})
if version not in versions:
    raise SystemExit(f"generated manifest has no entry for {version}")
manifest["versions"] = {version: versions[version]}
manifest["minSupported"] = version
manifest["servedClientRelease"] = version
with open(path, "w", encoding="utf-8") as handle:
    json.dump(manifest, handle, indent=2)
    handle.write("\n")
PRUNE

"${PYTHON}" "${REPO_ROOT}/tools/check_mobile_web_manifest.py" \
  --direct-hosted \
  --manifest "${OUT_DIR}/manifest.json" \
  --protocol-source "${REPO_ROOT}/protocol/src/types.rs" \
  "${VERSION}" \
  || die "generated manifest failed release/protocol validation"

[ -f "${OUT_DIR}/index.html" ] || die "assembled bundle has no index.html"

log "bundle ready: ${OUT_DIR}"
echo "${OUT_DIR}"
