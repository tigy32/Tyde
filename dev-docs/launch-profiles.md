# Launch Profiles

Launch Profiles are server-owned presets for starting new agents. A client may
still launch with an explicit `backend_kind`, but it may also include a
`launch_profile_id` such as `claude:default`.

The protocol source of truth is `protocol/src/types.rs`:

- `HostBootstrapPayload.launch_profile_catalog` carries the initial catalog.
- `launch_profile_catalog_notify` carries live catalog updates.
- `SpawnAgentParams::New.launch_profile_id` selects a profile for a spawn.

Clients treat profile IDs as opaque strings. The current server convention is
`backend:slug`.

## Spawn semantics

The server resolves the profile before spawning the agent:

1. Unknown profile IDs fail the `spawn_agent` command visibly.
2. Unavailable profile IDs fail visibly; there is no silent fallback to the
   backend default.
3. If `launch_profile_id` and explicit `backend_kind` disagree, the command
   fails visibly.
4. Profile session settings are merged with explicit launch
   `session_settings`; explicit launch settings win.
5. The merged settings are validated against the backend session schema before
   startup.

Existing backend-kind launches remain supported by omitting
`launch_profile_id`.

## Current IDs and explicit profiles

Every enabled backend gets a default profile:

- `tycode:default`
- `kiro:default`
- `claude:default`
- `codex:default`
- `antigravity:default`
- `hermes:default`

Only enabled backends appear in the catalog.

Additional named profiles come only from explicit host settings
(`HostSettings.launch_profiles` / `HostSettingValue::LaunchProfiles`). They
are server-owned, typed presets over `SessionSettingsValues`, and the server
validates them against the backend session schema before marking them ready.
If their backend is disabled they do not appear; if their settings are invalid
or a dynamic schema is unavailable, they appear as `unavailable`.

The Settings UI editor should write this explicit settings surface. It should
not infer named profiles from backend model names, and it should render the
server-emitted catalog rather than constructing launch options locally.

Tyde does not infer named Hermes profiles from `model.options` names. Today
Hermes exposes `hermes:default` automatically when Hermes is enabled. Profiles
such as `hermes:claude`, `hermes:codex`, and `hermes:grok` are emitted only if
they are explicitly configured with exact Hermes session settings; the native
Hermes JSON-RPC shape currently used by Tyde exposes model/provider options,
not a typed named-launch-profile catalog.

## MCP

Agent-control MCP exposes:

- `tyde_list_launch_options`: read-only discovery of the server-owned catalog,
  default backend, and current session schemas.
- `tyde_spawn_agent`: accepts optional `launch_profile_id` and optional
  `session_settings`. No Hermes-specific arguments are exposed.

Example:

```json
{
  "workspace_roots": ["/repo"],
  "prompt": "Investigate failing tests",
  "launch_profile_id": "claude:default",
  "session_settings": {
    "model": { "string": "haiku" }
  }
}
```
