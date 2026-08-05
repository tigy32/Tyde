from __future__ import annotations

import hashlib
import importlib.util
import json
import os
import pathlib
import platform
import re
import shutil
import subprocess
import sys
import tempfile
import time
import unittest
from unittest import mock


TOOLS_DIR = pathlib.Path(__file__).resolve().parent
REPO_ROOT = TOOLS_DIR.parent
TOOLCHAIN_UPDATE_LOG = "rustup update stable toolchain=unset"
TOOLCHAIN_INSTALL_LOG = "rustup toolchain install toolchain=unset"
# wasm-bindgen-test-runner parses WASM_BINDGEN_TEST_TIMEOUT as a Rust `u64`, so
# that is the exact domain tools/run-wasm-tests.sh must accept and reject.
U64_MAX = 2**64 - 1
U64_MAX_SECONDS = str(U64_MAX)


def raw_pcm_desktop_ipc_signatures(source: str) -> list[str]:
    source = re.sub(r"/\*.*?\*/", "", source, flags=re.DOTALL)
    source = re.sub(r"//[^\n]*", "", source)
    signatures = []
    command_pattern = re.compile(
        r"#\[tauri::command\]\s*(?:pub\s+)?(?:async\s+)?fn\s+\w+\s*\((.*?)\)",
        re.DOTALL,
    )
    bridge_pattern = re.compile(
        r"pub\s+(?:async\s+)?fn\s+(?:voice_media_\w+|send_host_frame)\s*\((.*?)\)",
        re.DOTALL,
    )
    serde_struct_pattern = re.compile(
        r"#\[derive\([^\]]*(?:Serialize|Deserialize)[^\]]*\)\]"
        r"(?:\s*#\[[^\]]+\])*\s*(?:pub\s+)?struct\s+\w+\s*\{(.*?)\}",
        re.DOTALL,
    )
    boundaries = (
        *command_pattern.finditer(source),
        *bridge_pattern.finditer(source),
        *serde_struct_pattern.finditer(source),
    )
    for match in boundaries:
        signature = match.group(1)
        if re.search(
            r"(?:(?:Vec|VecDeque)\s*<|(?:Box|Arc)\s*<\s*\[|&\s*\[|\[)"
            r"\s*(?:f32|f64|i16|i32)\s*(?:>|\]|;)",
            signature,
        ):
            signatures.append(signature.strip())
            continue
        if re.search(
            r"\b(?:pcm|raw_pcm|pcm_samples|samples|raw_audio)\b\s*:\s*(?:Vec\s*<\s*u8\s*>|&\s*\[\s*u8\s*\])",
            signature,
        ):
            signatures.append(signature.strip())
    return signatures


def native_voice_authorization_is_validated(source: str) -> bool:
    production = source.split("#[cfg(test)]", maxsplit=1)[0]
    production = re.sub(r"/\*.*?\*/", "", production, flags=re.DOTALL)
    production = re.sub(r"//[^\n]*", "", production)
    reader_start = production.find("async fn reader_task(")
    reader_end = production.find("\nasync fn writer_task(", reader_start)
    if reader_start < 0 or reader_end < 0:
        return False
    production = production[reader_start:reader_end]
    authorization_pattern = re.compile(
        r"\.\s*voice_media\s*\.\s*authorize\s*\(", re.DOTALL
    )
    authorizations = list(authorization_pattern.finditer(production))
    if len(authorizations) != 1:
        return False

    authorization = authorizations[0]
    condition_pattern = re.compile(r"\bif\b(?P<condition>[^{};]+)\{", re.DOTALL)
    for conditional in condition_pattern.finditer(production):
        condition_start = conditional.start("condition")
        condition_end = conditional.end("condition")
        if not condition_start <= authorization.start() < condition_end:
            continue
        condition = conditional.group("condition")
        kind = re.search(
            r"(?P<frame>[A-Za-z_]\w*)\s*\.\s*envelope\s*\.\s*kind\s*==\s*"
            r"protocol\s*::\s*FrameKind\s*::\s*VoiceAccepted",
            condition,
        )
        if kind is None:
            return False
        frame = re.escape(kind.group("frame"))
        payload = re.search(
            r"let\s+Ok\s*\(\s*(?P<payload>[A-Za-z_]\w*)\s*\)\s*=\s*"
            rf"{frame}\s*\.\s*envelope\s*\.\s*parse_payload\s*::\s*<\s*"
            r"protocol\s*::\s*VoiceAcceptedPayload\s*>\s*\(\s*\)",
            condition,
        )
        call = re.search(
            r"\.\s*voice_media\s*\.\s*authorize\s*\(\s*host_id\s*\.\s*"
            r"clone\s*\(\s*\)\s*,\s*(?P<payload>[A-Za-z_]\w*)\s*\.\s*"
            r"generation\s*\)",
            condition,
        )
        if payload is None or call is None:
            return False
        if payload.group("payload") != call.group("payload"):
            return False
        if not kind.end() < payload.start() < call.start():
            return False
        return (
            "&&" in condition[kind.end() : payload.start()]
            and "&&" in condition[payload.end() : call.start()]
        )
    return False


NATIVE_VOICE_AEC_VENDOR = "vendor/webrtc-audio-processing-sys"
NATIVE_VOICE_AEC_PATCH = (
    'webrtc-audio-processing-sys = { path = '
    '"vendor/webrtc-audio-processing-sys" }'
)
NATIVE_VOICE_ABSEIL_CACHE = {
    f"{NATIVE_VOICE_AEC_VENDOR}/webrtc-audio-processing/subprojects/packagecache/abseil-cpp-20240722.0.tar.gz": (
        "f50e5ac311a81382da7fa75b97310e4b9006474f9560ac46f54a9967f07d4ae3",
        2_242_861,
    ),
    f"{NATIVE_VOICE_AEC_VENDOR}/webrtc-audio-processing/subprojects/packagecache/abseil-cpp_20240722.0-3_patch.zip": (
        "12dd8df1488a314c53e3751abd2750cf233b830651d168b6a9f15e7d0cf71f7b",
        5_929,
    ),
    f"{NATIVE_VOICE_AEC_VENDOR}/webrtc-audio-processing/subprojects/packagecache/ABSEIL-LICENSE": (
        "c79a7fea0e3cac04cd43f20e7b648e5a0ff8fa5344e644b0ee09ca1162b62747",
        11_361,
    ),
    f"{NATIVE_VOICE_AEC_VENDOR}/webrtc-audio-processing/subprojects/packagecache/WRAPDB-LICENSE": (
        "7939f4c45423cec4a18236ad0a88570e33508dd7462e07b1038001f90ece65fb",
        1_070,
    ),
    f"{NATIVE_VOICE_AEC_VENDOR}/webrtc-audio-processing/subprojects/packagecache/PROVENANCE.md": (
        "9b7a1f87ca75fba86903b4f5804ed1940efdffb6b585f548d23051720e12966b",
        1_266,
    ),
}
NATIVE_VOICE_ABSEIL_WRAP = (
    f"{NATIVE_VOICE_AEC_VENDOR}/webrtc-audio-processing/"
    "subprojects/abseil-cpp.wrap"
)
NATIVE_VOICE_ABSEIL_FORCE_FALLBACK = (
    "--force-fallback-for=absl_base,absl_flags,absl_strings,absl_numeric,"
    "absl_synchronization,absl_bad_optional_access"
)
NATIVE_VOICE_ABSEIL_WRAP_LINES = (
    "directory = abseil-cpp-20240722.0",
    "source_filename = abseil-cpp-20240722.0.tar.gz",
    "source_hash = f50e5ac311a81382da7fa75b97310e4b9006474f9560ac46f54a9967f07d4ae3",
    "patch_filename = abseil-cpp_20240722.0-3_patch.zip",
    "patch_hash = 12dd8df1488a314c53e3751abd2750cf233b830651d168b6a9f15e7d0cf71f7b",
    "wrapdb_version = 20240722.0-3",
)


def toml_without_comment(line: str) -> str:
    quote = None
    escaped = False
    for index, character in enumerate(line):
        if quote is not None:
            if escaped:
                escaped = False
            elif quote == '"' and character == "\\":
                escaped = True
            elif character == quote:
                quote = None
            continue
        if character in ('"', "'"):
            quote = character
        elif character == "#":
            return line[:index]
    return line


def toml_split_top_level(text: str, delimiter: str) -> list[str] | None:
    parts = []
    start = 0
    depth = 0
    quote = None
    escaped = False
    for index, character in enumerate(text):
        if quote is not None:
            if escaped:
                escaped = False
            elif quote == '"' and character == "\\":
                escaped = True
            elif character == quote:
                quote = None
            continue
        if character in ('"', "'"):
            quote = character
        elif character == "{":
            depth += 1
        elif character == "}":
            depth -= 1
            if depth < 0:
                return None
        elif character == delimiter and depth == 0:
            parts.append(text[start:index])
            start = index + 1
    if quote is not None or depth != 0:
        return None
    parts.append(text[start:])
    return parts


def parse_toml_quoted_key(text: str) -> str | None:
    if len(text) < 2 or text[0] != text[-1] or text[0] not in ('"', "'"):
        return None
    if text[0] == "'":
        return text[1:-1]
    value = []
    index = 1
    while index < len(text) - 1:
        character = text[index]
        if character == "\\":
            index += 1
            if index >= len(text) - 1 or text[index] not in ('"', "\\"):
                return None
            character = text[index]
        elif character == text[0]:
            return None
        value.append(character)
        index += 1
    return "".join(value)


def parse_toml_string_array(text: str) -> list[str] | None:
    text = text.strip()
    if not text.startswith("[") or not text.endswith("]"):
        return None
    body = text[1:-1].strip()
    if not body:
        return []
    parts = toml_split_top_level(body, ",")
    if parts is None:
        return None
    values = []
    for part in parts:
        part = part.strip()
        if not part:
            continue
        value = parse_toml_quoted_key(part)
        if value is None:
            return None
        values.append(value)
    return values


def parse_cargo_config(source: str) -> dict[tuple[str, ...], object]:
    settings = {}
    section = []
    lines = iter(source.splitlines())
    for raw_line in lines:
        line = toml_without_comment(raw_line).strip()
        if not line:
            continue
        if line.startswith("["):
            if not line.endswith("]") or line.startswith("[["):
                raise ValueError(f"unsupported Cargo config table: {line}")
            parsed_section = parse_toml_dotted_key(line[1:-1])
            if parsed_section is None:
                raise ValueError(f"invalid Cargo config table: {line}")
            section = parsed_section
            continue
        pieces = toml_split_top_level(line, "=")
        if pieces is None or len(pieces) != 2:
            raise ValueError(f"invalid Cargo config setting: {line}")
        key = parse_toml_dotted_key(pieces[0])
        if key is None:
            raise ValueError(f"invalid Cargo config key: {pieces[0]}")
        value_source = pieces[1].strip()
        if value_source.startswith("["):
            while not value_source.endswith("]"):
                try:
                    continuation = toml_without_comment(next(lines)).strip()
                except StopIteration as error:
                    raise ValueError("unterminated Cargo config array") from error
                if continuation:
                    value_source += " " + continuation
            value = parse_toml_string_array(value_source)
        else:
            value = parse_toml_quoted_key(value_source)
        if value is None:
            raise ValueError(f"unsupported Cargo config value: {value_source}")
        path = tuple(section + key)
        if path in settings:
            raise ValueError(f"duplicate Cargo config setting: {'.'.join(path)}")
        settings[path] = value
    return settings


def cargo_config_discovery_paths(
    repository_root: pathlib.Path, working_directory: pathlib.Path
) -> list[pathlib.Path]:
    repository_root = repository_root.resolve()
    current = working_directory.resolve()
    if current != repository_root and repository_root not in current.parents:
        raise ValueError("Cargo working directory is outside the repository")
    configs = []
    while True:
        cargo_directory = current / ".cargo"
        for filename in ("config.toml", "config"):
            candidate = cargo_directory / filename
            if candidate.is_file():
                configs.append(candidate)
        if current == repository_root:
            return configs
        current = current.parent


def workflow_env_mapping_keys(source: str) -> set[str]:
    keys = set()
    lines = source.splitlines()
    for index, raw_line in enumerate(lines):
        line = toml_without_comment(raw_line).rstrip()
        mapping = re.match(r"^(?P<indent>\s*)env:\s*$", line)
        if mapping is None:
            continue
        mapping_indent = len(mapping.group("indent"))
        for child_raw_line in lines[index + 1 :]:
            child = toml_without_comment(child_raw_line).rstrip()
            if not child.strip():
                continue
            child_indent = len(child) - len(child.lstrip())
            if child_indent <= mapping_indent:
                break
            key = re.match(r"^\s*['\"]?([A-Za-z_][A-Za-z0-9_-]*)['\"]?\s*:", child)
            if key is not None:
                keys.add(key.group(1))
    return keys


def parse_toml_dotted_key(text: str) -> list[str] | None:
    parts = toml_split_top_level(text.strip(), ".")
    if parts is None or not parts:
        return None
    parsed = []
    for part in parts:
        part = part.strip()
        if not part:
            return None
        if part[0] in ('"', "'"):
            value = parse_toml_quoted_key(part)
            if value is None:
                return None
            parsed.append(value)
        elif re.fullmatch(r"[A-Za-z0-9_-]+", part):
            parsed.append(part)
        else:
            return None
    return parsed


def insert_toml_value(target: dict, path: list[str], value: object) -> bool:
    if not path:
        return False
    current = target
    for component in path[:-1]:
        existing = current.get(component)
        if existing is None:
            existing = {}
            current[component] = existing
        if not isinstance(existing, dict):
            return False
        current = existing
    if path[-1] in current:
        return False
    current[path[-1]] = value
    return True


def parse_toml_patch_value(text: str) -> object | None:
    text = text.strip()
    if not text:
        return None
    if text[0] in ('"', "'"):
        return parse_toml_quoted_key(text)
    if not (text.startswith("{") and text.endswith("}")):
        return None
    result = {}
    body = text[1:-1].strip()
    if not body:
        return result
    assignments = toml_split_top_level(body, ",")
    if assignments is None:
        return None
    for assignment in assignments:
        pieces = toml_split_top_level(assignment, "=")
        if pieces is None or len(pieces) != 2:
            return None
        key = parse_toml_dotted_key(pieces[0])
        value = parse_toml_patch_value(pieces[1])
        if key is None or value is None or not insert_toml_value(result, key, value):
            return None
    return result


