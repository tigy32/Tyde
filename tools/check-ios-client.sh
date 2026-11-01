#!/usr/bin/env bash
# Native acceptance and private late-offer delivery, invoked through ./dev.sh check.
set -euo pipefail
umask 077
[[ -z "${TYDE_IOS_NATIVE_INPUT_CAPTURE+x}" && -z "${TEST_RUNNER_TYDE_IOS_NATIVE_INPUT_CAPTURE+x}" ]] || {
    echo 'The debugger input-capture path was retired; use canonical input diagnostics without private hooks' >&2
    exit 2
}
[[ -z "${TYDE_IOS_ANIMATION_DIAGNOSTICS+x}" && -z "${TEST_RUNNER_TYDE_IOS_ANIMATION_DIAGNOSTICS+x}" ]] || {
    echo 'Use --ios-animation-diagnostics explicitly; ordinary acceptance must not stop after input' >&2
    exit 2
}
[[ -z "${TYDE_IOS_ANIMATION_CONTROL+x}" && -z "${TEST_RUNNER_TYDE_IOS_ANIMATION_CONTROL+x}" ]] || {
    echo 'Use --ios-animation-control explicitly; ordinary acceptance must not load a control document' >&2
    exit 2
}
cd "$(dirname "$0")/.."
if [[ "${1:-}" == --offer ]]; then
    [[ $# -eq 3 ]] || { echo 'Use --offer <ready-fifo> <owner-only-handoff.json>' >&2; exit 2; }
    python3 - "$2" "$3" <<'OFFER'
import json
import os
from pathlib import Path
import stat
import sys
from urllib.parse import urlsplit

fifo, handoff = map(Path, sys.argv[1:])
for path in (fifo, handoff):
    info = path.lstat()
    if info.st_uid != os.getuid() or stat.S_IMODE(info.st_mode) != 0o600:
        raise SystemExit("Native pairing input must be owner-only0600")
if not stat.S_ISFIFO(fifo.lstat().st_mode) or not stat.S_ISREG(handoff.lstat().st_mode):
    raise SystemExit("Native delivery requires the live ready FIFO and a regular private handoff")
try:
    payload = json.loads(handoff.read_text())
    offer = payload["pairing_link"]
    if not isinstance(offer, str):
        raise ValueError()
    url = urlsplit(offer)
    if url.scheme != "https" or not url.hostname or not url.fragment or url.username or url.password:
        raise ValueError()
    if "expires_at_ms" in payload and (type(payload["expires_at_ms"]) is not int or payload["expires_at_ms"] <= 0):
        raise ValueError()
except (KeyError, TypeError, ValueError):
    raise SystemExit("Invalid private native pairing handoff") from None
# Opening the FIFO synchronizes with the live driver's reader, not a delay.
with fifo.open("w") as stream:
    json.dump(payload, stream)
print("Native pairing handoff delivered")
OFFER
    exit 0
fi
[[ "$(uname -s)" == Darwin ]] || { echo 'Native simulator validation requires a Mac' >&2; exit 1; }
mode=test
pairing=false
if [[ "${1:-}" == --build-only && $# -eq 1 ]]; then
    mode=build
    destination="generic/platform=iOS Simulator"
    set --
elif [[ "${1:-}" == --platform-only && $# -eq 2 && "$2" =~ ^[A-Fa-f0-9-]{36}$ ]]; then
    destination="platform=iOS Simulator,id=$2"
    set -- -only-testing:TydeUITests/NativeNavigationTests/testNativeSecureStorage
elif [[ "${1:-}" == --pairing-only && $# -eq 2 && "$2" =~ ^[A-Fa-f0-9-]{36}$ ]]; then
    destination="platform=iOS Simulator,id=$2"
    pairing=true
    set -- -only-testing:TydeUITests/NativeNavigationTests/testNormalManualPairing
elif [[ "${1:-}" == --paste-pairing-only && $# -eq 2 && "$2" =~ ^[A-Fa-f0-9-]{36}$ ]]; then
    destination="platform=iOS Simulator,id=$2"
    pairing=true
    set -- -only-testing:TydeUITests/NativeNavigationTests/testNormalSystemPastePairing
elif [[ "${1:-}" == --animation-diagnostics && $# -eq 2 && "$2" =~ ^[A-Fa-f0-9-]{36}$ ]]; then
    destination="platform=iOS Simulator,id=$2"
    pairing=true
    export TEST_RUNNER_TYDE_IOS_ANIMATION_DIAGNOSTICS=1
    set -- -only-testing:TydeUITests/NativeNavigationTests/testNormalManualPairing
elif [[ "${1:-}" == --animation-control && $# -eq 2 && "$2" =~ ^[A-Fa-f0-9-]{36}$ ]]; then
    destination="platform=iOS Simulator,id=$2"
    pairing=true
    export TEST_RUNNER_TYDE_IOS_ANIMATION_CONTROL=1
    set -- -only-testing:TydeUITests/NativeNavigationTests/testNormalManualPairing
elif [[ "${1:-}" == --hosts-diagnostics && $# -eq 2 && "$2" =~ ^[A-Fa-f0-9-]{36}$ ]]; then
    destination="platform=iOS Simulator,id=$2"
    set -- -only-testing:TydeUITests/NativeNavigationTests/testCapturePairedHostsAccessibility
elif [[ "${1:-}" == --agents-diagnostics && $# -eq 2 && "$2" =~ ^[A-Fa-f0-9-]{36}$ ]]; then
    destination="platform=iOS Simulator,id=$2"
    set -- -only-testing:TydeUITests/NativeNavigationTests/testCapturePairedAgentsAccessibility
elif [[ "${1:-}" == --project-picker-diagnostics && $# -eq 2 && "$2" =~ ^[A-Fa-f0-9-]{36}$ ]]; then
    destination="platform=iOS Simulator,id=$2"
    set -- -only-testing:TydeUITests/NativeNavigationTests/testCapturePairedProjectPickerAccessibility
elif [[ "${1:-}" == --input-diagnostics && $# -eq 2 && "$2" =~ ^[A-Fa-f0-9-]{36}$ ]]; then
    destination="platform=iOS Simulator,id=$2"
    export TEST_RUNNER_TYDE_IOS_INPUT_DIAGNOSTICS=1
    set -- -only-testing:TydeUITests/NativeNavigationTests/testPairedNativeNavigationRetainsComposer
elif [[ $# -eq 1 && "$1" =~ ^[A-Fa-f0-9-]{36}$ ]]; then
    destination="platform=iOS Simulator,id=$1"
    set -- -only-testing:TydeUITests/NativeNavigationTests/testPairedNativeNavigationRetainsComposer
else
    echo 'An explicit owned simulator UUID or --build-only is required' >&2
    exit 2
fi
[[ -f target/ios-client/Web/native-manifest.json && -f target/ios-client/NativeBridge.swift ]] || {
    echo 'Prepare exact bundled assets with ./dev.sh ios-bundle before native validation' >&2; exit 1;
}
[[ -z "${TYDE_IOS_PAIRING_URL:-}" && -z "${TEST_RUNNER_TYDE_IOS_PAIRING_URL:-}" ]] || {
    echo 'Do not pre-issue a pairing URL; wait for driver READY, then use --ios-offer' >&2; exit 1;
}
python3 tools/ios-source-manifest.py verify
result="$PWD/target/ios-results/$(date -u +%Y%m%dT%H%M%SZ)-$$"
mkdir -p "$result"
echo "Native build/test evidence: $result"
if [[ "$pairing" == true ]]; then
    mkfifo -m 600 "$result/pairing-input"
    export TEST_RUNNER_TYDE_IOS_PAIRING_INPUT="$result/pairing-input"
    trap 'rm -f "$result/pairing-input"' EXIT
fi
xcodebuild -project ios/Tyde.xcodeproj -scheme Tyde -configuration Debug \
    -destination "$destination" \
    -derivedDataPath "$PWD/target/ios-derived-data" -resultBundlePath "$result/Native.xcresult" \
    -parallel-testing-enabled NO -test-timeouts-enabled YES \
    -default-test-execution-time-allowance 600 -maximum-test-execution-time-allowance 600 \
    CODE_SIGN_IDENTITY=- "$@" "$mode" 2>&1 | tee "$result/xcodebuild.log" || exit 1
python3 tools/ios-source-manifest.py verify
echo "Native simulator $mode passed; evidence: $result"
