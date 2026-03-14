# Driver MCP Test Plan

## What an agent should be able to do

The driver MCP server (`127.0.0.1:47773/mcp`) gives an agent the ability to:

1. **Spawn a fresh Tyde dev instance** from a project directory (`tyde_dev_instance_start`)
2. **Fully control that dev instance's UI** via 11 proxied debug tools (click, type, screenshot, etc.)
3. **Tear down the dev instance** when done (`tyde_dev_instance_stop`)

The agent never touches the host Tyde window. All debug tools route through the driver to the spawned dev instance. If no dev instance is running, every debug tool errors.

---

## Test Script

### Prerequisites
- Host Tyde is running
- Driver MCP server is enabled in Settings > Agent Control > "Enable MCP Driver"
- Agent session has the `tyde_driver` MCP server available (either via autoload toggle or manual config)

### Phase 1: No Instance Running — Tools Should Error

1. Call `tyde_debug_snapshot` with no arguments
   - **Expected:** Error message indicating no dev instance is running

2. Call `tyde_debug_capture_screenshot` with no arguments
   - **Expected:** Same error — no dev instance

3. Call `tyde_debug_click` with `{"selector": "button"}`
   - **Expected:** Same error — no dev instance

### Phase 2: Start a Dev Instance

4. Call `tyde_dev_instance_start` with `{"project_dir": "/path/to/tyde/repo"}`
   - **Expected:** Returns `{"debug_mcp_url": "http://127.0.0.1:<port>/mcp", "status": "running"}`
   - This may take a few minutes on first build (runs `npx tauri dev`)

5. Call `tyde_dev_instance_start` again while one is already running
   - **Expected:** Error — only one dev instance can run at a time

### Phase 3: Inspect the Dev Instance

6. Call `tyde_debug_snapshot`
   - **Expected:** JSON with `timestamp_ms`, `conversations`, `runtime_agents`, server status fields
   - Confirms the proxy is working end-to-end

7. Call `tyde_debug_capture_screenshot`
   - **Expected:** PNG image of the dev instance's UI (not the host window)

8. Call `tyde_debug_list_testids`
   - **Expected:** Array of `data-testid` values from the dev instance DOM

9. Call `tyde_debug_query_elements` with `{"selector": "[data-testid]", "include_text": true, "max_nodes": 5}`
   - **Expected:** Array of element info objects with text content

10. Call `tyde_debug_get_text` with `{"selector": "body"}`
    - **Expected:** Text content from the dev instance body

### Phase 4: Drive the Dev Instance UI

11. Call `tyde_debug_list_testids` to find a clickable element (e.g. a button or tab)
    - Note a testid to use in the next step

12. Call `tyde_debug_click` with `{"selector": "[data-testid='<testid-from-step-11>']"}`
    - **Expected:** Success acknowledgment

13. Call `tyde_debug_capture_screenshot`
    - **Expected:** Screenshot reflects the click (different UI state than step 7)

14. Call `tyde_debug_type` with `{"selector": "textarea, input", "text": "hello from the driver"}`
    - **Expected:** Success — text was typed into the dev instance

15. Call `tyde_debug_keypress` with `{"key": "Escape"}`
    - **Expected:** Success

16. Call `tyde_debug_scroll` with `{"dy": 200}`
    - **Expected:** Success — window scrolled

17. Call `tyde_debug_wait_for` with `{"selector": "[data-testid='<some-testid>']", "timeout_ms": 5000}`
    - **Expected:** Success — element already exists so it resolves immediately

### Phase 5: Event Log

18. Call `tyde_debug_events_since` with `{"since_seq": 0, "limit": 10}`
    - **Expected:** Array of debug event log entries from the dev instance

### Phase 6: Stop the Dev Instance

19. Call `tyde_dev_instance_stop`
    - **Expected:** `{"status": "stopped"}`

20. Call `tyde_debug_snapshot`
    - **Expected:** Error — no dev instance running (back to phase 1 behavior)

21. Call `tyde_dev_instance_stop` again
    - **Expected:** Error — no dev instance to stop

---

## Key Invariants

- Debug tools on the host **never** target the host's own UI — they always proxy through the driver
- The debug MCP server only runs on dev instances (enabled via `TYDE_DEBUG_MCP_HTTP_ENABLED` env var)
- The dev instance cannot spawn its own dev instances (recursion prevented via `TYDE_DRIVER_MCP_HTTP_ENABLED=false`)
- The dev instance cannot run the agent control MCP (`TYDE_MCP_HTTP_ENABLED=false`)
- Only one dev instance can exist at a time