def expected_native_voice_patch_surface(root_manifest: str) -> bool:
    patch = {}
    section = []
    saw_patch = False
    for raw_line in root_manifest.splitlines():
        line = toml_without_comment(raw_line).strip()
        if not line:
            continue
        if line.startswith("["):
            if line.startswith("[["):
                if not line.endswith("]]"):
                    return False
                array_section = parse_toml_dotted_key(line[2:-2])
                if array_section is None or array_section[:1] == ["patch"]:
                    return False
                section = []
                continue
            if not line.endswith("]"):
                return False
            section = parse_toml_dotted_key(line[1:-1])
            if section is None:
                return False
            if section[:1] == ["patch"]:
                saw_patch = True
            continue
        pieces = toml_split_top_level(line, "=")
        if pieces is None or len(pieces) != 2:
            if section[:1] == ["patch"]:
                return False
            continue
        key = parse_toml_dotted_key(pieces[0])
        if section[:1] == ["patch"] and key is None:
            return False
        patch_path = section[1:] + key if section[:1] == ["patch"] and key else None
        if not section and key and key[:1] == ["patch"]:
            patch_path = key[1:]
        if patch_path is None:
            continue
        saw_patch = True
        value = parse_toml_patch_value(pieces[1])
        if value is None or not insert_toml_value(patch, patch_path, value):
            return False
    return saw_patch and patch == {
        "crates-io": {
            "webrtc-audio-processing-sys": {
                "path": "vendor/webrtc-audio-processing-sys"
            }
        }
    }


def native_voice_vendor_surface_violations(
    root_manifest: str,
    vendor_files: dict[str, str],
) -> list[str]:
    violations = []
    if not expected_native_voice_patch_surface(root_manifest):
        violations.append("root [patch.crates-io] must contain only the pinned AEC DSP")

    vendor_roots = {
        "/".join(pathlib.PurePosixPath(path).parts[:2])
        for path in vendor_files
        if pathlib.PurePosixPath(path).parts[:1] == ("vendor",)
    }
    if vendor_roots != {NATIVE_VOICE_AEC_VENDOR}:
        violations.append("vendor surface contains a dependency other than the pinned AEC DSP")

    manifest_path = f"{NATIVE_VOICE_AEC_VENDOR}/Cargo.toml"
    manifest = vendor_files.get(manifest_path)
    if manifest is None or re.search(
        r'(?m)^name\s*=\s*"webrtc-audio-processing-sys"\s*$', manifest
    ) is None:
        violations.append("vendored AEC manifest provenance is missing")

    for path, (digest, size) in NATIVE_VOICE_ABSEIL_CACHE.items():
        expected = f"sha256:{digest}:bytes:{size}"
        if vendor_files.get(path) != expected:
            violations.append(f"{path} is missing or does not match its pinned checksum")

    wrap = vendor_files.get(NATIVE_VOICE_ABSEIL_WRAP, "")
    if any(line not in wrap.splitlines() for line in NATIVE_VOICE_ABSEIL_WRAP_LINES):
        violations.append("vendored Abseil wrap does not match the pinned package cache")

    build_script = vendor_files.get(f"{NATIVE_VOICE_AEC_VENDOR}/build.rs", "")
    force_fallback_literal = f'"{NATIVE_VOICE_ABSEIL_FORCE_FALLBACK}"'
    if (
        build_script.count('arg("--wrap-mode=nodownload")') != 1
        or build_script.count(force_fallback_literal) != 1
        or build_script.count("arg(ABSEIL_FORCE_FALLBACK)") != 1
        or 'probe("absl_base")' in build_script
        or "remove_materialized_abseil" not in build_script
        or "removing incomplete AEC build directory" not in build_script
    ):
        violations.append("vendored AEC build is not offline and self-recovering")

    dependency_section = re.compile(
        r"^(?:target\..+\.)?(?:build-)?dependencies(?:\..+)?$"
    )
    executable_network_symbol = re.compile(
        r"\b(?:UdpSocket|RtcPeerConnection|RtcIceCandidate|IceCandidate|"
        r"StunServer|TurnServer|AF_INET|AF_INET6|SOCK_DGRAM)\b|"
        r"\b(?:std|tokio|async_std)::net::|\b(?:sendto|recvfrom)\s*\(",
        re.IGNORECASE,
    )
    for path, source in vendor_files.items():
        if pathlib.PurePosixPath(path).name == "Cargo.toml":
            section = ""
            for line in source.splitlines():
                header = re.match(r"\s*\[([^]]+)\]\s*$", line)
                if header is not None:
                    section = header.group(1)
                    continue
                assignment = re.match(r'\s*["\']?([^"\'\s=]+)["\']?\s*=', line)
                if (
                    assignment is not None
                    and dependency_section.match(section)
                    and rejected_network_name(assignment.group(1))
                ):
                    violations.append(
                        f"{path} contains rejected network dependency "
                        f"{assignment.group(1)}"
                    )
            continue
        is_meson = pathlib.PurePosixPath(path).name == "meson.build"
        executable_without_literals, literals = lex_executable_source(
            source, hash_comments=is_meson
        )
        meson_network = is_meson and any(
            re.search(r"\b(?:dependency|subproject)\s*\(\s*$", context)
            and rejected_network_name(value)
            for context, value in literals
        )
        if (
            executable_network_symbol.search(executable_without_literals) is not None
            or any(re.search(r"(?:stun|turn)://", value, re.IGNORECASE) for _, value in literals)
            or meson_network
        ):
            violations.append(f"{path} contains executable network transport code")
    return violations


def rejected_network_name(name: str) -> bool:
    tokens = [token for token in re.split(r"[^a-z0-9]+", name.lower()) if token]
    return any(
        token in {"udp", "ice", "stun", "turn", "network", "socket", "str0m", "mdns"}
        for token in tokens
    )


def lex_executable_source(
    source: str, *, hash_comments: bool = False
) -> tuple[str, list[tuple[str, str]]]:
    code = []
    literals = []
    index = 0
    while index < len(source):
        if source.startswith("//", index):
            newline = source.find("\n", index + 2)
            index = len(source) if newline < 0 else newline
            continue
        if source.startswith("/*", index):
            end = source.find("*/", index + 2)
            index = len(source) if end < 0 else end + 2
            continue
        if hash_comments and source[index] == "#":
            newline = source.find("\n", index + 1)
            index = len(source) if newline < 0 else newline
            continue
        quote = source[index]
        if (
            not hash_comments
            and quote == "'"
            and re.match(r"'(?:\\.|[^'\\\n])'", source[index:]) is None
        ):
            code.append(quote)
            index += 1
            continue
        if quote in ('"', "'"):
            context = "".join(code[-128:])
            index += 1
            value = []
            while index < len(source):
                character = source[index]
                if character == "\\" and index + 1 < len(source):
                    value.extend(source[index : index + 2])
                    index += 2
                    continue
                if character == quote:
                    index += 1
                    break
                value.append(character)
                index += 1
            literals.append((context, "".join(value)))
            code.append(" ")
            continue
        code.append(source[index])
        index += 1
    return "".join(code), literals


def native_voice_vendor_surfaces(repo_root: pathlib.Path) -> dict[str, str]:
    vendor_root = repo_root / "vendor"
    paths = []
    for package in sorted(vendor_root.iterdir()):
        if not package.is_dir():
            continue
        for path in package.rglob("*"):
            if not path.is_file():
                continue
            relative = path.relative_to(repo_root).as_posix()
            if relative in NATIVE_VOICE_ABSEIL_CACHE:
                paths.append(path)
                continue
            if path.name in (
                "Cargo.toml",
                "build.rs",
                "meson.build",
                "abseil-cpp.wrap",
            ) or path.suffix in (
                ".rs",
                ".c",
                ".cc",
                ".cpp",
                ".h",
                ".hpp",
            ):
                paths.append(path)
    if len(paths) > 10_000:
        return {"vendor/__scan_limit__/Cargo.toml": ""}
    surfaces = {}
    for path in paths:
        relative = path.relative_to(repo_root).as_posix()
        if relative in NATIVE_VOICE_ABSEIL_CACHE:
            data = path.read_bytes()
            surfaces[relative] = (
                f"sha256:{hashlib.sha256(data).hexdigest()}:bytes:{len(data)}"
            )
            continue
        if path.stat().st_size > 1_048_576:
            surfaces[relative] = "UdpSocket"
            continue
        surfaces[relative] = path.read_text(
            encoding="utf-8", errors="replace"
        )
    return surfaces


class ReleaseBuildConfigContractTests(unittest.TestCase):
    @classmethod
    def setUpClass(cls) -> None:
        cls.root_config_path = REPO_ROOT / ".cargo/config.toml"
        cls.root_config = parse_cargo_config(
            cls.root_config_path.read_text(encoding="utf-8")
        )

    def test_cmake_policy_floor_is_shared(self) -> None:
        self.assertEqual(
            self.root_config[("env", "CMAKE_POLICY_VERSION_MINIMUM")], "3.5"
        )

    def test_musl_targets_have_exact_static_libc_tail(self) -> None:
        expected = ["-C", "link-arg=-Wl,-Bstatic", "-C", "link-arg=-lc"]

        for target in (
            "x86_64-unknown-linux-musl",
            "aarch64-unknown-linux-musl",
        ):
            with self.subTest(target=target):
                flags = self.root_config[("target", target, "rustflags")]
                self.assertEqual(flags, expected)
                self.assertNotIn("-lm", flags)

    def test_wasm_rustflags_remain_intact(self) -> None:
        self.assertEqual(
            self.root_config[
                ("target", "wasm32-unknown-unknown", "rustflags")
            ],
            [
                "--cfg",
                'getrandom_backend="wasm_js"',
                "-C",
                "link-arg=-zstack-size=16777216",
            ],
        )

    def test_release_workflow_has_no_global_rustflags_mapping(self) -> None:
        workflow = (
            REPO_ROOT / ".github/workflows/release.yml"
        ).read_text(encoding="utf-8")
        keys = workflow_env_mapping_keys(workflow)

        self.assertIn("RELEASE_TAG", keys)
        self.assertIn("GITHUB_TOKEN", keys)
        self.assertNotIn("RUSTFLAGS", keys)
        self.assertNotIn(
            "RUSTFLAGS",
            workflow_env_mapping_keys(
                "# RUSTFLAGS must stay unset\nenv:\n  SAFE_SETTING: value\n"
            ),
        )

    def test_cargo_discovers_unshadowed_config_from_release_build_paths(
        self,
    ) -> None:
        root_config = self.root_config_path.resolve()
        headless_configs = cargo_config_discovery_paths(REPO_ROOT, REPO_ROOT)
        tauri_configs = cargo_config_discovery_paths(
            REPO_ROOT, REPO_ROOT / "frontend/tauri-shell"
        )

        self.assertEqual(headless_configs, [root_config])
        self.assertEqual(
            tauri_configs,
            [(REPO_ROOT / "frontend/.cargo/config.toml").resolve(), root_config],
        )
        protected_settings = {
            ("env", "CMAKE_POLICY_VERSION_MINIMUM"),
            ("target", "x86_64-unknown-linux-musl", "rustflags"),
            ("target", "aarch64-unknown-linux-musl", "rustflags"),
        }
        for nested_config in tauri_configs[:-1]:
            nested_settings = parse_cargo_config(
                nested_config.read_text(encoding="utf-8")
            )
            self.assertTrue(
                protected_settings.isdisjoint(nested_settings),
                f"{nested_config} shadows root release-build settings",
            )


class TrunkCommandTests(unittest.TestCase):
    def test_percent_encoded_checkout_uses_safe_wasm_target_alias(self) -> None:
        with tempfile.TemporaryDirectory() as temp:
            root = pathlib.Path(temp) / "repo%2Fworktree"
            tools = root / "tools"
            frontend = root / "frontend"
            fake_bin = root / "bin"
            tools.mkdir(parents=True)
            frontend.mkdir()
            fake_bin.mkdir()
            shutil.copy2(REPO_ROOT / "tools" / "trunk-command.mjs", tools)
            (frontend / "Trunk.toml").write_text("", encoding="utf-8")
            capture = root / "trunk-env.txt"
            trunk = fake_bin / "trunk"
            trunk.write_text(
                "#!/usr/bin/env bash\n"
                "printf '%s\\n' \"${CARGO_TARGET_DIR-}\" > \"$TRUNK_ENV_CAPTURE\"\n",
                encoding="utf-8",
            )
            trunk.chmod(0o755)
            env = os.environ.copy()
            env.pop("CARGO_TARGET_DIR", None)
            env["PATH"] = f"{fake_bin}:{env['PATH']}"
            env["TRUNK_ENV_CAPTURE"] = str(capture)

            result = subprocess.run(
                ["node", str(tools / "trunk-command.mjs"), "build"],
                cwd=root,
                env=env,
                text=True,
                capture_output=True,
                check=False,
            )

            self.assertEqual(result.returncode, 0, result.stderr)
            alias = pathlib.Path(capture.read_text(encoding="utf-8").strip())
            self.assertNotIn("%", str(alias))
            self.assertTrue(alias.is_symlink())
            self.assertEqual(alias.readlink().resolve(), (root / "target").resolve())
            alias.unlink()


