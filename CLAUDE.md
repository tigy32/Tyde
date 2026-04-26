# Tyde2 — Claude guidance

## Frontend UI tests

Component-level rendering tests live inline in their component file under
`#[cfg(all(test, target_arch = "wasm32"))] mod wasm_tests` and use
`wasm-bindgen-test`. They mount a real Leptos component into a real DOM in a
headless Chrome instance.

**How to run them:**

```sh
tools/run-wasm-tests.sh                  # all wasm tests
tools/run-wasm-tests.sh wasm_tests::     # filter
```

The script handles the fiddly setup (matching chromedriver to the installed
Chrome via Chrome for Testing, ad-hoc signing on macOS, installing
`wasm-bindgen-cli` at the lockfile-pinned version, caching under
`target/wasm-test-cache/`). It's the same entry point CI uses — don't bypass
it with raw `cargo test --target wasm32-unknown-unknown` unless you're
debugging the script itself.

Requires Chrome installed locally, plus `cargo`, `curl`, `unzip`, `python3`.

Native `cargo test` (no `--target`) still runs the existing logic-only tests.

**How to write them:**

Assert on **what the user perceives**, not on internal structure:

- Visible text content of rendered elements (`element.text_content()`).
- Geometry — `getBoundingClientRect()` for sizes, gaps between elements,
  alignment. The headless renderer is deterministic for synchronous render,
  so geometry assertions are not flaky as long as you let the reactive
  runtime flush a tick (`next_tick().await` in our tests).
- Element counts that match the input fixture.

Avoid asserting on:

- Specific CSS class names (a class rename would break the test for no real
  reason).
- Internal DOM structure beyond what's needed to find rendered output.
- Anything an AI refactor could trivially "fix" by editing the assertion
  rather than the code.

**The hard rule for AI agents (Claude included):**

When a UI test fails after a code change, you may **not** weaken or delete
the assertion to make the test pass without explicit human approval. Either:

1. Fix the code so the original assertion holds again, or
2. If the assertion turns out to be wrong (testing the wrong thing, not the
   user-visible behavior), explain why to the user and ask before changing
   it.

The whole point of these tests is to be a thing the AI can't silently route
around. Routing around them defeats their purpose.

The first such test lives at `frontend/src/components/file_view.rs` →
`mod wasm_tests`. It catches the class of bug where the file view
double-spaces lines (or any per-row text mangling).
