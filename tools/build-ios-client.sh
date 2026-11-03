#!/usr/bin/env bash
# Package only the native ABI and bootstrap policy, never a full protocol client.
set -euo pipefail
cd "$(dirname "$0")/.."
output="$PWD/target/ios-client"
mkdir -p "$output"
python3 tools/native-bootstrap-config.py "$@" > "$output/bootstrap-configuration.json"
python3 tools/check_ios_app_icon.py
python3 tools/ios-source-manifest.py write
cargo run --quiet -p mobile-shell-types --bin native-bindings > "$output/NativeBridge.swift"
cargo run --quiet -p mobile-shell-types --bin native-bindings -- --metadata > "$output/build-metadata.json"
cargo run --quiet -p mobile-shell-types --bin native-bindings -- --contract > "$output/NativeBridgeContract.json"
python3 - "$output" <<'METADATA'
import hashlib
import json
from pathlib import Path
import shutil
import sys
root = Path(sys.argv[1])
web = root / "Web"
if web.exists():
    shutil.rmtree(web)
web.mkdir()
metadata = json.loads((root / "build-metadata.json").read_text())
config = json.loads((root / "bootstrap-configuration.json").read_text())
metadata["bootstrap_url"] = config["bootstrap_url"]
metadata["configuration"] = config["kind"]
with (root / "NativeBridge.swift").open("a") as swift:
    if config["kind"] == "qa":
        swift.write('\n#if !DEBUG\n#error("QA bootstrap resources require a Debug build")\n#endif\n')
    swift.write('\nlet nativeBootstrapOrigin = ' + json.dumps(config["origin"]) + '\n')
metadata["bridge_sha256"] = hashlib.sha256(Path("ios/Tyde/native-host.js").read_bytes()).hexdigest()
(web / "native-manifest.json").write_text(json.dumps(metadata, indent=2) + "\n")
(root / "Version.xcconfig").write_text("MARKETING_VERSION = " + metadata["release"].split("-")[0] + "\n"
    + "TYDE_NATIVE_BOOTSTRAP_HOST = " + config["domain"] + "\n"
    + "TYDE_NATIVE_DEBUG_BUNDLE_IDENTIFIER = " + config["debug_bundle_identifier"] + "\n"
    + "TYDE_APP_DISPLAY_NAME = " + config["display_name"] + "\n")
print("Native bootstrap ABI: " + str(metadata["bootstrap_version"]) + ", bridge " + str(metadata["bridge_version"]))
METADATA
python3 tools/ios-source-manifest.py verify
