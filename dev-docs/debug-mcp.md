# Tyde Debug MCP HTTP Server

Tyde can expose a local MCP server intended for debugging Tyde itself.

This server is separate from the agent-control MCP server and is focused on:

- reading debug event logs
- inspecting UI state
- capturing screenshots
- driving basic UI interactions (click/type/keypress/scroll/wait)

## Enable / Disable

- The server is disabled by default.
- Auto-load into new sessions is also disabled by default.
- You can toggle both in Tyde Settings -> Agent Control:
  - `Enable Loopback MCP Debugging`
  - `Auto-load into new sessions`
- Both toggles persist in `~/.tyde/app-settings.json`:
  - `debug_mcp_http_enabled`
  - `debug_mcp_http_autoload`
- Turning the master server toggle off automatically turns auto-load off.

## Endpoint

When enabled, Tyde starts a Streamable HTTP MCP endpoint and exports:

- `TYDE_DEBUG_MCP_HTTP_URL`, for example `http://127.0.0.1:47772/mcp`

Binding rules:

- Default bind address: `127.0.0.1:47772`
- Override with `TYDE_DEBUG_MCP_HTTP_BIND_ADDR`
- Non-loopback addresses are rejected and replaced with loopback
- If the requested port is busy, Tyde falls back to an ephemeral loopback port

## MCP Tools

Exposed tools:

- `tyde_debug_snapshot`
- `tyde_debug_events_since`
- `tyde_debug_query_elements`
- `tyde_debug_get_text`
- `tyde_debug_list_testids`
- `tyde_debug_capture_screenshot`
- `tyde_debug_click`
- `tyde_debug_type`
- `tyde_debug_keypress`
- `tyde_debug_scroll`
- `tyde_debug_wait_for`

## Notes

- This server is loopback-only but high privilege. Keep it off unless needed.
- Screenshot output is PNG base64 returned via MCP image content.
- Auto-load injects this server into newly launched backend sessions (Tycode/Codex/Claude).
