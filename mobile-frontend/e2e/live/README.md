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

## Deterministic E2E OAuth

The secret-gated test provider signs in an allowlisted active-Pass fixture
without putting the production caller key in browser JavaScript, URLs, storage,
screenshots, traces, or logs:

```bash
npm run mobile:live:e2e:login
npm run mobile:live:e2e:test
```

The first command uses a separate
`.tyde-playwright/mobile-live-e2e-profile/`. Pair that profile to desktop Tyde
once, then use the second command for repeated real connection and reload
tests. The harness defaults to `active-pass-p1` and the production E2E OAuth
secret. `TYDE_E2E_FIXTURE_ID`, `TYGGS_E2E_OAUTH_SECRET_ID`, and `AWS_REGION`
may select another reviewed fixture or environment.

The Node process fetches the dedicated secret from AWS Secrets Manager, asks
the mobile service to bootstrap Account OAuth in `signin` mode, calls Account's
single-use test callback, and redeems the resulting handoff through the normal
mobile service endpoint. The browser receives only the normal HttpOnly mobile
session cookie. Never move this authentication into page code or enable
Playwright request tracing around it.
