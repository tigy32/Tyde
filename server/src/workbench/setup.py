"""Conventional artifact boundaries and runtime configuration."""
import json
import os
import re
from pathlib import Path
import shutil
import subprocess


class SetupFailure(Exception):
    pass


class ToolsUnavailable(Exception):
    pass


def tool(name, workspace=()):
    executable = shutil.which(name)
    if executable is None:
        raise ToolsUnavailable("Artifact reuse needs the project tool: " + name)
    if not Path(executable).is_absolute() or any(Path(executable).resolve().is_relative_to(record[key]) for record in workspace for key in ("root", "parent_root")):
        raise SetupFailure("Artifact discovery requires native host tools, not project scripts")
    return executable


def output(command, root):
    if Path(command[0]).resolve().is_relative_to(root):
        raise SetupFailure("Artifact discovery cannot execute project scripts")
    result = subprocess.run(command, cwd=root, env=dict(os.environ, RUSTUP_AUTO_INSTALL="0", RUSTUP_NO_UPDATE_CHECK="1", COREPACK_ENABLE_NETWORK="0", COREPACK_ENABLE_AUTO_PIN="0"), stdout=subprocess.PIPE, stderr=subprocess.PIPE)
    if result.returncode:
        raise SetupFailure("Artifact discovery could not inspect a declared build tool (exit %d)" % result.returncode)
    return result.stdout


def toml(path):
    try:
        import tomllib
    except ImportError as error:
        raise SetupFailure("Rust artifact reuse requires Python 3.11+ TOML support") from error
    with path.open("rb") as source:
        return tomllib.load(source)


def patch_paths(document):
    return [dependency["path"] for table in [document.get("replace", {}), *document.get("patch", {}).values()] for dependency in table.values() if isinstance(dependency, dict) and "path" in dependency]


def cargo_configuration(root, logical_root, config, workspace):
    home = Path(os.environ.get("CARGO_HOME", str(Path.home() / ".cargo"))).resolve()
    configuration, origins, provenance, seen = {}, {}, [], set()
    ancestors = {parent / ".cargo": index for index, parent in enumerate(root.parents)}
    logical_parents = list(Path(logical_root).parents)
    for directory in [home, *reversed(list(ancestors)), root / ".cargo"]:
        legacy, modern = directory / "config", directory / "config.toml"
        selected = legacy if legacy.exists() else modern
        if not selected.exists() or selected.resolve() in seen:
            continue
        seen.add(selected.resolve())
        provenance.append(str(selected.relative_to(root)) if selected.is_relative_to(root) else str(selected))
        if directory == home:
            logical_origin = home.parent
        elif directory == root / ".cargo":
            logical_origin = Path(logical_root)
        elif directory.parent == Path("/"):
            logical_origin = Path("/")
        else:
            index = ancestors[directory]
            if index >= len(logical_parents) or not logical_parents[index].is_relative_to("/mnt"):
                raise SetupFailure("Cargo ancestor configuration cannot be represented within the private logical layout")
            logical_origin = logical_parents[index]
            config["readonly_bindings"].append({"host_path": str(directory), "execution_path": str(logical_origin / ".cargo")})
        def read_configuration(current, execution_path, active):
            identity = current.resolve()
            if identity in active:
                raise SetupFailure("Cargo configuration includes form a cycle")
            document = toml(current)
            values, locations = {}, {}
            for value in patch_paths(document) + document.get("paths", []):
                private_source(execution_path.parent.parent / value, workspace)
            for item in document.get("include", []):
                value = item if isinstance(item, str) else item["path"]
                included = current.parent / value
                if isinstance(item, dict) and item.get("optional", False) and not included.exists():
                    continue
                reference = str(included.relative_to(root)) if included.is_relative_to(root) else str(included)
                if reference not in provenance:
                    provenance.append(reference)
                nested, nested_locations = read_configuration(included, execution_path.parent / value, active | {identity})
                values.update(nested)
                locations.update(nested_locations)
            for key, value in document.get("build", {}).items():
                values[key] = value
                locations[key] = current.parent.parent
            return values, locations
        values, locations = read_configuration(selected, logical_origin / ".cargo" / selected.name, set())
        configuration.update(values)
        origins.update(locations)
    projected = {binding["execution_path"] for binding in config["readonly_bindings"]}
    for parent in logical_parents:
        directory = parent / ".cargo"
        if str(directory) not in projected and directory.parent != Path("/") and any((directory / name).exists() for name in ("config", "config.toml")):
            raise SetupFailure("A host Cargo configuration conflicts with the private logical layout")
    return configuration, origins, provenance


