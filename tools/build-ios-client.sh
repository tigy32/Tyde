#!/usr/bin/env bash
# Package reviewed local executables, never the PWA release-switch loader.
set -euo pipefail
cd "$(dirname "$0")/.."
output="$PWD/target/ios-client"
mkdir -p "$output"
python3 tools/check_ios_app_icon.py
python3 tools/ios-source-manifest.py write
cargo run --quiet -p mobile-shell-types --bin native-bindings > "$output/NativeBridge.swift"
cargo run --quiet -p mobile-shell-types --bin native-bindings -- --metadata > "$output/build-metadata.json"
(cd mobile-frontend && env -u NO_COLOR trunk build --release --public-url / --dist "$output/Web" index.html)
python3 - "$output" <<'PY'
import base64
import hashlib
import html as html_module
import json
from pathlib import Path
import re
import sys
root = Path(sys.argv[1])
web = root / "Web"
metadata = json.loads((root / "build-metadata.json").read_text())
html = (web / "index.html").read_text()
html = html.replace("<head>", '<head>\n<meta name="tyde-native-host" content="' + str(metadata["bridge_version"]) + '" />', 1)
(web / "index.html").write_text(html)
if re.search(r'<script[^>]+src=["\']https?://', html):
    raise SystemExit("Native bundles cannot execute downloaded scripts")
asset_origin = "__TYDE_NATIVE_ASSET_ORIGIN__"
metadata["asset_format"] = 1
modules = re.findall(r'<script type="module">(.*?)</script>', html, flags=re.S)
if len(modules) != 1:
    raise SystemExit("Native packaging requires exactly one local module bootstrap")
bootstrap = modules[0]
pattern = r"import init, \* as bindings from '(/[^']+\.js)';\s*const wasm = await init\(\{ module_or_path: '(/[^']+\.wasm)' \}\);"
match = re.search(pattern, bootstrap)
if not match:
    raise SystemExit("Native module bootstrap format changed")
for path in match.groups():
    if ".." in Path(path).parts or not (web / path.lstrip("/")).is_file():
        raise SystemExit("Native bootstrap references a missing bundled executable")
bootstrap = bootstrap[:match.start()] + (
    "import init, * as bindings from '." + match[1] + "';\n"
    "const wasm = await init({ module_or_path: new URL('." + match[2] + "', import.meta.url) });"
) + bootstrap[match.end():]
(web / "native-bootstrap.js").write_text(bootstrap)
html = html.replace('<script type="module">' + modules[0] + '</script>',
                    '<script type="module" src="native-bootstrap.js" crossorigin="anonymous"></script>', 1)
# This preload has different request integrity from wasm-bindgen's fetch.
html, removed = re.subn(r'<link\b(?=[^>]*\bas="fetch")(?=[^>]*\btype="application/wasm")[^>]*>', '', html)
if removed != 1:
    raise SystemExit("Native packaging requires the known Wasm preload")
def asset_tag(match):
    tag = match[0]
    attribute = re.search(r'\b(src|href)=["\']([^"\']+)["\']', tag)
    if not attribute:
        return tag
    name = attribute[2]
    if ":" in name or name.startswith("//") or "?" in name or "#" in name or ".." in Path(name).parts:
        raise SystemExit("Native HTML contains a non-bundled resource URL")
    if not (web / name.lstrip("/")).is_file():
        raise SystemExit("Native HTML references a missing bundle resource")
    tag = tag[:attribute.start(2)] + asset_origin + "/" + name.lstrip("/") + tag[attribute.end(2):]
    if not re.search(r'\bcrossorigin=', tag):
        suffix = '/>' if tag.endswith('/>') else '>'
        tag = tag[:-len(suffix)] + ' crossorigin="anonymous"' + suffix
    return tag
html = re.sub(r'<(?:script|link)\b[^>]*>', asset_tag, html)
worker_path = web / "voice-codec-worker.js"
media_path = web / "voice-media.js"
media = media_path.read_text()
worker = 'worker = new Worker(assetUrl("voice-codec-worker.js"));'
if media.count(worker) != 1 or media.count("(() => {\n") != 1:
    raise SystemExit("Native voice worker packaging format changed")
media = media.replace("(() => {\n", "(() => {\n  const nativeCodecWorker = URL.createObjectURL(new Blob([" + json.dumps(worker_path.read_text()) + "], {type: 'text/javascript'}));\n", 1)
media_path.write_text(media.replace(worker, "worker = new Worker(nativeCodecWorker);", 1))
scripts = re.findall(r"<script\b[^>]*>(.*?)</script>", html, flags=re.S | re.I)
hashes = ["'sha256-" + base64.b64encode(hashlib.sha256(script.encode()).digest()).decode() + "'" for script in scripts if script.strip()]
metadata["csp"] = "; ".join([
    "default-src 'none'", "script-src " + asset_origin + " 'wasm-unsafe-eval' " + " ".join(hashes),
    "style-src " + asset_origin + " 'unsafe-inline'", "img-src " + asset_origin + " https: data: blob:",
    "media-src blob:", "connect-src " + asset_origin + " https: wss:", "worker-src blob:",
    "object-src 'none'", "base-uri 'none'", "form-action 'none'", "frame-src 'none'",
])
html = html.replace("<head>", '<head>\n<meta http-equiv="Content-Security-Policy" content="' + html_module.escape(metadata["csp"], quote=True) + '" />', 1)
(web / "index.html").write_text(html)
metadata["files"] = {
    str(path.relative_to(web)): hashlib.sha256(path.read_bytes()).hexdigest()
    for path in sorted(web.rglob("*")) if path.is_file() and path.name != "native-manifest.json"
}
(web / "native-manifest.json").write_text(json.dumps(metadata, indent=2) + "\n")
(root / "Version.xcconfig").write_text("MARKETING_VERSION = " + metadata["release"].split("-")[0] + "\n")
print("Bundled native client: " + metadata["release"] + ", protocol " + str(metadata["protocol_version"]))
PY

python3 tools/ios-source-manifest.py verify
