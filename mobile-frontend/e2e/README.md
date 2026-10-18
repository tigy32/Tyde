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
