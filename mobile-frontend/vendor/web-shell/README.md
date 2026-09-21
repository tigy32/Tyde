# Tyggs Web Shell

A **private, dependency-free JavaScript + CSS shell** shared by TypeScript and
Rust/WASM web apps. One floating glass title bar, a full-window scroller, and a
floating bottom stack containing a composer and/or tabs.

**`0.1.0-alpha.1`: migration candidate for the tested matrix, not a universal guarantee.**
The current runtime passed 342 desktop browser and three compiled Rust/WASM checks,
plus full 18-case matrices in iOS 26.5 Safari, iOS 26.5 Home Screen mode and installed
Android 16. The final document settings also passed the iPhone 15 Pro/iOS 18.1
critical keyboard path. Android browser resume and iOS 26.6 critical transitions
have explicit screenshot verification where native automation data was missing.
No production apps have been migrated; integration still needs each app's gate.
See [verification](TESTING.md) and [continued device testing](design/devicefarm-continuation-2026-09-20.md).

## What belongs here

- One owner of the visible viewport, including updates while an input stays focused.
- Content scrolling **behind** the glass, with measured start/end clearances.
- Safe-area spacing, changing chrome heights, optional follow-end scrolling.
- An optional native textarea sizing helper and complete lifecycle cleanup.

No React dependency, router, app state, message handling, telemetry, network,
storage, keyboard replacement, or separate Rust layout implementation.
The maintained runtime source is `src/shell.ts` + `src/shell.css` (423 lines; 4,812 bytes gzip for the combined
built JS and CSS in the current candidate).
The demo and tests are deliberately separate from that runtime.

## Try the sample

```bash
npm ci
npm run build
npm run dev
# http://127.0.0.1:4173/demo/
```

The settings button exercises themes, text up to 200%, RTL, long titles, optional
chrome and 0–1,000 messages. Explore and Notes exercise other content and inline
fields. Everything is invented/local. Typed text is neither saved nor sent.

The build also creates `artifacts/glass-shell-preview.html`: one downloadable,
self-contained desktop preview. On your Mac, from this remote server:

```bash
scp ty:/home/tyggs/TyggsWebShell/TyggsWebShell/artifacts/glass-shell-preview.html ~/Downloads/ && open ~/Downloads/glass-shell-preview.html
```

That file is **not** an installed PWA. For iPhone installation, serve `demo/` and
`dist/` at their current URL paths on an **HTTPS** origin, visit `/demo/` in Safari,
then Share → Add to Home Screen. The sample includes a manifest, icons and a
versioned static-only service worker. It is not deployed to a public origin by
this repository. `npm run dev` binds loopback; do not mistake plain LAN HTTP or an
SSH tunnel on a Mac for an HTTPS origin usable by an iPhone.

## Use from TypeScript / JavaScript

Pin an **exact reviewed Git commit**, not a moving branch:

```bash
npm install --save-exact 'git+https://github.com/tigy32/TyggsWebShell.git#<full-commit-sha>'
```

GitHub authentication is required. Never place a token in that URL or a lockfile.
Built `dist/` files are committed; consumers need no package build hook.

```html
<meta name="viewport" content="width=device-width, initial-scale=1, viewport-fit=cover, interactive-widget=resizes-content">
<meta name="apple-mobile-web-app-capable" content="yes">
<meta name="apple-mobile-web-app-status-bar-style" content="default">
<section class="tws-shell" id="shell">
  <header class="tws-header tws-glass">Your title and controls</header>
  <main class="tws-content">
    <div class="tws-flow">Your rendered content</div>
  </main>
  <footer class="tws-bottom">
    <form class="tws-composer tws-glass">
      <textarea aria-label="Message" rows="1"></textarea>
      <button type="submit">Send</button>
    </form>
    <nav class="tws-tabs tws-glass" aria-label="Navigation">
      <button type="button">Chat</button>
      <button type="button">Explore</button>
    </nav>
  </footer>
</section>
```

```ts
import { attachShell, attachTextarea } from '@tyggs/web-shell';
import '@tyggs/web-shell/shell.css';

const root = document.querySelector<HTMLElement>('#shell')!;
const shell = attachShell(root, { followEnd: true, initialScroll: 'end' });
const stopTextarea = attachTextarea(root.querySelector('textarea')!);

// Your framework's unmount/cleanup:
function cleanup() {
  stopTextarea();
  shell.destroy();
}
```

Use actual app rendering and form handlers; this package does not send anything.
Mount after DOM attachment and retain the same native textarea while editing.
Do not disable user zoom. Keep the shell out of ancestors with transforms,
filters or containment that establish a different fixed-position containing block.

