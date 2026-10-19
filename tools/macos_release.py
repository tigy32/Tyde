#!/usr/bin/env python3
"""Retain signed macOS bytes, submit once, and explicitly resume completion."""
from __future__ import annotations

import argparse
import hashlib
import io
import json
import os
from pathlib import Path
import plistlib
import re
import shutil
import subprocess
import sys
import tempfile
import zipfile
from urllib.parse import unquote

from check_release_version import collect_versions
from set_release_version import normalize_tag
import update_manifest

TARGETS = ("aarch64-apple-darwin", "x86_64-apple-darwin")
CONTROLLER = Path(__file__).resolve().parents[1]


class ControllerError(ValueError):
    pass


def command(argv, *, cwd=None):
    result = subprocess.run([str(a) for a in argv], cwd=cwd, capture_output=True)
    if result.returncode:
        raise ControllerError(f"{Path(str(argv[0])).name} failed (exit {result.returncode})")
    return result.stdout


def git(root, *args):
    return command(["git", "-C", root, *args]).decode().strip()


def digest(path):
    with path.open("rb") as stream:
        return hashlib.file_digest(stream, "sha256").hexdigest()


def save(path, data):
    temporary = path.with_suffix(".tmp")
    with temporary.open("w") as stream:
        json.dump(data, stream, indent=2)
        stream.write("\n")
        stream.flush()
        os.fsync(stream.fileno())
    os.chmod(temporary, 0o600)
    temporary.replace(path)


def api(args, endpoint, binary=False):
    argv = ["gh", "api", f"repos/{args.repository}/{endpoint}"]
    if binary:
        argv += ["-H", "Accept: application/octet-stream"]
    raw = command(argv)
    return raw if binary else json.loads(raw)


def draft(args):
    release = api(args, f"releases/tags/{args.tag}")
    if release["tag_name"] != args.tag or release["draft"] is not True:
        raise ControllerError("Published release is immutable or release tag mismatches")
    if release["prerelease"] != ("-" in args.tag):
        raise ControllerError("Release channel mismatches tag")
    return release


def context(args):
    normalize_tag(args.tag)
    if not re.fullmatch(r"[A-Za-z0-9_.-]+/[A-Za-z0-9_.-]+", args.repository):
        raise ControllerError("Invalid repository")
    for root, sha in ((CONTROLLER, args.controller_sha), (args.source, args.source_sha)):
        if not re.fullmatch(r"[0-9a-f]{40}", sha) or git(root, "rev-parse", "HEAD") != sha:
            raise ControllerError("Source/controller commit mismatch")
        remote = git(root, "config", "--get", "remote.origin.url")
        if remote not in (f"https://github.com/{args.repository}.git", f"https://github.com/{args.repository}", f"git@github.com:{args.repository}.git"):
            raise ControllerError("Wrong repository provenance")
        git(root, "diff", "--exit-code", "HEAD", "--")
        git(root, "merge-base", "--is-ancestor", sha, "refs/remotes/origin/main")
    remote_tags = dict(line.split()[::-1] for line in git(args.source, "ls-remote", "origin", f"refs/tags/{args.tag}", f"refs/tags/{args.tag}^{{}}").splitlines())
    remote_source = remote_tags.get(f"refs/tags/{args.tag}^{{}}", remote_tags.get(f"refs/tags/{args.tag}"))
    if remote_source != args.source_sha:
        raise ControllerError("Remote source tag changed or missing")
    if git(args.source, "rev-parse", f"refs/tags/{args.tag}^{{commit}}") != args.source_sha:
        raise ControllerError("Source tag mismatch")
    if set(collect_versions(args.source).values()) != {normalize_tag(args.tag)}:
        raise ControllerError("Source release version mismatch")
    draft(args)


def provenance(args, run_id, controller_sha=None):
    run = api(args, f"actions/runs/{run_id}")
    if (str(run["id"]) != str(run_id) or run["repository"]["full_name"] != args.repository
            or run["head_repository"]["full_name"] != args.repository
            or run["path"] not in (".github/workflows/release.yml", ".github/workflows/macos-release.yml")):
        raise ControllerError("Wrong repository/run provenance")
    if controller_sha is not None:
        if run["head_sha"] != controller_sha:
            raise ControllerError("Run controller mismatch")
    elif not (run["head_sha"] == args.source_sha and run["head_branch"] == args.tag
              or run["display_title"] == f"Release {args.tag} @ {args.source_sha}"):
        raise ControllerError("Fragment run source mismatch")
    git(CONTROLLER, "merge-base", "--is-ancestor", run["head_sha"], "refs/remotes/origin/main")
    return run


