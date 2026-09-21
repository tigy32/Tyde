# Shared mobile shell

Unmodified Tychat vendor archive, pinned to
`78dfe04bd2ca60dd3c101875e1f4443fa9566e7a`.
Archive SHA-256: `c1638aaf1a295dca411681e34ddda549a38251fdfcca61fed802d89b592efc33`.
The adjacent runtime files are extracted byte-for-byte from this archive.

Rust imports the JS with wasm-bindgen's local module mechanism so Trunk
ships it inside each versioned bundle (including the integrity manifest),
not at an unversioned origin-root URL. CSS is loaded before app styles.
No npm install, registry access, or independent viewport implementation.
