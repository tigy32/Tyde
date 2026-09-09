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
| `real_steering_compaction_and_resume` | Pass | Pass | — | — | — | — | Pass |
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