def download(args, artifact_id, run_id, output):
    metadata = api(args, f"actions/artifacts/{artifact_id}")
    if metadata["expired"] or str(metadata["workflow_run"]["id"]) != str(run_id):
        raise ControllerError("Expired artifact or wrong artifact run provenance")
    raw = api(args, f"actions/artifacts/{artifact_id}/zip", binary=True)
    if metadata.get("digest") != "sha256:" + hashlib.sha256(raw).hexdigest():
        raise ControllerError("Artifact transport digest mismatch or missing digest")
    with zipfile.ZipFile(io.BytesIO(raw)) as archive:
        for entry in archive.infolist():
            path = Path(entry.filename)
            if path.is_absolute() or ".." in path.parts or entry.external_attr >> 16 & 0o170000 == 0o120000:
                raise ControllerError("Unsafe retained artifact path")
        archive.extractall(output)
    return metadata


def receipt(args):
    path = args.retained / "receipt.json"
    data = json.loads(path.read_text())
    if any(data.get(k) != v for k, v in {
        "schema": 1, "repository": args.repository, "tag": args.tag,
        "source_sha": args.source_sha, "target": args.target,
    }.items()):
        raise ControllerError("Retained source/version/architecture/receipt mismatch")
    provenance(args, data["run_id"], data["controller_sha"])
    expected = {"app.zip", "server.zip"}
    if data.get("packaged"):
        expected |= {"signed.dmg", f"updater/artifacts/tyde-update-{normalize_tag(args.tag)}-{args.target}.app.tar.gz", f"updater/update-{args.target}.json"}
    if set(data["payloads"]) != expected:
        raise ControllerError("Incomplete retained payload receipt")
    verify_files(args.retained, data["payloads"])
    return data


def verify_files(root, files):
    for name, expected in files.items():
        path = Path(name)
        if path.is_absolute() or ".." in path.parts or not (root / path).is_file() or (root / path).is_symlink():
            raise ControllerError("Missing/unsafe retained payload")
        if digest(root / path) != expected:
            raise ControllerError("Retained payload digest mismatch")


def verify_app(args, app, server):
    with (app / "Contents/Info.plist").open("rb") as stream:
        info = plistlib.load(stream)
    if info["CFBundleShortVersionString"] != normalize_tag(args.tag):
        raise ControllerError("Application version mismatch")
    arch = "arm64" if args.target.startswith("aarch64") else "x86_64"
    for path in (app / "Contents/MacOS/tyde", server):
        if command(["lipo", "-archs", path]).decode().strip() != arch:
            raise ControllerError("Signed binary architecture mismatch")
        command(["codesign", "--verify", "--strict", path])
    command(["codesign", "--verify", "--deep", "--strict", app])


def prepare(args):
    if args.retained.exists():
        raise ControllerError("Retention directory already exists; select explicit resume")
    app = args.source / f"target/{args.target}/release/bundle/macos/Tyde.app"
    server = args.source / f"target/{args.target}/release/tyde-server"
    verify_app(args, app, server)
    args.retained.mkdir(parents=True)
    command(["ditto", "-c", "-k", "--keepParent", app, args.retained / "app.zip"])
    command(["ditto", "-c", "-k", "--keepParent", server, args.retained / "server.zip"])
    data = {"schema": 1, "repository": args.repository, "tag": args.tag,
            "source_sha": args.source_sha, "target": args.target,
            "controller_sha": args.controller_sha, "run_id": os.environ["GITHUB_RUN_ID"],
            "run_attempt": os.environ["GITHUB_RUN_ATTEMPT"], "phase": "retained",
            "checkpoint_run_id": os.environ["GITHUB_RUN_ID"], "checkpoint_controller_sha": args.controller_sha,
            "payloads": {name: digest(args.retained / name) for name in ("app.zip", "server.zip")},
            "submissions": {}, "events": []}
    save(args.retained / "receipt.json", data)