class NativeBuildToolsContractTests(unittest.TestCase):
    def test_prepared_environment_quotes_paths_and_runtime_path(self) -> None:
        module_path = REPO_ROOT / "tools" / "provision-native-build-tools.py"
        spec = importlib.util.spec_from_file_location("native_build_tools", module_path)
        self.assertIsNotNone(spec)
        self.assertIsNotNone(spec.loader)
        module = importlib.util.module_from_spec(spec)
        spec.loader.exec_module(module)

        with tempfile.TemporaryDirectory() as temp:
            root = pathlib.Path(temp) / "root dir;$(touch root-injection)'"
            directory = root / "bin dir;&$(touch path-injection)'"
            environment_file = pathlib.Path(temp) / "native tools.env"
            environment_file.write_text(
                module.prepared_environment(root, directory), encoding="utf-8"
            )
            runtime_path = (
                "/existing path:$HOME:$(touch runtime-injection):"
                "semi;colon:amp&ersand:glob*"
            )
            env = os.environ.copy()
            env["PATH"] = runtime_path

            result = subprocess.run(
                [
                    "/bin/bash",
                    "-c",
                    'set -eu; source "$1"; printf "%s\\n%s\\n" '
                    '"$PATH" "$TYDE_NATIVE_BUILD_TOOLS_ROOT"',
                    "bash",
                    str(environment_file),
                ],
                cwd=temp,
                env=env,
                text=True,
                capture_output=True,
                check=False,
            )

            self.assertEqual(result.returncode, 0, result.stderr)
            self.assertEqual(
                result.stdout.splitlines(),
                [f"{directory}:{runtime_path}", str(root.resolve())],
            )
            self.assertIn('"${PATH-}"', environment_file.read_text(encoding="utf-8"))
            self.assertFalse((pathlib.Path(temp) / "root-injection").exists())
            self.assertFalse((pathlib.Path(temp) / "path-injection").exists())
            self.assertFalse((pathlib.Path(temp) / "runtime-injection").exists())

    def test_offline_native_tools_fail_without_attempting_install(self) -> None:
        module_path = REPO_ROOT / "tools" / "provision-native-build-tools.py"
        spec = importlib.util.spec_from_file_location("offline_native_tools", module_path)
        self.assertIsNotNone(spec)
        self.assertIsNotNone(spec.loader)
        module = importlib.util.module_from_spec(spec)
        spec.loader.exec_module(module)

        with tempfile.TemporaryDirectory() as temp:
            root = pathlib.Path(temp) / "native-tools"
            with mock.patch.object(module, "ready", return_value=False), mock.patch.object(
                module.subprocess, "run"
            ) as run:
                with self.assertRaisesRegex(RuntimeError, "run ./dev.sh check first"):
                    module.require_cached(root)
                run.assert_not_called()

    def test_cargo_build_uses_lazy_pinned_native_tool_wrapper(self) -> None:
        workspace = (REPO_ROOT / "Cargo.toml").read_text(encoding="utf-8")
        build_script = (
            REPO_ROOT / "vendor/webrtc-audio-processing-sys/build.rs"
        ).read_text(encoding="utf-8")
        self.assertIn(
            'webrtc-audio-processing-sys = { path = "vendor/webrtc-audio-processing-sys" }',
            workspace,
        )
        self.assertEqual(build_script.count('repository_native_tool("meson")'), 1)
        self.assertEqual(build_script.count('repository_native_tool("ninja")'), 2)
        self.assertNotIn('Command::new("meson")', build_script)
        self.assertNotIn('Command::new("ninja")', build_script)

        with tempfile.TemporaryDirectory() as temp:
            root = pathlib.Path(temp) / "cached tools;$(touch injection)"
            directory = root / "bin"
            directory.mkdir(parents=True)
            capture = pathlib.Path(temp) / "meson-arguments"
            python_capture = pathlib.Path(temp) / "python-arguments"
            path_capture = pathlib.Path(temp) / "tool-path"
            python = directory / "python"
            python.write_text(
                "#!/bin/sh\n"
                'printf "%s\\n" "$*" >> "$TYDE_NATIVE_PYTHON_CAPTURE"\n'
                "printf '1.11.2\\n1.11.1.4\\n'\n",
                encoding="utf-8",
            )
            meson = directory / "meson"
            meson.write_text(
                "#!/bin/sh\n"
                'if [ "$1" = "--version" ]; then printf "1.11.2\\n"; exit 0; fi\n'
                'printf "%s\\n" "$@" > "$TYDE_NATIVE_TOOL_CAPTURE"\n'
                'printf "%s\\n" "$PATH" > "$TYDE_NATIVE_PATH_CAPTURE"\n',
                encoding="utf-8",
            )
            ninja = directory / "ninja"
            ninja.write_text(
                "#!/bin/sh\nprintf '1.11.1\\n'\n", encoding="utf-8"
            )
            for executable in (python, meson, ninja):
                executable.chmod(0o755)
            env = os.environ.copy()
            env["TYDE_NATIVE_BUILD_TOOLS_ROOT"] = str(root)
            env["TYDE_NATIVE_TOOL_CAPTURE"] = str(capture)
            env["TYDE_NATIVE_PYTHON_CAPTURE"] = str(python_capture)
            env["TYDE_NATIVE_PATH_CAPTURE"] = str(path_capture)

            result = subprocess.run(
                [
                    sys.executable,
                    str(REPO_ROOT / "tools/native-build-tool.py"),
                    "meson",
                    "--",
                    "setup",
                    "path with spaces",
                ],
                cwd=temp,
                env=env,
                text=True,
                capture_output=True,
                check=False,
            )

            self.assertEqual(result.returncode, 0, result.stderr)
            self.assertEqual(capture.read_text(encoding="utf-8"), "setup\npath with spaces\n")
            self.assertNotIn("-m pip", python_capture.read_text(encoding="utf-8"))
            self.assertEqual(
                pathlib.Path(
                    path_capture.read_text(encoding="utf-8").split(os.pathsep)[0]
                ).resolve(),
                directory.resolve(),
            )
            self.assertFalse((pathlib.Path(temp) / "injection").exists())


