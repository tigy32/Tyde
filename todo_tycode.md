# Tycode Backend — TODO

Priorities: **P0** = broken/blocking, **P1** = important, **P2** = should do, **P3** = nice to have, **P4** = someday, **P5** = wishlist

## P1 — Important

- [ ] **Session deletion.** The Tycode backend has no `delete_session` handler. When a user deletes a session from the sessions panel, the frontend calls `adminDeleteSession` which routes to the backend — but Tycode has no handler, so the delete silently fails and the session reappears on refresh. Needs a `delete_session` command that removes or archives the session data.
- [ ] **Ephemeral sessions / no-persistence flag.** Hidden/UI-only agents need to run without being saved in session history, but Tycode currently has no backend flag equivalent to Claude/Codex no-persistence modes. Add a session-level `no_persist`/`ephemeral` option so hidden agents do not show up in sessions.

## P2 — Should Do

- [ ] **HTTP MCP servers.** The Tycode backend does not support connecting to MCP servers over HTTP. Claude and Codex backends can connect to remote MCP servers via HTTP/SSE transport, but Tycode lacks this. Needs HTTP/SSE transport support for remote MCP servers.
