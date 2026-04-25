use std::collections::HashMap;
use std::io::{Cursor, Read};
use std::process::Stdio;

use host_config::{
    ConfiguredHost, HostLifecycleEvent, HostTransportConfig, RemoteArchitecture,
    RemoteHostLifecycleConfig, RemoteHostLifecycleSnapshot, RemoteHostLifecycleStatus,
    RemoteHostLifecycleStep, RemoteOperatingSystem, RemotePlatform, RemoteTydeRunningState,
    TydeReleaseTarget,
};
use protocol::Version;
use reqwest::header::USER_AGENT;
use serde::Deserialize;
use tauri::{AppHandle, Emitter};
use tokio::io::AsyncWriteExt;
use tokio::process::Command;

use crate::bridge::HOST_LIFECYCLE_EVENT;

const GITHUB_RELEASES_API: &str = "https://api.github.com/repos/tigy32/Tyde/releases";
const GITHUB_USER_AGENT: &str = "Tyde remote lifecycle";

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

    emit_running(&app, &host.id, RemoteHostLifecycleStep::ProbePlatform, None);
    let platform = probe_platform(&managed.ssh_destination).await?;

    emit_running(
        &app,
        &host.id,
        RemoteHostLifecycleStep::ResolveRelease,
        None,
    );
    let release = resolve_release(&managed.release).await?;

    emit_running(
        &app,
        &host.id,
        RemoteHostLifecycleStep::ProbeInstallation,
        Some(release.version),
    );
    let mut snapshot = probe_snapshot(&managed.ssh_destination, platform, release.version).await?;
    emit_snapshot(&app, &host.id, snapshot.clone());

    if !snapshot.installed_target {
        emit_running(
            &app,
            &host.id,
            RemoteHostLifecycleStep::DownloadAsset,
            Some(snapshot.target_version),
        );
        let asset = select_release_asset(&release, snapshot.platform)?;
        let archive = download_asset(&asset.download_url).await?;
        let binary = extract_tyde_binary(&archive, asset.kind)?;

        emit_running(
            &app,
            &host.id,
            RemoteHostLifecycleStep::InstallBinary,
            Some(snapshot.target_version),
        );
        install_binary(&managed.ssh_destination, snapshot.target_version, &binary).await?;
        snapshot = probe_snapshot(
            &managed.ssh_destination,
            snapshot.platform,
            snapshot.target_version,
        )
        .await?;
        emit_snapshot(&app, &host.id, snapshot.clone());
    }

    match snapshot.running.clone() {
        RemoteTydeRunningState::Managed { version } if version == snapshot.target_version => {
            emit_running(
                &app,
                &host.id,
                RemoteHostLifecycleStep::Connect,
                Some(snapshot.target_version),
            );
            emit_snapshot(&app, &host.id, snapshot.clone());
            Ok(snapshot)
        }
        RemoteTydeRunningState::Managed { .. } => {
            emit_running(
                &app,
                &host.id,
                RemoteHostLifecycleStep::StopOldServer,
                Some(snapshot.target_version),
            );
            stop_managed_server(&managed.ssh_destination).await?;
            launch_and_verify(
                &app,
                &host.id,
                &managed.ssh_destination,
                snapshot.target_version,
            )
            .await
        }
        RemoteTydeRunningState::NotRunning => {
            launch_and_verify(
                &app,
                &host.id,
                &managed.ssh_destination,
                snapshot.target_version,
            )
            .await
        }
        RemoteTydeRunningState::UnknownSocket => {
            let message = "remote Tyde socket exists, but it was not launched by Tyde's managed lifecycle; stop it manually or use a manual host configuration".to_string();
            emit_error(&app, &host.id, message.clone());
            Err(message)
        }
    }
}

struct ManagedSshHost {
    ssh_destination: String,
    release: TydeReleaseTarget,
}

