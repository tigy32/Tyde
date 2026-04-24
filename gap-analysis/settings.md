# Settings Gap Analysis

## Pass 8 - GPT-5 Codex - 2026-04-24

### Legacy reference points
- `~/Tyde/src/settings.ts`
- `~/Tyde/src/chat/session_settings.ts`
- `~/Tyde/src/bridge.ts`

### Rewrite reference points
- `frontend/src/components/settings_panel.rs`
- `frontend/src/state.rs`
- `frontend/src/bridge.rs`
- `protocol/src/types.rs`

### Implemented since earlier passes
- Settings surface expanded substantially: Hosts, Appearance, General, Backends, Custom Agents, MCP Servers, Steering, Skills, and Debug tabs.
- Host registry is now implemented (add/remove/select hosts, per-host connect/disconnect, remote lifecycle integration).
- Per-host backend enable/default controls are implemented and connected to runtime behavior.
- Backend setup/install/sign-in flows are implemented and surfaced.
- Remote Tyde server readiness/install/launch lifecycle is surfaced for managed SSH hosts.
- MCP toggles for Tyde Debug and Agent Control are implemented.
- Custom-agent and MCP-server editors are implemented.
- Skill refresh and steering management are implemented.
- Appearance settings now persist (theme, font size, font family, tab bar, diff prefs).
- Settings search/filter is implemented.
- Backend coverage now includes `tycode`, `kiro`, `claude`, `codex`, and `gemini`.

### Remaining gaps vs legacy
- No notifications settings tab.
- No dedicated provider/API-credential management tab comparable to legacy provider configuration.
- No dedicated advanced settings tab.
- No dedicated agent-models administration tab in the legacy sense.
- Theme selector still lacks a true "System" mode.
- Active settings tab is not persisted across open/close (panel resets to default tab on reopen).