def remote(args, data, phase, argv):
    data["events"].append({"phase": phase, "state": "started"})
    save(args.retained / "receipt.json", data)
    result = subprocess.run([str(a) for a in argv], capture_output=True)
    event = {"phase": phase, "exit_code": result.returncode}
    if result.returncode:
        # Never retain raw tool output: it can contain credentials and submission URLs.
        text = (result.stdout + result.stderr).decode(errors="replace")
        event["network_codes"] = sorted(set(re.findall(r"(?:Code[=: ]+|Error[ :]+)(-?\d+)", text)))
        event["endpoints"] = sorted(set(re.findall(r"(?:https://)?([\w.-]+\.(?:apple\.com|icloud\.com))", text)))
        event["routes"] = []
        for host in ("appstoreconnect.apple.com", "api.apple-cloudkit.com"):
            route = {"host": host}
            dns = subprocess.run(["dscacheutil", "-q", "host", "-a", "name", host], capture_output=True)
            route["dns_exit_code"] = dns.returncode
            route["addresses"] = re.findall(r"(?:ip_address|ipv6_address): ([0-9a-fA-F:.]+)", dns.stdout.decode(errors="replace"))
            routing = subprocess.run(["route", "-n", "get", host], capture_output=True)
            route["route_exit_code"] = routing.returncode
            route["route"] = re.findall(r"^\s*(destination|gateway|interface):\s*([\w.:%-]+)\s*$", routing.stdout.decode(errors="replace"), re.M)
            event["routes"].append(route)
    data["events"].append(event)
    if not phase.startswith("submit-") or result.returncode:
        save(args.retained / "receipt.json", data)
    if result.returncode:
        raise ControllerError(f"{phase} failed (exit {result.returncode}); retained checkpoint and sanitized diagnostics available")
    return result.stdout


def auth():
    return ["--issuer", os.environ["APPLE_API_ISSUER"], "--key-id", os.environ["APPLE_API_KEY"], "--key", os.environ["APPLE_API_KEY_PATH"]]


def submit(args):
    data = receipt(args)
    kind = args.kind
    if kind in data["submissions"]:
        raise ControllerError("Submission already recorded; use completion, not resubmission")
    if any(e["phase"] == f"submit-{kind}" and e.get("state") == "started" for e in data["events"]):
        raise ControllerError("Ambiguous prior submission; reconcile privately, never blindly resubmit")
    name = {"app": "app.zip", "server": "server.zip", "dmg": "signed.dmg"}[kind]
    if name not in data["payloads"]:
        raise ControllerError("Missing signed payload receipt")
    raw = remote(args, data, f"submit-{kind}", ["xcrun", "notarytool", "submit", args.retained / name, *auth(), "--output-format", "json"])
    response = json.loads(raw)
    if not re.fullmatch(r"[0-9a-fA-F-]{36}", response["id"]):
        raise ControllerError("Invalid Apple submission response")
    data["submissions"][kind] = {"id": response["id"], "sha256": data["payloads"][name]}
    data["phase"] = f"submitted-{kind}"
    save(args.retained / "receipt.json", data)


def wait_accepted(args, data, kind, name):
    if kind not in data["submissions"]:
        raise ControllerError("Missing submission receipt; completion cannot submit or compile")
    submission = data["submissions"][kind]
    if submission["sha256"] != data["payloads"][name]:
        raise ControllerError("Submission payload digest mismatch")
    if submission.get("status") != "Accepted":
        raw = remote(args, data, f"wait-{kind}", ["xcrun", "notarytool", "wait", submission["id"], *auth(), "--output-format", "json"])
        status = json.loads(raw)
        if status["id"].lower() != submission["id"].lower() or status["status"] != "Accepted":
            raise ControllerError("Notarization not Accepted; refusing completion")
        submission["status"] = "Accepted"
        data["phase"] = f"accepted-{kind}"
        save(args.retained / "receipt.json", data)


