#!/usr/bin/env python3
"""Reject the iOS marketing icon Apple refuses for transparency (90717)."""
import struct
import sys
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
ICON = ROOT / "ios/Tyde/Assets.xcassets/AppIcon.appiconset/AppIcon.png"


def icon_failure(data: bytes) -> str | None:
    if not data.startswith(b"\x89PNG\r\n\x1a\n"):
        return "marketing icon is not a PNG"
    width = height = bit_depth = color_type = None
    transparent = False
    index = 8
    while index + 8 <= len(data):
        length = struct.unpack(">I", data[index : index + 4])[0]
        kind = data[index + 4 : index + 8]
        if index + 12 + length > len(data):
            return "marketing icon PNG is truncated"
        if kind == b"IHDR":
            if length != 13:
                return "marketing icon IHDR is invalid"
            width, height, bit_depth, color_type, compression, filtering, interlace = (
                struct.unpack(">IIBBBBB", data[index + 8 : index + 21])
            )
            if compression or filtering or interlace:
                return "marketing icon PNG is not a plain image"
        elif kind == b"tRNS":
            transparent = True
        elif kind == b"IEND":
            break
        index += 12 + length
    else:
        return "marketing icon PNG has no IEND"
    if (width, height, bit_depth, color_type) != (1024, 1024, 8, 2) or transparent:
        return (
            "marketing icon must be 1024x1024 8-bit RGB without alpha "
            f"(got {width}x{height} bit {bit_depth} color {color_type} tRNS {transparent})"
        )
    return None


def main() -> None:
    if len(sys.argv) > 2:
        raise SystemExit("Usage: check_ios_app_icon.py [png]")
    path = Path(sys.argv[1]) if len(sys.argv) == 2 else ICON
    failure = icon_failure(path.read_bytes())
    if failure:
        raise SystemExit(failure)
    print("Opaque App Store icon: " + str(path))


if __name__ == "__main__":
    main()
