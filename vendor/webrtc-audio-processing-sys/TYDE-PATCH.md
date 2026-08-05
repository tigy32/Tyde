# Tyde build-tool patch

This directory vendors `webrtc-audio-processing-sys` 2.1.0 from crates.io
(published checksum
`f8f6c2b4b20a03b165378172f0dbe23c49b2630b70091026589afbe04e79cd3e`).
The bundled C/C++ audio-processing sources, bindings, features, and library
code are unchanged.

The upstream `build.rs` directly constructs `Command::new("meson")` once and
`Command::new("ninja")` twice. It does not read `MESON`, `NINJA`, or another
executable override. Tyde replaces those constructors with
`repository_native_tool` and `repository_native_command`, which invoke the
repository's lazy `tools/native-build-tool.py` wrapper and apply command specs
from `build_support.rs`. The wrapper supplies pinned repository-local
executables to every Cargo entry point. `meson_spec` retains offline setup and
fallback selection. `ninja_spec` uses `-C <build_dir>` for build and install so
the wrapper always starts from a stable working directory. The wrapper launches
the resolved executable synchronously on every host, inherits its environment
and standard streams, and returns only after propagating the child's status.

For Rust MSVC targets, `resolve_msvc_tools` uses `cc`'s target-aware Visual
Studio discovery to resolve `cl.exe`, `link.exe`, and `lib.exe` and capture the
toolchain environment. `msvc_command_env` adds `CCACHE_DISABLE=1`, and both the
Meson and Ninja specs inherit that complete environment. `msvc_native_file`
generates a Meson native file containing the resolved compiler, linker, and
librarian paths. Other targets receive the original Meson environment and
arguments.

The bundled library's symbols must still be prefixed on MSVC: the wrapper
archive contains unresolved C++ references to those definitions, and both
archives must receive the same rename for Tyde's multi-version isolation to
link. After Meson install, target-aware discovery accepts either its canonical
MSVC `webrtc-audio-processing-2.lib` output or the documented Meson
`libwebrtc-audio-processing-2.a` fallback, requires exactly one existing
archive, and never modifies that Meson-owned source. The build script copies it
to `OUT_DIR/bundled-link` under the target-canonical name and prefixes only that
staged copy. Rust's MSVC `static=webrtc-audio-processing-2` directive consumes
the staged `.lib`. `cc` emits
`webrtc_audio_processing_wrapper.lib`, which receives the same symbol rewrite.
Unix and macOS keep their single `libwebrtc-audio-processing-2.a` candidate,
staged `.a` name, and unchanged link directive.

The package-cache Meson patch defines `absl_strings` as a static library, and
the build script emits no other direct bundled Abseil link dependency. The
actual subproject output directory is part of `lib_paths`. The same exact-one
discovery and staging contract therefore handles `absl_strings.lib` and
the Meson `libabsl_strings.a` fallback without symbol prefixing it. MSVC always
links the staged `absl_strings.lib`; Unix and macOS stage
`libabsl_strings.a`. All use `static=absl_strings`. Missing or ambiguous Meson
candidates fail before cargo link emission.

The staging directory is emitted first as a native link-search path. Each run
removes and overwrites only the exact canonical files owned there, recopies
fresh Meson outputs, and prefixes the staged WebRTC archive once. It never
scans, renames, deletes, or prefixes Meson install/build/subproject files, so
Ninja can recreate its outputs without producing ambiguity or double-prefixing.

MSVC symbol operations use the active Rust toolchain's absolute `llvm-nm` and
`llvm-objcopy` paths. Executable suffixes come from `HOST`, so Unix-hosted MSVC
cross compilation does not incorrectly request `.exe`; a Windows host does.
Resolution searches only the `llvm-tools-preview` directory under the active
host rustlib and fails with the exact rustup remedy unless both tools exist.
Unix keeps the upstream bare `nm` and `rust-objcopy` command paths. The
production helpers are `bundled_library_candidates`,
`discover_bundled_archive`, `stage_bundled_archive`,
`stage_and_prepare_bundled_archive`,
`static_link_directive`, `wrapper_library_filename`, `prefixed_archive_path`,
`replace_with_prefixed_archive`, `llvm_symbol_tool_candidates`,
`symbol_list_spec`, and `symbol_prefix_spec`.

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

`tools/test_dev_check.py` inspects both `build.rs` and the production spec
builders in `build_support.rs` to pin this patch boundary, the Abseil artifacts,
licenses, wrap metadata, no-download configuration, pinned wrapper routing, and
bounded recovery. It also rejects ambient Meson/Ninja lookup and executes the
lazy wrapper on a verified offline cache with shell-hostile paths. A portable
production-wrapper test proves the child completes before return and that a
nonzero child status is propagated. The normal workspace integration target
`tests/tests/webrtc_build_support.rs` imports the
production module so `./dev.sh check` executes its Meson, Ninja, MSVC
environment, machine-file quoting, symbol-tool/artifact, owned-staging
idempotency, Unix, and macOS contract tests.