class DevCheckCacheTests(unittest.TestCase):
    def setUp(self) -> None:
        self.temp = tempfile.TemporaryDirectory()
        self.root = pathlib.Path(self.temp.name) / "repo"
        self.bin = pathlib.Path(self.temp.name) / "bin"
        self.log = pathlib.Path(self.temp.name) / "commands.log"
        self.root.mkdir()
        self.bin.mkdir()

        shutil.copy2(REPO_ROOT / "dev.sh", self.root / "dev.sh")
        shutil.copy2(
            REPO_ROOT / "rust-toolchain.toml", self.root / "rust-toolchain.toml"
        )
        (self.root / ".config").mkdir()
        (self.root / ".config" / "nextest.toml").write_text(
            'nextest-version = "0.9.100"\n', encoding="utf-8"
        )
        (self.root / "tools").mkdir()
        shutil.copy2(
            REPO_ROOT / "tools" / "run-nextest-binary.sh",
            self.root / "tools" / "run-nextest-binary.sh",
        )
        wasm_script = self.root / "tools" / "run-wasm-tests.sh"
        wasm_script.write_text(
            """#!/usr/bin/env bash
set -euo pipefail
identity() {
  printf 'wasm.chrome.path=%s\\n' "${CHROME:-provisioned-chrome}"
  printf 'wasm.chrome.version=%s\\n' "$("${CHROME:-$DEV_CHECK_FAKE_CHROME}" --version)"
  printf 'wasm.chromedriver.path=%s\\n' "${CHROMEDRIVER:-provisioned-driver}"
  printf 'wasm.chromedriver.version=%s\\n' "$("${CHROMEDRIVER:-$DEV_CHECK_FAKE_CHROMEDRIVER}" --version)"
  printf 'wasm.bindgen.required=0.2.118\\n'
  printf 'wasm.bindgen.path=%s\\n' "${WASM_BINDGEN_TEST_RUNNER:-provisioned-runner}"
  printf 'wasm.bindgen.version=%s\\n' "$("${WASM_BINDGEN_TEST_RUNNER:-$DEV_CHECK_FAKE_RUNNER}" --version)"
}
if [[ "${1:-}" == "--identity" ]]; then identity; exit 0; fi
if [[ "${1:-}" == "--prepare" ]]; then
  output="$2"
  identity_file="$output.identity"
  identity > "$identity_file"
  {
    printf 'export TYDE_WASM_TOOLS_PREPARED=1\\n'
    printf 'export CHROME=%q\\n' "${CHROME:-$DEV_CHECK_FAKE_CHROME}"
    printf 'export CHROMEDRIVER=%q\\n' "${CHROMEDRIVER:-$DEV_CHECK_FAKE_CHROMEDRIVER}"
    printf 'export WASM_BINDGEN_TEST_RUNNER=%q\\n' "${WASM_BINDGEN_TEST_RUNNER:-$DEV_CHECK_FAKE_RUNNER}"
    printf 'export WASM_BINDGEN_TEST_WEBDRIVER_JSON=%q\\n' "$output.webdriver.json"
    printf 'export TYDE_WASM_IDENTITY_FILE=%q\\n' "$identity_file"
  } > "$output"
  echo "wasm-prepare" >> "$DEV_CHECK_TEST_LOG"
  exit 0
fi
[[ "${TYDE_WASM_TOOLS_PREPARED:-0}" == 1 ]]
if [[ -n "${DEV_CHECK_EXPECT_WASM_TARGET:-}" ]]; then
  [[ "${CARGO_TARGET_DIR:-}" != *%* ]]
  [[ -L "${CARGO_TARGET_DIR:-}" ]]
  [[ "$(readlink "$CARGO_TARGET_DIR")" == "$DEV_CHECK_EXPECT_WASM_TARGET" ]]
fi
echo "wasm" >> "$DEV_CHECK_TEST_LOG"
if [[ "${DEV_CHECK_FAIL_COMMAND:-}" == "wasm" ]]; then exit 9; fi
""",
            encoding="utf-8",
        )
        wasm_script.chmod(0o755)
        native_script = self.root / "tools" / "provision-native-build-tools.py"
        native_script.write_text(
            """#!/usr/bin/env python3
import os
import pathlib
import sys

output = pathlib.Path(sys.argv[sys.argv.index("--prepare") + 1])
output.write_text('export PATH="${PATH-}"\\n', encoding="utf-8")
with open(os.environ["DEV_CHECK_TEST_LOG"], "a", encoding="utf-8") as log:
    log.write("native-tools-prepare\\n")
""",
            encoding="utf-8",
        )
        native_script.chmod(0o755)
        (self.root / "web" / "loader" / "test").mkdir(parents=True)
        (self.root / "web" / "loader" / "test" / "loader.test.js").write_text(
            "", encoding="utf-8"
        )
        (self.root / "tools" / "test_dev_check.py").write_text(
            """import os
with open(os.environ["DEV_CHECK_TEST_LOG"], "a", encoding="utf-8") as log:
    log.write("contract\\n")
""",
            encoding="utf-8",
        )
        (self.root / ".gitignore").write_text("/target\n", encoding="utf-8")
        (self.root / "tracked.txt").write_text("base\n", encoding="utf-8")

        self._write_fake_commands()
        self._git("init", "-q")
        self._git("config", "user.email", "dev-check@example.com")
        self._git("config", "user.name", "Dev Check Test")
        self._git("add", ".")
        self._git("commit", "-qm", "Initial")

        self.env = os.environ.copy()
        self.env.pop("CI", None)
        self.env.update(
            {
                "PATH": f"{self.bin}:{self.env['PATH']}",
                "DEV_CHECK_TEST_LOG": str(self.log),
                "RUSTUP_TOOLCHAIN": "nightly",
                "TMPDIR": str(pathlib.Path(self.temp.name) / "tmp"),
                "CHROME": str(self.bin / "google-chrome"),
                "CHROMEDRIVER": str(self.bin / "chromedriver"),
                "WASM_BINDGEN_TEST_RUNNER": str(
                    self.bin / "wasm-bindgen-test-runner"
                ),
                "DEV_CHECK_FAKE_CHROME": str(self.bin / "google-chrome"),
                "DEV_CHECK_FAKE_CHROMEDRIVER": str(self.bin / "chromedriver"),
                "DEV_CHECK_FAKE_RUNNER": str(self.bin / "wasm-bindgen-test-runner"),
                "DEV_CHECK_REAL_PYTHON": sys.executable,
                "TYDE_RUN_REAL_AI_TESTS": "must-be-unset",
                "TYDE_LIVE_CODEX_TEST": "must-be-unset",
                "TYDE_RUN_CLAUDE_INTEGRATION": "must-be-unset",
            }
        )
        pathlib.Path(self.env["TMPDIR"]).mkdir()

    def tearDown(self) -> None:
        self.temp.cleanup()

    def _write(self, name: str, content: str) -> None:
        path = self.bin / name
        path.write_text(content, encoding="utf-8")
        path.chmod(0o755)

    def _write_fake_commands(self) -> None:
        self._write(
            "cargo",
            """#!/usr/bin/env bash
set -euo pipefail
case "$*" in
  "-Vv") echo "cargo stable-test (test)"; echo "release: stable-test"; exit 0 ;;
  "nextest --version") echo "cargo-nextest 0.9.100"; exit 0 ;;
esac
echo "successful cargo output that must stay in the stage log"
echo "cargo $* toolchain=${RUSTUP_TOOLCHAIN-unset} real-ai=${TYDE_RUN_REAL_AI_TESTS-unset}/${TYDE_LIVE_CODEX_TEST-unset}/${TYDE_RUN_CLAUDE_INTEGRATION-unset}" >> "$DEV_CHECK_TEST_LOG"
if [[ "${DEV_CHECK_NATIVE_MULTI_FAILURE:-0}" == 1 && "cargo $*" == "cargo nextest run" ]]; then
  echo "FAIL [0.010s] tests::native first_independent_failure" >&2
  echo "first independent failure diagnostics" >&2
  echo "FAIL [0.020s] tests::native second_independent_failure" >&2
  echo "second independent failure diagnostics" >&2
  exit 9
fi
if [[ "${DEV_CHECK_FAIL_COMMAND:-}" == "cargo $*" ]]; then
  if [[ -n "${DEV_CHECK_FAIL_ON_RUN:-}" ]]; then
    count_file="$DEV_CHECK_TEST_LOG.fail-count"
    count=0
    [[ -f "$count_file" ]] && count="$(cat "$count_file")"
    count=$((count + 1))
    printf '%s\\n' "$count" > "$count_file"
    printf 'failure-controlled invocation=%s\\n' "$count"
    [[ "$count" == "$DEV_CHECK_FAIL_ON_RUN" ]] || exit 0
  fi
  echo "complete actionable failure from cargo $*" >&2
  exit 9
fi
""",
        )
        self._write(
            "cargo-nextest",
            "#!/usr/bin/env bash\necho 'cargo-nextest 0.9.100'\n",
        )
        self._write(
            "rustc",
            """#!/usr/bin/env bash
echo "rustc stable-test (test)"
echo "host: test-host"
""",
        )
        self._write(
            "rustup",
            """#!/usr/bin/env bash
case "$*" in
  "update stable")
    echo "rustup update stable toolchain=${RUSTUP_TOOLCHAIN-unset}" >> "$DEV_CHECK_TEST_LOG"
    [[ "${DEV_CHECK_FAIL_COMMAND:-}" == "rustup update stable" ]] && exit 9
    :
    ;;
  "toolchain install")
    echo "rustup toolchain install toolchain=${RUSTUP_TOOLCHAIN-unset}" >> "$DEV_CHECK_TEST_LOG"
    [[ "${DEV_CHECK_FAIL_COMMAND:-}" == "rustup toolchain install" ]] && exit 9
    :
    ;;
  "show active-toolchain") echo "stable-test-host (environment override by RUSTUP_TOOLCHAIN)" ;;
  "target list --installed") printf 'test-host\\nwasm32-unknown-unknown\\n' ;;
  *) exit 2 ;;
esac
""",
        )
        self._write(
            "node",
            """#!/usr/bin/env bash
set -euo pipefail
if [[ "${1:-}" == "--version" ]]; then echo "v22.0.0"; exit 0; fi
echo "node $*" >> "$DEV_CHECK_TEST_LOG"
if [[ "${DEV_CHECK_FAIL_COMMAND:-}" == "node" ]]; then exit 9; fi
""",
        )
        self._write(
            "sccache",
            """#!/usr/bin/env bash
set -euo pipefail
case "$*" in
  "--version") echo "sccache 0.16.0" ;;
  "--start-server") : ;;
  "--show-stats --stats-format=json")
    cache_dir="$SCCACHE_DIR"
    [[ "${DEV_CHECK_BAD_SCCACHE:-0}" == 1 ]] && cache_dir="/wrong-cache"
    python3 - "$cache_dir" <<'PY'
import json
import sys

print(json.dumps({
    "stats": {
        "compile_requests": 0,
        "cache_hits": {"counts": {}},
        "cache_misses": {"counts": {}},
        "cache_errors": {"counts": {}},
        "cache_writes": 0,
    },
    "cache_location": f'Local disk: "{sys.argv[1]}"',
    "cache_size": 0,
    "max_cache_size": 10737418240,
}))
PY
    ;;
  *) exit 2 ;;
esac
""",
        )
        self._write(
            "google-chrome",
            "#!/usr/bin/env bash\necho 'Google Chrome 150.0.7871.102'\n",
        )
        self._write(
            "chromedriver",
            "#!/usr/bin/env bash\necho 'ChromeDriver 150.0.7871.115 test'\n",
        )
        self._write(
            "wasm-bindgen-test-runner",
            "#!/usr/bin/env bash\necho 'wasm-bindgen-test-runner 0.2.118'\n",
        )
        self._write("meson", "#!/usr/bin/env bash\necho '1.11.2'\n")
        self._write("ninja", "#!/usr/bin/env bash\necho '1.11.1'\n")
        self._write(
            "python3",
            """#!/usr/bin/env bash
if [[ "${1:-}" == "--version" ]]; then
  echo "Python ${DEV_CHECK_FAKE_PYTHON_VERSION:-3.test}"
  exit 0
fi
exec "$DEV_CHECK_REAL_PYTHON" "$@"
""",
        )

    def _git(self, *args: str) -> subprocess.CompletedProcess[str]:
        return subprocess.run(
            ["git", *args],
            cwd=self.root,
            text=True,
            capture_output=True,
            check=True,
        )

    def _run(
        self, *args: str, env: dict[str, str] | None = None, check: bool = True
    ) -> subprocess.CompletedProcess[str]:
        result = subprocess.run(
            [str(self.root / "dev.sh"), "check", *args],
            cwd=self.root,
            env=env or self.env,
            text=True,
            capture_output=True,
            check=False,
        )
        if check and result.returncode != 0:
            self.fail(
                f"dev.sh failed with {result.returncode}:\n{result.stdout}\n{result.stderr}"
            )
        return result

    def _log_lines(self) -> list[str]:
        if not self.log.exists():
            return []
        return self.log.read_text(encoding="utf-8").splitlines()

    def _explain_key(self, env: dict[str, str] | None = None) -> str:
        result = self._run("--explain-cache", env=env)
        for line in result.stdout.splitlines():
            if line.startswith("cache.key="):
                return line.removeprefix("cache.key=")
        self.fail(f"cache key missing from output:\n{result.stdout}")

    def _index_digest(self) -> str:
        index = pathlib.Path(self._git("rev-parse", "--git-path", "index").stdout.strip())
        if not index.is_absolute():
            index = self.root / index
        return hashlib.sha256(index.read_bytes()).hexdigest()

    def test_miss_runs_required_counts_then_hit_only_updates_toolchain(self) -> None:
        first = self._run()

        self.assertIn("CACHE MISS", first.stdout)
        self.assertIn("START cargo fmt --all --check", first.stdout)
        self.assertIn("PASS  cargo fmt --all --check (1/1", first.stdout)
        self.assertNotIn("successful cargo output", first.stdout)
        lines = self._log_lines()
        self.assertEqual(lines[:2], [TOOLCHAIN_UPDATE_LOG, TOOLCHAIN_INSTALL_LOG])
        self.assertEqual(sum(line.startswith("cargo fmt ") for line in lines), 1)
        self.assertEqual(sum(line.startswith("cargo check ") for line in lines), 1)
        self.assertEqual(sum(line.startswith("cargo clippy ") for line in lines), 1)
        self.assertEqual(sum(line.startswith("cargo nextest run ") for line in lines), 1)
        self.assertEqual(lines.count("native-tools-prepare"), 1)
        self.assertEqual(lines.count("wasm-prepare"), 1)
        self.assertLess(
            lines.index("native-tools-prepare"), lines.index("wasm-prepare")
        )
        self.assertEqual(lines.count("wasm"), 1)
        self.assertEqual(sum(line.startswith("node --test ") for line in lines), 1)
        self.assertTrue(
            all(
                "real-ai=unset/unset/unset" in line
                for line in lines
                if line.startswith("cargo ")
            )
        )
        self.assertTrue(
            all(
                "toolchain=stable" in line
                for line in lines
                if line.startswith("cargo ")
            )
        )
        records = list((self.root / "target" / "dev-check-cache").glob("*.success"))
        self.assertEqual(len(records), 1)
        record = records[0].read_text(encoding="utf-8")
        self.assertIn("schema=4", record)
        self.assertIn("complete=true", record)
        self.assertTrue(record.endswith("record.end=true\n"))
        self.assertEqual(
            list((self.root / "target" / "dev-check-cache").glob(".success.*")), []
        )
        run_dir = max((self.root / "target" / "dev-check-logs").glob("run-*"))
        metadata = (run_dir / "metadata.txt").read_text(encoding="utf-8")
        self.assertIn("disk.start.", metadata)
        self.assertIn("disk.finish.", metadata)
        self.assertIn("cleanup.reclaimed_bytes=", metadata)
        self.assertIn("sccache.delta.requests=0", metadata)
        self.assertIn("overall.cache=miss", metadata)
        fmt_log = next(run_dir.glob("*-cargo-fmt-all-check.log"))
        self.assertIn(
            "successful cargo output that must stay in the stage log",
            fmt_log.read_text(encoding="utf-8"),
        )

        before = list(lines)
        second = self._run()

        self.assertIn("CACHE HIT", second.stdout)
        self.assertIn("PRIOR PASS  cargo nextest run (1/1", second.stdout)
        self.assertEqual(
            self._log_lines(),
            before
            + [
                TOOLCHAIN_UPDATE_LOG,
                TOOLCHAIN_INSTALL_LOG,
                "native-tools-prepare",
                "wasm-prepare",
            ],
        )

    def test_fingerprint_tracks_commit_and_worktree_content(self) -> None:
        base_key = self._explain_key()
        base_index = self._index_digest()
        self.assertEqual(self._index_digest(), base_index)

        (self.root / "untracked.txt").write_text("new\n", encoding="utf-8")
        untracked_key = self._explain_key()
        self.assertNotEqual(untracked_key, base_key)

        ignored = self.root / "target" / "ignored.txt"
        ignored.parent.mkdir(parents=True, exist_ok=True)
        ignored.write_text("ignored\n", encoding="utf-8")
        self.assertEqual(self._explain_key(), untracked_key)
        (self.root / "untracked.txt").unlink()

        tracked = self.root / "tracked.txt"
        tracked.write_text("unstaged\n", encoding="utf-8")
        unstaged_key = self._explain_key()
        self.assertNotEqual(unstaged_key, base_key)
        index_before = self._index_digest()
        cached_diff_before = self._git("diff", "--cached", "--binary").stdout
        self._explain_key()
        self.assertEqual(self._index_digest(), index_before)
        self.assertEqual(
            self._git("diff", "--cached", "--binary").stdout, cached_diff_before
        )

        self._git("add", "tracked.txt")
        staged_key = self._explain_key()
        self.assertEqual(staged_key, unstaged_key)
        self._git("commit", "-qm", "Update tracked content")
        committed_key = self._explain_key()
        self.assertNotEqual(committed_key, unstaged_key)

        index_after_commit = self._index_digest()
        tracked.unlink()
        deleted_key = self._explain_key()
        self.assertNotEqual(deleted_key, committed_key)
        self.assertEqual(self._index_digest(), index_after_commit)

    def test_percent_encoded_checkout_uses_safe_wasm_target_alias(self) -> None:
        encoded_root = self.root.with_name("repo%2Fworktree")
        self.root.rename(encoded_root)
        self.root = encoded_root
        env = self.env.copy()
        env["DEV_CHECK_EXPECT_WASM_TARGET"] = str(self.root / "target")

        result = self._run(env=env)

        self.assertEqual(result.returncode, 0, result.stderr)
        run_dir = max((self.root / "target" / "dev-check-logs").glob("run-*"))
        metadata = (run_dir / "metadata.txt").read_text(encoding="utf-8")
        alias_line = next(
            line
            for line in metadata.splitlines()
            if line.startswith("wasm.cargo_target_directory=")
        )
        alias = pathlib.Path(alias_line.split("=", 1)[1])
        self.assertNotIn("%", str(alias))
        self.assertFalse(alias.exists())

    def test_fingerprint_ignores_environment_and_tool_identities(self) -> None:
        base_key = self._explain_key()

        chrome = self.bin / "google-chrome"
        chrome.write_text(
            "#!/usr/bin/env bash\necho 'Google Chrome 151.0.8000.1'\n",
            encoding="utf-8",
        )
        chrome.chmod(0o755)
        self.assertEqual(self._explain_key(), base_key)

        chrome.write_text(
            "#!/usr/bin/env bash\necho 'Google Chrome 150.0.7871.102'\n",
            encoding="utf-8",
        )
        chrome.chmod(0o755)
        runner = self.bin / "wasm-bindgen-test-runner"
        runner.write_text(
            "#!/usr/bin/env bash\necho 'wasm-bindgen-test-runner 0.2.119'\n",
            encoding="utf-8",
        )
        runner.chmod(0o755)
        self.assertEqual(self._explain_key(), base_key)

        runner.write_text(
            "#!/usr/bin/env bash\necho 'wasm-bindgen-test-runner 0.2.118'\n",
            encoding="utf-8",
        )
        runner.chmod(0o755)
        changed_config = self.env.copy()
        changed_config["SCCACHE_RECACHE"] = "1"
        changed_config["SCCACHE_BUCKET"] = "must-not-be-used"
        changed_config["SCCACHE_SERVER_PORT"] = "1"
        self.assertEqual(self._explain_key(changed_config), base_key)

        changed_python = self.env.copy()
        changed_python["DEV_CHECK_FAKE_PYTHON_VERSION"] = "3.changed"
        self.assertEqual(self._explain_key(changed_python), base_key)

    def test_environment_and_failures_obey_cache_contract(self) -> None:
        self._run()
        initial_records = list((self.root / "target" / "dev-check-cache").glob("*.success"))
        initial_log_count = len(self._log_lines())

        for removed_option in ("--force", "--no-cache"):
            rejected = self._run(removed_option, check=False)
            self.assertEqual(rejected.returncode, 2)
        self.assertEqual(len(self._log_lines()), initial_log_count)
        self.assertEqual(
            len(list((self.root / "target" / "dev-check-cache").glob("*.success"))),
            len(initial_records),
        )

        env_one = self.env.copy()
        env_one["TYDE_RUN_REAL_LSP_TESTS"] = "one"
        env_two = self.env.copy()
        env_two["TYDE_RUN_REAL_LSP_TESTS"] = "two"
        self.assertEqual(self._explain_key(env_one), self._explain_key(env_two))

        without_real_ai = self.env.copy()
        without_real_ai.pop("TYDE_RUN_REAL_AI_TESTS")
        without_real_ai.pop("TYDE_LIVE_CODEX_TEST")
        without_real_ai.pop("TYDE_RUN_CLAUDE_INTEGRATION")
        self.assertEqual(self._explain_key(), self._explain_key(without_real_ai))

        (self.root / "failure.txt").write_text("new key\n", encoding="utf-8")
        failing_env = self.env.copy()
        failing_env["DEV_CHECK_FAIL_COMMAND"] = "cargo nextest run"
        failing_env["DEV_CHECK_FAIL_ON_RUN"] = "1"
        failed = self._run(env=failing_env, check=False)
        self.assertEqual(failed.returncode, 9)
        self.assertIn("FAIL  cargo nextest run (1/1", failed.stderr)
        self.assertIn(
            "complete actionable failure from cargo nextest run", failed.stderr
        )
        self.assertIn("Failing repetition diagnostics:", failed.stderr)
        self.assertIn("Complete stage log:", failed.stderr)
        self.assertIn("failure-controlled invocation=1", failed.stderr)
        failure_run = max(
            (self.root / "target" / "dev-check-logs").glob("run-*")
        )
        nextest_log = next(failure_run.glob("*-cargo-nextest-run.log"))
        full_log = nextest_log.read_text(encoding="utf-8")
        self.assertIn("failure-controlled invocation=1", full_log)
        failure_metadata = (failure_run / "metadata.txt").read_text(encoding="utf-8")
        self.assertIn("failure_log=", failure_metadata)
        self.assertEqual(
            len(list((self.root / "target" / "dev-check-cache").glob("*.success"))),
            len(initial_records),
        )
        self.assertEqual(
            list((self.root / "target" / "dev-check-cache").glob(".success.*")),
            [],
        )

    def test_native_failure_retains_all_diagnostics_and_gates_later_work(self) -> None:
        env = self.env.copy()
        env["DEV_CHECK_NATIVE_MULTI_FAILURE"] = "1"

        failed = self._run(env=env, check=False)

        self.assertEqual(failed.returncode, 9)
        self.assertIn("FAIL  cargo nextest run (1/1", failed.stderr)
        for diagnostic in (
            "first_independent_failure",
            "first independent failure diagnostics",
            "second_independent_failure",
            "second independent failure diagnostics",
        ):
            self.assertIn(diagnostic, failed.stderr)
        self.assertIn("Complete stage log:", failed.stderr)
        lines = self._log_lines()
        self.assertEqual(sum(line.startswith("cargo nextest run ") for line in lines), 1)
        self.assertNotIn("wasm", lines)
        self.assertFalse(any(line.startswith("node --test ") for line in lines))
        run_dir = max((self.root / "target" / "dev-check-logs").glob("run-*"))
        nextest_log = next(run_dir.glob("*-cargo-nextest-run.log"))
        full_log = nextest_log.read_text(encoding="utf-8")
        self.assertLess(
            full_log.index("first_independent_failure"),
            full_log.index("second_independent_failure"),
        )
        metadata = (run_dir / "metadata.txt").read_text(encoding="utf-8")
        metadata_values = dict(
            line.split("=", maxsplit=1)
            for line in metadata.splitlines()
            if "=" in line
        )
        native_stage = next(
            key.removesuffix(".label")
            for key, value in metadata_values.items()
            if key.endswith(".label") and value == "cargo nextest run"
        )
        self.assertEqual(metadata_values[f"{native_stage}.result"], "FAIL")
        repetition_log = pathlib.Path(
            metadata_values[f"{native_stage}.failure_log"]
        )
        self.assertEqual(repetition_log.parent, run_dir)
        self.assertRegex(repetition_log.name, r"^\.repetition-\d+-1\.log$")
        repetition_output = repetition_log.read_text(encoding="utf-8")
        self.assertIn("first_independent_failure", repetition_output)
        self.assertIn("second_independent_failure", repetition_output)
        self.assertIn(f"{native_stage}.failure_log={repetition_log}", metadata)

    def test_toolchain_update_failure_precedes_cache_evaluation_and_checks(self) -> None:
        self._run()
        before = self._log_lines()
        env = self.env.copy()
        env["DEV_CHECK_FAIL_COMMAND"] = "rustup update stable"

        rejected = self._run(env=env, check=False)

        self.assertEqual(rejected.returncode, 9)
        self.assertIn("FAIL  Update stable Rust toolchain", rejected.stderr)
        self.assertNotIn("CACHE HIT", rejected.stdout)
        self.assertEqual(self._log_lines(), before + [TOOLCHAIN_UPDATE_LOG])

    def test_workflow_toolchain_entrypoint_uses_the_check_update_path(self) -> None:
        result = subprocess.run(
            [str(self.root / "dev.sh"), "rust-toolchain"],
            cwd=self.root,
            env=self.env,
            text=True,
            capture_output=True,
            check=False,
        )

        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(
            self._log_lines(), [TOOLCHAIN_UPDATE_LOG, TOOLCHAIN_INSTALL_LOG]
        )

    def test_pr_and_local_release_guards_use_canonical_check(self) -> None:
        ci_env = self.env.copy()
        ci_env["CI"] = "true"
        result = self._run(env=ci_env)
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertIn("CACHE MISS", result.stdout)

        release_check = (REPO_ROOT / "tools" / "release_check.sh").read_text(
            encoding="utf-8"
        )
        self.assertIn("./dev.sh check\n", release_check)
        self.assertNotIn("./dev.sh check --", release_check)
        release_workflow = (
            REPO_ROOT / ".github" / "workflows" / "release.yml"
        ).read_text(encoding="utf-8")
        self.assertNotIn("run: ./dev.sh check", release_workflow)
        check_workflow = (
            REPO_ROOT / ".github" / "workflows" / "check.yml"
        ).read_text(encoding="utf-8")
        self.assertIn("pull_request:", check_workflow)
        self.assertNotIn("push:", check_workflow)
        self.assertIn("runs-on: ubuntu-latest", check_workflow)
        self.assertIn("run: ./dev.sh check", check_workflow)
        install = "cargo install sccache --version 0.16.0 --locked --force"
        self.assertIn(install, check_workflow)
        self.assertLess(
            check_workflow.index(install),
            check_workflow.index("run: ./dev.sh check"),
        )
        for workflow in (check_workflow, release_workflow):
            self.assertIn("Provision native audio build tools", workflow)
            self.assertIn("tools/provision-native-build-tools.py", workflow)
            self.assertIn('--github-path "$GITHUB_PATH"', workflow)
            self.assertIn('--github-env "$GITHUB_ENV"', workflow)
        provisioner = (
            REPO_ROOT / "tools" / "provision-native-build-tools.py"
        ).read_text(encoding="utf-8")
        self.assertIn('MESON_VERSION = "1.11.2"', provisioner)
        self.assertIn('NINJA_PACKAGE_VERSION = "1.11.1.4"', provisioner)
        self.assertIn('f"meson=={MESON_VERSION}"', provisioner)
        self.assertIn('f"ninja=={NINJA_PACKAGE_VERSION}"', provisioner)

    def test_linux_gui_workflows_install_tauri_dbus_build_dependencies(
        self,
    ) -> None:
        for workflow_name in ("check.yml", "release.yml"):
            workflow = (
                REPO_ROOT / ".github" / "workflows" / workflow_name
            ).read_text(encoding="utf-8")
            linux_install = workflow[
                workflow.index("- name: Install Linux dependencies") :
            ]
            self.assertIn("libdbus-1-dev", linux_install)
            self.assertIn("libasound2-dev", linux_install)
            self.assertIn("clang", linux_install)
            self.assertIn("pkg-config", linux_install)
            self.assertNotIn("autoconf", linux_install)
            self.assertNotIn("automake", linux_install)
            self.assertNotIn("libtool", linux_install)
            self.assertLess(
                linux_install.index("libdbus-1-dev"),
                linux_install.index("- name: Install repository Rust toolchain"),
            )

    def test_native_voice_architecture_and_contract_guards(self) -> None:
        production_sources = "\n".join(
            path.read_text(encoding="utf-8")
            for root in (
                "client/src",
                "dev-driver/src",
                "frontend/src",
                "frontend/tauri-shell/src",
                "mobile-frontend/src",
                "mqtt-transport/src",
                "protocol/src",
                "server/src",
            )
            for path in (REPO_ROOT / root).rglob("*.rs")
        )
        for forbidden in ("UdpSocket", "RtcPeerConnection", "RtcIceCandidate",
                          "RTCIceCandidate", "StunServer", "TurnServer", "stun://", "turn://", "mdns-sd",
                          "VoiceIceCandidate"):
            self.assertNotIn(forbidden, production_sources)

        manifests = "\n".join(
            (REPO_ROOT / relative).read_text(encoding="utf-8")
            for relative in (
                "Cargo.toml",
                "server/Cargo.toml",
                "frontend/Cargo.toml",
                "frontend/tauri-shell/Cargo.toml",
                "mobile-frontend/Cargo.toml",
            )
        )
        for forbidden in ("str0m", "mdns-sd", "webrtc =", "webrtc-ice",
                          "webrtc-dtls", "webrtc-sctp"):
            self.assertNotIn(forbidden, manifests)
        self.assertEqual(
            native_voice_vendor_surface_violations(
                (REPO_ROOT / "Cargo.toml").read_text(encoding="utf-8"),
                native_voice_vendor_surfaces(REPO_ROOT),
            ),
            [],
            "only the pinned local AEC DSP may occupy vendored build surfaces",
        )

        shell_sources = [
            (REPO_ROOT / relative).read_text(encoding="utf-8")
            for relative in (
                "frontend/tauri-shell/src/lib.rs",
                "frontend/tauri-shell/src/router.rs",
                "frontend/src/bridge.rs",
            )
        ]
        shell_bridge = "\n".join(shell_sources)
        self.assertEqual(
            [
                signature
                for source in shell_sources
                for signature in raw_pcm_desktop_ipc_signatures(source)
            ],
            [],
            "raw PCM buffers must not appear in desktop Tauri command or bridge signatures",
        )
        self.assertIn("opus: Vec<u8>", shell_bridge)

        protocol_source = (REPO_ROOT / "protocol/src/framing.rs").read_text()
        for required in ("RECORD_MAGIC", "MAX_RECORD_BODY", "FrameReader",
                         "checksum mismatch", "fragment reassembly"):
            self.assertIn(required, protocol_source)
        scheduler = (REPO_ROOT / "server/src/stream.rs").read_text()
        for required in ("CONTROL_LIMIT", "CHAT_LIMIT", "BULK_LIMIT",
                         "AUDIO_PACKET_LIMIT: usize = 8", "discard_voice_audio"):
            self.assertIn(required, scheduler)
        voice_tests = (REPO_ROOT / "tests/tests/native_voice.rs").read_text()
        for required in ("run_connection_with_synthetic_voice", "start_production_writer_probe",
                         "start_plain_mqtt_broker", "FrameKind::VoiceInterrupt",
                         "FrameKind::ProjectFileContents", "4 * 1024 * 1024",
                         "voice_settings_refresh_capabilities_for_every_live_connection"):
            self.assertIn(required, voice_tests)
        server_voice = (REPO_ROOT / "server/src/voice.rs").read_text()
        server_connection = (REPO_ROOT / "server/src/connection.rs").read_text()
        server_host = (REPO_ROOT / "server/src/host.rs").read_text()
        for required in ("NovaInput::Interrupt", "output_generation", "discard_voice_audio",
                         "agent_handle_for_instance", "ObserverGuard"):
            self.assertIn(required, server_voice)
        shell_media = (REPO_ROOT / "frontend/tauri-shell/src/voice_media.rs").read_text()
        shell_router = (REPO_ROOT / "frontend/tauri-shell/src/router.rs").read_text()
        devtools_manifest = (REPO_ROOT / "devtools-protocol/Cargo.toml").read_text()
        server_manifest = (REPO_ROOT / "server/Cargo.toml").read_text()
        shell_manifest = (REPO_ROOT / "frontend/tauri-shell/Cargo.toml").read_text()
        frontend_manifest = (REPO_ROOT / "frontend/Cargo.toml").read_text()
        devtools_source = (REPO_ROOT / "devtools-protocol/src/lib.rs").read_text()
        shell_lib = (REPO_ROOT / "frontend/tauri-shell/src/lib.rs").read_text()
        removed_debug_feature = "voice-" + "debug-pipeline"
        manifest_paths = [REPO_ROOT / "Cargo.toml"]
        manifest_paths.extend(REPO_ROOT.glob("*/Cargo.toml"))
        manifest_paths.extend(REPO_ROOT.glob("*/*/Cargo.toml"))
        for manifest_path in manifest_paths:
            self.assertNotIn(
                removed_debug_feature,
                manifest_path.read_text(encoding="utf-8"),
                f"removed voice instrumentation feature returned in {manifest_path}",
            )
        self.assertIn('default = ["launcher"]', devtools_manifest)
        self.assertIn("default = []", server_manifest)
        self.assertIn("default = []", shell_manifest)
        for cargo_surface in (
            devtools_manifest,
            server_manifest,
            shell_manifest,
            frontend_manifest,
        ):
            self.assertNotIn(removed_debug_feature, cargo_surface)
        for rust_surface in (
            devtools_source,
            server_connection,
            server_host,
            server_voice,
            shell_media,
            shell_router,
            shell_lib,
        ):
            self.assertNotIn(removed_debug_feature, rust_surface)
        self.assertFalse((REPO_ROOT / "tools/run-branch-debug-voice.sh").exists())
        self.assertNotIn("debug-voice-smoke", (REPO_ROOT / "dev.sh").read_text())
        for removed_scaffold in (
            "record_dev_instance_voice_pipeline",
            "record_dev_instance_voice_connection",
            "DevInstanceVoicePipeline",
            "voice_debug_connection_snapshot",
            "record_voice_webview_outcome",
        ):
            self.assertNotIn(
                removed_scaffold,
                devtools_source
                + server_connection
                + server_host
                + server_voice
                + shell_media
                + shell_router
                + shell_lib,
            )
        for production_entry in (
            "build.sh",
            ".github/workflows/check.yml",
            ".github/workflows/release.yml",
        ):
            production_source = (REPO_ROOT / production_entry).read_text()
            self.assertNotIn(
                removed_debug_feature,
                production_source,
                f"{production_entry} must not enable dev-instance audio instrumentation",
            )
            self.assertNotIn(
                "debug-assertions=yes",
                production_source,
                f"{production_entry} must preserve release instrumentation exclusion",
            )
        self.assertIn("authorized by VoiceAccepted", shell_media)
        self.assertTrue(
            native_voice_authorization_is_validated(shell_router),
            "native media must be authorized by a parsed server VoiceAccepted payload",
        )
        self.assertFalse(
            native_voice_authorization_is_validated(
                shell_router.replace(
                    "protocol::FrameKind::VoiceAccepted",
                    "protocol::FrameKind::VoiceStart",
                    1,
                )
            ),
            "authorization on unvalidated client input must fail the guard",
        )
        self.assertFalse(
            native_voice_authorization_is_validated(
                shell_router.replace(
                    ".parse_payload::<protocol::VoiceAcceptedPayload>()",
                    ".parse_payload()",
                    1,
                )
            ),
            "authorization without typed payload validation must fail the guard",
        )
        self.assertFalse(
            native_voice_authorization_is_validated(
                shell_router.replace(".authorize(host_id.clone(), accepted.generation)", "", 1)
            ),
            "removing native media authorization must fail the guard",
        )
        self.assertIn('name("tyde-native-audio"', shell_media)
        self.assertIn("ControlCommand::Shutdown", shell_media)
        self.assertIn("acknowledgement timed out; media is fail-closed", shell_media)
        self.assertNotIn("Mutex<Option<(String, u64, Session)>>", shell_media)

        self.assertIn("playback_epoch", shell_media)
        mobile_media = (REPO_ROOT / "mobile-frontend/voice-media.js").read_text()
        mobile_codec = (REPO_ROOT / "mobile-frontend/voice-codec-worker.js").read_text()
        self.assertLess(mobile_media.index("startWait.promise"), mobile_media.index("getUserMedia"))
        self.assertIn("AudioEncoder.isConfigSupported", mobile_codec)
        self.assertIn("frame.sampleRate", mobile_codec)
        self.assertIn("TOOL_INACTIVITY", server_voice)
        self.assertIn("Decoder::new(16_000", server_voice)
        self.assertIn("fatal_overflow", scheduler)
        self.assertIn("categorize_start_failure", (REPO_ROOT / "server/src/voice_aws.rs").read_text())

        entitlements = (REPO_ROOT / "frontend/tauri-shell/Entitlements.plist").read_text()
        info = (REPO_ROOT / "frontend/tauri-shell/Info.plist").read_text()
        import xml.etree.ElementTree as ET
        ET.parse(REPO_ROOT / "frontend/tauri-shell/Entitlements.plist")
        ET.parse(REPO_ROOT / "frontend/tauri-shell/Info.plist")
        build = (REPO_ROOT / "build.sh").read_text()
        tauri_config = (REPO_ROOT / "frontend/tauri-shell/tauri.conf.json").read_text()
        self.assertIn("com.apple.security.device.audio-input", entitlements)
        self.assertIn("NSMicrophoneUsageDescription", info)
        self.assertIn('--entitlements "$SCRIPT_DIR/frontend/tauri-shell/Entitlements.plist"', build)
        self.assertIn('codesign -d --entitlements :- "$target"', build)
        self.assertIn('"infoPlist": "Info.plist"', tauri_config)
        for backend in ("server/src/backend/codex.rs", "server/src/backend/acp/mod.rs"):
            source = (REPO_ROOT / backend).read_text()
            self.assertIn("serde_json::from_str::<Value>(&line)", source)
        self.assertIn(
            '"MediaDevices"',
            (REPO_ROOT / "mobile-frontend/Cargo.toml").read_text(),
        )
        self.assertIn(
            "VoiceOver/TalkBack",
            (REPO_ROOT / "mobile-frontend/src/components/bottom_nav.rs").read_text(),
        )

    def test_native_voice_vendor_guard_rejects_network_mutations(self) -> None:
        manifest = (REPO_ROOT / "Cargo.toml").read_text(encoding="utf-8")
        surfaces = native_voice_vendor_surfaces(REPO_ROOT)
        self.assertEqual(native_voice_vendor_surface_violations(manifest, surfaces), [])

        patched = manifest.replace(
            NATIVE_VOICE_AEC_PATCH,
            f'{NATIVE_VOICE_AEC_PATCH}\nstr0m = {{ path = "vendor/str0m" }}',
        )
        self.assertTrue(native_voice_vendor_surface_violations(patched, surfaces))
        subtable_patch = (
            f'{manifest}\n[patch.crates-io.transport-shim]\n'
            'git = "https://example.invalid/network-transport"\n'
        )
        self.assertTrue(
            native_voice_vendor_surface_violations(subtable_patch, surfaces)
        )
        source_patch = (
            f'{manifest}\n[patch."https://example.invalid/index"]\n'
            'transport-shim = { git = "https://example.invalid/transport" }\n'
        )
        self.assertTrue(native_voice_vendor_surface_violations(source_patch, surfaces))

        mutations = {
            "udp dependency": (
                "vendor/webrtc-audio-processing-sys/Cargo.toml",
                '\n[dependencies]\nudp-network = "1"\n',
            ),
            "ICE source": (
                "vendor/webrtc-audio-processing-sys/src/lib.rs",
                "\nstruct IceCandidate;\n",
            ),
            "STUN source": (
                "vendor/webrtc-audio-processing-sys/src/lib.rs",
                '\nconst ENDPOINT: &str = "stun://127.0.0.1";\n',
            ),
            "TURN source": (
                "vendor/webrtc-audio-processing-sys/src/lib.rs",
                '\nconst ENDPOINT: &str = "turn://127.0.0.1";\n',
            ),
            "literal comment marker cannot hide code": (
                "vendor/webrtc-audio-processing-sys/src/lib.rs",
                '\nconst TEXT: &str = "https://safe.invalid"; struct IceCandidate;\n',
            ),
        }
        for label, (path, addition) in mutations.items():
            mutated = dict(surfaces)
            mutated[path] += addition
            self.assertTrue(
                native_voice_vendor_surface_violations(manifest, mutated), label
            )

        extra_vendor = dict(surfaces)
        extra_vendor["vendor/network-stack/Cargo.toml"] = (
            '[package]\nname = "network-stack"\nversion = "1.0.0"\n'
        )
        self.assertTrue(
            native_voice_vendor_surface_violations(manifest, extra_vendor)
        )

    def test_native_voice_vendor_guard_pins_offline_abseil_cache(self) -> None:
        manifest = (REPO_ROOT / "Cargo.toml").read_text(encoding="utf-8")
        surfaces = native_voice_vendor_surfaces(REPO_ROOT)
        self.assertEqual(native_voice_vendor_surface_violations(manifest, surfaces), [])

        for path in NATIVE_VOICE_ABSEIL_CACHE:
            missing = dict(surfaces)
            del missing[path]
            self.assertTrue(
                native_voice_vendor_surface_violations(manifest, missing), path
            )

            with tempfile.TemporaryDirectory() as temp:
                artifact = pathlib.Path(temp) / pathlib.PurePosixPath(path).name
                artifact.write_bytes((REPO_ROOT / path).read_bytes() + b"tampered")
                data = artifact.read_bytes()
                tampered = dict(surfaces)
                tampered[path] = (
                    f"sha256:{hashlib.sha256(data).hexdigest()}:bytes:{len(data)}"
                )
                self.assertTrue(
                    native_voice_vendor_surface_violations(manifest, tampered), path
                )

        for wrap_line in NATIVE_VOICE_ABSEIL_WRAP_LINES:
            wrap_drift = dict(surfaces)
            wrap_drift[NATIVE_VOICE_ABSEIL_WRAP] = wrap_drift[
                NATIVE_VOICE_ABSEIL_WRAP
            ].replace(wrap_line, f"{wrap_line}-tampered")
            self.assertTrue(
                native_voice_vendor_surface_violations(manifest, wrap_drift),
                wrap_line,
            )

        downloadable = dict(surfaces)
        build_path = f"{NATIVE_VOICE_AEC_VENDOR}/build.rs"
        downloadable[build_path] = downloadable[build_path].replace(
            '.arg("--wrap-mode=nodownload")', ""
        )
        self.assertTrue(native_voice_vendor_surface_violations(manifest, downloadable))

        partial_fallback = dict(surfaces)
        partial_fallback[build_path] = partial_fallback[build_path].replace(
            NATIVE_VOICE_ABSEIL_FORCE_FALLBACK,
            "--force-fallback-for=absl_base",
        )
        self.assertTrue(native_voice_vendor_surface_violations(manifest, partial_fallback))

        system_abseil = dict(surfaces)
        system_abseil[build_path] += '\npkg_config::Config::new().probe("absl_base");\n'
        self.assertTrue(native_voice_vendor_surface_violations(manifest, system_abseil))

        no_recovery = dict(surfaces)
        no_recovery[build_path] = no_recovery[build_path].replace(
            "remove_materialized_abseil", "leave_materialized_abseil"
        )
        self.assertTrue(native_voice_vendor_surface_violations(manifest, no_recovery))

    def test_native_voice_patch_reader_is_python39_and_dependency_free(self) -> None:
        source = pathlib.Path(__file__).read_text(encoding="utf-8")
        self.assertNotIn("toml" + "lib", source)
        equivalents = (
            """
            [patch.crates-io] # semantic source
            webrtc-audio-processing-sys = { path = "vendor/webrtc-audio-processing-sys" } # pinned
            """,
            """
            [patch.crates-io.webrtc-audio-processing-sys]
            path = "vendor/webrtc-audio-processing-sys"
            """,
            """
            [patch."crates-io"]
            "webrtc-audio-processing-sys" = { path = "vendor/webrtc-audio-processing-sys" }
            """,
            """
            [patch]
            crates-io = { webrtc-audio-processing-sys = { path = "vendor/webrtc-audio-processing-sys" } }
            """,
        )
        escapes = (
            """
            [patch.crates-io]
            webrtc-audio-processing-sys = { path = "vendor/webrtc-audio-processing-sys" }
            [patch.crates-io.transport-shim]
            git = "https://example.invalid/network"
            """,
            """
            [patch.crates-io]
            webrtc-audio-processing-sys = { path = "vendor/webrtc-audio-processing-sys" }
            [patch."https://example.invalid/index"]
            transport-shim = { git = "https://example.invalid/network" }
            """,
        )
        malformed = (
            '[patch.crates-io]\nwebrtc-audio-processing-sys = { path = "unterminated"\n',
            '[[patch.crates-io]]\nname = "ambiguous"\n',
        )
        with mock.patch.dict(sys.modules, {"toml" + "lib": None}):
            for equivalent in equivalents:
                self.assertTrue(expected_native_voice_patch_surface(equivalent))
            for escape in escapes:
                self.assertFalse(expected_native_voice_patch_surface(escape))
            for invalid in malformed:
                self.assertFalse(expected_native_voice_patch_surface(invalid))

    def test_native_voice_vendor_guard_tokenizes_meson_names(self) -> None:
        manifest = (REPO_ROOT / "Cargo.toml").read_text(encoding="utf-8")
        surfaces = native_voice_vendor_surfaces(REPO_ROOT)
        path = "vendor/webrtc-audio-processing-sys/meson.build"
        for safe_name in ("device", "service", "notice", "return"):
            mutated = dict(surfaces)
            mutated[path] = f"dependency('{safe_name}')\n"
            self.assertEqual(
                native_voice_vendor_surface_violations(manifest, mutated), [], safe_name
            )
        for rejected_name in ("lib-ice", "udp-transport", "mdns-sd"):
            mutated = dict(surfaces)
            mutated[path] = f"subproject('{rejected_name}')\n"
            self.assertTrue(
                native_voice_vendor_surface_violations(manifest, mutated), rejected_name
            )

    def test_raw_pcm_guard_inspects_ipc_signatures_not_prose(self) -> None:
        safe = """
        // PCM is intentionally confined to the native audio engine.
        #[tauri::command]
        async fn voice_media_push(opus: Vec<u8>, pcm_clock: u64) {}
        """
        self.assertEqual(raw_pcm_desktop_ipc_signatures(safe), [])
        for unsafe in (
            "#[tauri::command] fn leak(samples: Vec<i16>) {}",
            "pub async fn voice_media_push(raw_audio: Vec<u8>) {}",
            "pub fn send_host_frame(pcm: &[f32]) {}",
            "#[derive(Serialize)] struct Event { pcm_samples: Vec<u8> }",
            "#[derive(Serialize)] struct Event { audio: [i16; 480] }",
        ):
            self.assertTrue(raw_pcm_desktop_ipc_signatures(unsafe), unsafe)

    def test_contract_stage_is_reachable_without_recursive_checks(self) -> None:
        env = self.env.copy()
        env["DEV_CHECK_CONTRACT_CHILD"] = "1"

        result = self._run(env=env)

        self.assertIn("START dev check contract tests", result.stdout)
        self.assertIn("PASS  dev check contract tests (1/1", result.stdout)
        self.assertEqual(self._log_lines().count("contract"), 1)

    def test_lock_contention_fails_immediately_with_owner(self) -> None:
        lock = self.root / "target" / "dev-check.lock"
        lock.mkdir(parents=True)
        (lock / "owner").write_text(
            f"pid={os.getpid()}\nrepository={self.root}\n", encoding="utf-8"
        )

        rejected = self._run(check=False)

        self.assertEqual(rejected.returncode, 1)
        self.assertIn("another ./dev.sh check is already running", rejected.stderr)
        self.assertIn(f"PID {os.getpid()}", rejected.stderr)
        self.assertEqual(self._log_lines(), [])

    def test_invalid_and_partial_cache_records_are_never_hits(self) -> None:
        first = self._run()
        self.assertIn("CACHE MISS", first.stdout)
        record = next(
            (self.root / "target" / "dev-check-cache").glob("*.success")
        )
        original = record.read_text(encoding="utf-8")
        record.write_text(original.removesuffix("record.end=true\n"), encoding="utf-8")
        before = len(self._log_lines())

        rerun = self._run()

        self.assertIn("CACHE MISS", rerun.stdout)
        self.assertGreater(len(self._log_lines()), before)
        self.assertTrue(record.read_text(encoding="utf-8").endswith("record.end=true\n"))
        self.assertEqual(
            list((self.root / "target" / "dev-check-cache").glob(".success.*")), []
        )

    def test_wrong_sccache_version_fails_instead_of_falling_back(self) -> None:
        sccache = self.bin / "sccache"
        contents = sccache.read_text(encoding="utf-8")
        sccache.write_text(
            contents.replace("sccache 0.16.0", "sccache 0.15.0"),
            encoding="utf-8",
        )
        sccache.chmod(0o755)

        rejected = self._run(check=False)

        self.assertEqual(rejected.returncode, 1)
        self.assertIn("required sccache 0.16.0", rejected.stderr)
        self.assertIn("cargo install sccache --version 0.16.0 --locked", rejected.stderr)
        self.assertIn("Failing repetition diagnostics:", rejected.stderr)
        self.assertFalse(
            any(line.startswith("cargo fmt ") for line in self._log_lines())
        )

    # test_identity_failure_preserves_underlying_diagnostics_and_status was
    # removed: "Move checks to pull requests" (f604545) deleted
    # environment_identity from dev.sh — cache identity is now the schema plus
    # the git worktree fingerprint, and python3 --version is no longer probed —
    # so the failure path that test pinned no longer exists by design.

    def test_sccache_validation_failure_has_log_and_failure_stats(self) -> None:
        env = self.env.copy()
        env["DEV_CHECK_BAD_SCCACHE"] = "1"

        rejected = self._run(env=env, check=False)

        self.assertEqual(rejected.returncode, 1)
        self.assertIn("sccache is not using the check-local cache", rejected.stderr)
        self.assertIn("Failing repetition diagnostics:", rejected.stderr)
        run_dir = max((self.root / "target" / "dev-check-logs").glob("run-*"))
        metadata = (run_dir / "metadata.txt").read_text(encoding="utf-8")
        self.assertIn("stage.05.log=", metadata)
        self.assertIn("stage.05.failure_log=", metadata)
        self.assertIn("sccache.failure_stats=", metadata)

    def test_explain_cache_is_non_destructive_and_does_not_provision(self) -> None:
        logs = self.root / "target" / "dev-check-logs"
        logs.mkdir(parents=True)
        sentinel = logs / "run-sentinel"
        sentinel.mkdir()
        orphan = self.root / "target" / "dev-check-cache" / ".success.orphan"
        orphan.parent.mkdir(parents=True)
        orphan.write_text("partial\n", encoding="utf-8")

        result = self._run("--explain-cache")

        self.assertIn("cache.key=", result.stdout)
        self.assertTrue(sentinel.exists())
        self.assertTrue(orphan.exists())
        self.assertEqual(self._log_lines(), [])
        self.assertEqual(list(logs.glob("run-20*")), [])

    def test_cold_build_tools_provision_before_cache_identity_once(self) -> None:
        env = self.env.copy()
        env.pop("CHROME")
        env.pop("CHROMEDRIVER")
        env.pop("WASM_BINDGEN_TEST_RUNNER")

        result = self._run(env=env)

        self.assertIn("PASS  Provision native build tools", result.stdout)
        self.assertIn("PASS  Provision wasm test tools", result.stdout)
        self.assertEqual(self._log_lines().count("native-tools-prepare"), 1)
        self.assertEqual(self._log_lines().count("wasm-prepare"), 1)
        self.assertEqual(self._log_lines().count("wasm"), 1)
        self.assertEqual(
            len(list((self.root / "target" / "dev-check-cache").glob("*.success"))),
            1,
        )

    def test_success_retention_uses_mtime_and_removes_orphan_temp(self) -> None:
        cache = self.root / "target" / "dev-check-cache"
        cache.mkdir(parents=True)
        records = []
        for index in range(18):
            record = cache / f"{index:02x}.success"
            record.write_text("old\n", encoding="utf-8")
            os.utime(record, (1000 + index, 1000 + index))
            records.append(record)
        orphan = cache / ".success.interrupted"
        orphan.write_text("partial\n", encoding="utf-8")

        self._run()

        self.assertFalse(records[0].exists())
        self.assertFalse(records[1].exists())
        self.assertTrue(all(record.exists() for record in records[2:]))
        self.assertFalse(orphan.exists())

    def test_empty_cleanup_directories_are_valid_under_nounset(self) -> None:
        target = self.root / "target"
        (target / "dev-check-logs").mkdir(parents=True)
        (target / "dev-check-cache").mkdir()

        result = self._run()

        self.assertIn("RESULT PASS", result.stdout)


