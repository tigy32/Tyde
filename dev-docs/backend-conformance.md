# Backend conformance at the trait boundary

The backend conformance suite is `tests/tests/conformance2.rs`. Its harness calls the production `Backend` trait directly. It runs real installed providers and checks normalized events, native history, settings, and filesystem results. Server protocol behavior belongs in the mock-server simulations.

The 29 scenarios below retain the original scenario names. Native-provider configuration and filesystem regressions also retain their original provider-specific oracles. Capability exclusions come from backend declarations; they are not passing backend runs.

## Migration coverage

Passing real runs recorded on 2026-09-09. A dash means the backend does not declare the case’s required capabilities or native skill delivery. The existing Codex configuration case applies only to Codex.

| Scenario | Claude | Codex | Kiro | Antigravity | Grok | OpenCode | Hermes |
|---|---|---|---|---|---|---|---|
| `real_codex_global_settings` | — | Pass | — | — | — | — | — |
| `real_conversation` | Pass | Pass | Pass | Pass | Pass | Pass | Pass |
| `real_skills` | Pass | Pass | — | Pass | — | — | — |
| `real_image_input` | Pass | Pass | Pass | — | Pass | Pass | Pass |
| `real_session_settings` | Pass | Pass | Pass | Pass | Pass | Pass | Pass |
| `real_session_speed` | Pass | Pass | — | — | — | — | — |
| `real_tool_type_mappings` | Pass | Pass | Pass | Pass | Pass | Pass | Pass |
| `real_interruption` | Pass | Pass | — | Pass | Pass | — | Pass |
| `real_interrupt_after_background_response` | Pass | Pass | — | — | — | — | Pass |
| `real_conversation_on_resumed_session` | Pass | Pass | Pass | Pass | Pass | Pass | Pass |
| `real_resumed_session_groups_parallel_tool_calls` | Pass | Pass | Pass | Pass | Pass | Pass | Pass |
| `real_steering_compaction_and_resume` | Pass | Pass | — | — | Pass | — | Pass |
| `real_user_question` | Pass | — | — | Pass | — | — | Pass |
| `real_watched_command_shows_every_interaction` | — | Pass | — | — | — | — | — |
| `real_background_task_outlives_its_turn` | Pass | Pass | — | — | — | — | Pass |
| `real_background_task_cancel` | Pass | Pass | — | — | — | — | Pass |
| `real_conversation_in_native_subagent` | Pass | Pass | — | Pass | Pass | Pass | Pass |
| `real_native_wait_excludes_completed_children` | — | Pass | — | — | — | — | — |
| `real_nested_subagent_ownership` | Pass | Pass | — | Pass | Pass | Pass | Pass |
| `real_native_workflow` | Pass | — | — | — | — | — | — |
| `real_usage_accounting` | Pass | Pass | — | Pass | Pass | Pass | Pass |
| `real_subscription_capacity` | Pass | Pass | Pass | Pass | Pass | — | — |
| `real_capacity_without_a_conversation` | Pass | Pass | Pass | Pass | Pass | — | — |
| `real_task_list` | Pass | Pass | Pass | Pass | Pass | Pass | Pass |
| `real_mcp_tool_call` | Pass | Pass | Pass | Pass | Pass | Pass | Pass |
| `real_mcp_slow_server_is_not_reported_unavailable` | Pass | Pass | Pass | Pass | Pass | Pass | Pass |
| `real_tyde_agent_spawn` | Pass | Pass | Pass | Pass | Pass | Pass | Pass |
| `real_agent_await_survives_a_resumed_session` | Pass | Pass | Pass | Pass | Pass | Pass | Pass |
| `real_native_goal_lifecycle` | — | Pass | — | — | — | — | — |

Additional real regressions cover Claude’s native session location, Codex’s abandoned and legacy dynamic await tools, Claude plan approval, and Codex discovery lifecycle. The lifecycle test starts the real CLI and uses a transparent proxy to delay or disconnect transport; it supplies no provider responses.

Server-only guarantees remain in `server/tests/session_resume.rs`: history paging and bootstrap barriers, server session-list metadata, and idle/busy compaction admission with a single correlated timeline entry. Catalog refresh and error propagation remain in `tests/tests/bootstrap.rs`, driven by typed `MockBackend::discover` results.

Normal validation is `./dev.sh check`; it builds the test binary and MCP bridge without running paid cases. Real cases are ignored and additionally require `TYDE_RUN_REAL_AI_TESTS=1`; `TYDE_REAL_BACKENDS` selects providers. Follow the authorization rules in `AGENTS.md` before running them.

## Exhausted-account regression

`real_exhausted_account_stays_open` requires an authenticated real account
whose balance or usage quota is already exhausted for the selected model.
Set `TYDE_REAL_ACCOUNT_EXHAUSTED=1` in addition to the normal paid-run opt-in.
Missing exhaustion is a failure; this scenario never spends down an account
or injects a provider error. It submits two small prompts in the same session,
requires a capacity rejection and idle state for each, and checks that the
backend event stream remains open after each rejection. The scenario is
registered for every supported backend with identical setup and assertions.
Real runs, including a baseline run without the fix, still require approval.

## Workspace relocation

Real runs on 2026-09-11 used the same filesystem and retained-history
assertions for every eligible backend:

| Scenario | Claude | Codex | Hermes | Kiro | Grok | OpenCode | Antigravity |
| --- | --- | --- | --- | --- | --- | --- | --- |
| `real_workspace_relocation` | Pass | Pass | Pass | Pass | Pass | Pass | Pass |
| `real_multiple_workspace_relocation` | — | Pass | — | — | — | — | Pass |

Models were Claude Haiku, Codex `gpt-5.6-luna`, Hermes
`openai/gpt-4.1-mini` through OpenRouter, Kiro's `auto` selection, Grok
`grok-4.6`, OpenCode `opencode/mimo-v2.5-free`, and Antigravity
`Gemini 3.7 Flash (Medium)`.
Claude used isolated configuration trusting only disposable fixture roots.
Hermes's default DeepSeek model repeatedly continued reasoning after correct
file operations and timed out; the passing run used the existing
`TYDE_HERMES_TEST_MODEL` override without changing assertions.

The first Codex runs failed because loaded-thread `thread/resume` ignored
cwd/root overrides. The first Hermes run failed because its native session
remained busy during cleanup after Tyde's idle event. These real failures
establish red coverage for the corresponding fixes.

The remaining providers first failed this same scenario: Grok could not find
its directory-scoped transcript; aliasing it alone then left `pwd` in the
original directory. OpenCode retained the original cwd after both ACP reload
and process restart. Antigravity retained old workspace URIs in its native
trajectory despite different launch roots; `--new-project` was ignored when
resuming. These failures establish red coverage for the native metadata
relocation paths documented in `06-projects.md`.

After the fixes, Grok, OpenCode, and Antigravity passed the unchanged shared
single-root scenario, including retained conversation context, exact file
locations, rejected inputs, and the return move. Antigravity additionally
passed the two-root scenario. Kiro passed the single-root scenario again to
cover the shared ACP transport changes. All seven now declare relocation
capability; multiple-root capability belongs to Codex and Antigravity.
