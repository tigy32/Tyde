#!/bin/sh
set -eu

TYDE_SETTINGS_FILE="${HOME}/.tyde/settings.json"
CODEX_CONFIG_FILE="${HOME}/.codex/config.toml"

tmp_ps="$(mktemp)"
trap 'rm -f "$tmp_ps"' EXIT INT TERM HUP

ps -axo pid,ppid,command >"$tmp_ps"

printf 'Tyde settings file: %s\n' "$TYDE_SETTINGS_FILE"
if [ -f "$TYDE_SETTINGS_FILE" ]; then
    sed -n '1,120p' "$TYDE_SETTINGS_FILE"
else
    printf 'missing\n'
fi

printf '\nCodex config file: %s\n' "$CODEX_CONFIG_FILE"
if [ -f "$CODEX_CONFIG_FILE" ]; then
    sed -n '1,120p' "$CODEX_CONFIG_FILE"
else
    printf 'missing\n'
fi

printf '\nRelevant app-server processes:\n'
grep -E 'codex app-server|tauri-shell' "$tmp_ps" || true

plain_codex_lines="$(grep -F '/Applications/Codex.app/Contents/Resources/codex app-server' "$tmp_ps" || true)"
plain_codex_without_mcp="$(printf '%s\n' "$plain_codex_lines" | grep -v 'mcp_servers\.' || true)"
tyde_codex_with_mcp="$(grep -E 'codex app-server .*mcp_servers\.tyde-debug\.url=.*mcp_servers\.tyde-agent-control\.url=' "$tmp_ps" || true)"
tyde_settings_enabled=0
codex_config_has_mcp=0

if [ -f "$TYDE_SETTINGS_FILE" ] \
    && grep -q '"tyde_debug_mcp_enabled": true' "$TYDE_SETTINGS_FILE" \
    && grep -q '"tyde_agent_control_mcp_enabled": true' "$TYDE_SETTINGS_FILE"; then
    tyde_settings_enabled=1
fi

if [ -f "$CODEX_CONFIG_FILE" ] && grep -q '^\[mcp_servers\.' "$CODEX_CONFIG_FILE"; then
    codex_config_has_mcp=1
fi

printf '\nSummary:\n'
printf '  tyde_settings_enabled=%s\n' "$tyde_settings_enabled"
printf '  codex_config_has_mcp=%s\n' "$codex_config_has_mcp"
if [ -n "$plain_codex_without_mcp" ]; then
    printf '  standalone_codex_app_server_without_mcp=yes\n'
else
    printf '  standalone_codex_app_server_without_mcp=no\n'
fi
if [ -n "$tyde_codex_with_mcp" ]; then
    printf '  tyde_spawned_codex_with_mcp=yes\n'
else
    printf '  tyde_spawned_codex_with_mcp=no\n'
fi

if [ "$tyde_settings_enabled" -eq 1 ] \
    && [ "$codex_config_has_mcp" -eq 0 ] \
    && [ -n "$plain_codex_without_mcp" ] \
    && [ -n "$tyde_codex_with_mcp" ]; then
    printf '\nReproduced: Tyde MCP is enabled and injected into Tyde-spawned Codex backends, but not into the standalone Codex desktop app-server for this session.\n'
    exit 0
fi

printf '\nNot reproduced: this machine does not currently match the specific split-brain visibility issue.\n' >&2
exit 1