def gatekeeper(args, data, label, path):
    for action in ("staple", "validate"):
        remote(args, data, f"{action}-{label}", ["xcrun", "stapler", action, path])
    argv = ["spctl", "-a", "-v"]
    argv += ["-t", "exec"] if label == "app" else ["-t", "open", "--context", "context:primary-signature"]
    remote(args, data, f"gatekeeper-{label}", [*argv, path])


def package(args):
    data = receipt(args)
    if data.get("packaged") or (args.retained / "signed.dmg").exists():
        raise ControllerError("DMG already retained; select submit or completion, never repackage")
    wait_accepted(args, data, "server", "server.zip")
    wait_accepted(args, data, "app", "app.zip")
    work = args.retained / "work"
    if work.exists():
        shutil.rmtree(work)
    work.mkdir()
    command(["ditto", "-x", "-k", args.retained / "app.zip", work])
    command(["ditto", "-x", "-k", args.retained / "server.zip", work])
    app = work / "Tyde.app"
    verify_app(args, app, work / "tyde-server")
    gatekeeper(args, data, "app", app)
    bundle = work / "bundle/macos"
    bundle.mkdir(parents=True)
    archive = bundle / "Tyde.app.tar.gz"
    command(["tar", "-czf", archive, "-C", work, "Tyde.app"])
    command(["npx", "--no-install", "tauri", "signer", "sign", archive], cwd=args.source)
    output = args.retained / "updater"
    if output.exists():
        shutil.rmtree(output)
    update_manifest.package(argparse.Namespace(tag=args.tag, repository=args.repository, target=args.target,
                            bundle=bundle.parent, output=output, config=args.source / "frontend/tauri-shell/tauri.conf.json"))
    verify_update(args.source / "frontend/tauri-shell/tauri.conf.json",
                  output / f"artifacts/tyde-update-{normalize_tag(args.tag)}-{args.target}.app.tar.gz",
                  json.loads((output / f"update-{args.target}.json").read_text())["platforms"][update_manifest.TARGETS[args.target][0]]["signature"])
    # The DMG must contain the stapled app, not the original unstapled copy.
    staging = work / "dmg"
    staging.mkdir()
    command(["ditto", app, staging / "Tyde.app"])
    (staging / "Applications").symlink_to("/Applications")
    command(["hdiutil", "create", "-volname", "Tyde", "-srcfolder", staging,
             "-ov", "-format", "UDZO", args.retained / "signed.dmg"])
    command(["codesign", "--force", "--timestamp", "--sign", os.environ["APPLE_SIGNING_IDENTITY"], args.retained / "signed.dmg"])
    command(["codesign", "--verify", "--strict", args.retained / "signed.dmg"])
    command(["hdiutil", "verify", args.retained / "signed.dmg"])
    for path in [args.retained / "signed.dmg", *(p for p in output.rglob("*") if p.is_file())]:
        data["payloads"][str(path.relative_to(args.retained))] = digest(path)
    data["packaged"] = True
    data["phase"] = "dmg-retained"
    save(args.retained / "receipt.json", data)


def complete(args):
    data = receipt(args)
    if not data.get("packaged"):
        raise ControllerError("Missing signed DMG receipt; select package explicitly")
    for kind, name in (("app", "app.zip"), ("server", "server.zip"), ("dmg", "signed.dmg")):
        wait_accepted(args, data, kind, name)
    data["phase"] = "completing-dmg"
    save(args.retained / "receipt.json", data)
    work = args.retained / "work"
    if work.exists():
        shutil.rmtree(work)
    work.mkdir()
    dmg = work / f"Tyde_{normalize_tag(args.tag)}_{args.target}.dmg"
    shutil.copyfile(args.retained / "signed.dmg", dmg)
    gatekeeper(args, data, "dmg", dmg)
    output = args.retained / "output"
    if output.exists():
        shutil.rmtree(output)
    shutil.copytree(args.retained / "updater", output)
    shutil.copyfile(dmg, output / dmg.name)
    shutil.copyfile(args.retained / "server.zip", output / f"tyde-server-{args.target}.zip")
    data["outputs"] = {str(p.relative_to(args.retained)): digest(p) for p in output.rglob("*") if p.is_file()}
    data["phase"] = "complete"
    save(args.retained / "receipt.json", data)


