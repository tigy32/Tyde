# Settings Gap Analysis

## Pass 1 - GPT-5 Codex - 2026-04-13

### Legacy reference points
- `~/Tyde/src/settings.ts`
- `~/Tyde/src/chat/session_settings.ts`
- `~/Tyde/src/bridge.ts`

### Rewrite reference points
- `frontend/src/components/settings_panel.rs`
- `frontend/src/state.rs`
- `frontend/src/app.rs`

### Legacy coverage
- Multi-tab settings surface with `appearance`, `notifications`, `backends`, `agents`, `tyde`, `remote-server`, `providers`, `mcp`, `agent-models`, and `advanced`.
- Host management inside settings: add host, remove host, select host, persist selected host, and drive per-host configuration.
- Per-host backend controls: enabled backends, default backend, dependency checks/install flows, backend usage, and host-specific defaults.
- Admin/profile controls: active profile switching, default spawn profile, module schema loading, and other backend-admin state.
- Remote Tyde server management: inspect remote status, install, launch, upgrade, and toggle remote control.
- MCP controls: loopback MCP control server, driver MCP server, autoload toggle.
- Provider and model configuration editors.
- Agent definition management, including MCP servers, tool policy, skills, and default backend.

### Rewrite coverage
- Three tabs only: `Appearance`, `General`, and `Backends`.
- `font_size` and `default_backend` live only in frontend app state.
- Theme switching is incomplete: dark is hardcoded active, `Light` and `System` are disabled UI.
- No bridge from settings UI to host administration or persisted config.

### Confirmed gaps vs legacy
- No host registry UI. The rewrite cannot add, remove, or select hosts from settings.
- No per-host settings model. The app connects to a single hardcoded host path and does not expose host-scoped configuration.
- No per-host backend enable/disable controls, default backend selection, dependency install flows, or usage reporting.
- No admin/profile controls. Active profile selection and default spawn profile are missing.
- No remote Tyde server status/install/upgrade/launch surface.
- No provider configuration tab.
- No MCP settings tab for control server, driver server, or autoload behavior.
- No agent models tab.
- No advanced settings tab.
- No agent definition editor, MCP server editor, skills picker, or tool policy configuration.
- Appearance parity is incomplete: theme persistence and system/light behavior from the legacy app are absent.
- Even the settings that do exist are not persisted the way the legacy app persisted them.

### Suggested next slices
- Add a real settings domain model in the rewrite instead of keeping settings as local UI-only signals.
- Land host selection and per-host backend configuration before expanding provider/MCP/model tabs.
- Treat session settings as a separate but related parity item; those belong in chat, not just the global settings panel.

## Pass 2 - GPT-5 Codex - 2026-04-13

### Additional confirmed gaps
- Backend coverage itself regressed. The legacy app exposes `tycode`, `codex`, `claude`, `kiro`, and `gemini` in settings and home flows, while the rewrite protocol `BackendKind` only contains `Claude`, `Codex`, and `Gemini`.
- The legacy settings surface is not just a UI panel; it is a host-admin control plane. The rewrite has no comparable bridge/protocol surface for host registry, admin settings, profiles, providers, MCP servers, or agent definitions. `frontend/src/bridge.rs` only exposes connect/disconnect/send-line plumbing to the local host transport.
- Legacy settings persist selected host and use that host selection to drive every other tab. The rewrite settings have no notion of selected host, and therefore cannot scope any configuration by host even in principle.
- Legacy provider, MCP, model, and advanced tabs are data-driven against backend/module settings. The rewrite settings panel is static UI and has no schema-driven editing path.
- Legacy settings materially affect runtime behavior outside the panel: host-enabled backends feed new-chat availability, default backend menus, remote readiness messaging, MCP autoload, and agent-definition defaults. In the rewrite, settings are effectively isolated local UI state.
- Notifications parity is also absent. The legacy settings nav includes `notifications`; the rewrite has no notifications settings surface at all.

### Architectural note
- This parity area is blocked by missing application-level control APIs, not just missing Rust/Leptos components. A realistic parity plan needs a settings/admin protocol, host registry model, and persistence story before the settings UI can meaningfully grow.

## Pass 3 - GPT-5 Codex - 2026-04-13

### Interaction-level gaps
- Legacy settings support in-panel search/filtering across nav items, panels, cards, and fields. The rewrite settings panel has no settings search at all.
- Legacy settings have grouped/collapsible navigation sections (`Tyde Settings` and `Tycode Settings`). The rewrite uses a flat three-tab switcher.
- Legacy settings include a dedicated host toolbar at the top of the panel with host subtitle, host selector, add-remote action, and conditional remove action. The rewrite has no host context anywhere in the panel.
- Legacy backend settings include dependency installation flows and usage querying from within the panel. The rewrite `Backends` tab is informational only.
- Legacy settings are navigable as a broader operations surface; the rewrite panel is currently closer to app preferences than to workspace/runtime administration.

## Pass 4 - GPT-5 Codex - 2026-04-13

### Test-backed behavior gaps
- Legacy E2E coverage explicitly exercises host add/remove and verifies that settings apply per selected host. The rewrite has no host selector, so this entire workflow is absent.
- Legacy E2E coverage verifies that enabled backends dynamically affect backend option lists elsewhere in the app. The rewrite settings do not drive downstream backend availability at all.
- Legacy E2E coverage verifies MCP/control-server toggles, driver toggles, and dependent autoload enable/disable behavior. The rewrite has no comparable toggle dependency logic.
- Legacy E2E coverage checks provider-card rendering and module-schema-driven fields. The rewrite has no provider cards or module schema editing path.
- Legacy E2E coverage verifies that opening settings does not create a workspace/chat tab. The rewrite does keep settings as an overlay, which is good, but the overlay only covers a small subset of the tested legacy behavior.

## Pass 6 - GPT-5 Codex - 2026-04-13

### Additional state-persistence gaps
- Legacy settings persist the selected host across overlay open/close and app restarts. The rewrite has no host-selection concept at all, so there is no equivalent persistence path.
- Legacy settings also persist navigation state such as the active settings tab. The rewrite `frontend/src/components/settings_panel.rs` creates `active_tab` as a fresh local signal and always resets to `Appearance` when the overlay remounts.
- Legacy E2E coverage verifies that settings values populate from backend/admin state on open rather than only reflecting prior frontend-local choices. The rewrite settings tabs are initialized entirely from in-memory UI signals and static constants.
