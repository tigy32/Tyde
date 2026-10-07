# Shared mobile shell

Unmodified npm archive of the private upstream repository
`https://github.com/tigy32/TyggsWebShell`, pinned to
`b7e76f56111bfd557c25d8bb83f4f48cf7e4a88f`.
Archive SHA-256: `92b406fe380c4aaa21ff2ec30d11712883e2b072fa9666404b5dfc862fa38692`.
Produced locally with `npm pack --ignore-scripts` from that clean committed
checkout after verifying the commit was pushed to upstream `main`.
The adjacent runtime, README, license and package metadata are extracted
byte-for-byte from this archive. This is not an npm registry publication or
a deployed Tyde release.

Rust imports the JS with wasm-bindgen's local module mechanism so Trunk
ships it inside each versioned bundle (including the integrity manifest),
not at an unversioned origin-root URL. CSS is loaded before app styles.
No npm install, registry access, or independent viewport implementation.

This revision adds the drag-the-composer-down gesture: a quick single-finger
downward drag on the focused composer blurs the textarea so the keyboard hides
and the draft stays. It also retains completed-tap focus and recovers only the legacy
standalone status-bar shortfall. It grows the document paint surface with the
shell, retains that validated surface during keyboard pan to keep paint and
Send hit-testing aligned, preserves recovered shell height while focused without
viewport contraction, and clips scrolling content below safe-top.
Root-loader assets are unchanged; no LOADER_CACHE bump is needed for these
versioned bundle assets.
Regression and physical evidence are recorded in `dev-docs/mobile-web-pwa.md`.
