use std::collections::HashMap;

use host_config::{
    ConfiguredHost, HostLifecycleEvent, HostTransportConfig, RemoteArchitecture,
    RemoteHostLifecycleConfig, RemoteHostLifecycleSnapshot, RemoteHostLifecycleStatus,
    RemoteHostLifecycleStep, RemoteOperatingSystem, RemotePlatform, RemoteTydeRunningState,
    TydeReleaseVersion,
};
use tauri::{AppHandle, Emitter};
use tokio::process::Command;

use crate::bridge::HOST_LIFECYCLE_EVENT;

const GITHUB_RELEASE_DOWNLOADS: &str = "https://github.com/tigy32/Tyde/releases/download";

pub(crate) fn current_app_release_version() -> Result<TydeReleaseVersion, String> {
    let tag = option_env!("TYDE_RELEASE_TAG").ok_or_else(|| {
        "this Tyde build does not include release metadata, so managed remote install is disabled; use an official release build or configure a manual remote command"
            .to_string()
    })?;
    tag.parse::<TydeReleaseVersion>()
        .map_err(|err| format!("invalid TYDE_RELEASE_TAG {tag:?}: {err}"))
}

pub async fn probe_configured_host_lifecycle(
    app: AppHandle,
    host: ConfiguredHost,
) -> Result<RemoteHostLifecycleSnapshot, String> {
    let managed = managed_ssh_host(&host)?;
    probe_managed_host(&app, &host.id, &managed).await
}

pub async fn ensure_configured_host_ready(
    app: AppHandle,
    host: ConfiguredHost,
) -> Result<RemoteHostLifecycleSnapshot, String> {
    let managed = managed_ssh_host(&host)?;
    let target_version = current_app_release_version()?;

    let mut snapshot = probe_target_snapshot(&app, &host.id, &managed, target_version).await?;
    match plan_lifecycle_action(&snapshot) {
        LifecycleAction::ServeAsIs => {
            emit_running(
                &app,
                &host.id,
                RemoteHostLifecycleStep::Connect,
                Some(snapshot.target_version.clone()),
            );
            emit_snapshot(&app, &host.id, snapshot.clone());
            Ok(snapshot)
        }
        LifecycleAction::Launch { needs_install } => {
            if needs_install {
                snapshot =
                    ensure_target_installed(&app, &host.id, &managed.ssh_destination, &snapshot)
                        .await?;
            }
            launch_and_verify(
                &app,
                &host.id,
                &managed.ssh_destination,
                snapshot.target_version.clone(),
            )
            .await
        }
        LifecycleAction::Upgrade { needs_install } => {
            if needs_install {
                snapshot =
                    ensure_target_installed(&app, &host.id, &managed.ssh_destination, &snapshot)
                        .await?;
            }
            emit_running(
                &app,
                &host.id,
                RemoteHostLifecycleStep::StopOldServer,
                Some(snapshot.target_version.clone()),
            );
            stop_managed_server(&managed.ssh_destination).await?;
            launch_and_verify(
                &app,
                &host.id,
                &managed.ssh_destination,
                snapshot.target_version.clone(),
            )
            .await
        }
        LifecycleAction::MissingTargetBinary => lifecycle_error(
            &app,
            &host.id,
            missing_target_binary_message(&snapshot.target_version),
        ),
        LifecycleAction::UnknownSocket => {
            let message = "remote Tyde socket exists, but it was not launched by Tyde's managed lifecycle; stop it manually or use a manual host configuration".to_string();
            lifecycle_error(&app, &host.id, message)
        }
    }
}

