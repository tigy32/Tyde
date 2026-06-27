from __future__ import annotations

import importlib.util
import json
import pathlib
import subprocess
import sys
import tempfile
import unittest


MODULE_PATH = pathlib.Path(__file__).with_name("check_mobile_web_manifest.py")
MERGE_PATH = pathlib.Path(__file__).with_name("merge_mobile_web_manifest.py")
SPEC = importlib.util.spec_from_file_location("check_mobile_web_manifest", MODULE_PATH)
assert SPEC and SPEC.loader
check = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(check)


def entry(version: str, protocol_version: int = 23) -> dict[str, object]:
    return {
        "path": f"/tyde/v{version}/",
        "entry": f"/tyde/v{version}/mobile.js",
        "integrity": "sha384-" + "A" * 64,
        "protocolVersion": protocol_version,
        "artifacts": {
            f"/tyde/v{version}/mobile_bg.wasm": "sha384-" + "B" * 64,
        },
    }


class MobileWebManifestCheckTests(unittest.TestCase):
    def test_accepts_matching_protocol_entry(self) -> None:
        manifest = {"versions": {"0.8.19-beta.9": entry("0.8.19-beta.9")}}

        check.validate_manifest_entry(manifest, "0.8.19-beta.9", 23)

    def test_accepts_matching_stable_protocol_entry(self) -> None:
        manifest = {"versions": {"0.8.19": entry("0.8.19")}}

        check.validate_manifest_entry(manifest, "0.8.19", 23)

    def test_rejects_missing_entry(self) -> None:
        with self.assertRaisesRegex(check.CheckError, "missing"):
            check.validate_manifest_entry({"versions": {}}, "0.8.19-beta.9", 23)

    def test_rejects_protocol_mismatch(self) -> None:
        manifest = {"versions": {"0.8.19-beta.9": entry("0.8.19-beta.9", 22)}}

        with self.assertRaisesRegex(check.CheckError, "expected 23"):
            check.validate_manifest_entry(manifest, "0.8.19-beta.9", 23)

    def test_rejects_entry_outside_version_prefix(self) -> None:
        bad = entry("0.8.19-beta.9")
        bad["entry"] = "/tyde/v0.8.19-beta.8/mobile.js"
        manifest = {"versions": {"0.8.19-beta.9": bad}}

        with self.assertRaisesRegex(check.CheckError, "outside"):
            check.validate_manifest_entry(manifest, "0.8.19-beta.9", 23)

    def test_rejects_supported_entry_without_protocol_version(self) -> None:
        manifest = {
            "minSupported": "0.8.19-beta.1",
            "versions": {
                "0.8.19-beta.8": {
                    "path": "/tyde/v0.8.19-beta.8/",
                    "entry": "/tyde/v0.8.19-beta.8/mobile.js",
                    "integrity": "sha384-" + "A" * 64,
                    "artifacts": {
                        "/tyde/v0.8.19-beta.8/mobile_bg.wasm": "sha384-" + "B" * 64,
                    },
                }
            },
        }

        with self.assertRaisesRegex(check.CheckError, "lacks protocolVersion"):
            check.validate_supported_entries_have_protocol_versions(manifest)

    def test_protocol_floor_blocks_old_entries_without_protocol_version(self) -> None:
        manifest = {
            "minSupported": "0.8.19-beta.1",
            "versions": {
                "0.8.19-beta.8": {
                    "path": "/tyde/v0.8.19-beta.8/",
                    "entry": "/tyde/v0.8.19-beta.8/mobile.js",
                    "integrity": "sha384-" + "A" * 64,
                    "artifacts": {
                        "/tyde/v0.8.19-beta.8/mobile_bg.wasm": "sha384-" + "B" * 64,
                    },
                },
                "0.8.19-beta.9": entry("0.8.19-beta.9"),
            },
        }

        self.assertTrue(check.enforce_protocol_floor(manifest))
        self.assertEqual(manifest["minSupported"], "0.8.19-beta.9")
        check.validate_supported_entries_have_protocol_versions(manifest)

    def test_merge_target_entry_preserves_newer_live_entry_and_raises_floor(self) -> None:
        base = {
            "minSupported": "0.8.19-beta.1",
            "versions": {
                "0.8.19-beta.8": {
                    "path": "/tyde/v0.8.19-beta.8/",
                    "entry": "/tyde/v0.8.19-beta.8/mobile.js",
                    "integrity": "sha384-" + "A" * 64,
                    "artifacts": {
                        "/tyde/v0.8.19-beta.8/mobile_bg.wasm": "sha384-" + "B" * 64,
                    },
                },
                "0.8.19-beta.10": entry("0.8.19-beta.10"),
            },
        }
        source = {"versions": {"0.8.19-beta.9": entry("0.8.19-beta.9")}}

        merged = check.merge_target_entry(base, source, "0.8.19-beta.9", 23)

        self.assertEqual(merged["minSupported"], "0.8.19-beta.9")
        self.assertIn("0.8.19-beta.10", merged["versions"])
        self.assertIn("0.8.19-beta.9", merged["versions"])
        check.validate_supported_entries_have_protocol_versions(merged)

    def test_cli_reads_protocol_source(self) -> None:
        with tempfile.TemporaryDirectory() as raw_dir:
            root = pathlib.Path(raw_dir)
            protocol_source = root / "types.rs"
            protocol_source.write_text("pub const PROTOCOL_VERSION: u32 = 42;\n", encoding="utf-8")
            manifest = root / "manifest.json"
            manifest.write_text(
                json.dumps({"versions": {"0.8.19-beta.9": entry("0.8.19-beta.9", 42)}}),
                encoding="utf-8",
            )

            status = check.main(
                [
                    "v0.8.19-beta.9",
                    "--manifest",
                    str(manifest),
                    "--protocol-source",
                    str(protocol_source),
                ]
            )

        self.assertEqual(status, 0)

    def test_merge_cli_uses_live_base_and_generated_entry(self) -> None:
        with tempfile.TemporaryDirectory() as raw_dir:
            root = pathlib.Path(raw_dir)
            protocol_source = root / "types.rs"
            protocol_source.write_text("pub const PROTOCOL_VERSION: u32 = 23;\n", encoding="utf-8")
            base = root / "base.json"
            base.write_text(
                json.dumps(
                    {
                        "minSupported": "0.8.19-beta.1",
                        "versions": {
                            "0.8.19-beta.8": {
                                "path": "/tyde/v0.8.19-beta.8/",
                                "entry": "/tyde/v0.8.19-beta.8/mobile.js",
                                "integrity": "sha384-" + "A" * 64,
                                "artifacts": {
                                    "/tyde/v0.8.19-beta.8/mobile_bg.wasm": "sha384-" + "B" * 64,
                                },
                            }
                        },
                    }
                ),
                encoding="utf-8",
            )
            source = root / "source.json"
            source.write_text(
                json.dumps({"versions": {"0.8.19-beta.9": entry("0.8.19-beta.9")}}),
                encoding="utf-8",
            )
            out = root / "out.json"

            result = subprocess.run(
                [
                    sys.executable,
                    str(MERGE_PATH),
                    "v0.8.19-beta.9",
                    "--base",
                    str(base),
                    "--entry-source",
                    str(source),
                    "--out",
                    str(out),
                    "--protocol-source",
                    str(protocol_source),
                ],
                text=True,
                capture_output=True,
                check=False,
            )

            self.assertEqual(result.returncode, 0, result.stderr)
            merged = json.loads(out.read_text(encoding="utf-8"))
            self.assertEqual(merged["minSupported"], "0.8.19-beta.9")


if __name__ == "__main__":
    unittest.main()
