"""Opaque project snapshots and an owned Linux process environment."""
import array
import contextlib
import ctypes
import errno
import fcntl
import hashlib
import json
import os
from pathlib import Path
import selectors
import signal
import shutil
import socket
import stat
import struct
import string
import subprocess
import sys
import tempfile
import termios
import uuid

# Nested environments replace the parent's runtime mount, including its venv.
# The runner uses only stdlib, so retain the real interpreter, not that alias.
PYTHON = str(Path(sys.executable).resolve(strict=True))
SCHEMA = 3
ARTIFACT_CONTRACT = "existing_artifacts_v1"
LOGICAL_ROOT = "/mnt/tyde-workspace"
RUNTIME = "/mnt/tyde-runtime"
RECEIPTS = "/mnt/tyde-producer-receipts"
AT_FDCWD = -100
RENAME_NOREPLACE = 1


class Failure(Exception):
    def __init__(self, reason, message):
        self.reason, self.message = reason, message


def fail(reason, message):
    raise Failure(reason, message)


def runtime_diagnostic(phase, **details):
    try:
        print("Workbench runtime diagnostic: " + json.dumps(dict(phase=phase, **details), sort_keys=True), file=sys.stderr, flush=True)
    except BrokenPipeError:
        # A caller dropping captured stderr must not prevent owned teardown.
        pass


def digest(value):
    return hashlib.sha256(value).hexdigest()


def load(path):
    return json.loads(Path(path).read_text())


def save(path, value):
    path = Path(path)
    temporary = path.with_name(path.name + "." + uuid.uuid4().hex)
    with temporary.open("x") as output:
        json.dump(value, output, sort_keys=True)
        output.flush()
        os.fsync(output.fileno())
    os.replace(temporary, path)


def checked(args, **kwargs):
    result = subprocess.run(args, stdout=subprocess.PIPE, stderr=subprocess.PIPE, **kwargs)
    if result.returncode:
        fail("incompatible_environment", "Workbench environment command failed (exit %d)" % result.returncode)
    return result.stdout


def git(root, *args):
    return checked(["git", "-C", str(root), *args])


def unsupported(reason, message):
    return {"kind": "unsupported", "reason": reason, "message": message}


def probe():
    if sys.platform != "linux":
        result = unsupported("not_linux", "Bubblewrap workbenches require Linux")
        return {"bubblewrap": result, "nested_namespaces": result}
    if not hasattr(os, "pidfd_open") or not hasattr(signal, "pidfd_send_signal") or sys.version_info < (3, 9):
        result = unsupported("runtime_tool_missing", "Bubblewrap requires Python 3.9+ and Linux pidfd process completion events")
        return {"bubblewrap": result, "nested_namespaces": result}
    missing = next((name for name in ("bwrap", "cp", "git") if shutil.which(name) is None), None)
    if missing:
        reason = "bubblewrap_missing" if missing == "bwrap" else "runtime_tool_missing"
        result = unsupported(reason, "Required host tool is missing: " + missing)
        return {"bubblewrap": result, "nested_namespaces": result}
    runtime_diagnostic("probe_started", python=PYTHON, user_namespace=os.readlink("/proc/self/ns/user"))
    # Probe the actual command topology, not bwrap's restricted child profile.
    with tempfile.TemporaryDirectory(prefix="tyde-namespace-probe-") as run:
        run = Path(run)
        shutil.copyfile(__file__, run / "runner.py")
        save(run / "manifest.json", {"configuration": {"services": []}})
        args = [shutil.which("bwrap"), "--unshare-user", "--dev-bind", "/", "/", *private_mountpoints(), "--bind", str(run), RUNTIME, "--", PYTHON, RUNTIME + "/runner.py", "namespace", RUNTIME + "/control.sock"]
        command = [PYTHON, "-c", "import subprocess,sys; sys.exit(42 if subprocess.run(sys.argv[1:]).returncode else 0)", shutil.which("bwrap"), "--unshare-user", "--ro-bind", "/", "/", "--", "/bin/true"]
        result = guarded_launch(args, dict(os.environ), run, command, "/")
    if result not in (0, 42):
        result = unsupported("namespace_denied", "Host policy prevents the Bubblewrap mount environment")
        return {"bubblewrap": result, "nested_namespaces": result}
    return {"bubblewrap": {"kind": "available"}, "nested_namespaces": {"kind": "available"} if result == 0 else unsupported("nested_namespaces_denied", "Host policy denies nested namespaces; explicit backend namespace requirements cannot be satisfied.")}