**Remove the previous viewport/keyboard owner when integrating.** Do not keep
app-specific keyboard padding, fixed composer offsets, a second document scroller
or another root-height controller running alongside this one. This is where most
of the intended code reduction comes from; it has not yet been measured in either
production app. Their integrations and release gates are separate work.

### API

`attachShell(root, { followEnd?, initialScroll? })` requires the four structural
classes shown above. One attached shell per document. Empty or hidden header and
bottom regions are allowed; their DOM containers remain present.

| Handle | Purpose |
| --- | --- |
| `refresh()` | Schedule a geometry refresh after a non-resizing app change. |
| `scrollToEnd()` | Explicitly jump to the latest content and resume following. |
| `getState()` | Current applied top/left/width/height, keyboard heuristic and viewport scale; no message data. |
| `destroy()` | Restore owned styles/attributes and disconnect listeners, observers and frames; idempotent. |

`followEnd` defaults to false. When enabled, readers within 48 CSS px of the end
follow new content. Reading history turns it off until they return to the end.
Virtualized apps should keep their own anchoring with `followEnd: false`.
`initialScroll: 'start'` explicitly resets to the start; `'end'` starts at the
end. Omission preserves the
existing scroll position. A window cannot show an arbitrarily tall message in
full, but every portion remains reachable by scrolling.

`attachTextarea(textarea)` returns an idempotent cleanup function. It sizes the
real, border-box textarea without reading its value or intercepting composition.
Use the supplied composer CSS. After **programmatic** draft updates, dispatch an
`input` event so sizing catches up; ordinary typing already does this.

### Styling

Glass is opt-in through `tws-glass`. Set the `--tws-glass`, `--tws-rim`,
`--tws-shadow`, `--tws-ink`, `--tws-focus`, `--tws-edge` and `--tws-gap` tokens.
Safe-area tokens default to browser `env(safe-area-inset-*)` values. The controller
owns the geometry/clearance tokens; do not write those from the application.
Use the native `hidden` attribute to remove optional chrome. The sample bounds
oversized bottom chrome with an internal scroller so controls remain reachable.

Tabs are covered while the keyboard is open; they never join the composer above
it. They also hide below 181px of usable height. When measured title and composer
heights cannot both fit, the composer takes priority; the title restores as soon as space returns. Native controls
retain their 44px minimum and multiline text scrolls inside the same textarea.

Chrome uses `interactive-widget=resizes-content`; the shell temporarily disables
VirtualKeyboard overlay mode and restores its previous value on detach. Safari
uses VisualViewport. Keyboard classification controls tabs/safe areas only and
is a documented viewport-contraction heuristic, never a guessed composer offset.
The actual shell rectangle always comes from current browser viewport geometry.
Use the standard iOS system status bar, not legacy `black-translucent`: the
physical sample shows the latter can leave an extra bottom strip. The app bars
remain floating glass; content still scrolls underneath them. Verify metadata and
service-worker updates on existing Home Screen installations during app migration;
the farm sample used fresh installations.
At non-unit pinch zoom, shell geometry freezes to preserve browser zoom/pan.

## Rust / Leptos

[The executable Rust example](examples/rust-binding/src/lib.rs) imports the same
`/dist/shell.js` module through `wasm-bindgen`. It does **not** reimplement any
viewport math. `MountedShell` releases the JS owner in Rust's `Drop`.

In Tyde, render the structure using Leptos, attach when its root is connected,
retain the owner, and drop it from component cleanup. Serve pinned JS/CSS assets
at the import URL. Adjust that URL to the application's asset layout; use
[`raw_module`](https://wasm-bindgen.github.io/wasm-bindgen/reference/attributes/on-js-imports/raw_module.html)
for an externally served module. This example proves the WASM → JS boundary,
not the actual Tyde integration.

```bash
rustup target add wasm32-unknown-unknown
cargo install wasm-bindgen-cli --version 0.2.118 --locked
npm run check:rust
```

The CLI version intentionally matches the example's lockfile. The library itself
needs neither Cargo nor WASM; only this integration test does.

## Maintain

```bash
npm ci
npx playwright install --with-deps chromium firefox webkit
npm run check
npm run check:rust
```

Tests use real browser engines and static/local sample content, with no model
calls, accounts, cloud devices or production data. Retries, traces and recordings
are disabled. The GitHub workflow is **manual-only** to avoid duplicate hosted
runs/cost after local validation. Run it explicitly when wanted.

Update [the design contract](design/shell.md) before behavior changes, keep
`dist/` and the demo worker synchronized with `npm run build`, and follow
[AGENTS.md](AGENTS.md). Do not publicly publish this repository or npm package.

For a nested desktop workspace, `attachShell` also accepts
`regions: { header, content, bottom, flow }`, all DOM elements within the root.
The application supplies pane layout CSS and keeps its own history anchoring.
This changes region discovery only, not the shared viewport algorithm.
