"""Whole-workspace clones viewed at their parent paths in an owned Linux environment."""
import array
import contextlib
import ctypes
import errno
import fcntl
import glob
import hashlib
import json
import os
from pathlib import Path
import pwd
import shlex
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
SCHEMA = 4
# Schema-3 environments viewed their roots here; current views use parent paths.
LOGICAL_ROOT = "/mnt/tyde-workspace"
GIT_VIEW = "/mnt/tyde-git"
RUNTIME = "/mnt/tyde-runtime"
RECEIPTS = "/mnt/tyde-producer-receipts"
AT_FDCWD = -100
RENAME_NOREPLACE = 1


class Failure(Exception):
    def __init__(self, reason, message):
        self.reason, self.message = reason, message


def fail(reason, message):
    raise Failure(reason, message)


class NamespaceBuildFailed(Failure):
    def __init__(self, status):
        super().__init__("incompatible_environment", "Bubblewrap exited with status %d before delivering its namespaces" % status)


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
        try:
            result = guarded_launch(args, dict(os.environ), run, command, "/")
        except NamespaceBuildFailed as error:
            result = unsupported("namespace_denied", "Host policy prevents the Bubblewrap mount environment: " + error.message)
            return {"bubblewrap": result, "nested_namespaces": result}
    if result not in (0, 42):
        result = unsupported("incompatible_environment", "The Bubblewrap probe command exited with status %d inside its namespace" % result)
        return {"bubblewrap": result, "nested_namespaces": result}
    return {"bubblewrap": {"kind": "available"}, "nested_namespaces": {"kind": "available"} if result == 0 else unsupported("nested_namespaces_denied", "Host policy denies nested namespaces; explicit backend namespace requirements cannot be satisfied.")}


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


@contextlib.contextmanager
def admission(manifest_path, exclusive=False):
    with (Path(manifest_path).parent / "active.lock").open("a+b") as lease:
        try:
            fcntl.flock(lease, fcntl.LOCK_EX if exclusive else (fcntl.LOCK_SH | fcntl.LOCK_NB))
        except BlockingIOError:
            fail("incompatible_environment", "Workbench processes are active or the workbench is being removed")
        yield


def report_failure(reason, message):
    print(json.dumps({"error": {"reason": reason, "message": message}}), file=sys.stderr)


def root_records(manifest):
    records = manifest.get("roots")
    if not isinstance(records, list) or not records:
        fail("incompatible_environment", "Workbench manifest lacks authoritative registered root mappings; recreate this older or malformed environment from its parent")
    return records


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


# Operations whose state lives in per-worktree administration and cannot be
# resumed from a copy; the parent must settle them before capture.
IN_PROGRESS = ("MERGE_HEAD", "CHERRY_PICK_HEAD", "REVERT_HEAD", "BISECT_LOG", "rebase-merge", "rebase-apply", "sequencer")
FICLONE = 0x40049409


def git_path(root, *args):
    return Path(os.fsdecode(git(root, "rev-parse", "--path-format=absolute", *args)).strip())


def head_commit(root):
    return git(root, "rev-parse", "--verify", "HEAD^{commit}").strip()


def check_capturable(parent, base):
    if head_commit(parent) != base:
        fail("incompatible_environment", "The parent checkout moved away from the workbench base commit; create the workbench from the parent's current commit")
    for name in IN_PROGRESS:
        if git_path(parent, "--git-path", name).exists():
            fail("incompatible_environment", "The parent checkout has an in-progress Git operation (%s); finish or abort it before creating a workbench" % name)


def check_git_layout(parent):
    for key in ("core.sparseCheckout", "core.splitIndex", "worktree.useRelativePaths", "extensions.relativeWorktrees"):
        if git(parent, "config", "--type=bool", "--default=false", "--get", key).strip() == b"true":
            fail("unsupported_roots", "Bubblewrap workbenches do not support %s parent repositories" % key)
    for entry in git(parent, "ls-files", "--stage", "-z").split(b"\0"):
        if entry.startswith(b"160000 ") and (parent / os.fsdecode(entry.split(b"\t", 1)[1]) / ".git").is_file():
            fail("unsupported_roots", "Bubblewrap workbenches do not support submodules with absorbed Git directories")


