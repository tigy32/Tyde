//! Shell scripts and probe parsing for the managed remote Tyde host
//! lifecycle. The desktop shell runs these over SSH; they live here so the
//! `tyde-server` lifecycle tests drive the exact production scripts against
//! the real binary.

use std::collections::HashMap;

use crate::{
    RemoteHostLifecycleSnapshot, RemotePlatform, RemoteTydeRunningState, TydeReleaseVersion,
};

const SHELL_LIB: &str = r#"pid_file="$HOME/.tyde/run/tyde-host.pid"
version_file="$HOME/.tyde/run/tyde-host-version"
socket="$HOME/.tyde/tyde.sock"
launch_lock="$HOME/.tyde/run/tyde-host-launch.pid"

# The lock is a noclobber redirection because that is an O_EXCL create inside
# the shell itself. `mkdir` is not a lock: uutils coreutils reports success
# when it loses the create race, which let two launches run at once.
acquire_launch_lock() {
  mkdir -p "$HOME/.tyde/run"
  i=0
  while ! (set -C; printf '%s\n' "$$" > "$launch_lock") 2>/dev/null; do
    lock_pid="$(cat "$launch_lock" 2>/dev/null || true)"
    if [ -n "$lock_pid" ] && ! kill -0 "$lock_pid" 2>/dev/null; then
      rm -f "$launch_lock"
      continue
    fi
    if [ "$i" -ge 100 ]; then
      echo "timed out waiting for another managed Tyde launch" >&2
      exit 1
    fi
    i=$((i + 1))
    sleep 0.1
  done
  trap 'rm -f "$launch_lock"' EXIT
}

release_launch_lock() {
  rm -f "$launch_lock"
  trap - EXIT
}

recorded_pid_alive() {
  [ -f "$pid_file" ] || return 1
  recorded_pid="$(cat "$pid_file" 2>/dev/null || true)"
  [ -n "$recorded_pid" ] && kill -0 "$recorded_pid" 2>/dev/null
}

socket_is_live() {
  [ -S "$socket" ] && "$1" host --status-uds > /dev/null 2>&1
}

# Releases whose launch script wrote the pid file could record a server that
# then lost the bind race, leaving the real socket owner untracked. Reclaim it
# when exactly one server launched from the managed bin directory is running.
# Call with the launch lock held.
adopt_orphaned_server() {
  i=0
  while :; do
    orphan_pids="$(pgrep -u "$(id -u)" -f "^$HOME/\.tyde/bin/[^/]+/tyde-server host --uds" || true)"
    orphan_count="$(printf '%s\n' "$orphan_pids" | grep -c . || true)"
    if [ "$orphan_count" -le 1 ] || [ "$i" -ge 10 ]; then
      break
    fi
    i=$((i + 1))
    sleep 0.2
  done
  [ "$orphan_count" -eq 1 ] || return 1
  orphan_version="$(ps -o args= -p "$orphan_pids" | sed -n "s|^$HOME/\.tyde/bin/\([^/]*\)/tyde-server host --uds.*|\1|p")"
  [ -n "$orphan_version" ] || return 1
  printf '%s\n' "$orphan_version" > "$version_file.adopt"
  mv "$version_file.adopt" "$version_file"
  printf '%s\n' "$orphan_pids" > "$pid_file.adopt"
  mv "$pid_file.adopt" "$pid_file"
}
"#;

const PROBE_BODY: &str = r#"if [ -x "$HOME/.tyde/bin/$target_version/tyde-server" ]; then
  echo installed_target=1
else
  echo installed_target=0
fi
if [ -L "$HOME/.tyde/bin/current" ]; then
  printf 'current_link_version=%s\n' "$(readlink "$HOME/.tyde/bin/current")"
else
  echo current_link_version=
fi
status_bin=
for candidate in "$HOME/.tyde/bin/$target_version/tyde-server" "$HOME/.tyde/bin/current/tyde-server"; do
  if [ -x "$candidate" ]; then
    status_bin="$candidate"
    break
  fi
done
running=not_running
if recorded_pid_alive && [ -S "$socket" ]; then
  running=managed
elif [ -S "$socket" ] && [ -z "$status_bin" ]; then
  if [ ! -f "$pid_file" ]; then
    running=unknown_socket
  fi
elif socket_is_live "$status_bin"; then
  acquire_launch_lock
  if recorded_pid_alive || adopt_orphaned_server; then
    running=managed
  else
    running=unknown_socket
  fi
  release_launch_lock
fi
echo "running=$running"
if [ "$running" = managed ] && [ -f "$version_file" ]; then
  printf 'running_version=%s\n' "$(cat "$version_file" 2>/dev/null || true)"
else
  echo running_version=
fi
"#;

const LAUNCH_BODY: &str = r#"bin="$HOME/.tyde/bin/$version/tyde-server"
log_file="$HOME/.tyde/logs/tyde-host-$version.log"
mkdir -p "$HOME/.tyde/logs"
acquire_launch_lock
if [ ! -x "$bin" ]; then
  echo "managed tyde-server binary is not executable: $bin" >&2
  exit 1
fi
if socket_is_live "$bin"; then
  if recorded_pid_alive || adopt_orphaned_server; then
    release_launch_lock
    exit 0
  fi
  echo "remote Tyde socket $socket is held by a server Tyde's managed lifecycle did not launch" >&2
  exit 1
