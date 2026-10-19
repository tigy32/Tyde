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
import release_tool

TARGETS = ("aarch64-apple-darwin", "x86_64-apple-darwin")
CONTROLLER = Path(__file__).resolve().parents[1]


class ControllerError(ValueError):
    pass


def command(argv, *, cwd=None, operation=None):
    result = subprocess.run([str(a) for a in argv], cwd=cwd, capture_output=True)
    if result.returncode:
        raise ControllerError(f"{operation or Path(str(argv[0])).name} failed (exit {result.returncode})")
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


def api(args, endpoint, binary=False, *, operation=None, method="GET", body=None):
    # Log only controlled operation names and status codes, never raw gh output:
    # headers/bodies/errors may contain credentials, signed URLs or private IDs.
    operation = operation or ("artifact-download" if binary else "metadata-read")
    argv = ["gh", "api", f"repos/{args.repository}/{endpoint}", "--include", "--method", method]
    if binary:
        argv += ["-H", "Accept: application/octet-stream"]
    if body is not None:
        argv += ["--input", "-"]
    result = subprocess.run(argv, input=json.dumps(body).encode() if body is not None else None, capture_output=True)
    raw = result.stdout
    statuses = []
    while raw.startswith(b"HTTP/"):
        header, separator, body = raw.partition(b"\r\n\r\n")
        if not separator:
            header, separator, body = raw.partition(b"\n\n")
        match = re.match(rb"HTTP/[^ ]+ (\d{3})\b", header)
        if not separator or not match:
            raise ControllerError(f"GitHub {operation}: malformed HTTP response")
        statuses.append(int(match[1]))
        raw = body
    status = statuses[-1] if statuses else None
    if result.returncode or status is None or not 200 <= status < 300:
        if status is None:
            match = re.search(rb"\bHTTP (\d{3})\b", result.stderr)
            status = int(match[1]) if match else None
        category = {401: "authentication rejected", 403: "authorization or rate limit rejected",
                    404: "not found", 429: "rate limit rejected"}.get(status, "API/transport failure")
        raise ControllerError(f"GitHub {operation}: HTTP {status if status else 'unavailable'}; {category} (gh exit {result.returncode})")
    return raw if binary else json.loads(raw)


def pages(args, endpoint, *, operation):
    values = []
    page = 1
    while True:
        batch = api(args, f"{endpoint}?per_page=100&page={page}", operation=operation)
        if not isinstance(batch, list):
            raise ControllerError(f"GitHub {operation}: expected a page array")
        values.extend(batch)
        if len(batch) < 100:
            return values
        page += 1


def release_by_id(args, release_id, *, published=False):
    release = api(args, f"releases/{release_id}", operation="release-by-ID")
    if (release["id"] != release_id or release["tag_name"] != args.tag
            or release["draft"] is not (not published)):
        raise ControllerError("Published release is immutable or release ID/tag/state mismatches")
    if release["prerelease"] is not ("-" in args.tag):
        raise ControllerError("Release channel mismatches tag")
    release["assets"] = pages(args, f"releases/{release_id}/assets", operation="release-assets")
    return release


def draft(args):
    matches = [r for r in pages(args, "releases", operation="release-list") if r["tag_name"] == args.tag]
    if len(matches) != 1:
        raise ControllerError("Expected one unique exact-tag draft release")
    release_id = matches[0]["id"]
    if type(release_id) is not int or release_id <= 0 or matches[0]["draft"] is not True:
        raise ControllerError("Published release is immutable or release ID is invalid")
    return release_by_id(args, release_id)


def selection(args):
    normalize_tag(args.tag)
    if not re.fullmatch(r"[A-Za-z0-9_.-]+/[A-Za-z0-9_.-]+", args.repository):
        raise ControllerError("Invalid repository")


def context(args):
    selection(args)
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


def assemble(args, *, upload_manifest=True):
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
        candidate = draft(args)
        candidate["assets"] = [a for a in candidate["assets"] if a["name"] != "tyde-update.json"] + [{"name": "tyde-update.json"}]
        release_tool.validate_release(candidate, args.tag)
        assembly = {"repository": args.repository, "tag": args.tag,
                    "source_sha": args.source_sha, "controller_sha": args.controller_sha,
                    "manifest_sha256": digest(manifest), "assets": asset_receipts}
        if upload_manifest:
            command(["gh", "release", "upload", args.tag, manifest, "--repo", args.repository, "--clobber"])
            shutil.copyfile(manifest, args.retained / "tyde-update.json")
            save(args.retained / "assembly.json", assembly)
        return assembly