class TestingBehaviorContractTests(unittest.TestCase):
    def test_nextest_profiles_preserve_failure_and_output_policy(self) -> None:
        source = (REPO_ROOT / ".config" / "nextest.toml").read_text(
            encoding="utf-8"
        )
        shared = {
            'global-timeout = "5m"',
            'fail-fast = false',
            'status-level = "slow"',
            'final-status-level = "slow"',
            'success-output = "never"',
            'failure-output = "final"',
        }
        profile_specific = {
            "default": {'slow-timeout = "30s"', "retries = 0"},
            "ci": {'slow-timeout = "60s"', "retries = 2"},
        }
        for profile in ("default", "ci"):
            body = source.split(f"[profile.{profile}]\n", 1)[1].split(
                f"\n[[profile.{profile}.scripts]]", 1
            )[0]
            lines = [line.strip() for line in body.splitlines()]
            for setting in shared | profile_specific[profile]:
                self.assertEqual(
                    lines.count(setting),
                    1,
                    f"profile {profile} must contain {setting}",
                )

    def test_fixture_tracing_defaults_to_warn_without_hiding_rust_log(self) -> None:
        source = (REPO_ROOT / "tests" / "tests" / "fixture.rs").read_text(
            encoding="utf-8"
        )
        self.assertIn(
            ".with_default_directive(tracing_subscriber::filter::LevelFilter::WARN.into())",
            source,
        )
        self.assertIn(".from_env_lossy()", source)
        self.assertNotIn("EnvFilter::from_default_env()", source)

    def test_workbench_watcher_exception_is_exact_and_test_local(self) -> None:
        source = (REPO_ROOT / "tests" / "tests" / "workbenches.rs").read_text(
            encoding="utf-8"
        )
        helper = source.split("async fn expect_project_notify", 1)[1].split(
            "async fn expect_command_error", 1
        )[0]
        for diagnostic in (
            "context={context}",
            "envelope_stream={}",
            "request_kind={:?}",
            "operation={}",
            "code={:?}",
            "message={:?}",
            "fatal={}",
        ):
            self.assertIn(diagnostic, helper)

        test_body = source.split(
            "async fn workbench_remove_succeeds_when_worktree_dir_was_deleted_out_of_band()",
            1,
        )[1].split("\n#[tokio::test]", 1)[0]
        for exact_match in (
            'assert_eq!(env.stream, expected_stream',
            'assert_eq!(error.stream, expected_stream',
            "assert_eq!(error.request_kind, FrameKind::ProjectFileList)",
            'assert_eq!(error.operation, "project_watch")',
            "assert_eq!(error.code, CommandErrorCode::Internal)",
            "assert!(error.fatal",
            "error.message.contains(deleted_root.as_ref())",
        ):
            self.assertIn(exact_match, test_body)
        self.assertIn("!tolerated_watcher_error", test_body)