def sccache_runtime(config, wrapper, root, workspace):
    version = output([wrapper, "--version"], root)
    if not version.startswith(b"sccache "):
        return
    try:
        executable = tool("sccache", workspace)
    except ToolsUnavailable as error:
        raise SetupFailure("Configured compiler caching requires installed sccache") from error
    if output([executable, "--version"], root) != version:
        raise SetupFailure("Configured compiler cache and the installed sccache have different versions")
    # Select the same compiler-cache implementation with an owned local service,
    # not a host daemon whose filesystem cannot see the stable workspace paths.
    config["remove_environment"] += [key for key in os.environ if key.startswith("SCCACHE_")]
    config["environment"].update({
        "RUSTC_WRAPPER": executable,
        "SCCACHE_CONF": "${TYDE_WORKBENCH_RUNTIME}/compiler-cache.toml",
        "SCCACHE_DIR": "${TYDE_WORKBENCH_RUNTIME}/compiler-cache",
        "SCCACHE_SERVER_UDS": "${TYDE_WORKBENCH_RUNTIME}/compiler-cache.sock",
        "SCCACHE_IDLE_TIMEOUT": "0",
    })
    config["runtime_files"]["compiler-cache.toml"] = ""
    config["runtime_directories"].append("compiler-cache")
    config["services"].append({
        "name": "compiler-cache",
        "command": [executable],
        "environment": {
            "SCCACHE_START_SERVER": "1", "SCCACHE_NO_DAEMON": "1",
            "SCCACHE_STARTUP_NOTIFY": "${TYDE_WORKBENCH_RUNTIME}/compiler-cache-ready.sock",
        },
        "ready_notification": "compiler-cache-ready.sock",
        "stop_command": [executable, "--stop-server"],
    })


def jsonc(path):
    text = re.sub(r'"(?:\\.|[^"\\])*"|/\*[\s\S]*?\*/|//[^\r\n]*', lambda match: match[0] if match[0].startswith('"') else "", path.read_text())
    text = re.sub(r'"(?:\\.|[^"\\])*"|,\s*(?=[}\]])', lambda match: match[0] if match[0].startswith('"') else "", text)
    return json.loads(text)


def private_source(logical, workspace):
    logical = Path(os.path.normpath(str(logical)))
    for record in workspace:
        if logical.is_relative_to(record["logical_root"]):
            physical = (Path(record["root"]) / logical.relative_to(record["logical_root"])).resolve()
            if any(physical.is_relative_to(other["root"]) for other in workspace):
                return
    raise SetupFailure("A declared local build input resolves outside registered workspace roots")


def check_local_inputs(root, logical_root, workspace, rust, javascript):
    tracked = output([tool("git", workspace), "ls-files", "-z"], root).split(b"\0")
    for raw in tracked:
        if not raw:
            continue
        relative = Path(os.fsdecode(raw))
        logical = Path(logical_root) / relative.parent
        if rust and relative.name == "Cargo.toml":
            manifest = toml(root / relative)
            tables = [manifest, manifest.get("workspace", {}), *manifest.get("target", {}).values()]
            for table in tables:
                for group in ("dependencies", "dev-dependencies", "build-dependencies"):
                    for dependency in table.get(group, {}).values():
                        if isinstance(dependency, dict) and "path" in dependency:
                            private_source(logical / dependency["path"], workspace)
            for value in patch_paths(manifest):
                private_source(logical / value, workspace)
            for member in manifest.get("workspace", {}).get("members", []):
                private_source(logical / member, workspace)
        if javascript and relative.name == "package.json":
            manifest = json.loads((root / relative).read_text())
            for group in ("dependencies", "devDependencies", "optionalDependencies"):
                for dependency in manifest.get(group, {}).values():
                    if dependency.startswith(("file:", "link:", "portal:")):
                        private_source(logical / dependency.split(":", 1)[1], workspace)