def asset_snapshot(release):
    return {a["name"]: {k: a[k] for k in ("id", "size", "digest")} for a in release["assets"]}


def verify_release_payloads(args, assembly, root):
    release = draft(args)
    release_tool.validate_release(release, args.tag)
    data = receipt(args)
    for asset in release["assets"]:
        name = asset["name"]
        if Path(name).name != name:
            raise ControllerError("Unsafe release asset name")
        path = root / name
        path.write_bytes(api(args, f"releases/assets/{asset['id']}", binary=True, operation="release-asset-download"))
        sha = digest(path)
        if asset.get("size") != path.stat().st_size or asset.get("digest") != "sha256:" + sha:
            raise ControllerError("Release asset size/SHA-256 mismatch or missing digest")
        if name in assembly["assets"] and sha != assembly["assets"][name]["sha256"]:
            raise ControllerError("Release asset changed since signature verification")
        retained_name = f"output/{name}" if not name.startswith("tyde-update-") else f"output/artifacts/{name}"
        if retained_name in data["outputs"] and sha != data["outputs"][retained_name]:
            raise ControllerError("Release asset differs from retained source-bound output")
        if name == "tyde-update.json" and sha != assembly["manifest_sha256"]:
            raise ControllerError("Aggregate manifest differs from verified same-source fragments")
        if name.startswith("tyde-server-"):
            with zipfile.ZipFile(path) as archive:
                binary = "tyde-server.exe" if "windows" in name else "tyde-server"
                if archive.namelist() != [binary] or archive.getinfo(binary).file_size == 0 or archive.testzip() is not None:
                    raise ControllerError("Invalid headless server archive")
        if name.endswith(".dmg"):
            command(["hdiutil", "verify", path])
            command(["codesign", "--verify", "--strict", path])
            command(["xcrun", "stapler", "validate", path])
            command(["spctl", "-a", "-v", "-t", "open", "--context", "context:primary-signature", path])
    for path in root.glob("*.sha256"):
        package = path.with_suffix("")
        fields = path.read_text().split()
        if (len(fields) != 2 or fields[0].lower() != digest(package)
                or Path(fields[1].lstrip("*")).name != package.name):
            raise ControllerError("Linux installer checksum mismatch")
    return release


def verify_mobile_web(args, root):
    manifest = root / "mobile-manifest.json"
    command(["aws", "s3", "cp", "s3://tycode-static/tyde/manifest.json", manifest, "--region", "us-west-2"])
    command([sys.executable, CONTROLLER / "tools/check_mobile_web_manifest.py", "--manifest", manifest,
             "--protocol-source", args.source / "protocol/src/types.rs", args.tag])
    value = json.loads(manifest.read_text())
    live = root / "public-manifest.json"
    command(["curl", "--fail", "--silent", "--show-error", "--location", "--output", live,
             "https://tycode.dev/tyde/manifest.json"])
    version = normalize_tag(args.tag)
    if json.loads(live.read_text())["versions"][version] != value["versions"][version]:
        raise ControllerError("Public mobile manifest differs from deployed entry")
    plan = release_tool.manifest_plan(value, args.tag, "https://tycode.dev")
    for index, item in enumerate(plan["items"]):
        body, headers = root / f"body-{index}", root / f"headers-{index}"
        command(["curl", "--fail", "--silent", "--show-error", "--location", "--output", body,
                 "--dump-header", headers, item["url"]])
        release_tool.validate_download(body, item["integrity"],
                                       release_tool.header_content_type(headers.read_text()), item["wasm"])


