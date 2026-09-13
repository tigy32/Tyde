"""Package signed desktop updates and assemble a complete release manifest."""
from __future__ import annotations

import argparse
import base64
import json
from pathlib import Path
import shutil
from urllib.parse import quote

from set_release_version import normalize_tag

TARGETS = {
    "aarch64-apple-darwin": ("darwin-aarch64", {"app": "macos/*.app.tar.gz"}),
    "x86_64-apple-darwin": ("darwin-x86_64", {"app": "macos/*.app.tar.gz"}),
    "x86_64-unknown-linux-gnu": ("linux-x86_64", {"appimage": "appimage/*.AppImage", "deb": "deb/*.deb", "rpm": "rpm/*.rpm"}),
    "aarch64-unknown-linux-gnu": ("linux-aarch64", {"appimage": "appimage/*.AppImage", "deb": "deb/*.deb", "rpm": "rpm/*.rpm"}),
    "x86_64-pc-windows-msvc": ("windows-x86_64", {"nsis": "nsis/*-setup.exe"}),
}


def expected_assets(tag: str) -> set[str]:
    version = normalize_tag(tag)
    return {"tyde-update.json"} | {
        f"tyde-update-{version}-{target}.app.tar.gz"
        for target, (base, _) in TARGETS.items() if base.startswith("darwin")
    }


def package(args: argparse.Namespace) -> None:
    version = normalize_tag(args.tag)
    base, patterns = TARGETS[args.target]
    patterns = dict(patterns)
    if base.startswith("windows") and "-" not in version:
        patterns["msi"] = "msi/*.msi"
    public_key = json.loads(args.config.read_text())["plugins"]["updater"]["pubkey"]
    key_bytes = base64.b64decode(base64.b64decode(public_key).splitlines()[1], validate=True)
    packages = []
    for installer, pattern in patterns.items():
        matches = list(args.bundle.glob(pattern))
        if len(matches) != 1:
            raise RuntimeError(f"Expected one {installer} artifact for {args.target}, found {matches}")
        source = matches[0]
        signature_path = source.with_name(source.name + ".sig")
        if not signature_path.is_file():
            raise RuntimeError(f"Missing updater signature: {signature_path}")
        signature = signature_path.read_text().strip()
        signature_bytes = base64.b64decode(base64.b64decode(signature).splitlines()[1], validate=True)
        # Catch an incorrectly configured CI key before publishing unusable updates.
        # The installed application independently verifies the full signature.
        if len(key_bytes) != 42 or len(signature_bytes) != 74 or key_bytes[2:10] != signature_bytes[2:10]:
            raise RuntimeError("Updater signing key does not match the application's pinned public key")
        # Both macOS architectures otherwise produce the same Tyde.app.tar.gz name.
        name = f"tyde-update-{version}-{args.target}.app.tar.gz" if installer == "app" else source.name
        packages.append((installer, source, name, signature))
    artifacts = args.output / "artifacts"
    artifacts.mkdir(parents=True, exist_ok=True)
    unexpected = {path.name for path in artifacts.iterdir()} - {name for _, _, name, _ in packages}
    if unexpected:
        raise RuntimeError(f"Unexpected staged release artifacts: {sorted(unexpected)}")
    platforms = {}
    for installer, source, name, signature in packages:
        artifact = artifacts / name
        shutil.copyfile(source, artifact)
        print(f"Staged {args.target} {installer}: {name}")
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
            command.add_argument("--config", type=Path, default=Path(__file__).resolve().parents[1] / "frontend/tauri-shell/tauri.conf.json")
        else:
            command.add_argument("--fragments", type=Path, required=True)
    args = parser.parse_args()
    if args.command == "package":
        package(args)
    else:
        assemble(args)


if __name__ == "__main__":
    main()