class NextestWrapperContractTests(unittest.TestCase):
    def test_lock_release_requires_current_owner(self) -> None:
        source = (REPO_ROOT / "tools" / "run-nextest-binary.sh").read_text(
            encoding="utf-8"
        )
        self.assertIn("LOCK_HELD=false", source)
        self.assertIn('if [[ "$owner_pid" == "$$" ]]', source)
        self.assertIn('if mkdir "$lock_dir"', source)
        self.assertIn('lease_dir="$(mktemp', source)
        self.assertIn("ownerless_grace_seconds=5", source)


@unittest.skipUnless(platform.system() == "Darwin", "macOS clone wrapper")
class NextestCloneTests(unittest.TestCase):
    def test_logical_target_replaces_stale_clone_and_dead_lock(self) -> None:
        with tempfile.TemporaryDirectory() as temp_name:
            temp = pathlib.Path(temp_name)
            root = temp / "repo"
            tools = root / "tools"
            tools.mkdir(parents=True)
            wrapper = tools / "run-nextest-binary.sh"
            shutil.copy2(REPO_ROOT / "tools" / "run-nextest-binary.sh", wrapper)
            tmpdir = temp / "tmp"
            tmpdir.mkdir()
            env = os.environ.copy()
            env["TMPDIR"] = str(tmpdir)

            first = root / "sample-aaaaaaaaaaaaaaaa"
            second = root / "sample-bbbbbbbbbbbbbbbb"
            for binary in (first, second):
                binary.write_text("#!/usr/bin/env bash\nexit 0\n", encoding="utf-8")
                binary.chmod(0o755)

            subprocess.run([str(wrapper), str(first)], env=env, check=True)
            workspace = next((tmpdir / "tyde-nextest").iterdir())
            lock = workspace / "sample.lock"
            lock.mkdir()
            old = time.time() - 10
            os.utime(lock, (old, old))

            subprocess.run([str(wrapper), str(second)], env=env, check=True)

            clones = [path for path in workspace.glob("sample.*") if path.is_file()]
            self.assertEqual(len(clones), 1)
            self.assertFalse(lock.exists())
            partial_lock = workspace / "partial.lock"
            partial_lease = workspace / "sample.partial.use.ownerless"
            recent_lock = workspace / "recent.lock"
            recent_lease = workspace / "sample.recent.use.ownerless"
            partial_lock.mkdir()
            partial_lease.write_text("", encoding="utf-8")
            recent_lock.mkdir()
            recent_lease.write_text("", encoding="utf-8")
            os.utime(partial_lock, (old, old))
            os.utime(partial_lease, (old, old))
            for index in range(70):
                extra = workspace / f"extra.{index:02d}"
                extra.write_text("x", encoding="utf-8")
                extra.chmod(0o755)
                os.utime(extra, (1000 + index, 1000 + index))
            cleanup_env = env.copy()
            cleanup_env["TYDE_DEV_CHECK_LOCK_HELD"] = "1"
            cleanup = subprocess.run(
                [str(wrapper), "--cleanup-stale"],
                env=cleanup_env,
                text=True,
                capture_output=True,
                check=True,
            )
            self.assertGreater(int(cleanup.stdout), 0)
            self.assertTrue(workspace.exists())
            self.assertFalse(partial_lock.exists())
            self.assertFalse(partial_lease.exists())
            self.assertTrue(recent_lock.exists())
            self.assertTrue(recent_lease.exists())
            self.assertLessEqual(
                len(
                    [
                        path
                        for path in workspace.iterdir()
                        if path.is_file() and os.access(path, os.X_OK)
                    ]
                ),
                64,
            )