def finalize(args):
    if not args.confirm or "-" in args.tag:
        raise ControllerError("Finalization requires explicit --confirm and an approved stable tag")
    # Assembly stays read-only here: finalization requires the complete remote
    # aggregate already uploaded by the separately selected assemble operation.
    assembly = assemble(args, upload_manifest=False)
    with tempfile.TemporaryDirectory() as directory:
        root = Path(directory)
        assets = root / "assets"
        assets.mkdir()
        release = verify_release_payloads(args, assembly, assets)
        snapshot = asset_snapshot(release)
        # deploy.sh writes its generated manifest. Give immutable controller
        # tooling a disposable copy; never overwrite frozen application files.
        deploy = root / "deploy"
        deploy.mkdir()
        archive = root / "deploy.tar"
        command(["git", "-C", CONTROLLER, "archive", "--output", archive, args.controller_sha, "web", "tools"])
        command(["tar", "-xf", archive, "-C", deploy])
        command(["bash", deploy / "web/deploy/deploy.sh", args.tag, "--confirm", "--live-manifest-base",
                 "--source-root", args.source], cwd=args.source, operation="frozen-source mobile-web deployment")
        verify_mobile_web(args, root)
        context(args)
        current = draft(args)
        if current["id"] != release["id"] or asset_snapshot(current) != snapshot:
            raise ControllerError("Draft assets changed during deployment; refusing publication")
        save(args.retained / "finalization.json", {"tag": args.tag, "source_sha": args.source_sha,
             "controller_sha": args.controller_sha, "assets": snapshot, "mobile_web": "verified", "state": "publishing"})
        api(args, f"releases/{release['id']}", operation="publish-stable", method="PATCH",
            body={"draft": False, "prerelease": False, "make_latest": "true"})
        published = release_by_id(args, release["id"], published=True)
        release_tool.validate_release(published, args.tag, require_published=True)
        latest = api(args, "releases/latest", operation="verify-latest-stable")
        if latest["id"] != release["id"] or latest["tag_name"] != args.tag:
            raise ControllerError("Published stable is not latest; inspect publication before any further action")
        if asset_snapshot(published) != snapshot:
            raise ControllerError("Published asset metadata changed; inspect publication before any further action")
        save(args.retained / "finalization.json", {"tag": args.tag, "source_sha": args.source_sha,
             "controller_sha": args.controller_sha, "assets": snapshot, "mobile_web": "verified", "state": "published"})


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("operation", choices=("verify", "prepare", "submit", "complete", "upload", "restore", "checkpoint", "assemble", "package", "finalize", "lookup-draft"))
    parser.add_argument("--repository", required=True)
    parser.add_argument("--tag", required=True)
    parser.add_argument("--source-sha")
    parser.add_argument("--controller-sha")
    parser.add_argument("--source", type=Path)
    parser.add_argument("--target", choices=TARGETS)
    parser.add_argument("--retained", type=Path)
    parser.add_argument("--kind", choices=("app", "server", "dmg"))
    parser.add_argument("--run-id")
    parser.add_argument("--artifact-id")
    parser.add_argument("--fragments-run-id")
    parser.add_argument("--confirm", action="store_true")
    args = parser.parse_args()
    try:
        selection(args)
        if args.operation == "lookup-draft":
            release = draft(args)
            hashes = sum(bool(re.fullmatch(r"sha256:[0-9a-f]{64}", a.get("digest") or "")) for a in release["assets"])
            print(f"Exact-tag draft lookup: PASS; channel verified; assets={len(release['assets'])}; SHA-256 metadata={hashes}; mutations=0")
            return 0
        if not all((args.source, args.source_sha, args.controller_sha, args.target, args.retained)):
            raise ControllerError("Operation requires explicit source/controller/target/retained arguments")
        args.source = args.source.resolve()
        args.retained = args.retained.resolve()
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
             "checkpoint": checkpoint, "assemble": assemble, "finalize": finalize}[args.operation](args)
        if args.operation in ("prepare", "restore") and os.environ.get("GITHUB_ENV"):
            with open(os.environ["GITHUB_ENV"], "a") as env:
                env.write("MACOS_PAYLOAD_RETAINED=true\n")
        print(f"macOS {args.operation}: PASS ({args.target}); no tests or validation attestation created")
    except (ControllerError, OSError, ValueError, KeyError, RuntimeError, zipfile.BadZipFile) as error:
        # Parser/I/O values can contain private submission IDs; never dump them.
        message = str(error) if isinstance(error, ControllerError) else type(error).__name__
        print(f"macOS controller: {message}", file=sys.stderr)
        return 1
    return 0


if __name__ == "__main__":
    sys.exit(main())
