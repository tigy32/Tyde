# Shared mobile shell

Unmodified Tychat vendor archive, pinned to
`b7b25b117b379c49e0419c612e3a8fbeef060241`.
Archive SHA-256: `66de2f17c61101c0e0476c5216bbe9af6c16c462ff45c3997d943359ea64c400`.
The adjacent runtime files are extracted byte-for-byte from this archive.

Rust imports the JS with wasm-bindgen's local module mechanism so Trunk
ships it inside each versioned bundle (including the integrity manifest),
not at an unversioned origin-root URL. CSS is loaded before app styles.
No npm install, registry access, or independent viewport implementation.


This revision removes early pointerdown focus. The source regression and physical
focus-observation limitations are recorded in `dev-docs/mobile-web-pwa.md`.