class WasmToolScriptTests(unittest.TestCase):
    def test_identity_is_read_only_and_prepare_pins_exact_overrides(self) -> None:
        with tempfile.TemporaryDirectory() as temp_name:
            root = pathlib.Path(temp_name) / "repo"
            tools = root / "tools"
            binaries = root / "bin"
            tools.mkdir(parents=True)
            binaries.mkdir()
            script = tools / "run-wasm-tests.sh"
            shutil.copy2(REPO_ROOT / "tools" / "run-wasm-tests.sh", script)
            (root / "Cargo.lock").write_text(
                'name = "wasm-bindgen"\nversion = "0.2.118"\n',
                encoding="utf-8",
            )

            chrome = binaries / "chrome"
            driver = binaries / "chromedriver"
            runner = binaries / "wasm-bindgen-test-runner"
            chrome.write_text(
                "#!/usr/bin/env bash\necho 'Google Chrome 150.0.7871.102'\n",
                encoding="utf-8",
            )
            driver.write_text(
                "#!/usr/bin/env bash\necho 'ChromeDriver 150.0.7871.115'\n",
                encoding="utf-8",
            )
            runner.write_text(
                "#!/usr/bin/env bash\necho 'wasm-bindgen-test-runner 0.2.118'\n",
                encoding="utf-8",
            )
            for binary in (chrome, driver, runner):
                binary.chmod(0o755)
            env = os.environ.copy()
            env.update(
                {
                    "CHROME": str(chrome),
                    "CHROMEDRIVER": str(driver),
                    "WASM_BINDGEN_TEST_RUNNER": str(runner),
                }
            )
            webdriver = root / "webdriver.json"
            webdriver.write_text('{"capabilities": "external"}\n', encoding="utf-8")
            env["WASM_BINDGEN_TEST_WEBDRIVER_JSON"] = str(webdriver)

            identity = subprocess.run(
                [str(script), "--identity"],
                env=env,
                text=True,
                capture_output=True,
                check=True,
            )

            self.assertIn(f"wasm.chrome.path={chrome}", identity.stdout)
            self.assertIn(f"wasm.chromedriver.path={driver}", identity.stdout)
            webdriver_hash = hashlib.sha256(webdriver.read_bytes()).hexdigest()
            self.assertIn(
                f"wasm.webdriver.identity=sha256:{webdriver_hash}", identity.stdout
            )
            self.assertFalse((root / "target").exists())
            source = script.read_text(encoding="utf-8")
            self.assertIn(
                'if [[ "$mode" == "prepare" && $downloaded_driver -eq 1', source
            )
            self.assertIn("wasm.webdriver.identity=sha256:", source)
            self.assertIn('validate_prepared_identity "$prepared_identity"', source)
            self.assertIn('export PATH="$(dirname "$runner_bin"):$PATH"', source)

            prepared = root / "prepared.env"
            subprocess.run(
                [str(script), "--prepare", str(prepared)],
                env=env,
                text=True,
                capture_output=True,
                check=True,
            )
            prepared_text = prepared.read_text(encoding="utf-8")
            self.assertIn(f"export CHROME={chrome}", prepared_text)
            self.assertIn(f"export CHROMEDRIVER={driver}", prepared_text)
            self.assertTrue(pathlib.Path(f"{prepared}.identity").is_file())

            (root / "Cargo.lock").write_text(
                'name = "wasm-bindgen"\nversion = "0.2.119"\n',
                encoding="utf-8",
            )
            chrome.write_text(
                "#!/usr/bin/env bash\necho 'Google Chrome 151.0.8000.1'\n",
                encoding="utf-8",
            )
            driver.write_text(
                "#!/usr/bin/env bash\necho 'ChromeDriver 151.0.8000.2'\n",
                encoding="utf-8",
            )
            runner.write_text(
                "#!/usr/bin/env bash\necho 'wasm-bindgen-test-runner 0.2.119'\n",
                encoding="utf-8",
            )
            updated = root / "updated.env"
            subprocess.run(
                [str(script), "--prepare", str(updated)],
                env=env,
                text=True,
                capture_output=True,
                check=True,
            )
            updated_identity = pathlib.Path(f"{updated}.identity").read_text(
                encoding="utf-8"
            )
            self.assertIn("wasm.chrome.version=151.0.8000.1", updated_identity)
            self.assertIn("wasm.bindgen.required=0.2.119", updated_identity)

    def test_invalid_or_mismatched_explicit_overrides_fail(self) -> None:
        with tempfile.TemporaryDirectory() as temp_name:
            root = pathlib.Path(temp_name) / "repo"
            tools = root / "tools"
            binaries = root / "bin"
            tools.mkdir(parents=True)
            binaries.mkdir()
            script = tools / "run-wasm-tests.sh"
            shutil.copy2(REPO_ROOT / "tools" / "run-wasm-tests.sh", script)
            (root / "Cargo.lock").write_text(
                'name = "wasm-bindgen"\nversion = "0.2.118"\n',
                encoding="utf-8",
            )
            chrome = binaries / "chrome"
            driver = binaries / "chromedriver"
            runner = binaries / "wasm-bindgen-test-runner"
            chrome.write_text(
                "#!/usr/bin/env bash\necho 'Google Chrome 150.0.7871.102'\n",
                encoding="utf-8",
            )
            driver.write_text(
                "#!/usr/bin/env bash\necho 'ChromeDriver 149.0.7827.155'\n",
                encoding="utf-8",
            )
            runner.write_text(
                "#!/usr/bin/env bash\necho 'wasm-bindgen-test-runner 0.2.118'\n",
                encoding="utf-8",
            )
            for binary in (chrome, driver, runner):
                binary.chmod(0o755)
            env = os.environ.copy()
            env.update(
                {
                    "CHROME": str(chrome),
                    "CHROMEDRIVER": str(driver),
                    "WASM_BINDGEN_TEST_RUNNER": str(runner),
                }
            )

            mismatch = subprocess.run(
                [str(script), "--identity"],
                env=env,
                text=True,
                capture_output=True,
                check=False,
            )
            self.assertNotEqual(mismatch.returncode, 0)
            self.assertIn("different major versions", mismatch.stderr)

            env["CHROME"] = str(binaries / "missing")
            missing = subprocess.run(
                [str(script), "--identity"],
                env=env,
                text=True,
                capture_output=True,
                check=False,
            )
            self.assertNotEqual(missing.returncode, 0)
            self.assertIn("CHROME is not executable", missing.stderr)

            custom_runner = binaries / "custom-runner"
            custom_runner.write_text(
                runner.read_text(encoding="utf-8"), encoding="utf-8"
            )
            custom_runner.chmod(0o755)
            driver.write_text(
                "#!/usr/bin/env bash\necho 'ChromeDriver 150.0.7871.115'\n",
                encoding="utf-8",
            )
            driver.chmod(0o755)
            env["CHROME"] = str(chrome)
            env["WASM_BINDGEN_TEST_RUNNER"] = str(custom_runner)
            wrong_name = subprocess.run(
                [str(script), "--identity"],
                env=env,
                text=True,
                capture_output=True,
                check=False,
            )
            self.assertNotEqual(wrong_name.returncode, 0)
            self.assertIn(
                "must be named wasm-bindgen-test-runner", wrong_name.stderr
            )

    def test_identity_never_signs_an_unusable_explicit_driver(self) -> None:
        with tempfile.TemporaryDirectory() as temp_name:
            root = pathlib.Path(temp_name) / "repo"
            tools = root / "tools"
            binaries = root / "bin"
            tools.mkdir(parents=True)
            binaries.mkdir()
            script = tools / "run-wasm-tests.sh"
            shutil.copy2(REPO_ROOT / "tools" / "run-wasm-tests.sh", script)
            (root / "Cargo.lock").write_text(
                'name = "wasm-bindgen"\nversion = "0.2.118"\n',
                encoding="utf-8",
            )

            marker = root / "codesigned"
            chrome = binaries / "chrome"
            driver = binaries / "chromedriver"
            runner = binaries / "wasm-bindgen-test-runner"
            chrome.write_text(
                "#!/usr/bin/env bash\necho 'Google Chrome 150.0.7871.102'\n",
                encoding="utf-8",
            )
            driver.write_text(
                "#!/usr/bin/env bash\n"
                '[[ -e "$SIGN_MARKER" ]] || exit 9\n'
                "echo 'ChromeDriver 150.0.7871.115'\n",
                encoding="utf-8",
            )
            runner.write_text(
                "#!/usr/bin/env bash\necho 'wasm-bindgen-test-runner 0.2.118'\n",
                encoding="utf-8",
            )
            (binaries / "uname").write_text(
                "#!/usr/bin/env bash\n"
                "case \"${1:-}\" in\n"
                "  -s) echo Darwin ;;\n"
                "  -m) echo x86_64 ;;\n"
                "  *) echo Darwin ;;\n"
                "esac\n",
                encoding="utf-8",
            )
            (binaries / "codesign").write_text(
                '#!/usr/bin/env bash\ntouch "$SIGN_MARKER"\n', encoding="utf-8"
            )
            for binary in (
                chrome,
                driver,
                runner,
                binaries / "uname",
                binaries / "codesign",
            ):
                binary.chmod(0o755)

            env = os.environ.copy()
            env.update(
                {
                    "PATH": f"{binaries}:{env['PATH']}",
                    "CHROME": str(chrome),
                    "CHROMEDRIVER": str(driver),
                    "WASM_BINDGEN_TEST_RUNNER": str(runner),
                    "SIGN_MARKER": str(marker),
                }
            )
            identity = subprocess.run(
                [str(script), "--identity"],
                env=env,
                text=True,
                capture_output=True,
                check=False,
            )

            self.assertNotEqual(identity.returncode, 0)
            self.assertIn("run preparation to provision", identity.stderr)
            self.assertFalse(marker.exists())

    # ── Browser-suite timeout contract ────────────────────────────────────
    #
    # wasm-bindgen-test-runner applies WASM_BINDGEN_TEST_TIMEOUT to a whole
    # headless session, not to each test, and defaults it to 20 seconds. The
    # frontend suite measured 19.25-19.58s against that default, so the stage
    # failed roughly a third of the time with no assertion text. These tests
    # pin the explicit default, the caller override across the runner's full
    # `u64` domain, propagation to both wasm invocations, and the absence of any
    # test-selection argument — because the tempting "fix" for a slow suite is
    # to shard or skip coverage, and that is exactly what AGENTS.md forbids.

    def _write_run_fixture(
        self, root: pathlib.Path
    ) -> tuple[pathlib.Path, dict[str, str], pathlib.Path]:
        """Copy the real runner into a fake repo whose `cargo` records argv."""
        tools = root / "tools"
        binaries = root / "bin"
        tools.mkdir(parents=True)
        binaries.mkdir()
        (root / "frontend").mkdir()
        (root / "mobile-frontend").mkdir()
        script = tools / "run-wasm-tests.sh"
        shutil.copy2(REPO_ROOT / "tools" / "run-wasm-tests.sh", script)
        (root / "Cargo.lock").write_text(
            'name = "wasm-bindgen"\nversion = "0.2.118"\n', encoding="utf-8"
        )

        chrome = binaries / "chrome"
        driver = binaries / "chromedriver"
        runner = binaries / "wasm-bindgen-test-runner"
        cargo = binaries / "cargo"
        record = root / "cargo-invocations.txt"
        chrome.write_text(
            "#!/usr/bin/env bash\necho 'Google Chrome 150.0.7871.102'\n",
            encoding="utf-8",
        )
        driver.write_text(
            "#!/usr/bin/env bash\necho 'ChromeDriver 150.0.7871.115'\n",
            encoding="utf-8",
        )
        runner.write_text(
            "#!/usr/bin/env bash\necho 'wasm-bindgen-test-runner 0.2.118'\n",
            encoding="utf-8",
        )
        cargo.write_text(
            "#!/usr/bin/env bash\n"
            "set -eu\n"
            "{\n"
            "  printf 'cwd=%s\\n' \"$(basename \"$PWD\")\"\n"
            "  printf 'argv=%s\\n' \"$*\"\n"
            "  printf 'WASM_BINDGEN_TEST_TIMEOUT=%s\\n' "
            '"${WASM_BINDGEN_TEST_TIMEOUT-<unset>}"\n'
            "  printf 'RUSTFLAGS=%s\\n' \"${RUSTFLAGS-<unset>}\"\n"
            "  printf '=== end ===\\n'\n"
            '} >>"$CARGO_RECORD"\n'
            'if [[ "${CARGO_FAIL_IN:-}" == "$(basename "$PWD")" ]]; then\n'
            "  exit 101\n"
            "fi\n"
            "exit 0\n",
            encoding="utf-8",
        )
        for binary in (chrome, driver, runner, cargo):
            binary.chmod(0o755)

        env = os.environ.copy()
        for inherited in (
            "WASM_BINDGEN_TEST_TIMEOUT",
            "WASM_BINDGEN_TEST_WEBDRIVER_JSON",
            "TYDE_WASM_WEBDRIVER_SOURCE_JSON",
            "TYDE_WASM_TOOLS_PREPARED",
            "RUSTFLAGS",
            "CARGO_FAIL_IN",
        ):
            env.pop(inherited, None)
        env.update(
            {
                "PATH": f"{binaries}:{env['PATH']}",
                "CHROME": str(chrome),
                "CHROMEDRIVER": str(driver),
                "WASM_BINDGEN_TEST_RUNNER": str(runner),
                "CARGO_RECORD": str(record),
            }
        )
        return script, env, record

    def _cargo_invocations(self, record: pathlib.Path) -> list[dict[str, str]]:
        if not record.exists():
            return []
        invocations: list[dict[str, str]] = []
        current: dict[str, str] = {}
        for line in record.read_text(encoding="utf-8").splitlines():
            if line == "=== end ===":
                invocations.append(current)
                current = {}
                continue
            key, _, value = line.partition("=")
            current[key] = value
        return invocations

    def test_canonical_run_sets_the_default_browser_suite_timeout(self) -> None:
        with tempfile.TemporaryDirectory() as temp_name:
            root = pathlib.Path(temp_name) / "repo"
            script, env, record = self._write_run_fixture(root)

            run = subprocess.run(
                [str(script)], env=env, text=True, capture_output=True, check=False
            )

            self.assertEqual(run.returncode, 0, run.stderr)
            invocations = self._cargo_invocations(record)
            self.assertEqual(len(invocations), 2)
            for invocation in invocations:
                # 120s, not the 20s the runner defaults to when nothing sets it.
                self.assertEqual(invocation["WASM_BINDGEN_TEST_TIMEOUT"], "120")

    def test_explicit_browser_suite_timeout_wins_and_invalid_values_fail(
        self,
    ) -> None:
        with tempfile.TemporaryDirectory() as temp_name:
            root = pathlib.Path(temp_name) / "repo"
            script, env, record = self._write_run_fixture(root)

            override = dict(env, WASM_BINDGEN_TEST_TIMEOUT="45")
            accepted = subprocess.run(
                [str(script)],
                env=override,
                text=True,
                capture_output=True,
                check=False,
            )
            self.assertEqual(accepted.returncode, 0, accepted.stderr)
            invocations = self._cargo_invocations(record)
            self.assertEqual(len(invocations), 2)
            for invocation in invocations:
                self.assertEqual(invocation["WASM_BINDGEN_TEST_TIMEOUT"], "45")

            # An empty value reads as unset, matching how CHROME and
            # CHROMEDRIVER treat empty explicit overrides.
            record.unlink()
            empty = subprocess.run(
                [str(script)],
                env=dict(env, WASM_BINDGEN_TEST_TIMEOUT=""),
                text=True,
                capture_output=True,
                check=False,
            )
            self.assertEqual(empty.returncode, 0, empty.stderr)
            self.assertEqual(
                self._cargo_invocations(record)[0]["WASM_BINDGEN_TEST_TIMEOUT"],
                "120",
            )

            positive = "must be a positive whole number of seconds"
            bounded = f"must not exceed {U64_MAX_SECONDS} seconds"
            rejections = (
                ("0", positive),
                ("00", positive),
                ("abc", positive),
                ("-5", positive),
                ("12.5", positive),
                ("20s", positive),
                (" 45", positive),
                # Past u64::MAX the runner's `.parse::<u64>()` fails, so the
                # guard has to fail too. Bash arithmetic wraps these to
                # positive values, which is what HIGH-1 caught.
                (str(U64_MAX + 1), bounded),
                ("99999999999999999999", bounded),
                ("184467440737095516150", bounded),
                ("0" * 8 + str(U64_MAX + 1), bounded),
            )
            for invalid, expected in rejections:
                with self.subTest(timeout=invalid):
                    record.unlink(missing_ok=True)
                    rejected = subprocess.run(
                        [str(script)],
                        env=dict(env, WASM_BINDGEN_TEST_TIMEOUT=invalid),
                        text=True,
                        capture_output=True,
                        check=False,
                    )
                    self.assertNotEqual(rejected.returncode, 0)
                    self.assertIn(
                        f"WASM_BINDGEN_TEST_TIMEOUT {expected}", rejected.stderr
                    )
                    # Fails before any browser or Cargo work, not as a
                    # `.parse().expect()` panic once Chrome is already up.
                    self.assertEqual(self._cargo_invocations(record), [])

    def test_browser_suite_timeout_accepts_the_runner_u64_boundary(self) -> None:
        with tempfile.TemporaryDirectory() as temp_name:
            root = pathlib.Path(temp_name) / "repo"
            script, env, record = self._write_run_fixture(root)

            # Everything the pinned runner can parse must survive the guard,
            # including the exact u64::MAX boundary and leading-zero forms. The
            # caller's string is forwarded verbatim; the runner normalises it.
            for accepted in (
                "1",
                "20",
                "007",
                "4294967296",
                str(U64_MAX - 1),
                U64_MAX_SECONDS,
            ):
                with self.subTest(timeout=accepted):
                    record.unlink(missing_ok=True)
                    run = subprocess.run(
                        [str(script)],
                        env=dict(env, WASM_BINDGEN_TEST_TIMEOUT=accepted),
                        text=True,
                        capture_output=True,
                        check=False,
                    )
                    self.assertEqual(run.returncode, 0, run.stderr)
                    invocations = self._cargo_invocations(record)
                    self.assertEqual(len(invocations), 2)
                    for invocation in invocations:
                        self.assertEqual(
                            invocation["WASM_BINDGEN_TEST_TIMEOUT"], accepted
                        )

    def test_browser_suite_timeout_propagates_to_both_wasm_invocations(
        self,
    ) -> None:
        with tempfile.TemporaryDirectory() as temp_name:
            root = pathlib.Path(temp_name) / "repo"
            script, env, record = self._write_run_fixture(root)

            run = subprocess.run(
                [str(script)], env=env, text=True, capture_output=True, check=False
            )
            self.assertEqual(run.returncode, 0, run.stderr)

            invocations = self._cargo_invocations(record)
            self.assertEqual(
                [invocation["cwd"] for invocation in invocations],
                ["frontend", "mobile-frontend"],
            )
            for invocation in invocations:
                self.assertEqual(invocation["WASM_BINDGEN_TEST_TIMEOUT"], "120")
                self.assertEqual(invocation["RUSTFLAGS"], "-C debuginfo=0")

            # Same override idiom as RUSTFLAGS: an explicit value reaches both.
            record.unlink()
            tuned = subprocess.run(
                [str(script)],
                env=dict(
                    env, WASM_BINDGEN_TEST_TIMEOUT="90", RUSTFLAGS="-C debuginfo=2"
                ),
                text=True,
                capture_output=True,
                check=False,
            )
            self.assertEqual(tuned.returncode, 0, tuned.stderr)
            for invocation in self._cargo_invocations(record):
                self.assertEqual(invocation["WASM_BINDGEN_TEST_TIMEOUT"], "90")
                self.assertEqual(invocation["RUSTFLAGS"], "-C debuginfo=2")

            # A failed frontend suite must not be followed by a mobile run that
            # could mask it in the stage output.
            record.unlink()
            halted = subprocess.run(
                [str(script)],
                env=dict(env, CARGO_FAIL_IN="frontend"),
                text=True,
                capture_output=True,
                check=False,
            )
            self.assertNotEqual(halted.returncode, 0)
            self.assertEqual(
                [
                    invocation["cwd"]
                    for invocation in self._cargo_invocations(record)
                ],
                ["frontend"],
            )

    def test_canonical_run_adds_no_filter_skip_or_shard_arguments(self) -> None:
        with tempfile.TemporaryDirectory() as temp_name:
            root = pathlib.Path(temp_name) / "repo"
            script, env, record = self._write_run_fixture(root)

            run = subprocess.run(
                [str(script)], env=env, text=True, capture_output=True, check=False
            )
            self.assertEqual(run.returncode, 0, run.stderr)

            invocations = self._cargo_invocations(record)
            # Exactly two invocations, neither carrying a filter, a --skip, or
            # any other test-selection argument. Sharding the suite to fit a
            # timeout would silently drop whatever falls outside the partition.
            self.assertEqual(len(invocations), 2)
            for invocation in invocations:
                self.assertEqual(
                    invocation["argv"], "test --target wasm32-unknown-unknown"
                )

            # Explicit developer filters still pass through unchanged.
            record.unlink()
            filtered = subprocess.run(
                [str(script), "components::chat_view"],
                env=env,
                text=True,
                capture_output=True,
                check=False,
            )
            self.assertEqual(filtered.returncode, 0, filtered.stderr)
            for invocation in self._cargo_invocations(record):
                self.assertEqual(
                    invocation["argv"],
                    "test --target wasm32-unknown-unknown components::chat_view",
                )