def copy_on_write(parent, child):
    # Reported only; the copy itself always lets cp choose CoW or a full copy.
    if parent.stat().st_dev != child.stat().st_dev:
        return False
    with tempfile.TemporaryDirectory(prefix=".tyde-clone-probe-", dir=child) as scratch:
        source, destination = Path(scratch) / "source", Path(scratch) / "clone"
        source.write_bytes(b"Tyde clone probe\n")
        with source.open("rb") as reader, destination.open("wb") as writer:
            try:
                fcntl.ioctl(writer.fileno(), FICLONE, reader.fileno())
            except OSError:
                return False
    return True


def capture(record):
    # The child is a no-checkout worktree at the parent's commit. Everything
    # except the parent's Git administration is copied as-is, then the
    # parent's index becomes the child's so staged state carries over.
    parent, child = Path(record["parent_root"]), Path(record["root"])
    if sorted(os.listdir(child)) != [".git"] or not (child / ".git").is_file():
        fail("incompatible_environment", "The workbench checkout must be an empty no-checkout worktree before capture")
    base = head_commit(child)
    check_git_layout(parent)
    check_capturable(parent, base)
    cow = copy_on_write(parent, child)
    entries = sorted(entry for entry in os.listdir(parent) if entry != ".git")
    if entries:
        result = subprocess.run(["cp", "-a", "--reflink=auto", "-t", str(child), "--", *(str(parent / entry) for entry in entries)], stdout=subprocess.DEVNULL, stderr=subprocess.PIPE)
        if result.returncode:
            detail = os.fsdecode(result.stderr).strip().splitlines()[-3:]
            fail("incompatible_environment", "Copying the parent workspace failed: " + " | ".join(detail))
    index, child_index = git_path(parent, "--git-path", "index"), git_path(child, "--git-path", "index")
    if index.exists():
        staged = child_index.with_name(child_index.name + ".tyde-capture")
        shutil.copy2(index, staged)
        os.replace(staged, child_index)
    else:
        child_index.unlink(missing_ok=True)
    check_capturable(parent, base)
    git(child, "update-index", "-q", "--refresh")
    common, admin = git_path(child, "--git-common-dir"), git_path(child, "--git-dir")
    if not admin.is_relative_to(common):
        fail("incompatible_environment", "Workbench Git administration is outside its common directory")
    record["git_common"] = str(common)
    record["git_admin"] = str(admin.relative_to(common))
    return cow


def engine_path(path):
    return any(path == Path(engine) or Path(engine) in path.parents or path in Path(engine).parents for engine in (LOGICAL_ROOT, RUNTIME, RECEIPTS, GIT_VIEW))


def prepare(request_path, directory, claim):
    request = load(request_path)
    roots = request["roots"]
    if not roots:
        fail("unsupported_roots", "Bubblewrap requires at least one registered Git root")
    parents = [Path(root["parent_root"]).resolve(strict=True) for root in roots]
    if len(set(parents)) != len(parents) or any(a != b and a in b.parents for a in parents for b in parents):
        fail("unsupported_roots", "Duplicate or nested Git roots need distinct source ownership; no roots were flattened")
    if any(engine_path(parent) for parent in parents):
        fail("unsupported_roots", "Workbench parent roots must not overlap engine-owned mountpoints")
    records = [{"root": str(Path(item["worktree_root"]).resolve(strict=True)), "logical_root": str(parents[index]), "parent_root": str(parents[index]), "configuration": request["configurations"][index]} for index, item in enumerate(roots)]
    support = probe()
    if support["bubblewrap"]["kind"] != "available":
        result = support["bubblewrap"]
        fail(result["reason"], result["message"])
    for record in records:
        validate_configuration(record["configuration"])
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
        cow = all([capture(record) for record in records])
        (directory / "view").mkdir()
        for index, record in enumerate(records):
            (directory / "view" / ("gitfile-%d" % index)).write_text("gitdir: %s/%d/%s\n" % (GIT_VIEW, index, record["git_admin"]))
        root = records[0]["root"]
        manifest = {"schema": SCHEMA, "root": root, "roots": records, "configuration": config, "support": support, "python": PYTHON, "bwrap": shutil.which("bwrap"), "copy_on_write": cow}
        (directory / "receipts").mkdir()
        (directory / "runs").mkdir()
        save(directory / "owner.json", {"schema": SCHEMA, "environment": str(directory), "source": root})
        save(directory / "manifest.json", manifest)
        shutil.copyfile(__file__, directory / "runner.py")
        return {"kind": "bubblewrap", "manifest_path": str(directory / "manifest.json"), "logical_root": records[0]["logical_root"], "additional_roots": [record["logical_root"] for record in records[1:]], "support": support, "initialization": {"kind": "captured", "copy_on_write": cow}}
    except BaseException:
        shutil.rmtree(directory)
        raise


