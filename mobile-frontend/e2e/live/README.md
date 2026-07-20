# Live mobile browser

This harness is separate from the deterministic UI fixture. It opens the
deployed `https://tycode.dev/tyde/` application and keeps a real Chromium
profile under `.tyde-playwright/mobile-live-profile/`.

That profile contains sensitive Tyggs cookies and Tyde IndexedDB pairing
credentials. It is git-ignored. Never copy, commit, upload, or share it.

## One-time login and pairing

```bash
npm run mobile:live:login
```

Complete Tyggs OAuth in the opened browser, then pair it from desktop Tyde.
Paste pairing URI is the most reliable automation setup because it does not
depend on a test machine camera. Return to the terminal and press Enter only
after the browser says it is connected.

## Repeatable smoke test

```bash
npm run mobile:live:test
```

The smoke test verifies the real mobile session, real paired-host connection,
connection UI, and reconnection after a full page reload. It captures failure
screenshots, videos, and traces under `test-results/mobile-playwright-live/`.

For visible debugging instead of headless execution:

```bash
npm run mobile:live:show
```

The smoke test deliberately does not send an agent prompt. A real prompt can
start a paid backend turn and must remain an explicit, separately approved live
test.