class RustToolchainParityTests(unittest.TestCase):
    def test_repository_pin_declares_every_required_rust_tool(self) -> None:
        self.assertEqual(
            (REPO_ROOT / "rust-toolchain.toml").read_text(encoding="utf-8"),
            """[toolchain]
channel = "stable"
components = ["clippy", "rustfmt"]
targets = ["wasm32-unknown-unknown"]
profile = "minimal"
""",
        )

    def test_release_workflows_install_the_repository_pin(self) -> None:
        release_workflow = (
            REPO_ROOT / ".github" / "workflows" / "release.yml"
        ).read_text(encoding="utf-8")
        mobile_workflow = (
            REPO_ROOT / ".github" / "workflows" / "mobile-web-release.yml"
        ).read_text(encoding="utf-8")

        root_install = """      - name: Install repository Rust toolchain
        run: ./dev.sh rust-toolchain
"""
        nested_install = """      - name: Install repository Rust toolchain
        working-directory: deploy-tools
        shell: bash
        run: |
          ./dev.sh rust-toolchain
"""

        # Two, not three, since "Move checks to pull requests" (f604545)
        # removed the release job whose validation now runs on pull requests.
        self.assertEqual(release_workflow.count(root_install), 2)
        self.assertEqual(mobile_workflow.count(nested_install), 1)
        self.assertNotIn("dtolnay/rust-toolchain@stable", release_workflow)
        self.assertNotIn("dtolnay/rust-toolchain@stable", mobile_workflow)
        self.assertNotIn("rustup update stable", release_workflow)
        self.assertNotIn("rustup update stable", mobile_workflow)
        self.assertIn(
            "RUSTUP_TOOLCHAIN=$(rustup show active-toolchain", mobile_workflow
        )

    def test_check_source_keeps_portable_timing_and_contract_guards(self) -> None:
        source = (REPO_ROOT / "dev.sh").read_text(encoding="utf-8")

        self.assertIn("LC_ALL=C /usr/bin/time", source)
        self.assertIn("GNU [Tt]ime", source)
        self.assertIn("Resource timing parser failure", source)
        self.assertIn("unset DEV_CHECK_CONTRACT_CHILD", source)
        self.assertNotIn('if [[ "${DEV_CHECK_CONTRACT_CHILD', source)
        self.assertIn(
            'run_stage "dev check contract tests" 1 python3 tools/test_dev_check.py',
            source,
        )


if __name__ == "__main__":
    unittest.main()