fi
tail_launch_log() {
  if [ -f "$log_file" ]; then
    echo "last tyde-server launch log lines from $log_file:" >&2
    tail -n 80 "$log_file" >&2 || true
  fi
}
nohup "$bin" host --uds --managed >> "$log_file" 2>&1 < /dev/null &
pid=$!
i=0
while [ "$i" -lt 150 ]; do
  # The server records itself only after it has bound the socket, so a pid
  # file naming this launch proves this process owns it.
  if kill -0 "$pid" 2>/dev/null && [ "$(cat "$pid_file" 2>/dev/null || true)" = "$pid" ]; then
    ln -sfn "$version" "$HOME/.tyde/bin/current"
    release_launch_lock
    exit 0
  fi
  if ! kill -0 "$pid" 2>/dev/null; then
    # A launch this lock does not exclude (an older app's) can win the bind.
    if socket_is_live "$bin" && recorded_pid_alive; then
      release_launch_lock
      exit 0
    fi
    echo "managed tyde-server process exited before it owned $socket" >&2
    tail_launch_log
    exit 1
  fi
  i=$((i + 1))
  sleep 0.1
done
echo "managed tyde-server did not take ownership of $socket" >&2
tail_launch_log
exit 1
"#;

const STOP_BODY: &str = r#"if [ ! -f "$pid_file" ]; then
  exit 0
fi
pid="$(cat "$pid_file" 2>/dev/null || true)"
if [ -n "$pid" ] && kill -0 "$pid" 2>/dev/null; then
  kill "$pid"
  i=0
  while [ "$i" -lt 50 ]; do
    if ! kill -0 "$pid" 2>/dev/null; then
      rm -f "$pid_file" "$version_file"
      exit 0
    fi
    i=$((i + 1))
    sleep 0.1
  done
  echo "managed Tyde host process $pid did not stop" >&2
  exit 1
fi
rm -f "$pid_file" "$version_file"
"#;

pub fn shell_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\\''"))
}

pub fn probe_script(target_version: &TydeReleaseVersion) -> String {
    format!(
        "set -eu\ntarget_version={}\n{SHELL_LIB}{PROBE_BODY}",
        shell_quote(target_version.as_str())
    )
}

pub fn parse_probe_output(
    output: &str,
    platform: RemotePlatform,
    target_version: TydeReleaseVersion,
) -> Result<RemoteHostLifecycleSnapshot, String> {
    let parsed = parse_key_value_lines(output)?;
    let installed_target = parse_bool_field(&parsed, "installed_target")?;
    let current_link_version =
        parse_optional_release_version_path_result(parsed.get("current_link_version"))?;
    let running = match parsed.get("running").map(String::as_str) {
        Some("not_running") => RemoteTydeRunningState::NotRunning,
        Some("unknown_socket") => RemoteTydeRunningState::UnknownSocket,
        Some("managed") => {
            let version =
                parse_optional_release_version_path_result(parsed.get("running_version"))?
                    .ok_or_else(|| {
                        "managed remote Tyde process is missing its version file".to_string()
                    })?;
            RemoteTydeRunningState::Managed { version }
        }
        Some(other) => return Err(format!("unexpected remote running state {other:?}")),
        None => return Err("remote status probe did not return running state".to_string()),
    };

    Ok(RemoteHostLifecycleSnapshot {
        target_version,
        installed_target,
        current_link_version,
        running,
        platform,
    })
}

pub fn stop_script() -> String {
    format!("set -eu\n{SHELL_LIB}{STOP_BODY}")
}

pub fn launch_script(version: &TydeReleaseVersion) -> String {
    format!(
        "set -eu\nversion={}\n{SHELL_LIB}{LAUNCH_BODY}",
        shell_quote(version.as_str())
    )
}

fn parse_key_value_lines(output: &str) -> Result<HashMap<String, String>, String> {
    let mut parsed = HashMap::new();
    for line in output.lines() {
        let Some((key, value)) = line.split_once('=') else {
            return Err(format!("remote status line is not key=value: {line:?}"));
        };
        parsed.insert(key.to_string(), value.to_string());
    }
    Ok(parsed)
}

fn parse_bool_field(parsed: &HashMap<String, String>, key: &str) -> Result<bool, String> {
    match parsed.get(key).map(String::as_str) {
        Some("1") => Ok(true),
        Some("0") => Ok(false),
        Some(value) => Err(format!(
            "remote status field {key} is not boolean: {value:?}"
        )),
        None => Err(format!("remote status is missing field {key}")),
    }
}

fn parse_optional_release_version_path(value: &str) -> Option<Result<TydeReleaseVersion, String>> {
    let trimmed = value.trim();
    if trimmed.is_empty() {
        return None;
    }
    let last_component = trimmed.rsplit('/').next().unwrap_or(trimmed);
    Some(last_component.parse::<TydeReleaseVersion>())
}

fn parse_optional_release_version_path_result(
    value: Option<&String>,
) -> Result<Option<TydeReleaseVersion>, String> {
    match value {
        Some(value) => parse_optional_release_version_path(value).transpose(),
        None => Ok(None),
    }
}
