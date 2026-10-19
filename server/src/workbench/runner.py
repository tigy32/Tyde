"""Opaque project snapshots and an owned Linux process environment."""
import contextlib
import ctypes
import fcntl
import hashlib
import json
import os
from pathlib import Path
import selectors
import signal
import shlex
import shutil
import socket
import stat
import string
import subprocess
import sys
import tempfile
import uuid

SCHEMA = 2
LOGICAL_ROOT = "/mnt/tyde-workspace"
RUNTIME = "/mnt/tyde-runtime"
RECEIPTS = "/mnt/tyde-producer-receipts"


class Failure(Exception):
    def __init__(self, reason, message):
        self.reason, self.message = reason, message


def fail(reason, message):
    raise Failure(reason, message)


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
    if not hasattr(os, "pidfd_open") or sys.version_info < (3, 9):
        result = unsupported("runtime_tool_missing", "Bubblewrap requires Python 3.9+ and Linux pidfd process completion events")
        return {"bubblewrap": result, "nested_namespaces": result}
    missing = next((name for name in ("bwrap", "cp", "git") if shutil.which(name) is None), None)
    if missing:
        reason = "bubblewrap_missing" if missing == "bwrap" else "runtime_tool_missing"
        result = unsupported(reason, "Required host tool is missing: " + missing)
        return {"bubblewrap": result, "nested_namespaces": result}
    args = [shutil.which("bwrap"), "--unshare-user", "--ro-bind", "/", "/", "--", "/bin/true"]
    result = subprocess.run(args, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
    if result.returncode:
        result = unsupported("namespace_denied", "Host policy prevents an unprivileged Bubblewrap namespace")
        return {"bubblewrap": result, "nested_namespaces": result}
    nested = subprocess.run(args[:-1] + args, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
    return {"bubblewrap": {"kind": "available"}, "nested_namespaces": {"kind": "available"} if nested.returncode == 0 else unsupported("nested_namespaces_denied", "Host policy denies nested namespaces. Provider modes that require their own namespace are unsupported; their security policies are never weakened.")}


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


def runtime_environment(config):
    env = dict(os.environ)
    for key in config["remove_environment"]:
        env.pop(key, None)
    references = dict(env, TYDE_WORKBENCH_ROOT=LOGICAL_ROOT, TYDE_WORKBENCH_RUNTIME=RUNTIME)
    for key, value in config["environment"].items():
        env[key] = expand(value, references)
    env["TYDE_WORKBENCH_ROOT"] = LOGICAL_ROOT
    env["TYDE_WORKBENCH_RUNTIME"] = RUNTIME
    env["PATH"] = RUNTIME + "/bin:" + env["PATH"]
    return env


def validate_configuration(config):
    snapshot = config["snapshot"]
    if snapshot is not None:
        paths = [relative_path(value) for value in snapshot["include"]]
        if not snapshot["producer"]:
            fail("runtime_configuration_invalid", "Snapshots require one explicit configured producer command")
        if not paths or len(set(paths)) != len(paths):
            fail("runtime_configuration_invalid", "Snapshot include paths must be nonempty and unique")
        if any(a != b and a in b.parents for a in paths for b in paths):
            fail("runtime_configuration_invalid", "Snapshot include paths must not overlap")
        if not snapshot["provenance_paths"]:
            fail("runtime_configuration_invalid", "Snapshot reuse requires explicit toolchain/ABI/input provenance paths")
    for value in config["runtime_directories"] + list(config["runtime_files"]):
        path = relative_path(value)
        if path.parts[0] in ("bin", "runner.py", "manifest.json"):
            fail("runtime_configuration_invalid", "Runtime files/directories overlap engine-owned resources")
    for binding in config["runtime_file_bindings"]:
        if binding["runtime_file"] not in config["runtime_files"]:
            fail("runtime_configuration_invalid", "A runtime file binding must name a configured runtime file")
    names, notifications = set(), set()
    for service in config["services"]:
        notification = relative_path(service["ready_notification"])
        if notification.parts[0] in ("bin", "runner.py", "manifest.json") or str(notification) in config["runtime_files"]:
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
    for value in config["snapshot"]["include"]:
        path = root / relative_path(value)
        if not path.exists() and not path.is_symlink():
            fail("incompatible_environment", "A configured snapshot path does not exist after producer completion")
        if not path.parent.resolve().is_relative_to(root.resolve()):
            fail("incompatible_environment", "Snapshot include path traverses a symlink outside its project")
        result.append(path)
    return result


def artifact_inventory(root, config):
    entries = {}
    for selected in included_paths(root, config):
        paths = [selected]
        if selected.is_dir() and not selected.is_symlink():
            for base, dirs, files in os.walk(selected, followlinks=False):
                paths.extend(Path(base) / name for name in dirs + files)
        for path in paths:
            value = entry_identity(path)
            if value["kind"] == "symlink":
                link = Path(value["link"])
                resolved = (path.parent / link).resolve()
                if link.is_absolute() or not resolved.is_relative_to(root.resolve()):
                    fail("incompatible_environment", "Snapshot symlinks must remain relative and inside the project")
            entries[str(path.relative_to(root))] = value
    return entries


def source_inventory(root):
    entries = {}
    for raw in git(root, "ls-files", "-z").split(b"\0"):
        if raw:
            relative = os.fsdecode(raw)
            entries[relative] = entry_identity(root / relative)
    return entries


def assert_clean(root):
    if git(root, "status", "--porcelain=v1", "--untracked-files=normal"):
        fail("incompatible_environment", "Snapshot production requires clean committed source, including untracked inputs")


def input_identity(root, config, env):
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
    inputs["declared_inputs"] = external
    return inputs


def cohort_key(tree, inputs):
    return digest(json.dumps({"schema": SCHEMA, "tree": tree, "inputs": inputs, "logical_root": LOGICAL_ROOT}, sort_keys=True).encode())


@contextlib.contextmanager
def admission(manifest_path, exclusive=False):
    with (Path(manifest_path).parent / "active.lock").open("a+b") as lease:
        try:
            fcntl.flock(lease, fcntl.LOCK_EX if exclusive else (fcntl.LOCK_SH | fcntl.LOCK_NB))
        except BlockingIOError:
            fail("incompatible_environment", "Workbench processes are active or artifacts are being sealed")
        yield


def prepare(root, directory, configuration_path):
    root, directory = Path(root).resolve(), Path(directory).resolve()
    config = load(configuration_path)
    validate_configuration(config)
    support = probe()
    if support["bubblewrap"]["kind"] != "available":
        result = support["bubblewrap"]
        fail(result["reason"], result["message"])
    check_reflink(root)
    directory.mkdir(mode=0o700, parents=True, exist_ok=False)
    try:
        common = Path(os.fsdecode(git(root, "rev-parse", "--path-format=absolute", "--git-common-dir")).strip())
        snapshots = common / "tyde-workbench-snapshots-v2"
        snapshots.mkdir(mode=0o700, exist_ok=True)
        assert_clean(root)
        manifest = {"schema": SCHEMA, "root": str(root), "common": str(common), "snapshot_root": str(snapshots), "configuration": config, "support": support, "python": sys.executable, "bwrap": shutil.which("bwrap")}
        (directory / "receipts").mkdir()
        (directory / "runs").mkdir()
        initialization = {"kind": "cold"}
        if config["snapshot"] is not None:
            inputs = input_identity(root, config, runtime_environment(config))
            tree = git(root, "rev-parse", "HEAD^{tree}").decode().strip()
            key = cohort_key(tree, inputs)
            snapshot = snapshots / key
            if snapshot.exists():
                receipt = load(snapshot / "cohort.json")
                if receipt["schema"] != SCHEMA or receipt["key"] != key or receipt["inputs"] != inputs or receipt["tree"] != tree:
                    fail("incompatible_environment", "Stored cohort provenance does not match this environment")
                if artifact_inventory(snapshot / "files", config) != receipt["artifacts"]:
                    fail("incompatible_environment", "Stored cohort artifacts failed integrity verification")
                for relative in config["snapshot"]["include"]:
                    destination = root / relative
                    if destination.exists() or destination.is_symlink():
                        existing = artifact_inventory(root, {"snapshot": {"include": [relative]}})
                        tracked = source_inventory(root)
                        for name, entry in existing.items():
                            if entry["kind"] != "directory":
                                expected = tracked.get(name)
                                if expected is None or dict(entry, mtime_ns=expected["mtime_ns"]) != expected:
                                    fail("incompatible_environment", "A snapshot destination contains non-source files; refusing to overwrite")
                        if destination.is_dir() and not destination.is_symlink():
                            shutil.rmtree(destination)
                        else:
                            destination.unlink()
                    destination.parent.mkdir(parents=True, exist_ok=True)
                    strict_clone(snapshot / "files" / relative, destination)
                current = source_inventory(root)
                if current.keys() != receipt["source"].keys():
                    fail("incompatible_environment", "Cohort source inventory differs")
                for relative, entry in receipt["source"].items():
                    actual = dict(current[relative], mtime_ns=entry["mtime_ns"])
                    if actual != entry:
                        fail("incompatible_environment", "Cohort source identity differs")
                    os.utime(root / relative, ns=(entry["mtime_ns"], entry["mtime_ns"]), follow_symlinks=False)
                initialization = {"kind": "snapshot", "cohort": key}
        save(directory / "owner.json", {"schema": SCHEMA, "environment": str(directory), "source": str(root)})
        save(directory / "manifest.json", manifest)
        shutil.copyfile(__file__, directory / "runner.py")
        return {"kind": "bubblewrap", "manifest_path": str(directory / "manifest.json"), "logical_root": LOGICAL_ROOT, "support": support, "initialization": initialization}
    except BaseException:
        shutil.rmtree(directory)
        raise


def owned_environment(manifest_path, expected_root=None):
    manifest_path = Path(manifest_path).absolute()
    directory = manifest_path.parent
    owner = load(directory / "owner.json")
    if manifest_path.name != "manifest.json" or owner["schema"] != SCHEMA or owner["environment"] != str(directory) or directory.is_symlink():
        fail("incompatible_environment", "Workbench resource ownership is invalid")
    manifest = load(manifest_path)
    if manifest["schema"] != SCHEMA or owner["source"] != manifest["root"]:
        fail("incompatible_environment", "Workbench source ownership is invalid")
    if expected_root is not None and manifest["root"] != str(Path(expected_root).resolve()):
        fail("incompatible_environment", "Execution source does not match the canonical workbench record")
    return manifest


def runtime_binds(manifest, env):
    home = Path.home()
    paths = [home / ".codex", home / ".cache", home / ".config", home / ".local/share", home / ".local/state", Path("/tmp"), Path(manifest["common"]), Path(manifest["root"])]
    paths += [Path(env[key]) for key in ("CODEX_HOME", "XDG_CONFIG_HOME", "XDG_CACHE_HOME", "XDG_DATA_HOME", "XDG_STATE_HOME") if key in env]
    for value in manifest["configuration"]["writable_paths"]:
        path = Path(expand(value, env))
        if not path.exists():
            fail("runtime_configuration_invalid", "A configured writable runtime reference does not exist")
        paths.append(path)
    args = []
    for path in dict.fromkeys(paths):
        if not path.is_absolute():
            fail("runtime_configuration_invalid", "Writable runtime references must be absolute paths")
        if path.exists():
            args += ["--bind", str(path), str(path)]
    args += ["--ro-bind", manifest["snapshot_root"], manifest["snapshot_root"]]
    return args


def launch(manifest_path, expected_root, cwd, command):
    manifest_path = Path(manifest_path).resolve()
    manifest = owned_environment(manifest_path, expected_root)
    config = manifest["configuration"]
    env = runtime_environment(config)
    with admission(manifest_path):
        directory = manifest_path.parent
        with tempfile.TemporaryDirectory(prefix="run-", dir=directory / "runs") as run:
            run = Path(run)
            shutil.copyfile(directory / "runner.py", run / "runner.py")
            save(run / "manifest.json", manifest)
            (run / "bin").mkdir()
            (run / "bin/tyde-workbench-produce").write_text("#!/bin/sh\nexec " + shlex.quote(manifest["python"]) + " " + RUNTIME + "/runner.py produce \"$@\"\n")
            (run / "bin/tyde-workbench-produce").chmod(0o700)
            for value in config["runtime_directories"]:
                (run / relative_path(value)).mkdir(parents=True, exist_ok=False)
            for name, contents in config["runtime_files"].items():
                path = run / relative_path(name)
                path.parent.mkdir(parents=True, exist_ok=True)
                path.write_text(expand(contents, env))
            args = [manifest["bwrap"], "--unshare-user", "--unshare-pid", "--die-with-parent", "--ro-bind", "/", "/", "--proc", "/proc", "--dev", "/dev"]
            args += runtime_binds(manifest, env)
            for binding in config["runtime_file_bindings"]:
                destination = Path(expand(binding["host_path"], env))
                if not destination.is_absolute() or not destination.is_file():
                    fail("runtime_configuration_invalid", "A configured native runtime file binding is absent or not an absolute file")
                args += ["--ro-bind", str(run / binding["runtime_file"]), str(destination)]
            args += ["--tmpfs", "/mnt", "--bind", manifest["root"], LOGICAL_ROOT, "--bind", str(run), RUNTIME, "--bind", str(directory / "receipts"), RECEIPTS, "--chdir", cwd, "--", manifest["python"], RUNTIME + "/runner.py", "inside", *command]
            return subprocess.run(args, env=env).returncode


def join_children(excluded=(), services=(), terminate=False):
    codes = []
    pending = {}
    with selectors.DefaultSelector() as events:
        for pidfd in services:
            events.register(pidfd, selectors.EVENT_READ, "service")
        try:
            while True:
                children = Path("/proc/self/task/%d/children" % os.getpid()).read_text().split()
                children = [int(pid) for pid in children if int(pid) not in excluded]
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
                    return codes
                if not ready:
                    ready = events.select()
                if any(key.data == "service" for key, _ in ready):
                    fail("runtime_service_failed", "An owned runtime service exited before command descendants settled")
                for key, _ in ready:
                    pid = key.data
                    _, status = os.waitpid(pid, 0)
                    codes.append(os.waitstatus_to_exitcode(status))
                    events.unregister(pending[pid])
                    os.close(pending.pop(pid))
        finally:
            for pidfd in pending.values():
                os.close(pidfd)


def inside(command):
    # Bubblewrap owns namespace PID 1; adopt command/service descendants here
    # so their completion and service failures remain part of our lifecycle.
    if ctypes.CDLL(None, use_errno=True).prctl(36, 1, 0, 0, 0):
        fail("incompatible_environment", "Could not establish runtime descendant ownership")
    manifest = load(RUNTIME + "/manifest.json")
    env = dict(os.environ)
    services = []
    child = None
    failed_cleanup = False
    settled = False
    try:
        for spec in manifest["configuration"]["services"]:
            listener = socket.socket(socket.AF_UNIX)
            listener.bind(str(Path(RUNTIME) / spec["ready_notification"]))
            listener.listen(1)
            log = tempfile.TemporaryFile()
            service_env = dict(env, **{key: expand(value, env) for key, value in spec["environment"].items()})
            process = subprocess.Popen([expand(value, env) for value in spec["command"]], env=service_env, stdin=subprocess.DEVNULL, stdout=log, stderr=log)
            pidfd = os.pidfd_open(process.pid)
            services.append((process, pidfd, spec, log))
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
        child = subprocess.Popen(command, env=env)
        childfd = os.pidfd_open(child.pid)
        try:
            with selectors.DefaultSelector() as events:
                events.register(childfd, selectors.EVENT_READ, "child")
                for _, pidfd, _, _ in services:
                    events.register(pidfd, selectors.EVENT_READ, "service")
                if any(key.data == "service" for key, _ in events.select()):
                    fail("runtime_service_failed", "An owned runtime service exited during command execution")
            result = child.wait()
            descendants = join_children(
                {process.pid for process, _, _, _ in services},
                [pidfd for _, pidfd, _, _ in services],
            )
            settled = True
            return result if result else next((code for code in descendants if code), 0)
        finally:
            os.close(childfd)
    finally:
        if child is not None and child.poll() is None:
            child.kill()
            child.wait()
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
        join_children(terminate=not settled)
        if failed_cleanup:
            fail("runtime_service_failed", "Owned runtime service shutdown failed; resources were joined")


def produce(command):
    manifest = load(RUNTIME + "/manifest.json")
    config = manifest["configuration"]
    if command or config["snapshot"] is None:
        fail("runtime_configuration_invalid", "Invoke tyde-workbench-produce without arguments to run the explicitly configured producer")
    command = [expand(value, dict(os.environ)) for value in config["snapshot"]["producer"]]
    root = Path(LOGICAL_ROOT)
    if not Path.cwd().is_relative_to(root):
        fail("runtime_configuration_invalid", "Snapshot producers must run at the stable logical project path")
    # This process owns and joins even orphaned/background descendants of the
    # explicitly requested producer; daemonizing does not certify completion.
    if ctypes.CDLL(None, use_errno=True).prctl(36, 1, 0, 0, 0):
        fail("incompatible_environment", "Could not establish producer descendant ownership")
    with (Path(RECEIPTS) / "producer.lock").open("a+b") as lease:
        fcntl.flock(lease, fcntl.LOCK_EX)
        receipt_path = Path(RECEIPTS) / "completed.json"
        receipt_path.unlink(missing_ok=True)
        assert_clean(root)
        before = source_inventory(root)
        inputs = input_identity(root, config, dict(os.environ))
        tree = git(root, "rev-parse", "HEAD^{tree}").decode().strip()
        result = subprocess.run(command)
        descendants = join_children()
        if result.returncode or any(descendants):
            fail("incompatible_environment", "The explicit snapshot producer or one of its descendants failed")
        assert_clean(root)
        if before != source_inventory(root) or inputs != input_identity(root, config, dict(os.environ)):
            fail("incompatible_environment", "Snapshot producer inputs changed during execution")
        receipt = {"schema": SCHEMA, "tree": tree, "inputs": inputs, "key": cohort_key(tree, inputs), "source": before, "artifacts": artifact_inventory(root, config), "producer": digest(json.dumps(command).encode())}
        save(receipt_path, receipt)
    return 0


def snapshot(manifest_path, expected_root):
    with admission(manifest_path, exclusive=True):
        manifest = owned_environment(manifest_path, expected_root)
        config = manifest["configuration"]
        directory = Path(manifest_path).parent
        completed = directory / "receipts/completed.json"
        if config["snapshot"] is None or not completed.exists():
            return {"published": False, "reason": "No configured completed producer; no artifacts were advertised as a snapshot"}
        root = Path(manifest["root"])
        assert_clean(root)
        receipt = load(completed)
        inputs = input_identity(root, config, runtime_environment(config))
        tree = git(root, "rev-parse", "HEAD^{tree}").decode().strip()
        if receipt["key"] != cohort_key(tree, inputs) or receipt["source"] != source_inventory(root):
            fail("incompatible_environment", "Completed producer inputs changed; refusing to snapshot")
        if receipt["artifacts"] != artifact_inventory(root, config):
            fail("incompatible_environment", "Configured files changed after producer completion; refusing to snapshot")
        snapshots = Path(manifest["snapshot_root"])
        destination = snapshots / receipt["key"]
        if destination.exists():
            existing = load(destination / "cohort.json")
            if existing["inputs"] != inputs or existing["tree"] != tree or artifact_inventory(destination / "files", config) != existing["artifacts"]:
                fail("incompatible_environment", "Existing immutable cohort failed provenance or integrity verification")
            return {"published": True, "cohort": receipt["key"]}
        staging = Path(tempfile.mkdtemp(prefix=".preparing-", dir=snapshots))
        try:
            (staging / "files").mkdir()
            for relative in config["snapshot"]["include"]:
                target = staging / "files" / relative
                target.parent.mkdir(parents=True, exist_ok=True)
                strict_clone(root / relative, target)
            if artifact_inventory(staging / "files", config) != receipt["artifacts"]:
                fail("incompatible_environment", "CoW snapshot failed metadata/content integrity verification")
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
    if action == "launch":
        manifest, expected_root, cwd, *command = arguments
        sys.exit(launch(manifest, expected_root, cwd, command))
    if action == "inside":
        sys.exit(inside(arguments))
    if action == "produce":
        sys.exit(produce(arguments))
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
        print(json.dumps({"error": {"reason": error.reason, "message": error.message}}), file=sys.stderr)
        sys.exit(1)
    except (OSError, ValueError, KeyError, subprocess.SubprocessError) as error:
        print(json.dumps({"error": {"reason": "incompatible_environment", "message": "Workbench engine failed at an OS or artifact boundary (" + type(error).__name__ + ", errno=" + str(getattr(error, "errno", None)) + ", operation=" + sys.argv[1] + ")"}}), file=sys.stderr)
        sys.exit(1)
