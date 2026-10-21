"""Conventional project discovery; emits generic producer/runtime configuration."""
import json
import os
import re
from pathlib import Path
import shlex
import shutil
import subprocess


class SetupFailure(Exception):
    pass


def tool(name):
    executable = shutil.which(name)
    if executable is None:
        raise SetupFailure("Automatic warm setup requires the project tool: " + name)
    return executable


def output(command, root):
    result = subprocess.run(command, cwd=root, stdout=subprocess.PIPE, stderr=subprocess.PIPE)
    if result.returncode:
        raise SetupFailure("Automatic setup could not inspect a declared build tool (exit %d)" % result.returncode)
    return result.stdout


def toml(path):
    try:
        import tomllib
    except ImportError as error:
        raise SetupFailure("Automatic Rust setup requires Python 3.11+ TOML support") from error
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


def sccache_runtime(config, wrapper, root):
    version = output([wrapper, "--version"], root)
    if not version.startswith(b"sccache "):
        return
    executable = tool("sccache")
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
    tracked = output([tool("git"), "ls-files", "-z"], root).split(b"\0")
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


def configure(root, logical_root, config, workspace):
    rust = (root / "Cargo.toml").exists()
    javascript = (root / "package.json").exists()
    if not rust and not javascript:
        return False
    check_local_inputs(root, logical_root, workspace, rust, javascript)
    producers, required, optional, paths, commands, environment = [], [], [], [], [], ["PATH"]
    if rust:
        cargo = tool("cargo")
        tool("rustc")
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
            raise SetupFailure("Automatic Rust setup requires a project-relative private Cargo target directory")
        required.append(str(target))
        command = [cargo, "build"]
        if (root / "Cargo.lock").exists():
            command.append("--locked")
        else:
            optional.append("Cargo.lock")
        producers.append(shlex.join(command))
        commands += [[cargo, "--version"], [tool("rustc"), "-vV"]]
        environment += ["RUSTFLAGS", "CARGO_ENCODED_RUSTFLAGS", "RUSTC", "RUSTC_WRAPPER", "RUSTC_WORKSPACE_WRAPPER", "CARGO_BUILD_TARGET", "CARGO_TARGET_DIR", "CARGO_INCREMENTAL", "CC", "CXX", "CFLAGS", "CXXFLAGS", "LDFLAGS"]
        wrapper = os.environ.get("RUSTC_WRAPPER", build.get("rustc-wrapper"))
        if wrapper:
            if "RUSTC_WRAPPER" not in os.environ and not Path(wrapper).is_absolute() and "/" in wrapper:
                wrapper = str((origins["rustc-wrapper"] / wrapper).resolve())
            executable = tool(wrapper)
            paths.append(str(Path(executable).relative_to(root)) if Path(executable).is_relative_to(root) else executable)
            sccache_runtime(config, executable, root)
    if javascript:
        package = json.loads((root / "package.json").read_text())
        locks = [(name, manager) for name, manager in [("package-lock.json", "npm"), ("npm-shrinkwrap.json", "npm"), ("pnpm-lock.yaml", "pnpm"), ("yarn.lock", "yarn")] if (root / name).exists()]
        if len(locks) != 1:
            raise SetupFailure("Automatic JS/TS setup requires exactly one package-manager lockfile")
        manager = locks[0][1]
        declared = package.get("packageManager")
        if declared and declared.split("@")[0] != manager:
            raise SetupFailure("Declared package manager conflicts with the project lockfile")
        executable, node = tool(manager), tool("node")
        version = output([executable, "--version"], root).decode().strip()
        if declared and declared.split("@", 1)[1].split("+", 1)[0] != version:
            raise SetupFailure("Installed package manager does not match the version declared by the project")
        commands += [[executable, "--version"], [node, "--version"]]
        install = {"npm": ["ci"], "pnpm": ["install", "--frozen-lockfile"], "yarn": ["install", "--frozen-lockfile"] if version.startswith("1.") else ["install", "--immutable"]}[manager]
        if manager == "yarn" and not version.startswith("1."):
            config["environment"]["YARN_ENABLE_GLOBAL_CACHE"] = "false"
            optional += [".pnp.cjs", ".pnp.loader.mjs", ".yarn/cache", ".yarn/install-state.gz"]
        producers.append(shlex.join([executable, *install]))
        # Empty dependency graphs need not create node_modules themselves.
        producers.append("mkdir -p node_modules")
        required.append("node_modules")
        if "build" in package.get("scripts", {}):
            producers.append(shlex.join([executable, "run", "build"]))
            optional += ["dist", "build"]
            tsconfig = root / "tsconfig.json"
            if tsconfig.exists():
                options = jsonc(tsconfig).get("compilerOptions", {})
                for destination in [options.get("outDir"), options.get("tsBuildInfoFile"), "tsconfig.tsbuildinfo"]:
                    if destination:
                        if Path(destination).is_absolute() or ".." in Path(destination).parts:
                            raise SetupFailure("Automatic JS/TS setup requires private project-relative build outputs")
                        optional.append(destination.removeprefix("./"))
        environment += ["NODE_OPTIONS", "NODE_ENV", "npm_config_arch", "npm_config_platform"]
    optional = sorted(set(optional) - set(required))
    optional = [value for value in optional if not any(Path(parent) in Path(value).parents for parent in required + optional)]
    environment += [key for key in os.environ if key.startswith(("CARGO_", "RUST_", "npm_config_", "YARN_", "PNPM_"))]
    config["snapshot"] = {
        "producer": ["/bin/sh", "-eu", "-c", " && ".join(producers)],
        "include": required, "optional_include": optional,
        "provenance_paths": sorted(set(paths)),
        "provenance_commands": commands,
        "provenance_environment": sorted(set(environment)),
    }
    return True
