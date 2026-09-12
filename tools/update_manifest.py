"""Package signed desktop updates and assemble a complete release manifest."""
from __future__ import annotations

import argparse
import base64
import json
import os
from pathlib import Path
import shutil
import subprocess
from urllib.parse import quote

from set_release_version import normalize_tag

TARGETS = {
    "aarch64-apple-darwin": ("darwin-aarch64", {"app": "macos/*.app.tar.gz"}),
    "x86_64-apple-darwin": ("darwin-x86_64", {"app": "macos/*.app.tar.gz"}),
    "x86_64-unknown-linux-gnu": ("linux-x86_64", {"appimage": "appimage/*.AppImage", "deb": "deb/*.deb", "rpm": "rpm/*.rpm"}),
    "aarch64-unknown-linux-gnu": ("linux-aarch64", {"appimage": "appimage/*.AppImage", "deb": "deb/*.deb", "rpm": "rpm/*.rpm"}),
    "x86_64-pc-windows-msvc": ("windows-x86_64", {"nsis": "nsis/*-setup.exe"}),
}
EXTENSIONS = {"app": "app.tar.gz", "appimage": "AppImage", "deb": "deb", "rpm": "rpm", "nsis": "exe", "msi": "msi"}


def expected_assets(tag: str) -> set[str]:
    version = normalize_tag(tag)
    names = {"tyde-update.json"}
    for target, (base, patterns) in TARGETS.items():
        installers = set(patterns)
        if base.startswith("windows") and "-" not in version:
            installers.add("msi")
        for installer in installers:
            name = f"tyde-update-{version}-{target}.{EXTENSIONS[installer]}"
            names.update((name, name + ".sig"))
    return names


def package(args: argparse.Namespace) -> None:
    version = normalize_tag(args.tag)
    base, patterns = TARGETS[args.target]
    patterns = dict(patterns)
    if base.startswith("windows") and "-" not in version:
        patterns["msi"] = "msi/*.msi"
    args.output.mkdir(parents=True, exist_ok=True)
    platforms = {}
    npx = shutil.which("npx")
    if not npx:
        raise RuntimeError("npx is required to sign update artifacts")
    if not os.environ.get("TAURI_SIGNING_PRIVATE_KEY"):
        raise RuntimeError("TAURI_SIGNING_PRIVATE_KEY is required to sign update artifacts")
    for installer, pattern in patterns.items():
        matches = list(args.bundle.glob(pattern))
        if len(matches) != 1:
            raise RuntimeError(f"Expected one {installer} artifact for {args.target}, found {matches}")
        artifact = args.output / f"tyde-update-{version}-{args.target}.{EXTENSIONS[installer]}"
        shutil.copyfile(matches[0], artifact)
        subprocess.run([npx, "--no-install", "tauri", "signer", "sign", str(artifact)], check=True)
        signature = artifact.with_name(artifact.name + ".sig").read_text().strip()
        config_path = Path(__file__).resolve().parents[1] / "frontend/tauri-shell/tauri.conf.json"
        public_key = json.loads(config_path.read_text())["plugins"]["updater"]["pubkey"]
        key_bytes = base64.b64decode(base64.b64decode(public_key).splitlines()[1], validate=True)
        signature_bytes = base64.b64decode(base64.b64decode(signature).splitlines()[1], validate=True)
        # Catch an incorrectly configured CI key before publishing unusable updates.
        # The installed application independently verifies the full signature.
        if len(key_bytes) != 42 or len(signature_bytes) != 74 or key_bytes[2:10] != signature_bytes[2:10]:
            raise RuntimeError("Updater signing key does not match the application's pinned public key")
        entry = {"url": f"https://github.com/{args.repository}/releases/download/{args.tag}/{quote(artifact.name)}", "signature": signature}
        platforms[f"{base}-{installer}"] = entry
        if installer in {"app", "appimage", "nsis"}:
            platforms[base] = entry
    fragment = args.output / f"update-{args.target}.json"
    fragment.write_text(json.dumps({"version": version, "platforms": platforms}, indent=2) + "\n")


def assemble(args: argparse.Namespace) -> None:
    version = normalize_tag(args.tag)
    platforms = {}
    for target in TARGETS:
        path = args.fragments / f"update-{target}.json"
        fragment = json.loads(path.read_text())
        if fragment["version"] != version:
            raise RuntimeError(f"Update fragment {path} belongs to another version")
        base, patterns = TARGETS[target]
        required = {base, *(f"{base}-{installer}" for installer in patterns)}
        if base.startswith("windows") and "-" not in version:
            required.add(f"{base}-msi")
        if set(fragment["platforms"]) != required:
            raise RuntimeError(f"Incomplete or unexpected platforms in {path}")
        for platform, entry in fragment["platforms"].items():
            prefix = f"https://github.com/{args.repository}/releases/download/{args.tag}/"
            if not entry["url"].startswith(prefix) or not entry["signature"].strip():
                raise RuntimeError(f"Invalid signed update entry for {platform}")
            if platform in platforms:
                raise RuntimeError(f"Duplicate update platform: {platform}")
            platforms[platform] = entry
    args.output.write_text(json.dumps({"version": version, "platforms": platforms}, indent=2) + "\n")


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__)
    sub = parser.add_subparsers(dest="command", required=True)
    for name in ("package", "assemble"):
        command = sub.add_parser(name)
        command.add_argument("--tag", required=True)
        command.add_argument("--repository", default="tigy32/Tyde")
        command.add_argument("--output", type=Path, required=True)
        if name == "package":
            command.add_argument("--target", choices=TARGETS, required=True)
            command.add_argument("--bundle", type=Path, required=True)
        else:
            command.add_argument("--fragments", type=Path, required=True)
    args = parser.parse_args()
    if args.command == "package":
        package(args)
    else:
        assemble(args)


if __name__ == "__main__":
    main()
