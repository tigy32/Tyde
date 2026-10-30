# Mobile UI fixture

The Playwright fixture mounts the real Leptos mobile application with
deterministic local state. It does not contact AWS IoT, the managed mobile
service, a desktop Tyde process, or an AI backend.

Fixture code requires both the `ui-fixtures` Cargo feature and a debug build.
Normal development and every release build omit the fixture state and its
transport capture seam.

## Commands

```bash
npm install
npx playwright install chromium
npm run mobile:ui:test
```

Automated runs build the fixture first, then serve that checkout's output on
an OS-assigned loopback port. Global setup passes its actual bound address to
Playwright and awaits socket closure after the run. It never probes or reuses
an existing development server. The canonical check uses the same setup after
its fixture build; port 4173 is only for the interactive command below.

Run the local app for interactive inspection:

```bash
npm run mobile:ui:serve
```

Then open one of:

```text
http://127.0.0.1:4173/?tyde-fixture=onboarding
http://127.0.0.1:4173/?tyde-fixture=home
http://127.0.0.1:4173/?tyde-fixture=chat
http://127.0.0.1:4173/?tyde-fixture=images
http://127.0.0.1:4173/?tyde-fixture=chat-light
http://127.0.0.1:4173/?tyde-fixture=disconnected
http://127.0.0.1:4173/?tyde-fixture=error
```

`npm run mobile:ui:screenshots` captures every deterministic state under the
Playwright test-results directory. Failures retain a screenshot, trace, and
video for inspection.

The `images` fixture renders typed native image output in Summary mode using
only local fixture bytes. Use it for phone viewer QA: open either thumbnail,
pinch, pan, double-tap twice, use zoom/reset and image navigation, then close
with the accessible button, Escape, and browser Back. Forward reopens the
viewer. A focused composer draft, selection and history scroll must survive.
No provider call, URL inference or alternate artifact endpoint is involved.

## Adaptive mobile layout audit (2026-10-30)

The shell responds to its available CSS width, not a device name or posture.
At 640px it lays out whole project groups and settings sections in two columns;
chat keeps the existing presentation with bounded transcript/header/composer
rails, and primary navigation remains a compact dock. Narrow Split View returns
to the single-column layout without remounting the chat or settings controls.

Official platform basis:

- [Apple's Duo announcement](https://www.apple.com/newsroom/2026/09/apple-unveils-iphone-duo/)
  describes 5.4-inch outer and 7.6-inch inner displays, iOS 27 and Split View.
  Diagonals do not establish CSS viewport sizes.
- [WebKit's Safari 27.1 notes](https://webkit.org/blog/18486/webkit-features-for-safari-27-1/)
  publish outer portrait 382×580–618, outer landscape 594×368–406, inner
  portrait 669×867–902, inner landscape 951×585–620, and side-by-side windows
  385×571–609 (width × small-to-large viewport height, CSS pixels).
  Posture and viewport-segment APIs are behind flags; Tyde does not use them.
- [WebKit safe-area guidance](https://webkit.org/blog/7929/designing-websites-for-iphone-x/)
  requires insetting important content on all edges with viewport-fit=cover.

The existing `app.rs` mounted-shell wasm flow covers narrow/wide resizing,
unchanged textarea/transcript identity, draft/selection retention, touch targets,
chat rails, compact navigation, settings reflow and asymmetric sheet safe areas.
It runs through `./dev.sh check`; no separate local test command is required.

Manual browser evidence lives in `target/adaptive-mobile/` in this workbench.
`before-*` are exploratory captures of an older copied fixture, not current-main
evidence. `baseline-3748-*` use the 3748ac22 stylesheet on the rebuilt runtime
(the production Rust runtime is unchanged by this CSS-only change).
`after-source-*` are exploratory source-style renders; `after-*` without
`source` are rebuilt-fixture captures. Chromium viewport
emulation and Playwright WebKit 26.5 are browser evidence, **not** iOS Safari
27.1, a native simulator, keyboard-resume proof, or real Duo hardware coverage.
The fixture's missing release-loader/paired-host diagnostics are visible and
are not live-connection failures. Mobile web has one chat presentation, not the
desktop Flow/notebook selectors. No backend or lifecycle behavior is changed.

The sheet audit also injects a visual viewport of 180/320/420px while leaving
an 844px layout viewport intact. Before the overlay fix, the session backdrop
stayed 844px high and Done remained at y=704–748 even with a 320px shell. The
backdrops now use the positioned shell as their containing block; menus use
its container height. No measurement, stale-viewport recovery or keyboard
controller code changes. This is injected browser geometry, not a native
keyboard reproduction.
