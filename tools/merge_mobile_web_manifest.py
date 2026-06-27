#!/usr/bin/env python3

from __future__ import annotations

import argparse
import json
import pathlib
import sys

from check_mobile_web_manifest import (
    CheckError,
    load_manifest,
    merge_target_entry,
    normalize_release_version,
    read_protocol_version,
    repo_root,
)


def parse_args(argv: list[str]) -> argparse.Namespace:
    root = repo_root()
    parser = argparse.ArgumentParser(
        description=(
            "Merge one generated mobile-web manifest entry into a base manifest, "
            "preserving the base policy and other versions."
        )
    )
    parser.add_argument("version", help="release version or tag to merge")
    parser.add_argument("--base", type=pathlib.Path, required=True, help="base manifest path")
    parser.add_argument(
        "--entry-source",
        type=pathlib.Path,
        required=True,
        help="manifest containing the generated target version entry",
    )
    parser.add_argument("--out", type=pathlib.Path, required=True, help="merged manifest output path")
    parser.add_argument(
        "--protocol-source",
        type=pathlib.Path,
        default=root / "protocol" / "src" / "types.rs",
        help="Rust source containing PROTOCOL_VERSION (default: protocol/src/types.rs)",
    )
    return parser.parse_args(argv)


def main(argv: list[str] | None = None) -> int:
    args = parse_args(sys.argv[1:] if argv is None else argv)
    try:
        version = normalize_release_version(args.version)
        protocol_version = read_protocol_version(args.protocol_source)
        base_manifest = load_manifest(args.base)
        entry_source_manifest = load_manifest(args.entry_source)
        merged = merge_target_entry(
            base_manifest,
            entry_source_manifest,
            version,
            protocol_version,
        )
        args.out.write_text(json.dumps(merged, indent=2) + "\n", encoding="utf-8")
    except (OSError, CheckError) as error:
        print(f"ERROR: {error}", file=sys.stderr)
        return 1

    print(
        f"merged mobile web manifest: version={version} protocolVersion={protocol_version} "
        f"base={args.base} entry_source={args.entry_source} out={args.out}"
    )
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
