# Shared mobile shell

Unmodified npm archive of the private upstream repository
`https://github.com/tigy32/TyggsWebShell`, pinned to
`3b3fa78d7e56fa5dc6d4aa4f5fdca4564edaaf5c`.
Archive SHA-256: `052862b1f14847107119a9329260e95705183348dfe13a6c1e3f042237e312a3`.
Produced locally with `npm pack --ignore-scripts` from that clean committed
checkout after verifying the commit was pushed to upstream `main`.
The adjacent runtime, README, license and package metadata are extracted
byte-for-byte from this archive. This is not an npm registry publication or
a deployed Tyde release.

Rust imports the JS with wasm-bindgen's local module mechanism so Trunk
ships it inside each versioned bundle (including the integrity manifest),
not at an unversioned origin-root URL. CSS is loaded before app styles.
No npm install, registry access, or independent viewport implementation.

This revision retains completed-tap focus and recovers only the legacy
standalone status-bar shortfall. It grows the document paint surface with the
shell, retains that validated surface during keyboard pan to keep paint and
Send hit-testing aligned, preserves recovered shell height while focused without
viewport contraction, and clips scrolling content below safe-top.
Root-loader assets are unchanged; no LOADER_CACHE bump is needed for these
versioned bundle assets.
Regression and physical evidence are recorded in `dev-docs/mobile-web-pwa.md`.