def owned_environment(manifest_path, expected_root=None):
    manifest_path = Path(manifest_path).absolute()
    directory = manifest_path.parent
    owner = load(directory / "owner.json")
    if manifest_path.name != "manifest.json" or owner["schema"] not in (2, 3, SCHEMA) or owner["environment"] != str(directory) or directory.is_symlink():
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
    roots = [Path(record["logical_root"]) for record in root_records(manifest)]
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
        if engine_path(path) or any(path == root or root in path.parents or path in root.parents for root in roots):
            fail("runtime_configuration_invalid", "A writable runtime reference overlaps engine-owned mountpoints or workspace roots")
        if path.exists():
            args += ["--bind", str(path), str(path)]
    destinations = set()
    for binding in manifest["configuration"].get("readonly_bindings", []):
        source = Path(expand(binding["host_path"], env))
        destination = Path(expand(binding["execution_path"], env))
        if not source.is_absolute() or not source.exists() or not destination.is_absolute() or str(destination) != os.path.normpath(str(destination)):
            fail("runtime_configuration_invalid", "Read-only runtime bindings require existing absolute sources and canonical absolute destinations")
        legacy = manifest["schema"] < 4 and destination.is_relative_to(LOGICAL_ROOT) and destination != Path(LOGICAL_ROOT)
        if not (destination.parent == Path("/mnt") or legacy) or any(destination == Path(engine) or destination in Path(engine).parents or Path(engine) in destination.parents for engine in (RUNTIME, RECEIPTS, GIT_VIEW)):
            fail("runtime_configuration_invalid", "Read-only runtime bindings must stay inside private non-engine mountpoints")
        if any(destination == root or destination in root.parents or root in destination.parents for root in roots):
            fail("runtime_configuration_invalid", "Read-only runtime bindings must not overlap registered writable source roots")
        if destination in destinations:
            fail("runtime_configuration_invalid", "Read-only runtime bindings have conflicting destinations")
        destinations.add(destination)
        args += ["--ro-bind", str(source), str(destination)]
    return args


def ssh_words(line):
    try:
        words = shlex.split(line, comments=True)
    except ValueError:
        return []
    if words and "=" in words[0]:
        keyword, _, value = words[0].partition("=")
        words = [keyword] + ([value] if value else []) + words[1:]
    if len(words) > 1 and words[1].startswith("="):
        words = [words[0]] + ([words[1][1:]] if words[1] != "=" else []) + words[2:]
    return words


def ssh_includes(path, user, home, found):
    try:
        text = Path(path).read_text(errors="surrogateescape")
    except OSError:
        return
    for line in text.splitlines():
        words = ssh_words(line)
        if not words or words[0].lower() != "include":
            continue
        for pattern in words[1:]:
            if "%" in pattern or "${" in pattern:
                continue
            if pattern.startswith("~"):
                pattern = home + pattern[1:] if pattern == "~" or pattern.startswith("~/") else os.path.expanduser(pattern)
            elif not pattern.startswith("/"):
                pattern = str(Path(home) / ".ssh" / pattern) if user else "/etc/ssh/" + pattern
            for match in sorted(glob.glob(pattern)):
                resolved = os.path.realpath(match)
                if resolved not in found:
                    found[resolved] = None
                    ssh_includes(resolved, user, home, found)