def configure(root, logical_root, config, workspace, guard_tool_query):
    rust = (root / "Cargo.toml").exists()
    javascript = (root / "package.json").exists()
    if not rust and not javascript:
        return False
    check_local_inputs(root, logical_root, workspace, rust, javascript)
    required, optional, paths, tools, environment = [], [], [], [], ["PATH"]
    if rust:
        tool("cargo", workspace)
        tool("rustc", workspace)
        toml(root / "Cargo.toml")
        build, origins, inputs = cargo_configuration(root, logical_root, config, workspace)
        paths += inputs
        destination = os.environ.get("CARGO_TARGET_DIR", os.environ.get("CARGO_BUILD_TARGET_DIR"))
        if destination is not None:
            target = Path(destination)
        elif "target-dir" in build:
            absolute = (origins["target-dir"] / build["target-dir"]).resolve()
            if not absolute.is_relative_to(root):
                raise SetupFailure("Configured Cargo target directory is outside the private project root")
            target = absolute.relative_to(root)
        else:
            target = Path("target")
        if target.is_absolute() or ".." in target.parts or target == Path("."):
            raise SetupFailure("Rust artifact reuse requires a project-relative private Cargo target directory")
        required.append(str(target))
        if not (root / "Cargo.lock").exists():
            optional.append("Cargo.lock")
        tools += ["cargo", "rustc"]
        environment += ["RUSTFLAGS", "CARGO_ENCODED_RUSTFLAGS", "RUSTC", "RUSTC_WRAPPER", "RUSTC_WORKSPACE_WRAPPER", "CARGO_BUILD_TARGET", "CARGO_TARGET_DIR", "CARGO_INCREMENTAL", "CC", "CXX", "CFLAGS", "CXXFLAGS", "LDFLAGS"]
        wrapper = os.environ.get("RUSTC_WRAPPER", build.get("rustc-wrapper"))
        if wrapper:
            if "RUSTC_WRAPPER" not in os.environ and not Path(wrapper).is_absolute() and "/" in wrapper:
                wrapper = str((origins["rustc-wrapper"] / wrapper).resolve())
            executable = tool(wrapper, workspace)
            paths.append(str(Path(executable).relative_to(root)) if Path(executable).is_relative_to(root) else executable)
            sccache_runtime(config, executable, root, workspace)
    if javascript:
        package = json.loads((root / "package.json").read_text())
        locks = [(name, manager) for name, manager in [("package-lock.json", "npm"), ("npm-shrinkwrap.json", "npm"), ("pnpm-lock.yaml", "pnpm"), ("yarn.lock", "yarn")] if (root / name).exists()]
        if not locks:
            return False
        if len(locks) != 1:
            raise SetupFailure("JS/TS artifact reuse requires exactly one package-manager lockfile")
        manager = locks[0][1]
        declared = package.get("packageManager")
        if declared and declared.split("@")[0] != manager:
            raise SetupFailure("Declared package manager conflicts with the project lockfile")
        guard_tool_query(manager, root, os.environ)
        executable = tool(manager, workspace)
        tool("node", workspace)
        version = output([executable, "--version"], root).decode().strip()
        if declared and declared.split("@", 1)[1].split("+", 1)[0] != version:
            raise SetupFailure("Installed package manager does not match the version declared by the project")
        tools += [manager, "node"]
        if manager == "yarn" and not version.startswith("1."):
            config["environment"]["YARN_ENABLE_GLOBAL_CACHE"] = "false"
            optional += [".pnp.cjs", ".pnp.loader.mjs", ".yarn/cache", ".yarn/install-state.gz"]
        required.append("node_modules")
        if "build" in package.get("scripts", {}):
            optional += ["dist", "build"]
            tsconfig = root / "tsconfig.json"
            if tsconfig.exists():
                options = jsonc(tsconfig).get("compilerOptions", {})
                for destination in [options.get("outDir"), options.get("tsBuildInfoFile"), "tsconfig.tsbuildinfo"]:
                    if destination:
                        if Path(destination).is_absolute() or ".." in Path(destination).parts:
                            raise SetupFailure("JS/TS artifact reuse requires private project-relative build outputs")
                        optional.append(destination.removeprefix("./"))
        environment += ["NODE_OPTIONS", "NODE_ENV", "npm_config_arch", "npm_config_platform"]
    optional = sorted(set(optional) - set(required))
    optional = [value for value in optional if not any(Path(parent) in Path(value).parents for parent in required + optional)]
    environment += [key for key in os.environ if key.startswith(("CARGO_", "RUST_", "npm_config_", "YARN_", "PNPM_"))]
    config["snapshot"] = {
        "include": [], "optional_include": sorted(set(required + optional)),
        "provenance_paths": sorted(set(paths)),
        "provenance_tools": tools,
        "provenance_environment": sorted(set(environment)),
    }
    return True
