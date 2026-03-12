# Tyde Engineering Policies

These rules apply to ALL code changes — Rust, TypeScript, CSS, tests, everywhere.

## No Fallbacks, No Inference

- **NEVER implement fallback functionality.** If something fails, let it fail visibly — log it, show a notification, or let it propagate. Never silently swallow errors (`catch { return {}; }`, `if (!x) return;`, `unwrap_or_default()`).
- **NEVER infer or guess parameters that should be known.** No heuristic lookups, no "find the most likely match", no auto-fill from context. If a value should be available, plumb it explicitly through the call chain.
- **One call path, least branching possible, always works or errors.** If you find yourself writing "try A, fallback to B", stop — fix A instead.
- This applies everywhere: localStorage reads, callback lookups, map accesses, parameter resolution, native OS features.

## Keep It Simple

- Only make changes that are directly requested or clearly necessary.
- Don't add features, refactor code, or make "improvements" beyond what was asked.
- Don't add error handling for scenarios that can't happen.
- Don't create helpers or abstractions for one-time operations.
- Three similar lines of code is better than a premature abstraction.

## Testing Conventions

See `tests/TESTING.md` for full details. Summary:

- **One comprehensive test per feature area** — single long flow, not isolated unit tests.
- **UI-only assertions** — assert visible text, enabled/disabled controls, panel visibility. Never assert localStorage keys, internal class fields, method calls, or DOM tree shape.
- **Smoke tests** — goal is fast feedback that nothing is fundamentally broken.
- **Extend existing tests** — don't create new describe/it blocks for related functionality.