def upload(args):
    data = receipt(args)
    if data["phase"] != "complete":
        raise ControllerError("Incomplete outputs; refusing upload")
    verify_files(args.retained, data["outputs"])
    version = normalize_tag(args.tag)
    expected = {f"output/Tyde_{version}_{args.target}.dmg", f"output/tyde-server-{args.target}.zip",
                f"output/artifacts/tyde-update-{version}-{args.target}.app.tar.gz", f"output/update-{args.target}.json"}
    if set(data["outputs"]) != expected:
        raise ControllerError("Incomplete output receipt")
    for name in sorted(expected - {f"output/update-{args.target}.json"}):
        draft(args)
        command(["gh", "release", "upload", args.tag, args.retained / name, "--repo", args.repository, "--clobber"])



def checkpoint(args):
    data = receipt(args)
    data["checkpoint_run_id"] = os.environ["GITHUB_RUN_ID"]
    data["checkpoint_controller_sha"] = args.controller_sha
    save(args.retained / "receipt.json", data)


def verify_update(config, payload, signature):
    # Tauri uses the minisign envelope: ED signs a BLAKE2b-512 prehash, Ed the file.
    script = r"""
const fs = require('node:fs'), crypto = require('node:crypto');
const [config, file, encoded] = process.argv.slice(1);
const key = Buffer.from(Buffer.from(JSON.parse(fs.readFileSync(config)).plugins.updater.pubkey, 'base64').toString().trim().split('\n')[1], 'base64');
const lines = Buffer.from(encoded, 'base64').toString().trim().split('\n');
const sig = Buffer.from(lines[1], 'base64');
if (key.length !== 42 || sig.length !== 74 || !key.subarray(2,10).equals(sig.subarray(2,10))) process.exit(1);
const publicKey = crypto.createPublicKey({ key: Buffer.concat([Buffer.from('302a300506032b6570032100', 'hex'), key.subarray(10)]), format: 'der', type: 'spki' });
const bytes = fs.readFileSync(file), algorithm = sig.subarray(0,2).toString();
if (!['Ed', 'ED'].includes(algorithm)) process.exit(1);
const message = algorithm === 'ED' ? crypto.createHash('blake2b512').update(bytes).digest() : bytes;
if (!crypto.verify(null, message, publicKey, sig.subarray(10))) process.exit(1);
if (!lines[2].startsWith('trusted comment: ')) process.exit(1);
if (!crypto.verify(null, Buffer.concat([sig.subarray(10), Buffer.from(lines[2].slice(17))]), publicKey, Buffer.from(lines[3], 'base64'))) process.exit(1);
"""
    command(["node", "-e", script, config, payload, signature])