pub async fn force_upgrade_managed_host(
    app: AppHandle,
    host: ConfiguredHost,
) -> Result<RemoteHostLifecycleSnapshot, String> {
    let managed = managed_ssh_host(&host)?;
    let target_version = current_app_release_version()?;

    let mut snapshot = probe_target_snapshot(&app, &host.id, &managed, target_version).await?;
    if matches!(snapshot.running, RemoteTydeRunningState::UnknownSocket) {
        let message = "remote Tyde socket exists, but it was not launched by Tyde's managed lifecycle; stop it manually or use a manual host configuration".to_string();
        return lifecycle_error(&app, &host.id, message);
    }

    if !snapshot.installed_target {
        snapshot =
            ensure_target_installed(&app, &host.id, &managed.ssh_destination, &snapshot).await?;
    }

    match &snapshot.running {
        RemoteTydeRunningState::Managed { .. } => {
            emit_running(
                &app,
                &host.id,
                RemoteHostLifecycleStep::StopOldServer,
                Some(snapshot.target_version.clone()),
            );
            stop_managed_server(&managed.ssh_destination).await?;
            launch_and_verify(
                &app,
                &host.id,
                &managed.ssh_destination,
                snapshot.target_version.clone(),
            )
            .await
        }
        RemoteTydeRunningState::NotRunning => {
            launch_and_verify(
                &app,
                &host.id,
                &managed.ssh_destination,
                snapshot.target_version.clone(),
            )
            .await
        }
        RemoteTydeRunningState::UnknownSocket => {
            let message = "remote Tyde socket exists, but it was not launched by Tyde's managed lifecycle; stop it manually or use a manual host configuration".to_string();
            lifecycle_error(&app, &host.id, message)
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum LifecycleAction {
    ServeAsIs,
    Launch { needs_install: bool },
    Upgrade { needs_install: bool },
    MissingTargetBinary,
    UnknownSocket,
}

fn plan_lifecycle_action(snapshot: &RemoteHostLifecycleSnapshot) -> LifecycleAction {
    match &snapshot.running {
        RemoteTydeRunningState::Managed { version } if version == &snapshot.target_version => {
            if snapshot.installed_target {
                LifecycleAction::ServeAsIs
            } else {
                LifecycleAction::MissingTargetBinary
            }
        }
        RemoteTydeRunningState::Managed { .. } => LifecycleAction::Upgrade {
            needs_install: !snapshot.installed_target,
        },
        RemoteTydeRunningState::NotRunning => LifecycleAction::Launch {
            needs_install: !snapshot.installed_target,
        },
        RemoteTydeRunningState::UnknownSocket => LifecycleAction::UnknownSocket,
    }
}

struct ManagedSshHost {
    ssh_destination: String,
}

fn managed_ssh_host(host: &ConfiguredHost) -> Result<ManagedSshHost, String> {
    match &host.transport {
        HostTransportConfig::SshStdio {
            ssh_destination,
            lifecycle: RemoteHostLifecycleConfig::ManagedTyde,
            remote_command: None,
        } => {
            if ssh_destination.trim_start().starts_with('-') {
                return Err(format!(
                    "ssh destination for host '{}' must not start with '-'",
                    host.id
                ));
            }
            Ok(ManagedSshHost {
                ssh_destination: ssh_destination.clone(),
            })
        }
        HostTransportConfig::SshStdio {
            lifecycle: RemoteHostLifecycleConfig::ManagedTyde,
            remote_command: Some(_),
            ..
        } => Err(format!(
            "configured host '{}' has both managed lifecycle and a remote command override",
            host.id
        )),
        HostTransportConfig::SshStdio { .. } => Err(format!(
            "configured host '{}' uses a manual SSH lifecycle",
            host.id
        )),
        HostTransportConfig::LocalEmbedded => {
            Err("local embedded host has no remote lifecycle".to_string())
        }
    }
}

async fn probe_managed_host(
    app: &AppHandle,
    host_id: &str,
    managed: &ManagedSshHost,
) -> Result<RemoteHostLifecycleSnapshot, String> {
    let target_version = current_app_release_version()?;
    probe_target_snapshot(app, host_id, managed, target_version).await
}

async fn probe_target_snapshot(
    app: &AppHandle,
    host_id: &str,
    managed: &ManagedSshHost,
    target_version: TydeReleaseVersion,
) -> Result<RemoteHostLifecycleSnapshot, String> {
    emit_running(app, host_id, RemoteHostLifecycleStep::ProbePlatform, None);
    let platform = probe_platform(&managed.ssh_destination).await?;

    emit_running(
        app,
        host_id,
        RemoteHostLifecycleStep::ProbeInstallation,
        Some(target_version.clone()),
    );
    let snapshot = probe_snapshot(&managed.ssh_destination, platform, target_version).await?;
    emit_snapshot(app, host_id, snapshot.clone());
    Ok(snapshot)
}

async fn ensure_target_installed(
    app: &AppHandle,
    host_id: &str,
    ssh_destination: &str,
    snapshot: &RemoteHostLifecycleSnapshot,
) -> Result<RemoteHostLifecycleSnapshot, String> {
    if snapshot.installed_target {
        return Ok(snapshot.clone());
    }

    let target_version = snapshot.target_version.clone();
    emit_running(
        app,
        host_id,
        RemoteHostLifecycleStep::ResolveRelease,
        Some(target_version.clone()),
    );
    let asset_name = match release_asset_name(snapshot.platform) {
        Ok(asset_name) => asset_name,
        Err(err) => {
            return lifecycle_error(
                app,
                host_id,
                format!("failed to select Tyde {target_version} release asset: {err}"),
            );
        }
    };
    emit_running(
        app,
        host_id,
        RemoteHostLifecycleStep::DownloadAsset,
        Some(target_version.clone()),
    );
    if let Err(err) = install_release_on_remote(ssh_destination, &target_version, &asset_name).await
    {
        return lifecycle_error(app, host_id, remote_install_error(&target_version, err));
    }
    emit_running(
        app,
        host_id,
        RemoteHostLifecycleStep::InstallBinary,
        Some(target_version.clone()),
    );

    let snapshot =
        match probe_snapshot(ssh_destination, snapshot.platform, target_version.clone()).await {
            Ok(snapshot) => snapshot,
            Err(err) => {
                return lifecycle_error(
                    app,
                    host_id,
                    format!("failed to verify Tyde {target_version} after install: {err}"),
                );
            }
        };
    emit_snapshot(app, host_id, snapshot.clone());
    if !snapshot.installed_target {
        return lifecycle_error(
            app,
            host_id,
            format!(
                "installed Tyde {target_version}, but ~/.tyde/bin/{target_version}/tyde-server is still missing or not executable"
            ),
        );
    }
    Ok(snapshot)
}

fn missing_target_binary_message(target_version: &TydeReleaseVersion) -> String {
    format!(
        "managed Tyde server is already running expected release {target_version}, but ~/.tyde/bin/{target_version}/tyde-server is missing or not executable; refusing compatible connect because the managed SSH bridge must execute the exact target binary"
    )
}

fn remote_install_error(target_version: &TydeReleaseVersion, error: String) -> String {
    format!("failed to download and install Tyde {target_version} on the remote host: {error}")
}

fn lifecycle_error<T>(app: &AppHandle, host_id: &str, message: String) -> Result<T, String> {
    emit_error(app, host_id, message.clone());
    Err(message)
}

async fn launch_and_verify(
    app: &AppHandle,
    host_id: &str,
    ssh_destination: &str,
    version: TydeReleaseVersion,
) -> Result<RemoteHostLifecycleSnapshot, String> {
    emit_running(
        app,
        host_id,
        RemoteHostLifecycleStep::LaunchServer,
        Some(version.clone()),
    );
    launch_server(ssh_destination, &version).await?;

    emit_running(
        app,
        host_id,
        RemoteHostLifecycleStep::VerifyRunning,
        Some(version.clone()),
    );
    let platform = probe_platform(ssh_destination).await?;
    let snapshot = probe_snapshot(ssh_destination, platform, version.clone()).await?;
    match &snapshot.running {
        RemoteTydeRunningState::Managed { version: running } if running == &version => {
            emit_snapshot(app, host_id, snapshot.clone());
            Ok(snapshot)
        }
        other => {
            let message = format!(
                "remote Tyde server did not reach expected running state for {version}; observed {other:?}"
            );
            emit_error(app, host_id, message.clone());
            Err(message)
        }
    }
}

fn release_asset_name(platform: RemotePlatform) -> Result<String, String> {
    match (platform.os, platform.arch) {
        (RemoteOperatingSystem::Linux, RemoteArchitecture::X86_64) => {
            Ok("tyde-server-x86_64-unknown-linux-musl.zip".to_string())
        }
        (RemoteOperatingSystem::Linux, RemoteArchitecture::Aarch64) => {
            Ok("tyde-server-aarch64-unknown-linux-musl.zip".to_string())
        }
        (RemoteOperatingSystem::Macos, RemoteArchitecture::X86_64) => {
            Ok("tyde-server-x86_64-apple-darwin.zip".to_string())
        }
        (RemoteOperatingSystem::Macos, RemoteArchitecture::Aarch64) => {
            Ok("tyde-server-aarch64-apple-darwin.zip".to_string())
        }
    }
}

async fn probe_platform(ssh_destination: &str) -> Result<RemotePlatform, String> {
    let output = ssh_capture(ssh_destination, "uname -s; uname -m").await?;
    let mut lines = output.lines();
    let os_raw = lines
        .next()
        .ok_or_else(|| "remote platform probe did not return an operating system".to_string())?;
    let arch_raw = lines
        .next()
        .ok_or_else(|| "remote platform probe did not return an architecture".to_string())?;
    Ok(RemotePlatform {
        os: parse_remote_os(os_raw)?,
        arch: parse_remote_arch(arch_raw)?,
    })
}

fn parse_remote_os(value: &str) -> Result<RemoteOperatingSystem, String> {
    match value.trim() {
        "Linux" => Ok(RemoteOperatingSystem::Linux),
        "Darwin" => Ok(RemoteOperatingSystem::Macos),
        other => Err(format!(
            "unsupported remote operating system {other:?}; managed Tyde hosts currently require Linux or macOS"
        )),
    }
}

fn parse_remote_arch(value: &str) -> Result<RemoteArchitecture, String> {
    match value.trim() {
        "x86_64" | "amd64" => Ok(RemoteArchitecture::X86_64),
        "aarch64" | "arm64" => Ok(RemoteArchitecture::Aarch64),
        other => Err(format!(
            "unsupported remote architecture {other:?}; managed Tyde hosts currently require x86_64 or aarch64"
        )),
    }
}

async fn probe_snapshot(
    ssh_destination: &str,
    platform: RemotePlatform,
    target_version: TydeReleaseVersion,
) -> Result<RemoteHostLifecycleSnapshot, String> {
    let target_version_sh = shell_quote(target_version.as_str());
    let command = format!(
        r#"set -eu
target_version={target_version_sh}
if [ -x "$HOME/.tyde/bin/$target_version/tyde-server" ]; then
  echo installed_target=1
else
  echo installed_target=0
fi
if [ -L "$HOME/.tyde/bin/current" ]; then
  printf 'current_link_version=%s\n' "$(readlink "$HOME/.tyde/bin/current")"
else
  echo current_link_version=
fi
pid_file="$HOME/.tyde/run/tyde-host.pid"
version_file="$HOME/.tyde/run/tyde-host-version"
socket="$HOME/.tyde/tyde.sock"
if [ -f "$pid_file" ]; then
  pid="$(cat "$pid_file" 2>/dev/null || true)"
  if [ -n "$pid" ] && kill -0 "$pid" 2>/dev/null && [ -S "$socket" ]; then
    echo running=managed
    if [ -f "$version_file" ]; then
      printf 'running_version=%s\n' "$(cat "$version_file" 2>/dev/null || true)"
    else
      echo running_version=
    fi
  else
    echo running=not_running
    echo running_version=
  fi
elif [ -S "$socket" ]; then
  echo running=unknown_socket
  echo running_version=
else
  echo running=not_running
  echo running_version=
fi
"#
    );

    let output = ssh_capture(ssh_destination, &command).await?;
    let parsed = parse_key_value_lines(&output)?;
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

pub(crate) fn shell_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\\''"))
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

async fn install_release_on_remote(
    ssh_destination: &str,
    version: &TydeReleaseVersion,
    asset_name: &str,
) -> Result<(), String> {
    let version_sh = shell_quote(version.as_str());
    let url = format!(
        "{GITHUB_RELEASE_DOWNLOADS}/{}/{}",
        version.github_tag(),
        asset_name
    );
    let url_sh = shell_quote(&url);
    let command = format!(
        r#"set -eu
version={version_sh}
download_url={url_sh}
install_dir="$HOME/.tyde/bin/$version"
mkdir -p "$install_dir" "$HOME/.tyde/logs" "$HOME/.tyde/run"
archive="$install_dir/tyde-server.zip.tmp.$$"
binary="$install_dir/tyde-server.tmp.$$"
trap 'rm -f "$archive" "$binary"' EXIT
if command -v curl >/dev/null 2>&1; then
  curl --fail --location --silent --show-error "$download_url" --output "$archive"
elif command -v wget >/dev/null 2>&1; then
  wget -q "$download_url" -O "$archive"
else
  echo "remote Tyde install requires curl or wget" >&2
  exit 1
fi
if ! command -v unzip >/dev/null 2>&1; then
  echo "remote Tyde install requires unzip" >&2
  exit 1
fi
entry=$(unzip -Z1 "$archive" | awk '$0 == "tyde-server" || $0 ~ /\/tyde-server$/ {{ print; exit }}')
if [ -z "$entry" ]; then
  echo "Tyde release archive did not contain tyde-server" >&2
  exit 1
fi
unzip -p "$archive" "$entry" > "$binary"
if [ ! -s "$binary" ]; then
  echo "downloaded Tyde server binary is empty" >&2
  exit 1
fi
chmod 755 "$binary"
actual_version=$("$binary" --version)
if [ "$actual_version" != "$version" ]; then
  echo "downloaded Tyde server reports version $actual_version, expected $version" >&2
  exit 1
fi
mv "$binary" "$install_dir/tyde-server"
trap - EXIT
rm -f "$archive"
ln -sfn "$version" "$HOME/.tyde/bin/current"
"#
    );
    ssh_capture(ssh_destination, &command).await.map(|_| ())
}

async fn stop_managed_server(ssh_destination: &str) -> Result<(), String> {
    let command = r#"set -eu
pid_file="$HOME/.tyde/run/tyde-host.pid"
version_file="$HOME/.tyde/run/tyde-host-version"
if [ ! -f "$pid_file" ]; then
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
    ssh_capture(ssh_destination, command).await.map(|_| ())
}

async fn launch_server(ssh_destination: &str, version: &TydeReleaseVersion) -> Result<(), String> {
    let version_sh = shell_quote(version.as_str());
    let command = format!(
        r#"set -eu
version={version_sh}
bin="$HOME/.tyde/bin/$version/tyde-server"
socket="$HOME/.tyde/tyde.sock"
pid_file="$HOME/.tyde/run/tyde-host.pid"
version_file="$HOME/.tyde/run/tyde-host-version"
log_file="$HOME/.tyde/logs/tyde-host-$version.log"
mkdir -p "$HOME/.tyde/logs" "$HOME/.tyde/run"
launch_lock="$HOME/.tyde/run/tyde-host-launch.lock"
i=0
while ! mkdir "$launch_lock" 2>/dev/null; do
  if [ -f "$launch_lock/pid" ]; then
    lock_pid=$(cat "$launch_lock/pid" 2>/dev/null || true)
    if [ -n "$lock_pid" ] && ! kill -0 "$lock_pid" 2>/dev/null; then
      rm -rf "$launch_lock"
      continue
    fi
  fi
  if [ "$i" -ge 100 ]; then
    echo "timed out waiting for another managed Tyde launch" >&2
    exit 1
  fi
  i=$((i + 1))
  sleep 0.1
done
printf '%s\n' "$$" > "$launch_lock/pid"
trap 'rm -rf "$launch_lock"' EXIT
if [ ! -x "$bin" ]; then
  echo "managed tyde-server binary is not executable: $bin" >&2
  exit 1
fi
tail_launch_log() {{
  if [ -f "$log_file" ]; then
    echo "last tyde-server launch log lines from $log_file:" >&2
    tail -n 80 "$log_file" >&2 || true
  fi
}}
nohup "$bin" host --uds >> "$log_file" 2>&1 < /dev/null &
pid=$!
i=0
while [ "$i" -lt 50 ]; do
  if kill -0 "$pid" 2>/dev/null && [ -S "$socket" ]; then
    printf '%s\n' "$pid" > "$pid_file"
    printf '%s\n' "$version" > "$version_file"
    ln -sfn "$version" "$HOME/.tyde/bin/current"
    rm -rf "$launch_lock"
    trap - EXIT
    exit 0
  fi
  if ! kill -0 "$pid" 2>/dev/null; then
    echo "managed tyde-server process exited before socket became ready" >&2
    tail_launch_log
    exit 1
  fi
  i=$((i + 1))
  sleep 0.1
done
echo "managed tyde-server did not create $socket" >&2
tail_launch_log
exit 1
"#
    );
    ssh_capture(ssh_destination, &command).await.map(|_| ())
}

async fn ssh_capture(ssh_destination: &str, remote_command: &str) -> Result<String, String> {
    let output = Command::new("ssh")
        .arg("-T")
        .arg(ssh_destination)
        .arg(remote_command)
        .output()
        .await
        .map_err(|err| format!("failed to start ssh command for {ssh_destination}: {err}"))?;
    if !output.status.success() {
        return Err(format!(
            "ssh command failed for {ssh_destination} with status {}: {}",
            output.status,
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }
    String::from_utf8(output.stdout)
        .map_err(|err| format!("ssh command output from {ssh_destination} was not UTF-8: {err}"))
}

fn emit_running(
    app: &AppHandle,
    host_id: &str,
    step: RemoteHostLifecycleStep,
    target_version: Option<TydeReleaseVersion>,
) {
    let _ = app.emit(
        HOST_LIFECYCLE_EVENT,
        HostLifecycleEvent {
            host_id: host_id.to_string(),
            status: RemoteHostLifecycleStatus::Running {
                step,
                target_version,
            },
        },
    );
}

fn emit_snapshot(app: &AppHandle, host_id: &str, snapshot: RemoteHostLifecycleSnapshot) {
    let _ = app.emit(
        HOST_LIFECYCLE_EVENT,
        HostLifecycleEvent {
            host_id: host_id.to_string(),
            status: RemoteHostLifecycleStatus::Snapshot { snapshot },
        },
    );
}

fn emit_error(app: &AppHandle, host_id: &str, message: String) {
    let _ = app.emit(
        HOST_LIFECYCLE_EVENT,
        HostLifecycleEvent {
            host_id: host_id.to_string(),
            status: RemoteHostLifecycleStatus::Error { message },
        },
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    fn release(value: &str) -> TydeReleaseVersion {
        value.parse().unwrap()
    }

    fn platform() -> RemotePlatform {
        RemotePlatform {
            os: RemoteOperatingSystem::Linux,
            arch: RemoteArchitecture::X86_64,
        }
    }

    fn snapshot(
        target_version: TydeReleaseVersion,
        running: RemoteTydeRunningState,
        installed_target: bool,
    ) -> RemoteHostLifecycleSnapshot {
        RemoteHostLifecycleSnapshot {
            target_version,
            installed_target,
            current_link_version: None,
            running,
            platform: platform(),
        }
    }

    #[test]
    fn plans_lifecycle_actions_without_network() {
        let target = release("0.8.0");
        let other = release("0.8.1");
        let cases = vec![
            (
                RemoteTydeRunningState::Managed {
                    version: target.clone(),
                },
                true,
                LifecycleAction::ServeAsIs,
            ),
            (
                RemoteTydeRunningState::Managed {
                    version: target.clone(),
                },
                false,
                LifecycleAction::MissingTargetBinary,
            ),
            (
                RemoteTydeRunningState::Managed {
                    version: other.clone(),
                },
                true,
                LifecycleAction::Upgrade {
                    needs_install: false,
                },
            ),
            (
                RemoteTydeRunningState::Managed {
                    version: other.clone(),
                },
                false,
                LifecycleAction::Upgrade {
                    needs_install: true,
                },
            ),
            (
                RemoteTydeRunningState::NotRunning,
                true,
                LifecycleAction::Launch {
                    needs_install: false,
                },
            ),
            (
                RemoteTydeRunningState::NotRunning,
                false,
                LifecycleAction::Launch {
                    needs_install: true,
                },
            ),
            (
                RemoteTydeRunningState::UnknownSocket,
                true,
                LifecycleAction::UnknownSocket,
            ),
            (
                RemoteTydeRunningState::UnknownSocket,
                false,
                LifecycleAction::UnknownSocket,
            ),
        ];

        for (running, installed_target, expected) in cases {
            let snapshot = snapshot(target.clone(), running, installed_target);
            let action = plan_lifecycle_action(&snapshot);
            assert_eq!(action, expected);
            let should_serve_as_is = matches!(
                &snapshot.running,
                RemoteTydeRunningState::Managed { version } if version == &snapshot.target_version
            ) && snapshot.installed_target;
            assert_eq!(
                matches!(action, LifecycleAction::ServeAsIs),
                should_serve_as_is
            );
            match action {
                LifecycleAction::ServeAsIs => assert!(installed_target),
                LifecycleAction::Launch { needs_install }
                | LifecycleAction::Upgrade { needs_install } => {
                    assert_eq!(needs_install, !installed_target);
                }
                LifecycleAction::MissingTargetBinary | LifecycleAction::UnknownSocket => {}
            }
        }
    }

    #[test]
    fn parses_remote_platform_aliases() {
        assert_eq!(
            parse_remote_os("Linux").unwrap(),
            RemoteOperatingSystem::Linux
        );
        assert_eq!(
            parse_remote_os("Darwin").unwrap(),
            RemoteOperatingSystem::Macos
        );
        assert_eq!(
            parse_remote_arch("x86_64").unwrap(),
            RemoteArchitecture::X86_64
        );
        assert_eq!(
            parse_remote_arch("amd64").unwrap(),
            RemoteArchitecture::X86_64
        );
        assert_eq!(
            parse_remote_arch("aarch64").unwrap(),
            RemoteArchitecture::Aarch64
        );
        assert_eq!(
            parse_remote_arch("arm64").unwrap(),
            RemoteArchitecture::Aarch64
        );
    }

    #[test]
    fn selects_portable_assets() {
        let linux_x64 = RemotePlatform {
            os: RemoteOperatingSystem::Linux,
            arch: RemoteArchitecture::X86_64,
        };
        let mac_arm = RemotePlatform {
            os: RemoteOperatingSystem::Macos,
            arch: RemoteArchitecture::Aarch64,
        };
        assert_eq!(
            release_asset_name(linux_x64).unwrap(),
            "tyde-server-x86_64-unknown-linux-musl.zip"
        );
        assert_eq!(
            release_asset_name(mac_arm).unwrap(),
            "tyde-server-aarch64-apple-darwin.zip"
        );
    }

    #[test]
    fn parses_version_path_last_component() {
        assert_eq!(
            parse_optional_release_version_path("0.8.0")
                .unwrap()
                .unwrap(),
            release("0.8.0")
        );
        assert_eq!(
            parse_optional_release_version_path("/Users/me/.tyde/bin/0.8.0-beta.1")
                .unwrap()
                .unwrap(),
            release("0.8.0-beta.1")
        );
        assert!(parse_optional_release_version_path("").is_none());
    }
}