def ssh_projections(manifest, run):
    # OpenSSH requires its user and included configuration files to be owned
    # by root or the caller. Host root appears unmapped in this user namespace,
    # so root-owned files that pass that check on the host are bound as
    # caller-owned copies of identical bytes. Keys and other files are untouched.
    if os.getuid() == 0:
        return []
    home = pwd.getpwuid(os.getuid()).pw_dir
    found = {}
    user = os.path.realpath(Path(home) / ".ssh" / "config")
    found[user] = None
    ssh_includes(user, True, home, found)
    ssh_includes("/etc/ssh/ssh_config", False, home, found)
    roots = [Path(record["logical_root"]) for record in root_records(manifest)]
    args = []
    for index, path in enumerate(found):
        target = Path(path)
        if target.is_relative_to("/mnt") or any(target.is_relative_to(root) for root in roots):
            continue
        try:
            metadata = target.stat()
            contents = target.read_bytes()
        except OSError:
            continue
        if not stat.S_ISREG(metadata.st_mode) or metadata.st_uid != 0 or metadata.st_mode & 0o022:
            continue
        copy = run / "ssh-config" / str(index)
        copy.parent.mkdir(exist_ok=True)
        copy.write_bytes(contents)
        copy.chmod(stat.S_IMODE(metadata.st_mode))
        args += ["--ro-bind", str(copy), path]
    return args


def view_bindings(manifest, directory, run):
    # Each root is viewed at its parent's path. The parent's Git common
    # directory would be shadowed there, so it is viewed at GIT_VIEW and the
    # child's gitfile is replaced by one naming that view of its own admin dir.
    # A private tmpfs keeps an inherited GIT_VIEW (host or outer view) intact.
    args = ["--tmpfs", GIT_VIEW] if manifest["schema"] >= 4 else []
    for index, record in enumerate(root_records(manifest)):
        args += ["--bind", record["root"], record["logical_root"]]
        if manifest["schema"] >= 4:
            args += ["--bind", record["git_common"], "%s/%d" % (GIT_VIEW, index), "--ro-bind", str(directory / "view" / ("gitfile-%d" % index)), str(Path(record["logical_root"]) / ".git")]
    return args + ssh_projections(manifest, run)


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


def owns_terminal():
    if not os.isatty(0) or os.getsid(0) != os.getpid():
        return False
    try:
        os.tcgetpgrp(0)
    except OSError as error:
        if error.errno != errno.ENOTTY:
            raise
        return False
    return True


def guarded_launch(args, env, run, command, cwd, descriptor_path=None):
    # The guardian survives the caller's process-group kill, retaining its lease
    # until even orphaned/setsid native descendants have been killed and joined.
    # Only the session leader controlling stdin can release that terminal for
    # the runtime's new session to own. A command run from another runtime's
    # terminal is not that leader: its nested runtime uses the terminal as
    # plain I/O without job control. Pipe-based providers never touch it.
    terminal = owns_terminal()
    if terminal:
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
                                result = code if child is not None else {"namespace_build_exit": code}
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
                                    runtime_command = [PYTHON, RUNTIME + "/runner.py", "inside", "terminal" if terminal else "detached", *command]
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
            if "namespace_build_exit" in result:
                raise NamespaceBuildFailed(result["namespace_build_exit"])
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
                if not destination.is_absolute() or not destination.is_file() or any(destination.is_relative_to(record["logical_root"]) for record in root_records(manifest)):
                    fail("runtime_configuration_invalid", "A configured native runtime file binding is absent, not an absolute file, or inside a workspace root")
                args += ["--ro-bind", str(run / binding["runtime_file"]), str(destination)]
            args += view_bindings(manifest, directory, run)
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


def inside(terminal, command):
    own_descendants()
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
        mode, *command = arguments
        if mode not in ("terminal", "detached"):
            fail("incompatible_environment", "Unknown runtime terminal mode")
        sys.exit(inside(mode == "terminal", command))
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