def assemble(args):
    if not args.fragments_run_id:
        raise ControllerError("Assembly requires explicit same-source fragment run")
    data = receipt(args)
    if data["phase"] != "complete":
        raise ControllerError("Assembly requires complete macOS outputs")
    verify_files(args.retained, data["outputs"])
    provenance(args, args.fragments_run_id)
    artifacts = api(args, f"actions/runs/{args.fragments_run_id}/artifacts?per_page=100")
    if artifacts["total_count"] > 100:
        raise ControllerError("Too many artifacts; explicit bounded assembly cannot select fragments")
    release = draft(args)
    config = args.source / "frontend/tauri-shell/tauri.conf.json"
    with tempfile.TemporaryDirectory() as directory:
        root = Path(directory)
        fragments = root / "fragments"
        fragments.mkdir()
        asset_receipts = {}
        for target in update_manifest.TARGETS:
            if target == args.target:
                path = args.retained / f"output/update-{target}.json"
            else:
                matches = [a for a in artifacts["artifacts"] if a["name"] == f"update-{target}"]
                if len(matches) != 1:
                    raise ControllerError("Missing or ambiguous same-source platform fragment")
                folder = root / target
                download(args, matches[0]["id"], args.fragments_run_id, folder)
                path = folder / f"update-{target}.json"
            fragment = json.loads(path.read_text())
            for entry in fragment["platforms"].values():
                prefix = f"https://github.com/{args.repository}/releases/download/{args.tag}/"
                if not entry["url"].startswith(prefix):
                    raise ControllerError("Wrong-source updater URL")
                name = unquote(entry["url"][len(prefix):])
                if Path(name).name != name:
                    raise ControllerError("Unsafe updater asset name")
                if name in asset_receipts:
                    if asset_receipts[name]["signature"] != entry["signature"]:
                        raise ControllerError("Conflicting signatures for updater aliases")
                    continue
                matches = [a for a in release["assets"] if a["name"] == name]
                if len(matches) != 1:
                    raise ControllerError("Missing or ambiguous updater release asset")
                payload = root / name
                payload.write_bytes(api(args, f"releases/assets/{matches[0]['id']}", binary=True))
                verify_update(config, payload, entry["signature"])
                if target == args.target and digest(payload) != data["outputs"].get(f"output/artifacts/{name}"):
                    raise ControllerError("Release updater differs from retained source-bound output")
                asset_receipts[name] = {"sha256": digest(payload), "signature": entry["signature"], "asset_id": matches[0]["id"],
                                        "fragment_run_id": data["checkpoint_run_id"] if target == args.target else args.fragments_run_id}
            shutil.copyfile(path, fragments / path.name)
        manifest = root / "tyde-update.json"
        update_manifest.assemble(argparse.Namespace(tag=args.tag, repository=args.repository, fragments=fragments, output=manifest))
        # Validate the entire release asset set before writing the aggregate, never publish a partial.
        import release_tool
        candidate = draft(args)
        candidate["assets"] = [a for a in candidate["assets"] if a["name"] != "tyde-update.json"] + [{"name": "tyde-update.json"}]
        release_tool.validate_release(candidate, args.tag)
        command(["gh", "release", "upload", args.tag, manifest, "--repo", args.repository, "--clobber"])
        shutil.copyfile(manifest, args.retained / "tyde-update.json")
        save(args.retained / "assembly.json", {"repository": args.repository, "tag": args.tag,
             "source_sha": args.source_sha, "controller_sha": args.controller_sha,
             "manifest_sha256": digest(manifest), "assets": asset_receipts})

def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("operation", choices=("verify", "prepare", "submit", "complete", "upload", "restore", "checkpoint", "assemble", "package"))
    parser.add_argument("--repository", required=True)
    parser.add_argument("--tag", required=True)
    parser.add_argument("--source-sha", required=True)
    parser.add_argument("--controller-sha", required=True)
    parser.add_argument("--source", type=Path, required=True)
    parser.add_argument("--target", choices=TARGETS, required=True)
    parser.add_argument("--retained", type=Path, required=True)
    parser.add_argument("--kind", choices=("app", "server", "dmg"))
    parser.add_argument("--run-id")
    parser.add_argument("--artifact-id")
    parser.add_argument("--fragments-run-id")
    args = parser.parse_args()
    args.source = args.source.resolve()
    args.retained = args.retained.resolve()
    try:
        context(args)
        if args.operation == "verify":
            pass
        elif args.operation == "restore":
            if args.retained.exists() or not args.run_id or not args.artifact_id:
                raise ControllerError("Restore requires an empty destination and explicit run/artifact IDs")
            metadata = download(args, args.artifact_id, args.run_id, args.retained)
            data = receipt(args)
            if str(data["checkpoint_run_id"]) != args.run_id:
                raise ControllerError("Receipt run mismatch")
            if metadata["workflow_run"]["head_sha"] != data["checkpoint_controller_sha"]:
                raise ControllerError("Checkpoint controller mismatch")
            provenance(args, args.run_id, data["checkpoint_controller_sha"])
        elif args.operation == "submit":
            if not args.kind:
                raise ControllerError("Submit requires explicit payload kind")
            submit(args)
        else:
            {"prepare": prepare, "package": package, "complete": complete, "upload": upload,
             "checkpoint": checkpoint, "assemble": assemble}[args.operation](args)
        print(f"macOS {args.operation}: PASS ({args.target}); no tests or validation attestation created")
    except (ControllerError, OSError, ValueError, KeyError) as error:
        # Parser/I/O values can contain private submission IDs; never dump them.
        message = str(error) if isinstance(error, ControllerError) else type(error).__name__
        print(f"macOS controller: {message}", file=sys.stderr)
        return 1
    return 0


if __name__ == "__main__":
    sys.exit(main())