fn managed_ssh_host(host: &ConfiguredHost) -> Result<ManagedSshHost, String> {
    match &host.transport {
        HostTransportConfig::SshStdio {
            ssh_destination,
            lifecycle: RemoteHostLifecycleConfig::ManagedTyde { release },
            remote_command: None,
        } => Ok(ManagedSshHost {
            ssh_destination: ssh_destination.clone(),
            release: release.clone(),
        }),
        HostTransportConfig::SshStdio {
            lifecycle: RemoteHostLifecycleConfig::ManagedTyde { .. },
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
    emit_running(app, host_id, RemoteHostLifecycleStep::ProbePlatform, None);
    let platform = probe_platform(&managed.ssh_destination).await?;

    emit_running(app, host_id, RemoteHostLifecycleStep::ResolveRelease, None);
    let release = resolve_release(&managed.release).await?;

    emit_running(
        app,
        host_id,
        RemoteHostLifecycleStep::ProbeInstallation,
        Some(release.version),
    );
    let snapshot = probe_snapshot(&managed.ssh_destination, platform, release.version).await?;
    emit_snapshot(app, host_id, snapshot.clone());
    Ok(snapshot)
}

async fn launch_and_verify(
    app: &AppHandle,
    host_id: &str,
    ssh_destination: &str,
    version: Version,
) -> Result<RemoteHostLifecycleSnapshot, String> {
    emit_running(
        app,
        host_id,
        RemoteHostLifecycleStep::LaunchServer,
        Some(version),
    );
    launch_server(ssh_destination, version).await?;

    emit_running(
        app,
        host_id,
        RemoteHostLifecycleStep::VerifyRunning,
        Some(version),
    );
    let platform = probe_platform(ssh_destination).await?;
    let snapshot = probe_snapshot(ssh_destination, platform, version).await?;
    match snapshot.running {
        RemoteTydeRunningState::Managed { version: running } if running == version => {
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

#[derive(Debug, Deserialize)]
struct GitHubRelease {
    tag_name: String,
    assets: Vec<GitHubAsset>,
}

#[derive(Debug, Deserialize)]
struct GitHubAsset {
    name: String,
    browser_download_url: String,
}

#[derive(Debug, Clone)]
struct ReleaseInfo {
    version: Version,
    assets: Vec<ReleaseAssetInfo>,
}

#[derive(Debug, Clone)]
struct ReleaseAssetInfo {
    name: String,
    download_url: String,
}

async fn resolve_release(target: &TydeReleaseTarget) -> Result<ReleaseInfo, String> {
    let url = match target {
        TydeReleaseTarget::Latest => format!("{GITHUB_RELEASES_API}/latest"),
        TydeReleaseTarget::Version { version } => {
            format!("{GITHUB_RELEASES_API}/tags/v{version}")
        }
    };

    let release = reqwest::Client::new()
        .get(url)
        .header(USER_AGENT, GITHUB_USER_AGENT)
        .send()
        .await
        .map_err(|err| format!("failed to resolve Tyde release from GitHub: {err}"))?
        .error_for_status()
        .map_err(|err| format!("GitHub release lookup failed: {err}"))?
        .json::<GitHubRelease>()
        .await
        .map_err(|err| format!("failed to parse GitHub release response: {err}"))?;

    let version = release.tag_name.parse::<Version>().map_err(|err| {
        format!(
            "GitHub release tag {:?} is not semver: {err}",
            release.tag_name
        )
    })?;

    Ok(ReleaseInfo {
        version,
        assets: release
            .assets
            .into_iter()
            .map(|asset| ReleaseAssetInfo {
                name: asset.name,
                download_url: asset.browser_download_url,
            })
            .collect(),
    })
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ReleaseAssetKind {
    TarXz,
    Zip,
}

#[derive(Debug, Clone)]
struct SelectedReleaseAsset {
    download_url: String,
    kind: ReleaseAssetKind,
}

fn select_release_asset(
    release: &ReleaseInfo,
    platform: RemotePlatform,
) -> Result<SelectedReleaseAsset, String> {
    let (asset_name, kind) = release_asset_name(platform, release.version)?;
    let asset = release
        .assets
        .iter()
        .find(|asset| asset.name == asset_name)
        .ok_or_else(|| {
            format!(
                "Tyde release v{} does not contain required asset {}",
                release.version, asset_name
            )
        })?;
    Ok(SelectedReleaseAsset {
        download_url: asset.download_url.clone(),
        kind,
    })
}

fn release_asset_name(
    platform: RemotePlatform,
    _version: Version,
) -> Result<(String, ReleaseAssetKind), String> {
    match (platform.os, platform.arch) {
        (RemoteOperatingSystem::Linux, RemoteArchitecture::X86_64) => Ok((
            "tyde-server-x86_64-unknown-linux-gnu.tar.xz".to_string(),
            ReleaseAssetKind::TarXz,
        )),
        (RemoteOperatingSystem::Linux, RemoteArchitecture::Aarch64) => Ok((
            "tyde-server-aarch64-unknown-linux-gnu.tar.xz".to_string(),
            ReleaseAssetKind::TarXz,
        )),
        (RemoteOperatingSystem::Macos, RemoteArchitecture::X86_64) => Ok((
            "tyde-server-x86_64-apple-darwin.zip".to_string(),
            ReleaseAssetKind::Zip,
        )),
        (RemoteOperatingSystem::Macos, RemoteArchitecture::Aarch64) => Ok((
            "tyde-server-aarch64-apple-darwin.zip".to_string(),
            ReleaseAssetKind::Zip,
        )),
    }
}

async fn download_asset(url: &str) -> Result<Vec<u8>, String> {
    let bytes = reqwest::Client::new()
        .get(url)
        .header(USER_AGENT, GITHUB_USER_AGENT)
        .send()
        .await
        .map_err(|err| format!("failed to download Tyde release asset: {err}"))?
        .error_for_status()
        .map_err(|err| format!("Tyde release asset download failed: {err}"))?
        .bytes()
        .await
        .map_err(|err| format!("failed to read Tyde release asset bytes: {err}"))?;
    Ok(bytes.to_vec())
}

fn extract_tyde_binary(archive: &[u8], kind: ReleaseAssetKind) -> Result<Vec<u8>, String> {
    match kind {
        ReleaseAssetKind::TarXz => extract_tyde_from_tar_xz(archive),
        ReleaseAssetKind::Zip => extract_tyde_from_zip(archive),
    }
}

fn extract_tyde_from_tar_xz(archive: &[u8]) -> Result<Vec<u8>, String> {
    let decoder = xz2::read::XzDecoder::new(Cursor::new(archive));
    let mut archive = tar::Archive::new(decoder);
    let entries = archive
        .entries()
        .map_err(|err| format!("failed to read Tyde tar.xz entries: {err}"))?;
    for entry in entries {
        let mut entry = entry.map_err(|err| format!("failed to read Tyde tar entry: {err}"))?;
        let path = entry
            .path()
            .map_err(|err| format!("failed to read Tyde tar entry path: {err}"))?
            .to_path_buf();
        if path.file_name().is_some_and(|name| name == "tyde-server") {
            let mut bytes = Vec::new();
            entry
                .read_to_end(&mut bytes)
                .map_err(|err| format!("failed to extract tyde-server from tar.xz: {err}"))?;
            return Ok(bytes);
        }
    }
    Err("Tyde tar.xz asset did not contain a tyde-server binary".to_string())
}

fn extract_tyde_from_zip(archive: &[u8]) -> Result<Vec<u8>, String> {
    let cursor = Cursor::new(archive);
    let mut archive = zip::ZipArchive::new(cursor)
        .map_err(|err| format!("failed to read Tyde zip asset: {err}"))?;
    for index in 0..archive.len() {
        let mut file = archive
            .by_index(index)
            .map_err(|err| format!("failed to read Tyde zip entry {index}: {err}"))?;
        let name = file.name().to_string();
        if name == "tyde-server" || name.ends_with("/tyde-server") {
            let mut bytes = Vec::new();
            file.read_to_end(&mut bytes)
                .map_err(|err| format!("failed to extract tyde-server from zip: {err}"))?;
            return Ok(bytes);
        }
    }
    Err("Tyde zip asset did not contain a tyde-server binary".to_string())
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
    target_version: Version,
) -> Result<RemoteHostLifecycleSnapshot, String> {
    let command = format!(
        r#"set -eu
target_version={target_version}
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
        parse_optional_version_path_result(parsed.get("current_link_version"))?;
    let running = match parsed.get("running").map(String::as_str) {
        Some("not_running") => RemoteTydeRunningState::NotRunning,
        Some("unknown_socket") => RemoteTydeRunningState::UnknownSocket,
        Some("managed") => {
            let version = parse_optional_version_path_result(parsed.get("running_version"))?
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

fn parse_optional_version_path(value: &str) -> Option<Result<Version, String>> {
    let trimmed = value.trim();
    if trimmed.is_empty() {
        return None;
    }
    let last_component = trimmed.rsplit('/').next().unwrap_or(trimmed);
    Some(last_component.parse::<Version>())
}

fn parse_optional_version_path_result(value: Option<&String>) -> Result<Option<Version>, String> {
    match value {
        Some(value) => parse_optional_version_path(value).transpose(),
        None => Ok(None),
    }
}

async fn install_binary(
    ssh_destination: &str,
    version: Version,
    binary: &[u8],
) -> Result<(), String> {
    let command = format!(
        r#"set -eu
version={version}
install_dir="$HOME/.tyde/bin/$version"
mkdir -p "$install_dir" "$HOME/.tyde/logs" "$HOME/.tyde/run"
tmp="$install_dir/tyde-server.tmp.$$"
cat > "$tmp"
chmod 755 "$tmp"
mv "$tmp" "$install_dir/tyde-server"
ln -sfn "$version" "$HOME/.tyde/bin/current"
"#
    );
    ssh_with_stdin(ssh_destination, &command, binary).await
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

async fn launch_server(ssh_destination: &str, version: Version) -> Result<(), String> {
    let command = format!(
        r#"set -eu
version={version}
bin="$HOME/.tyde/bin/$version/tyde-server"
socket="$HOME/.tyde/tyde.sock"
pid_file="$HOME/.tyde/run/tyde-host.pid"
version_file="$HOME/.tyde/run/tyde-host-version"
log_file="$HOME/.tyde/logs/tyde-host-$version.log"
mkdir -p "$HOME/.tyde/logs" "$HOME/.tyde/run"
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
printf '%s\n' "$pid" > "$pid_file"
printf '%s\n' "$version" > "$version_file"
i=0
while [ "$i" -lt 50 ]; do
  if kill -0 "$pid" 2>/dev/null && [ -S "$socket" ]; then
    ln -sfn "$version" "$HOME/.tyde/bin/current"
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

async fn ssh_with_stdin(
    ssh_destination: &str,
    remote_command: &str,
    stdin_bytes: &[u8],
) -> Result<(), String> {
    let mut child = Command::new("ssh")
        .arg("-T")
        .arg(ssh_destination)
        .arg(remote_command)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|err| format!("failed to start ssh command for {ssh_destination}: {err}"))?;

    let mut stdin = child
        .stdin
        .take()
        .ok_or_else(|| format!("ssh command for {ssh_destination} has no stdin"))?;
    stdin
        .write_all(stdin_bytes)
        .await
        .map_err(|err| format!("failed to write tyde-server binary to ssh stdin: {err}"))?;
    stdin
        .shutdown()
        .await
        .map_err(|err| format!("failed to close ssh stdin for tyde-server upload: {err}"))?;
    drop(stdin);

    let output = child
        .wait_with_output()
        .await
        .map_err(|err| format!("failed waiting for ssh command for {ssh_destination}: {err}"))?;
    if !output.status.success() {
        return Err(format!(
            "ssh upload command failed for {ssh_destination} with status {}: {}",
            output.status,
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }
    Ok(())
}

fn emit_running(
    app: &AppHandle,
    host_id: &str,
    step: RemoteHostLifecycleStep,
    target_version: Option<Version>,
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

    fn v(major: u32, minor: u32, patch: u32) -> Version {
        Version {
            major,
            minor,
            patch,
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
    fn selects_versioned_portable_assets() {
        let version = v(0, 8, 0);
        let linux_x64 = RemotePlatform {
            os: RemoteOperatingSystem::Linux,
            arch: RemoteArchitecture::X86_64,
        };
        let mac_arm = RemotePlatform {
            os: RemoteOperatingSystem::Macos,
            arch: RemoteArchitecture::Aarch64,
        };
        assert_eq!(
            release_asset_name(linux_x64, version).unwrap(),
            (
                "tyde-server-x86_64-unknown-linux-gnu.tar.xz".to_string(),
                ReleaseAssetKind::TarXz
            )
        );
        assert_eq!(
            release_asset_name(mac_arm, version).unwrap(),
            (
                "tyde-server-aarch64-apple-darwin.zip".to_string(),
                ReleaseAssetKind::Zip
            )
        );
    }

    #[test]
    fn parses_version_path_last_component() {
        assert_eq!(
            parse_optional_version_path("0.8.0").unwrap().unwrap(),
            v(0, 8, 0)
        );
        assert_eq!(
            parse_optional_version_path("/Users/me/.tyde/bin/0.8.0")
                .unwrap()
                .unwrap(),
            v(0, 8, 0)
        );
        assert!(parse_optional_version_path("").is_none());
    }
}
