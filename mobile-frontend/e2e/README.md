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

Run the local app for interactive inspection:

```bash
npm run mobile:ui:serve
```

Then open one of:

```text
http://127.0.0.1:4173/?tyde-fixture=onboarding
http://127.0.0.1:4173/?tyde-fixture=home
http://127.0.0.1:4173/?tyde-fixture=chat
http://127.0.0.1:4173/?tyde-fixture=chat-light
http://127.0.0.1:4173/?tyde-fixture=disconnected
http://127.0.0.1:4173/?tyde-fixture=error
```

`npm run mobile:ui:screenshots` captures every deterministic state under the
Playwright test-results directory. Failures retain a screenshot, trace, and
video for inspection.
