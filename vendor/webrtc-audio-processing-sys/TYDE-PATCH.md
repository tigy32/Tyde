# Tyde build-tool patch

This directory vendors `webrtc-audio-processing-sys` 2.1.0 from crates.io
(published checksum
`f8f6c2b4b20a03b165378172f0dbe23c49b2630b70091026589afbe04e79cd3e`).
The bundled C/C++ audio-processing sources, bindings, features, and library
code are unchanged.

The upstream `build.rs` directly constructs `Command::new("meson")` once and
`Command::new("ninja")` twice. It does not read `MESON`, `NINJA`, or another
executable override. Tyde changes only those command constructors: they invoke
the repository's lazy `tools/native-build-tool.py` wrapper, which supplies the
pinned repository-local executables to every Cargo entry point.

The upstream Meson project references Abseil 20240722.0 through a WrapDB
`wrap-file`. Tyde stores the exact source and `20240722.0-3` WrapDB patch
archives in `webrtc-audio-processing/subprojects/packagecache/`, alongside
their Apache-2.0 and MIT licenses and a provenance record. The archive hashes
and sizes match `abseil-cpp.wrap`. Meson runs with `--wrap-mode=nodownload` and
forces the pinned fallback for all six Abseil dependencies requested by the
parent build instead of allowing mixed linkage with ambient system packages.

A killed configure can leave only this crate's copied source or build
directory incomplete. The build script removes an incomplete target-local
build directory before setup; if reconfiguration fails, it removes only that
AEC build directory and the materialized Abseil subproject and retries cleanly
once. It never cleans a shared Cargo target or downloads a replacement.

`tools/test_dev_check.py` pins this patch boundary, the Abseil artifacts,
licenses, wrap metadata, no-download configuration, and bounded recovery. It
also rejects ambient Meson/Ninja lookup and executes the lazy wrapper on a
verified offline cache with shell-hostile paths.
