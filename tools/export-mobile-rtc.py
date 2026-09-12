#!/usr/bin/env python3
"""Export the canonical Rust mobile transport types to the pairing service."""

import argparse
import pathlib


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("service_root", type=pathlib.Path)
    args = parser.parse_args()
    root = pathlib.Path(__file__).resolve().parent.parent
    source = (root / "protocol/src/types.rs").read_text()
    start = source.index("pub mod mobile_rtc {")
    end = source.index("pub use mobile_rtc::*;", start) + len("pub use mobile_rtc::*;")
    target = args.service_root / "src/mobile_rtc_protocol.rs"
    target.write_text(
        "// Generated from Tyde protocol/src/types.rs by tools/export-mobile-rtc.py.\n"
        "// Edit the canonical protocol and regenerate; do not edit this copy.\n"
        + source[start:end] + "\n"
    )


if __name__ == "__main__":
    main()