def strict_clone(source, destination):
    result = subprocess.run(["cp", "-a", "--reflink=always", "--", str(source), str(destination)], stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
    if result.returncode:
        fail("reflink_unsupported", "Strict CoW cloning failed; no full-copy or Native fallback was attempted")


def check_reflink(directory):
    with tempfile.TemporaryDirectory(prefix=".tyde-cow-probe-", dir=directory) as scratch:
        source = Path(scratch) / "source"
        source.write_bytes(b"Tyde private CoW probe\n")
        dest = Path(scratch) / "clone"
        strict_clone(source, dest)
        if source.stat().st_ino == dest.stat().st_ino:
            fail("reflink_unsupported", "CoW probe did not create an independent inode")
        dest.write_bytes(b"private change\n")
        if source.read_bytes() != b"Tyde private CoW probe\n":
            fail("reflink_unsupported", "CoW probe modified its source")


def relative_path(value):
    path = Path(value)
    if not value or path.is_absolute() or any(part in ("", ".", "..", ".git") for part in value.split("/")):
        fail("runtime_configuration_invalid", "Configured paths must be nonempty project-relative paths without traversal or .git")
    return path


def expand(value, environment):
    try:
        return string.Template(value).substitute(environment)
    except (KeyError, ValueError):
        fail("runtime_configuration_invalid", "A configured runtime reference has an invalid or unavailable environment variable")


def runtime_environment(config, logical_root=LOGICAL_ROOT):
    env = dict(os.environ)
    for key in config["remove_environment"]:
        env.pop(key, None)
    references = dict(env, TYDE_WORKBENCH_ROOT=logical_root, TYDE_WORKBENCH_RUNTIME=RUNTIME)
    for key, value in config["environment"].items():
        env[key] = expand(value, references)
    env["TYDE_WORKBENCH_ROOT"] = logical_root
    env["TYDE_WORKBENCH_RUNTIME"] = RUNTIME
    env["PATH"] = RUNTIME + "/bin:" + env["PATH"]
    return env


def validate_configuration(config):
    snapshot = config["snapshot"]
    if snapshot is not None:
        paths = [relative_path(value) for value in snapshot["include"] + snapshot.get("optional_include", [])]
        if not paths or len(set(paths)) != len(paths):
            fail("runtime_configuration_invalid", "Snapshot include paths must be nonempty and unique")
        if any(a != b and a in b.parents for a in paths for b in paths):
            fail("runtime_configuration_invalid", "Snapshot include paths must not overlap")
        if not snapshot["provenance_paths"] and not snapshot.get("provenance_tools"):
            fail("runtime_configuration_invalid", "Snapshot reuse requires explicit toolchain/ABI/input provenance paths")
    for value in config["runtime_directories"] + list(config["runtime_files"]):
        path = relative_path(value)
        if path.parts[0] in ("bin", "runner.py", "manifest.json", "control.sock", "execution.sock"):
            fail("runtime_configuration_invalid", "Runtime files/directories overlap engine-owned resources")
    for binding in config["runtime_file_bindings"]:
        if binding["runtime_file"] not in config["runtime_files"]:
            fail("runtime_configuration_invalid", "A runtime file binding must name a configured runtime file")
    names, notifications = set(), set()
    for service in config["services"]:
        notification = relative_path(service["ready_notification"])
        if notification.parts[0] in ("bin", "runner.py", "manifest.json", "control.sock", "execution.sock") or str(notification) in config["runtime_files"]:
            fail("runtime_configuration_invalid", "Service notification overlaps a runtime resource")
        if not service["command"] or not service["name"] or service["name"] in names or service["ready_notification"] in notifications:
            fail("runtime_configuration_invalid", "Services require unique names/notifications and a foreground command")
        names.add(service["name"])
        notifications.add(service["ready_notification"])


def entry_identity(path):
    metadata = path.lstat()
    result = {"mode": stat.S_IMODE(metadata.st_mode), "mtime_ns": metadata.st_mtime_ns}
    if stat.S_ISREG(metadata.st_mode):
        result.update(kind="file", bytes=metadata.st_size, digest=digest(path.read_bytes()))
    elif stat.S_ISDIR(metadata.st_mode):
        result.update(kind="directory")
    elif stat.S_ISLNK(metadata.st_mode):
        result.update(kind="symlink", link=os.readlink(path))
    else:
        fail("incompatible_environment", "Snapshots reject sockets, devices and other special files")
    return result


def included_paths(root, config):
    result = []
    for value in config["snapshot"]["include"] + config["snapshot"].get("optional_include", []):
        path = root / relative_path(value)
        if not path.exists() and not path.is_symlink():
            if value in config["snapshot"].get("optional_include", []):
                continue
            fail("incompatible_environment", "A configured snapshot path does not exist")
        if not path.parent.resolve().is_relative_to(root.resolve()):
            fail("incompatible_environment", "Snapshot include path traverses a symlink outside its project")
        result.append(path)
    return result


def resolved_workspace_link(logical, records, files, artifact):
    # Resolve each component against authoritative mounts, not the host's
    # differently named sibling worktrees. Artifact-store overlays supply the
    # selected outputs while committed source supplies the remaining links.
    links = 0
    while True:
        prefix = Path("/")
        for index, part in enumerate(logical.parts[1:], 1):
            prefix /= part
            mapped = next(((number, record) for number, record in enumerate(records) if prefix.is_relative_to(record["logical_root"])), None)
            if mapped is None:
                if any(Path(record["logical_root"]).is_relative_to(prefix) for record in records):
                    continue
                fail("incompatible_environment", "Artifact symlink %s resolves outside registered workspace roots at %s" % (artifact, prefix))
            number, record = mapped
            relative = prefix.relative_to(record["logical_root"])
            physical = Path(record["root"]) / relative
            snapshot = record["configuration"]["snapshot"]
            if files is not None and snapshot is not None and any(relative == Path(value) or Path(value) in relative.parents for value in snapshot["include"] + snapshot.get("optional_include", [])):
                physical = artifact_root(files, number, len(records)) / relative
            if physical.is_symlink():
                target = Path(os.readlink(physical))
                if target.is_absolute():
                    fail("incompatible_environment", "Artifact symlink %s traverses absolute link %s -> %s" % (artifact, prefix, target))
                links += 1
                if links > 40:
                    fail("incompatible_environment", "Artifact symlink %s is cyclic or exceeds the filesystem link limit at %s" % (artifact, prefix))
                logical = Path(os.path.normpath(str(prefix.parent / target / Path(*logical.parts[index + 1:]))))
                break
        else:
            return logical


def artifact_inventory(root, config, logical_root, records, files):
    entries = {}
    for selected in included_paths(root, config):
        paths = [selected]
        if selected.is_dir() and not selected.is_symlink():
            for base, dirs, names in os.walk(selected, followlinks=False):
                paths.extend(Path(base) / name for name in dirs + names)
        for path in paths:
            value = entry_identity(path)
            if value["kind"] == "symlink":
                link = Path(value["link"])
                if link.is_absolute():
                    fail("incompatible_environment", "Snapshot symlink %s -> %s must remain relative and inside registered workspace roots" % (Path(logical_root) / path.relative_to(root), link))
                logical = Path(os.path.normpath(str(Path(logical_root) / path.relative_to(root).parent / link)))
                resolved_workspace_link(logical, records, files, Path(logical_root) / path.relative_to(root))
            entries[str(path.relative_to(root))] = value
    return entries


def source_inventory(root):
    entries = {}
    for raw in git(root, "ls-files", "-z").split(b"\0"):
        if raw:
            relative = os.fsdecode(raw)
            entries[relative] = entry_identity(root / relative)
    return entries


def assert_clean(root, config=None):
    outputs = [] if config is None or config["snapshot"] is None else config["snapshot"]["include"] + config["snapshot"].get("optional_include", [])
    for raw in git(root, "status", "--porcelain=v1", "-z", "--untracked-files=all").split(b"\0"):
        if raw and not (raw.startswith(b"?? ") and any(Path(os.fsdecode(raw[3:])) == Path(output) or Path(output) in Path(os.fsdecode(raw[3:])).parents for output in outputs)):
            fail("incompatible_environment", "Artifact publication requires unchanged committed source and no undeclared untracked inputs")


def guard_tool_query(name, root, env):
    if name in ("node", "npm", "pnpm", "yarn") and any(env.get(key) for key in ("NODE_OPTIONS", "NODE_PATH")):
        fail("runtime_configuration_invalid", "JavaScript identity queries do not support Node loader environment overrides")
    if name != "yarn":
        return
    if env.get("YARN_RC_FILENAME", ".yarnrc.yml") != ".yarnrc.yml" or any(env.get(key) for key in ("YARN_YARN_PATH", "YARN_PATH")):
        fail("runtime_configuration_invalid", "Yarn identity queries do not support tool-dispatch environment overrides")
    # Both Yarn generations can execute project code from rc configuration.
    # Reject rc files rather than partially parsing their delegation/plugin rules.
    for directory in (root, *root.parents, Path(env["HOME"])):
        if any((directory / name).exists() or (directory / name).is_symlink() for name in (".yarnrc", ".yarnrc.yml")):
            fail("runtime_configuration_invalid", "Yarn identity queries require configuration-free installed tooling; rc files can dispatch project code")


def input_identity(root, config, env, records):
    snapshot = config["snapshot"]
    inputs = {"configuration": digest(json.dumps(config, sort_keys=True).encode()), "source_commit": git(root, "rev-parse", "HEAD").decode().strip(), "platform": list(os.uname()), "os_release": digest(Path("/etc/os-release").read_bytes())}
    inputs["environment"] = digest(json.dumps({key: env.get(key) for key in snapshot["provenance_environment"]}, sort_keys=True).encode())
    external = {}
    for value in snapshot["provenance_paths"]:
        reference = expand(value, env)
        path = Path(reference)
        if not path.is_absolute():
            path = root / relative_path(reference)
        resolved = path.resolve(strict=True)
        # Inputs are hashed, never copied into a snapshot. Directories are
        # explicit opaque trees, not language/toolchain-specific discovery.
        paths = [resolved]
        if resolved.is_dir():
            for base, dirs, files in os.walk(resolved, followlinks=False):
                paths.extend(Path(base) / name for name in dirs + files)
        identities = {}
        for path in paths:
            identity = entry_identity(path)
            # Checkout-local mtimes are not input content; artifact inventories
            # retain full metadata for integrity and exact CoW restoration.
            identity.pop("mtime_ns")
            identities[str(path.relative_to(resolved)) if path != resolved else "."] = identity
        external[value] = digest(json.dumps(identities, sort_keys=True).encode())
    tools = {}
    for name in snapshot.get("provenance_tools", []):
        if name not in ("cargo", "rustc", "node", "npm", "pnpm", "yarn"):
            fail("runtime_configuration_invalid", "Unknown artifact provenance tool")
        guard_tool_query(name, root, env)
        executable = shutil.which(name, path=env["PATH"])
        if executable is None:
            fail("runtime_tool_missing", "Artifact provenance tool is missing: " + name)
        if not Path(executable).is_absolute() or any(Path(executable).resolve().is_relative_to(record[key]) for record in records for key in ("root", "parent_root")):
            fail("runtime_configuration_invalid", "Artifact discovery requires native host tools, not project scripts")
        arguments = [executable, "-vV" if name == "rustc" else "--version"]
        tools[name] = digest(checked(arguments, cwd=root, env=dict(env, RUSTUP_AUTO_INSTALL="0", RUSTUP_NO_UPDATE_CHECK="1", COREPACK_ENABLE_NETWORK="0", COREPACK_ENABLE_AUTO_PIN="0")))
    inputs["tools"] = tools
    inputs["declared_inputs"] = external
    return inputs


def cohort_key(tree, inputs):
    return digest(json.dumps({"schema": SCHEMA, "artifact_contract": ARTIFACT_CONTRACT, "tree": tree, "inputs": inputs, "logical_root": LOGICAL_ROOT}, sort_keys=True).encode())


@contextlib.contextmanager
def admission(manifest_path, exclusive=False):
    with (Path(manifest_path).parent / "active.lock").open("a+b") as lease:
        try:
            fcntl.flock(lease, fcntl.LOCK_EX if exclusive else (fcntl.LOCK_SH | fcntl.LOCK_NB))
        except BlockingIOError:
            fail("incompatible_environment", "Workbench processes are active or artifacts are being sealed")
        yield


def report_failure(reason, message):
    failure = {"reason": reason, "message": message}
    if len(sys.argv) > 1 and sys.argv[1] == "inside":
        save(Path(RECEIPTS) / "failure.json", failure)
    print(json.dumps({"error": failure}), file=sys.stderr)


def root_records(manifest):
    records = manifest.get("roots")
    if not isinstance(records, list) or not records:
        fail("incompatible_environment", "Workbench manifest lacks authoritative registered root mappings; recreate this older or malformed environment from its parent")
    return records


def workspace_identity(manifest, environment):
    records = root_records(manifest)
    result = {}
    for record in records:
        root, config = Path(record["root"]), record["configuration"]
        entry = {"tree": git(root, "rev-parse", "HEAD^{tree}").decode().strip(), "commit": git(root, "rev-parse", "HEAD").decode().strip()}
        if config["snapshot"] is not None:
            entry["inputs"] = input_identity(root, config, environment, records)
        result[record["logical_root"]] = entry
    return result


def workspace_sources(manifest):
    return {record["logical_root"]: source_inventory(Path(record["root"])) for record in root_records(manifest)}


def workspace_artifacts(manifest, files=None):
    records = root_records(manifest)
    entries = {}
    for index, record in enumerate(records):
        if record["configuration"]["snapshot"] is not None:
            root = Path(record["root"]) if files is None else artifact_root(files, index, len(records))
            entries[record["logical_root"]] = artifact_inventory(root, record["configuration"], record["logical_root"], records, files)
    return entries


def artifact_root(files, index, count):
    return Path(files) if count == 1 else Path(files) / str(index)


def workspace_clean(manifest):
    for record in root_records(manifest):
        assert_clean(Path(record["root"]), record["configuration"])


def acquire_environment(directory, claim):
    # The directory appears atomically with its creation claim, so creation
    # recovery removes only what this attempt acquired, never an occupant.
    if not claim or len(claim) > 64 or any(not (character.isascii() and (character.isalnum() or character in "-_")) for character in claim):
        fail("incompatible_environment", "Workbench environment claim is malformed")
    directory.parent.mkdir(mode=0o700, parents=True, exist_ok=True)
    staging = directory.parent / (".claim-" + claim)
    staging.mkdir(mode=0o700)
    try:
        save(staging / "claim.json", {"claim": claim})
        descriptor = os.open(staging, os.O_RDONLY | os.O_DIRECTORY)
        try:
            os.fsync(descriptor)
        finally:
            os.close(descriptor)
        libc = ctypes.CDLL(None, use_errno=True)
        if libc.renameat2(AT_FDCWD, os.fsencode(staging), AT_FDCWD, os.fsencode(directory), RENAME_NOREPLACE):
            error = ctypes.get_errno()
            if error == errno.EEXIST:
                fail("incompatible_environment", "The workbench environment path is already occupied")
            raise OSError(error, os.strerror(error), str(directory))
    except BaseException:
        shutil.rmtree(staging)
        raise


def prepare(request_path, directory, setup_path, claim):
    import runpy
    request = load(request_path)
    roots = request["roots"]
    if not roots:
        fail("unsupported_roots", "Bubblewrap requires at least one registered Git root")
    parents = [Path(root["parent_root"]).resolve() for root in roots]
    if len(set(parents)) != len(parents) or any(a != b and a in b.parents for a in parents for b in parents):
        fail("unsupported_roots", "Duplicate or nested Git roots need distinct source ownership; no roots were flattened")
    anchor = Path(os.path.commonpath(parents)) if len(parents) > 1 else parents[0]
    records = [{"root": str(Path(item["worktree_root"]).resolve()), "logical_root": str(Path(LOGICAL_ROOT) / parents[index].relative_to(anchor)), "parent_root": str(parents[index]), "configuration": request["configurations"][index]} for index, item in enumerate(roots)]
    support = probe()
    if support["bubblewrap"]["kind"] != "available":
        result = support["bubblewrap"]
        fail(result["reason"], result["message"])
    for record in records:
        check_reflink(record["root"])
    blockers = []
    setup = runpy.run_path(setup_path)
    for index, record in enumerate(records):
        root = Path(record["root"])
        logical = record["logical_root"]
        config = record["configuration"]
        if request["automatic"][index]:
            try:
                setup["configure"](root, logical, config, records, guard_tool_query)
            except setup["SetupFailure"] as error:
                fail("runtime_configuration_invalid", str(error))
            except setup["ToolsUnavailable"]:
                config["snapshot"] = None
        validate_configuration(config)
    config = json.loads(json.dumps(records[0]["configuration"]))
    # Runtime references are project-wide; conflicting per-root definitions are
    # explicit configuration errors rather than last-root-wins behavior.
    for record in records[1:]:
        other = record["configuration"]
        for key in ("environment", "runtime_files"):
            for name, value in other[key].items():
                if name in config[key] and config[key][name] != value:
                    fail("runtime_configuration_invalid", "Workspace roots declare conflicting runtime references")
                config[key][name] = value
        for key in ("remove_environment", "writable_paths", "runtime_directories", "runtime_file_bindings", "readonly_bindings", "services"):
            for value in other[key]:
                if value not in config[key]:
                    config[key].append(value)
    validate_configuration(config)
    directory = Path(directory).resolve()
    acquire_environment(directory, claim)
    try:
        root = Path(records[0]["root"])
        common = Path(os.fsdecode(git(root, "rev-parse", "--path-format=absolute", "--git-common-dir")).strip())
        snapshots = common / "tyde-workbench-snapshots-v2"
        snapshots.mkdir(mode=0o700, exist_ok=True)
        manifest = {"schema": SCHEMA, "artifact_contract": ARTIFACT_CONTRACT, "root": str(root), "roots": records, "common": str(common), "snapshot_root": str(snapshots), "configuration": config, "support": support, "python": PYTHON, "bwrap": shutil.which("bwrap")}
        workspace_clean(manifest)
        (directory / "receipts").mkdir()
        (directory / "runs").mkdir()
        save(directory / "owner.json", {"schema": SCHEMA, "environment": str(directory), "source": str(root)})
        save(directory / "manifest.json", manifest)
        shutil.copyfile(__file__, directory / "runner.py")
        initialization = {"kind": "cold"}
        if any(record["configuration"]["snapshot"] is not None for record in records):
            identity = workspace_identity(manifest, runtime_environment(config, records[0]["logical_root"]))
            key = cohort_key(None, identity)
            cohort = snapshots / key
            if cohort.exists():
                receipt = load(cohort / "cohort.json")
                if receipt["schema"] != SCHEMA or receipt["key"] != key or receipt["inputs"] != identity:
                    fail("incompatible_environment", "Stored cohort provenance does not match this environment")
                if workspace_artifacts(manifest, cohort / "files") != receipt["artifacts"]:
                    fail("incompatible_environment", "Stored cohort artifacts failed integrity verification")
                for index, record in enumerate(records):
                    root = Path(record["root"])
                    selected = record["configuration"]["snapshot"]
                    if selected is not None:
                        files = artifact_root(cohort / "files", index, len(records))
                        for source in included_paths(files, record["configuration"]):
                            relative = source.relative_to(files)
                            destination = root / relative
                            if destination.exists() or destination.is_symlink():
                                fail("incompatible_environment", "Snapshot destination already exists; refusing to overwrite source or arbitrary artifacts")
                            destination.parent.mkdir(parents=True, exist_ok=True)
                            strict_clone(source, destination)
                    current = source_inventory(root)
                    expected = receipt["source"][record["logical_root"]]
                    if current.keys() != expected.keys():
                        fail("incompatible_environment", "Cohort source inventory differs")
                    for relative, entry in expected.items():
                        actual = dict(current[relative], mtime_ns=entry["mtime_ns"])
                        if actual != entry:
                            fail("incompatible_environment", "Cohort source identity differs")
                        os.utime(root / relative, ns=(entry["mtime_ns"], entry["mtime_ns"]), follow_symlinks=False)
                initialization = {"kind": "snapshot", "cohort": key}
        if any(record["configuration"]["snapshot"] is not None for record in records):
            save(directory / "artifact-inputs.json", {"inputs": identity, "source": workspace_sources(manifest)})
        return {"kind": "bubblewrap", "manifest_path": str(directory / "manifest.json"), "logical_root": records[0]["logical_root"], "additional_roots": [record["logical_root"] for record in records[1:]], "build_blockers": blockers, "support": support, "initialization": initialization}
    except BaseException:
        shutil.rmtree(directory)
        raise


def owned_environment(manifest_path, expected_root=None):
    manifest_path = Path(manifest_path).absolute()
    directory = manifest_path.parent
    owner = load(directory / "owner.json")
    if manifest_path.name != "manifest.json" or owner["schema"] not in (2, SCHEMA) or owner["environment"] != str(directory) or directory.is_symlink():
        fail("incompatible_environment", "Workbench resource ownership is invalid")
    manifest = load(manifest_path)
    if "roots" in manifest and (not manifest["roots"] or manifest["roots"][0]["root"] != manifest["root"]):
        fail("incompatible_environment", "Workbench primary root differs from its authoritative mount record")
    if manifest["schema"] != owner["schema"] or owner["source"] != manifest["root"]:
        fail("incompatible_environment", "Workbench source ownership is invalid")
    if expected_root is not None and manifest["root"] != str(Path(expected_root).resolve()):
        fail("incompatible_environment", "Execution source does not match the canonical workbench record")
    return manifest


def runtime_binds(manifest, env):
    paths = []
    for value in manifest["configuration"]["writable_paths"]:
        path = Path(expand(value, env))
        if not path.exists():
            fail("runtime_configuration_invalid", "A configured writable runtime reference does not exist")
        paths.append(path)
    args = []
    for path in dict.fromkeys(paths):
        if not path.is_absolute():
            fail("runtime_configuration_invalid", "Writable runtime references must be absolute paths")
        if any(path == engine or path in engine.parents for engine in map(Path, (LOGICAL_ROOT, RUNTIME, RECEIPTS))):
            fail("runtime_configuration_invalid", "A writable runtime reference overlaps engine-owned mountpoints")
        if path.exists():
            args += ["--bind", str(path), str(path)]
    destinations = set()
    for binding in manifest["configuration"].get("readonly_bindings", []):
        source = Path(expand(binding["host_path"], env))
        destination = Path(expand(binding["execution_path"], env))
        if not source.is_absolute() or not source.exists() or not destination.is_absolute() or str(destination) != os.path.normpath(str(destination)):
            fail("runtime_configuration_invalid", "Read-only runtime bindings require existing absolute sources and canonical absolute destinations")
        if not destination.is_relative_to("/mnt") or destination == Path("/mnt") or (destination.parent != Path("/mnt") and not destination.is_relative_to(LOGICAL_ROOT)) or any(destination == engine or destination in engine.parents or engine in destination.parents for engine in map(Path, (RUNTIME, RECEIPTS))):
            fail("runtime_configuration_invalid", "Read-only runtime bindings must stay inside private non-engine mountpoints")
        if any(destination == Path(record["logical_root"]) or destination in Path(record["logical_root"]).parents or Path(record["logical_root"]) in destination.parents for record in root_records(manifest)):
            fail("runtime_configuration_invalid", "Read-only runtime bindings must not overlap registered writable source roots")
        if destination in destinations:
            fail("runtime_configuration_invalid", "Read-only runtime bindings have conflicting destinations")
        destinations.add(destination)
        args += ["--ro-bind", str(source), str(destination)]
    return args


def private_mountpoints():
    # Engine mountpoints must not create directories or symlinks on the host.
    # Restore existing /mnt entries before applying narrower runtime overlays.
    args = ["--tmpfs", "/mnt"]
    reserved = {LOGICAL_ROOT, RUNTIME, RECEIPTS}
    for path in sorted(Path("/mnt").iterdir()):
        if str(path) in reserved:
            continue
        if path.is_symlink():
            args += ["--symlink", os.readlink(path), str(path)]
        else:
            args += ["--dev-bind", str(path), str(path)]
    return args


def own_descendants():
    if ctypes.CDLL(None, use_errno=True).prctl(36, 1, 0, 0, 0):
        fail("incompatible_environment", "Could not establish runtime descendant ownership")


def namespace(endpoint):
    with socket.socket(socket.AF_UNIX, socket.SOCK_SEQPACKET) as connection:
        connection.connect(endpoint)
        descriptors = array.array("i", [os.open("/proc/self/ns/user", os.O_RDONLY), os.open("/proc/self/ns/mnt", os.O_RDONLY)])
        try:
            connection.sendmsg([b"namespace"], [(socket.SOL_SOCKET, socket.SCM_RIGHTS, descriptors)])
            connection.recv(1)
        finally:
            for descriptor in descriptors:
                os.close(descriptor)


def enter_namespace(descriptors, cwd):
    libc = ctypes.CDLL(None, use_errno=True)
    for descriptor in descriptors:
        if libc.setns(descriptor, 0):
            raise OSError(ctypes.get_errno(), "Could not enter workbench namespace")
    # Joining the user namespace grants setup capabilities. Commands do not
    # need them; their own backend remains responsible for its permission mode.
    header = (ctypes.c_uint32 * 2)(0x20080522, 0)
    data = (ctypes.c_uint32 * 6)()
    if libc.capset(header, data):
        raise OSError(ctypes.get_errno(), "Could not release namespace setup capabilities")
    os.chdir(cwd)


def guarded_launch(args, env, run, command, cwd, descriptor_path=None):
    # The guardian survives the caller's process-group kill, retaining its lease
    # until even orphaned/setsid native descendants have been killed and joined.
    # The runtime takes controlling-terminal ownership after entering its own
    # session; pipe-based providers never touch the host's controlling terminal.
    if os.isatty(0):
        previous_hup = signal.signal(signal.SIGHUP, signal.SIG_IGN)
        try:
            fcntl.ioctl(0, termios.TIOCNOTTY)
        finally:
            signal.signal(signal.SIGHUP, previous_hup)
    control, guardian_control = socket.socketpair(socket.AF_UNIX, socket.SOCK_SEQPACKET)
    parentfd = os.pidfd_open(os.getpid())
    guardian = os.fork()
    if guardian == 0:
        control.close()
        result = None
        listener = None
        runtimefd = None
        namespace_connection = None
        builder = None
        child = None
        childfd = None
        descriptors = array.array("i")
        try:
            os.setsid()
            own_descendants()
            listener = socket.socket(socket.AF_UNIX, socket.SOCK_SEQPACKET)
            directoryfd = os.open(run, os.O_RDONLY | os.O_DIRECTORY)
            try:
                listener.bind("/proc/self/fd/%d/control.sock" % directoryfd)
            finally:
                os.close(directoryfd)
            listener.listen(2)
            pending_signals = []
            for signum in (signal.SIGTERM, signal.SIGINT, signal.SIGHUP):
                signal.signal(signum, signal.SIG_IGN)
            # Bubblewrap builds mounts only. Exec from its host parent so the
            # build environment does not inherit bwrap's AppArmor child profile.
            builder = subprocess.Popen(args, env=env, start_new_session=True)
            builderfd = os.pidfd_open(builder.pid)
            try:
                with selectors.DefaultSelector() as events:
                    events.register(parentfd, selectors.EVENT_READ, "parent")
                    events.register(guardian_control, selectors.EVENT_READ, "control")
                    events.register(builderfd, selectors.EVENT_READ, "builder")
                    events.register(listener, selectors.EVENT_READ, "runtime")
                    while result is None:
                        for key, _ in events.select():
                            if key.data == "builder":
                                code = builder.wait()
                                if code == 0:
                                    fail("incompatible_environment", "Namespace builder closed before its command settled")
                                result = code
                                break
                            if key.data == "child":
                                result = child.wait()
                                break
                            if key.data == "parent":
                                result = -signal.SIGKILL
                                break
                            if key.data == "runtime":
                                connection, _ = listener.accept()
                                message, ancillary, flags, _ = connection.recvmsg(16, socket.CMSG_SPACE(2 * descriptors.itemsize))
                                if message == b"namespace" and namespace_connection is None:
                                    namespace_connection = connection
                                    for level, kind, data in ancillary:
                                        if level == socket.SOL_SOCKET and kind == socket.SCM_RIGHTS:
                                            descriptors.frombytes(data[:len(data) - len(data) % descriptors.itemsize])
                                    if flags & socket.MSG_CTRUNC or len(descriptors) != 2:
                                        fail("incompatible_environment", "Namespace builder omitted its user/mount descriptors")
                                    runtime_command = [PYTHON, RUNTIME + "/runner.py", "inside", *command]
                                    child = subprocess.Popen(runtime_command, env=env, start_new_session=True, preexec_fn=lambda: enter_namespace(descriptors, cwd))
                                    for descriptor in descriptors:
                                        os.close(descriptor)
                                    descriptors = array.array("i")
                                    childfd = os.pidfd_open(child.pid)
                                    events.register(childfd, selectors.EVENT_READ, "child")
                                    runtime_diagnostic("namespace_entered", builder_pid=builder.pid, command_pid=child.pid)
                                    continue
                                with connection:
                                    if message != b"runtime" or child is None:
                                        fail("incompatible_environment", "Unexpected namespace readiness notification")
                                    runtimefd = os.pidfd_open(child.pid)
                                    connection.send(b"start")
                                if descriptor_path is not None:
                                    with socket.socket(socket.AF_UNIX) as descriptor:
                                        descriptor.connect(descriptor_path)
                                        value = os.fsencode(run)
                                        descriptor.sendall(struct.pack("!Q", len(value)) + value)
                                events.unregister(listener)
                                listener.close()
                                listener = None
                                for signum in pending_signals:
                                    signal.pidfd_send_signal(runtimefd, signum)
                                pending_signals.clear()
                                continue
                            message = guardian_control.recv(64)
                            if not message:
                                result = -signal.SIGKILL
                                break
                            signum = int(message)
                            if runtimefd is None:
                                pending_signals.append(signum)
                            else:
                                try:
                                    signal.pidfd_send_signal(runtimefd, signum)
                                except ProcessLookupError:
                                    pass
            finally:
                if child is not None:
                    try:
                        os.killpg(child.pid, signal.SIGKILL)
                    except ProcessLookupError:
                        pass
                    child.wait()
                    if childfd is not None:
                        os.close(childfd)
                if namespace_connection is not None:
                    namespace_connection.close()
                    namespace_connection = None
                try:
                    os.killpg(builder.pid, signal.SIGKILL)
                except ProcessLookupError:
                    pass
                builder.wait()
                os.close(builderfd)
        except Failure as error:
            result = {"error": {"reason": error.reason, "message": error.message}}
        except (OSError, ValueError, subprocess.SubprocessError) as error:
            result = {"error": {"reason": "incompatible_environment", "message": "Execution guardian failed at an OS boundary (" + type(error).__name__ + ", errno=" + str(getattr(error, "errno", None)) + ")"}}
        finally:
            try:
                join_children(terminate=True)
                shutil.rmtree(run)
            except (Failure, OSError) as error:
                result = {"error": {"reason": "incompatible_environment", "message": "Execution guardian could not finish owned teardown (" + type(error).__name__ + ")"}}
            try:
                guardian_control.send(json.dumps(result).encode())
            except BrokenPipeError:
                pass
            guardian_control.close()
            for descriptor in descriptors:
                os.close(descriptor)
            if namespace_connection is not None:
                namespace_connection.close()
            if listener is not None:
                listener.close()
            if runtimefd is not None:
                os.close(runtimefd)
            os.close(parentfd)
            os._exit(0)
    guardian_control.close()
    os.close(parentfd)
    previous = {}
    def forward(signum, _frame):
        try:
            control.send(str(signum).encode())
        except BrokenPipeError:
            pass
    try:
        for signum in (signal.SIGTERM, signal.SIGINT, signal.SIGHUP):
            previous[signum] = signal.signal(signum, forward)
        message = control.recv(4096)
        _, status = os.waitpid(guardian, 0)
        if not message or status:
            fail("incompatible_environment", "Execution guardian exited without a completion result")
        result = json.loads(message)
        if isinstance(result, dict):
            fail(result["error"]["reason"], result["error"]["message"])
        return result
    finally:
        control.close()
        for signum, handler in previous.items():
            signal.signal(signum, handler)


def launch(manifest_path, expected_root, cwd, command, configuration_identity=None, runner_identity=None, descriptor_path=None):
    manifest_path = Path(manifest_path).resolve()
    manifest = owned_environment(manifest_path, expected_root)
    config = manifest["configuration"]
    if configuration_identity is None:
        env = runtime_environment(config, root_records(manifest)[0]["logical_root"])
    else:
        if runner_identity != digest(Path(__file__).read_bytes()):
            fail("incompatible_environment", "The execution runner changed while its backend was active")
        if configuration_identity != digest(json.dumps(config, sort_keys=True).encode()):
            fail("runtime_configuration_invalid", "The execution configuration changed while its backend was active")
        env = dict(os.environ)
        env["PATH"] = RUNTIME + "/bin:" + env["PATH"]
    env["PWD"] = cwd
    with admission(manifest_path):
        directory = manifest_path.parent
        with tempfile.TemporaryDirectory(prefix="run-", dir=directory / "runs") as run:
            run = Path(run)
            shutil.copyfile(directory / "runner.py", run / "runner.py")
            save(run / "manifest.json", manifest)
            (run / "bin").mkdir()
            for value in config["runtime_directories"]:
                (run / relative_path(value)).mkdir(parents=True, exist_ok=False)
            for name, contents in config["runtime_files"].items():
                path = run / relative_path(name)
                path.parent.mkdir(parents=True, exist_ok=True)
                path.write_text(expand(contents, env))
            args = [manifest["bwrap"], "--unshare-user", "--dev-bind", "/", "/"]
            args += private_mountpoints()
            args += runtime_binds(manifest, env)
            for binding in config["runtime_file_bindings"]:
                destination = Path(expand(binding["host_path"], env))
                if not destination.is_absolute() or not destination.is_file():
                    fail("runtime_configuration_invalid", "A configured native runtime file binding is absent or not an absolute file")
                args += ["--ro-bind", str(run / binding["runtime_file"]), str(destination)]
            args += ["--ro-bind", manifest["snapshot_root"], manifest["snapshot_root"]]
            for record in root_records(manifest):
                args += ["--bind", record["root"], record["logical_root"]]
            args += ["--bind", str(run), RUNTIME, "--bind", str(directory / "receipts"), RECEIPTS, "--chdir", cwd, "--", str(Path(manifest["python"]).resolve(strict=True)), RUNTIME + "/runner.py", "namespace", RUNTIME + "/control.sock"]
            return guarded_launch(args, env, run, command, cwd, descriptor_path)


def join_children(excluded=(), services=(), terminate=False):
    pending = {}
    with selectors.DefaultSelector() as events:
        for pidfd in services:
            events.register(pidfd, selectors.EVENT_READ, "service")
        try:
            while True:
                children = Path("/proc/self/task/%d/children" % os.getpid()).read_text().split()
                children = [int(pid) for pid in children if int(pid) not in excluded]
                runtime_diagnostic("descendants_observed", count=len(children), terminate=terminate)
                for pid in children:
                    if pid not in pending:
                        pidfd = os.pidfd_open(pid)
                        pending[pid] = pidfd
                        events.register(pidfd, selectors.EVENT_READ, pid)
                        if terminate:
                            os.kill(pid, signal.SIGKILL)
                ready = events.select(0)
                if any(key.data == "service" for key, _ in ready):
                    fail("runtime_service_failed", "An owned runtime service exited before command descendants settled")
                if not pending:
                    return
                if not ready:
                    ready = events.select()
                if any(key.data == "service" for key, _ in ready):
                    fail("runtime_service_failed", "An owned runtime service exited before command descendants settled")
                for key, _ in ready:
                    pid = key.data
                    _, status = os.waitpid(pid, 0)
                    code = os.waitstatus_to_exitcode(status)
                    runtime_diagnostic("descendant_joined", pid=pid, exit_code=code, terminate=terminate)
                    events.unregister(pending[pid])
                    os.close(pending.pop(pid))
        finally:
            for pidfd in pending.values():
                os.close(pidfd)


def read_exact(connection, length):
    value = bytearray()
    while len(value) < length:
        chunk = connection.recv(length - len(value))
        if not chunk:
            fail("incompatible_environment", "Execution connection closed during a request")
        value.extend(chunk)
    return bytes(value)


def execute(endpoint, plan_path):
    encoded = Path(plan_path).read_bytes()
    if len(encoded) > 64 * 1024 * 1024:
        fail("incompatible_environment", "Execution request exceeds its protocol limit")
    with socket.socket(socket.AF_UNIX) as connection:
        connection.connect(endpoint)
        descriptors = array.array("i", [0, 1, 2])
        sent = connection.sendmsg([struct.pack("!Q", len(encoded))], [(socket.SOL_SOCKET, socket.SCM_RIGHTS, descriptors)])
        if sent != 8:
            fail("incompatible_environment", "Execution descriptor transfer was incomplete")
        connection.sendall(encoded)
        previous = {}
        def forward(signum, _frame):
            try:
                connection.sendall((json.dumps({"signal": signum}) + "\n").encode())
            except BrokenPipeError:
                pass
        try:
            for signum in (signal.SIGTERM, signal.SIGINT, signal.SIGHUP):
                previous[signum] = signal.signal(signum, forward)
            result = json.loads(connection.makefile("rb").readline())
            if "error" in result:
                fail(result["error"]["reason"], result["error"]["message"])
            return result["exit_code"]
        finally:
            for signum, handler in previous.items():
                signal.signal(signum, handler)


def execution_request(connection):
    own_descendants()
    child = None
    descriptors = array.array("i")
    try:
        for signum in (signal.SIGTERM, signal.SIGINT, signal.SIGHUP):
            signal.signal(signum, signal.SIG_DFL)
        header, ancillary, flags, _ = connection.recvmsg(8, socket.CMSG_SPACE(3 * descriptors.itemsize))
        for level, kind, data in ancillary:
            if level == socket.SOL_SOCKET and kind == socket.SCM_RIGHTS:
                descriptors.frombytes(data[:len(data) - len(data) % descriptors.itemsize])
        if flags & socket.MSG_CTRUNC or len(descriptors) != 3:
            fail("incompatible_environment", "Execution request requires exactly three stdio descriptors")
        header += read_exact(connection, 8 - len(header))
        length, = struct.unpack("!Q", header)
        if length > 64 * 1024 * 1024:
            fail("incompatible_environment", "Execution request exceeds its protocol limit")
        plan = json.loads(read_exact(connection, length))
        environment = dict(os.environb)
        for key, value in plan["environment"]:
            key = bytes(key)
            if value is None:
                environment.pop(key, None)
            else:
                environment[key] = bytes(value)
        cwd = None if plan["cwd"] is None else bytes(plan["cwd"])
        child = subprocess.Popen([bytes(value) for value in plan["command"]], cwd=cwd, env=environment,
                                 stdin=descriptors[0], stdout=descriptors[1], stderr=descriptors[2], start_new_session=True)
        for descriptor in descriptors:
            os.close(descriptor)
        descriptors = array.array("i")
        childfd = os.pidfd_open(child.pid)
        try:
            with selectors.DefaultSelector() as events:
                events.register(childfd, selectors.EVENT_READ, "child")
                events.register(connection, selectors.EVENT_READ, "control")
                buffer = bytearray()
                settled = False
                while not settled:
                    for key, _ in events.select():
                        if key.data == "child":
                            settled = True
                            break
                        data = connection.recv(65536)
                        if not data:
                            settled = True
                            break
                        buffer.extend(data)
                        while b"\n" in buffer:
                            message, _, buffer = buffer.partition(b"\n")
                            signum = json.loads(message)["signal"]
                            if signum not in (signal.SIGTERM, signal.SIGINT, signal.SIGHUP):
                                fail("incompatible_environment", "Execution control signal is invalid")
                            try:
                                os.killpg(child.pid, signum)
                            except ProcessLookupError:
                                pass
            result = child.poll()
            if result is None:
                result = -signal.SIGKILL
        finally:
            os.close(childfd)
        response = {"exit_code": result}
    except Failure as error:
        response = {"error": {"reason": error.reason, "message": error.message}}
    except (OSError, ValueError, KeyError, subprocess.SubprocessError) as error:
        response = {"error": {"reason": "incompatible_environment", "message": "Active execution request failed at an OS boundary (" + type(error).__name__ + ", errno=" + str(getattr(error, "errno", None)) + ")"}}
    finally:
        if child is not None:
            try:
                os.killpg(child.pid, signal.SIGKILL)
            except ProcessLookupError:
                pass
            child.wait()
        for descriptor in descriptors:
            os.close(descriptor)
        join_children(terminate=True)
    try:
        connection.sendall((json.dumps(response) + "\n").encode())
    except BrokenPipeError:
        pass
    connection.close()


def accept_execution(listener, requests, events):
    connection, _ = listener.accept()
    pid = os.fork()
    if pid == 0:
        listener.close()
        for other, _ in requests.values():
            other.close()
        try:
            execution_request(connection)
        finally:
            os._exit(0)
    pidfd = os.pidfd_open(pid)
    requests[pid] = (connection, pidfd)
    events.register(pidfd, selectors.EVENT_READ, ("request", pid))


def finish_execution(requests):
    for connection, _ in requests.values():
        connection.shutdown(socket.SHUT_RDWR)
    with selectors.DefaultSelector() as events:
        for pid, (_, pidfd) in requests.items():
            events.register(pidfd, selectors.EVENT_READ, pid)
        while requests:
            for key, _ in events.select():
                pid = key.data
                os.waitpid(pid, 0)
                connection, pidfd = requests.pop(pid)
                events.unregister(pidfd)
                os.close(pidfd)
                connection.close()


def inside(command):
    own_descendants()
    terminal = os.isatty(0)
    if terminal:
        if os.getsid(0) != os.getpid():
            os.setsid()
        fcntl.ioctl(0, termios.TIOCSCTTY, 0)
        signal.signal(signal.SIGTTOU, signal.SIG_IGN)
    manifest = load(RUNTIME + "/manifest.json")
    env = dict(os.environ)
    services = []
    child = None
    failed_cleanup = False
    requests = {}
    executor = socket.socket(socket.AF_UNIX)
    executor.bind(RUNTIME + "/execution.sock")
    executor.listen()
    def forward(signum, _frame):
        if child is None:
            fail("incompatible_environment", "Execution was cancelled before its native command started")
        try:
            os.killpg(child.pid, signum)
        except ProcessLookupError:
            pass
    for signum in (signal.SIGTERM, signal.SIGINT, signal.SIGHUP):
        signal.signal(signum, forward)
    with socket.socket(socket.AF_UNIX, socket.SOCK_SEQPACKET) as control:
        control.connect(RUNTIME + "/control.sock")
        control.send(b"runtime")
        if control.recv(16) != b"start":
            fail("incompatible_environment", "Execution guardian did not admit the native command")
    try:
        for spec in manifest["configuration"]["services"]:
            listener = socket.socket(socket.AF_UNIX)
            listener.bind(str(Path(RUNTIME) / spec["ready_notification"]))
            listener.listen(1)
            log = tempfile.TemporaryFile()
            service_env = dict(env, **{key: expand(value, env) for key, value in spec["environment"].items()})
            process = subprocess.Popen([expand(value, env) for value in spec["command"]], env=service_env, stdin=subprocess.DEVNULL, stdout=log, stderr=log, start_new_session=True)
            pidfd = os.pidfd_open(process.pid)
            services.append((process, pidfd, spec, log))
            runtime_diagnostic("service_started", pid=process.pid)
            try:
                with selectors.DefaultSelector() as events:
                    events.register(listener, selectors.EVENT_READ, "ready")
                    for _, servicefd, _, _ in services:
                        events.register(servicefd, selectors.EVENT_READ, "exit")
                    if any(key.data == "exit" for key, _ in events.select()):
                        fail("runtime_service_failed", "An owned runtime service exited before its readiness notification")
                connection, _ = listener.accept()
                with connection:
                    if not connection.recv(65536):
                        fail("runtime_service_failed", "An owned runtime service sent an empty readiness notification")
                if os.readlink("/proc/self/ns/mnt") != os.readlink("/proc/%d/ns/mnt" % process.pid):
                    fail("runtime_service_failed", "A runtime service is outside its workbench mount namespace")
            finally:
                listener.close()
        def command_foreground():
            os.setpgrp()
            if terminal:
                # Claim the terminal before exec can read and stop on SIGTTIN.
                os.tcsetpgrp(0, os.getpgrp())
        child = subprocess.Popen(command, env=env, preexec_fn=command_foreground)
        runtime_diagnostic("command_started", pid=child.pid, terminal=terminal)
        if terminal:
            runtime_diagnostic("terminal_foreground", process_group=os.tcgetpgrp(0))
        childfd = os.pidfd_open(child.pid)
        try:
            with selectors.DefaultSelector() as events:
                events.register(childfd, selectors.EVENT_READ, "child")
                for _, pidfd, _, _ in services:
                    events.register(pidfd, selectors.EVENT_READ, "service")
                events.register(executor, selectors.EVENT_READ, "execution")
                settled = False
                while not settled:
                    for key, _ in events.select():
                        if key.data == "service":
                            fail("runtime_service_failed", "An owned runtime service exited during command execution")
                        if key.data == "child":
                            settled = True
                            break
                        if key.data == "execution":
                            accept_execution(executor, requests, events)
                            continue
                        _, pid = key.data
                        os.waitpid(pid, 0)
                        connection, pidfd = requests.pop(pid)
                        events.unregister(pidfd)
                        os.close(pidfd)
                        connection.close()
            executor.close()
            finish_execution(requests)
            result = child.wait()
            runtime_diagnostic("command_exited", exit_code=result)
            join_children(
                {process.pid for process, _, _, _ in services},
                [pidfd for _, pidfd, _, _ in services],
                terminate=not services,
            )
            return result
        finally:
            os.close(childfd)
    finally:
        executor.close()
        finish_execution(requests)
        if child is not None:
            try:
                os.killpg(child.pid, signal.SIGKILL)
            except ProcessLookupError:
                pass
            child.wait()
        if terminal:
            os.tcsetpgrp(0, os.getpgrp())
        for process, pidfd, spec, log in reversed(services):
            if process.poll() is not None:
                failed_cleanup = True
            else:
                if spec["stop_command"]:
                    stopped = subprocess.run([expand(value, env) for value in spec["stop_command"]], env=env, stdin=subprocess.DEVNULL, stdout=log, stderr=log)
                    if stopped.returncode:
                        failed_cleanup = True
                        process.terminate()
                else:
                    process.terminate()
            code = process.wait()
            if spec["stop_command"] and code:
                failed_cleanup = True
            os.close(pidfd)
            log.close()
        join_children(terminate=True)
        if failed_cleanup:
            fail("runtime_service_failed", "Owned runtime service shutdown failed; resources were joined")


def snapshot(manifest_path, expected_root):
    with admission(manifest_path, exclusive=True):
        manifest = owned_environment(manifest_path, expected_root)
        directory = Path(manifest_path).parent
        if manifest["schema"] == 2:
            return {"published": False, "reason": "Legacy producer-only environment; artifacts discarded"}
        if manifest.get("artifact_contract") != ARTIFACT_CONTRACT:
            fail("incompatible_environment", "Unknown artifact compatibility contract")
        if not any(record["configuration"]["snapshot"] is not None for record in root_records(manifest)):
            return {"published": False, "reason": "No artifact boundaries; cold workbench removed"}
        authority = load(directory / "artifact-inputs.json")
        inputs = workspace_identity(manifest, runtime_environment(manifest["configuration"], root_records(manifest)[0]["logical_root"]))
        source_inventory = workspace_sources(manifest)
        if authority["inputs"] != inputs or authority["source"] != source_inventory:
            return {"published": False, "reason": "Artifact inputs changed; incompatible artifacts discarded"}
        workspace_clean(manifest)
        for record in root_records(manifest):
            selected = record["configuration"]["snapshot"]
            if selected is not None and any(not (Path(record["root"]) / value).exists() and not (Path(record["root"]) / value).is_symlink() for value in selected["include"]):
                return {"published": False, "reason": "Declared artifacts absent; cold workbench removed"}
        artifacts = workspace_artifacts(manifest)
        if not any(artifacts.values()):
            return {"published": False, "reason": "No existing artifacts; cold workbench removed"}
        receipt = {"schema": SCHEMA, "inputs": inputs, "key": cohort_key(None, inputs), "source": source_inventory, "artifacts": artifacts}
        snapshots = Path(manifest["snapshot_root"])
        destination = snapshots / receipt["key"]
        if destination.exists():
            existing = load(destination / "cohort.json")
            if existing["inputs"] != inputs or workspace_artifacts(manifest, destination / "files") != existing["artifacts"]:
                fail("incompatible_environment", "Existing immutable cohort failed provenance or integrity verification")
            return {"published": True, "cohort": receipt["key"]}
        staging = Path(tempfile.mkdtemp(prefix=".preparing-", dir=snapshots))
        try:
            records = root_records(manifest)
            (staging / "files").mkdir()
            for index, record in enumerate(records):
                if record["configuration"]["snapshot"] is None:
                    continue
                root = Path(record["root"])
                for source in included_paths(root, record["configuration"]):
                    target = artifact_root(staging / "files", index, len(records)) / source.relative_to(root)
                    target.parent.mkdir(parents=True, exist_ok=True)
                    strict_clone(source, target)
            if workspace_artifacts(manifest, staging / "files") != receipt["artifacts"]:
                fail("incompatible_environment", "CoW snapshot failed metadata/content integrity verification")
            if source_inventory != workspace_sources(manifest) or inputs != workspace_identity(manifest, runtime_environment(manifest["configuration"], root_records(manifest)[0]["logical_root"])):
                fail("incompatible_environment", "Artifact inputs changed while sealing")
            save(staging / "cohort.json", receipt)
            os.rename(staging, destination)
        finally:
            if staging.exists():
                shutil.rmtree(staging)
        return {"published": True, "cohort": receipt["key"]}


def main():
    action, *arguments = sys.argv[1:]
    if action == "probe":
        return probe()
    if action == "prepare":
        return prepare(*arguments)
    if action == "environment":
        manifest_path, expected_root = arguments
        with admission(manifest_path):
            manifest = owned_environment(manifest_path, expected_root)
            # Refresh the executable, not the stored paths, native identities,
            # provenance or artifacts of already-created workbenches.
            helper = Path(manifest_path).with_name("runner.py")
            with tempfile.NamedTemporaryFile(dir=helper.parent, delete=False) as staged:
                temporary = Path(staged.name)
                staged.write(Path(__file__).read_bytes())
            try:
                os.replace(temporary, helper)
            finally:
                temporary.unlink(missing_ok=True)
            values = runtime_environment(manifest["configuration"], root_records(manifest)[0]["logical_root"])
            # Launch owns its private bin prefix; a command's explicit PATH
            # remains an override of this same prepared environment.
            values["PATH"] = values["PATH"].removeprefix(RUNTIME + "/bin:")
            return {"support": probe(), "values": values, "configuration_identity": digest(json.dumps(manifest["configuration"], sort_keys=True).encode()), "runner_identity": digest(Path(__file__).read_bytes())}
    if action == "execute":
        sys.exit(execute(*arguments))
    if action == "launch_prepared_context":
        manifest, expected_root, cwd, descriptor, configuration_identity, runner_identity, *command = arguments
        sys.exit(launch(manifest, expected_root, cwd, command, configuration_identity, runner_identity, descriptor))
    if action == "launch_context":
        manifest, expected_root, cwd, descriptor, *command = arguments
        sys.exit(launch(manifest, expected_root, cwd, command, descriptor_path=descriptor))
    if action == "launch_prepared":
        manifest, expected_root, cwd, configuration_identity, runner_identity, *command = arguments
        sys.exit(launch(manifest, expected_root, cwd, command, configuration_identity, runner_identity))
    if action == "launch":
        manifest, expected_root, cwd, *command = arguments
        sys.exit(launch(manifest, expected_root, cwd, command))
    if action == "namespace":
        return namespace(*arguments)
    if action == "inside":
        sys.exit(inside(arguments))
    if action == "snapshot":
        return snapshot(*arguments)
    if action == "remove":
        directory = Path(arguments[0]).parent
        if not directory.exists():
            return {"removed": False, "already_absent": True}
        with admission(arguments[0], exclusive=True):
            owned_environment(arguments[0])
            shutil.rmtree(directory)
        return {"removed": True}
    fail("incompatible_environment", "Unknown workbench engine operation")


if __name__ == "__main__":
    try:
        result = main()
        if result is not None:
            print(json.dumps(result, sort_keys=True))
    except Failure as error:
        report_failure(error.reason, error.message)
        sys.exit(1)
    except (OSError, ValueError, KeyError, subprocess.SubprocessError) as error:
        report_failure("incompatible_environment", "Workbench engine failed at an OS or artifact boundary (" + type(error).__name__ + ", errno=" + str(getattr(error, "errno", None)) + ", operation=" + sys.argv[1] + ")")
        sys.exit(1)
