# Abseil package cache

This directory makes the bundled WebRTC audio-processing build reproducible
without network access. Meson reads both archives through
`subprojects/abseil-cpp.wrap` with `--wrap-mode=nodownload` and verifies the
hashes recorded by that wrap.

| Artifact | Upstream | SHA-256 | Bytes |
|---|---|---|---:|
| `abseil-cpp-20240722.0.tar.gz` | `https://github.com/abseil/abseil-cpp/releases/download/20240722.0/abseil-cpp-20240722.0.tar.gz` | `f50e5ac311a81382da7fa75b97310e4b9006474f9560ac46f54a9967f07d4ae3` | 2,242,861 |
| `abseil-cpp_20240722.0-3_patch.zip` | `https://wrapdb.mesonbuild.com/v2/abseil-cpp_20240722.0-3/get_patch` | `12dd8df1488a314c53e3751abd2750cf233b830651d168b6a9f15e7d0cf71f7b` | 5,929 |
| `ABSEIL-LICENSE` | source archive `LICENSE` | `c79a7fea0e3cac04cd43f20e7b648e5a0ff8fa5344e644b0ee09ca1162b62747` | 11,361 |
| `WRAPDB-LICENSE` | patch archive `LICENSE.build` | `7939f4c45423cec4a18236ad0a88570e33508dd7462e07b1038001f90ece65fb` | 1,070 |

Abseil is licensed under Apache-2.0; its archive license is reproduced in
`ABSEIL-LICENSE`. The Meson WrapDB build definition is MIT-licensed; its
archive license is reproduced in `WRAPDB-LICENSE`. These files were extracted
byte-for-byte from the corresponding pinned archives.
