# Tyde Steering Guidelines

These rules apply to all code changes across Rust, TypeScript, CSS, and tests.

## 1) How to Commit

- Validate the build compiles, formatting and checks pass, and tests pass before
  committing.
- Required verification commands before commit, unless the user explicitly
  waives one: `npm run lint`, `npm run build`, `npm run check`,
  `npm run test:e2e`.
- Do not commit if any required verification command fails, including
  pre-existing unrelated failures. Stop and ask for guidance.
- Treat every commit as blocked until verification has been run in the current
  working tree after the final edits.
- If anything fails and the fix is relatively simple, fix it directly.
- If a fix is complicated, changes logic, or should get human approval, stop
  and fail instead of pushing a risky fix.
- Generate commit messages with a 50-character summary line and a prose
  paragraph body.
- Do not use lists in commit messages.
- Do not include co-author lines for Claude.
- Wrap commit message text at 80 characters.
- Verify commit message limits mechanically before finishing:
  summary line <= 50 chars and every line <= 80 chars.
- In handoff, report each verification command and pass/fail status.
- Commit changes locally to the current branch.

## 2) How to Develop

- Keep code as simple as possible.
- Enforce invariants and raise errors.
- Do not swallow errors.
- Do not compensate for bugs by adding defensive code in unrelated layers.
- Keep a single code path.
- Do not add fallback branches that "try many options and hope one works."
- If input or state is wrong, raise an error and fix the source issue.
- If a backend has a bug, fix the backend bug rather than compensating in the
  conversation handler or other callers.

## 3) How to Debug

1. Take a quick code inspection pass and see if the bug is obvious.
2. Follow the scientific method: propose theories and seek evidence to disprove
   them.
3. If a theory cannot be proved or disproved quickly, add logging and/or use
   the Tyde debug instance for evidence.
4. Only make a fix when the bug is obvious from inspection or a theory is
   proven.
5. Most bug fixes remove code. If you are adding a lot of code, you are likely
   compensating for a bug elsewhere instead of fixing root cause.

## 4) Style Mandates

- YAGNI: only write code directly required to minimally satisfy the request.
  Never build throwaway code, new main methods, or test scripts unless
  explicitly requested.
- Avoid deep nesting: use early returns instead of if else chains. Maximum
  indentation depth is 4 levels.
- Separate policy from implementation: push decisions up and execution down.
  Avoid passing Optional or null values that force lower layers to invent
  fallback behavior.
- Comment why, not what: explain architectural purpose or reasoning only.
- Avoid over-generalizing and over-abstraction. Prefer functions over structs
  and structs over traits when a simpler shape works.
- Avoid global state and global constants.
- Surface errors immediately: never silently drop errors.
- Critical: never write mock implementations.
- Critical: never add fallback code paths that return hardcoded values or TODO
  placeholders instead of real implementation.
- If blocked on implementation difficulty, ask the user for guidance.

### Rust Specific

- No re-exports: make modules public directly. `pub use` is banned.
- Format errors with debug style, for example `{e:?}`, rather than calling
  `to_string()`.
- Put `use` statements at the top of each module. Do not use fully-qualified
  paths unless required.
