use std::collections::HashMap;
use std::io::{Cursor, Read};
use std::process::Stdio;

use host_config::{
    ConfiguredHost, HostLifecycleEvent, HostTransportConfig, RemoteArchitecture,
    RemoteHostLifecycleConfig, RemoteHostLifecycleSnapshot, RemoteHostLifecycleStatus,
    RemoteHostLifecycleStep, RemoteOperatingSystem, RemotePlatform, RemoteTydeRunningState,
    TydeReleaseVersion,
};
use reqwest::header::USER_AGENT;
use serde::Deserialize;
use tauri::{AppHandle, Emitter};
use tokio::io::AsyncWriteExt;
use tokio::process::Command;

use crate::bridge::HOST_LIFECYCLE_EVENT;

const GITHUB_RELEASES_API: &str = "https://api.github.com/repos/tigy32/Tyde/releases";
const GITHUB_USER_AGENT: &str = "Tyde remote lifecycle";

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
    let release = match resolve_release(target_version.clone()).await {
        Ok(release) => release,
        Err(err) => {
            return lifecycle_error(app, host_id, github_install_error(&target_version, err));
        }
    };

    emit_running(
        app,
        host_id,
        RemoteHostLifecycleStep::DownloadAsset,
        Some(target_version.clone()),
    );
    let asset = match select_release_asset(&release, snapshot.platform) {
        Ok(asset) => asset,
        Err(err) => {
            return lifecycle_error(
                app,
                host_id,
                format!("failed to select Tyde {target_version} release asset: {err}"),
            );
        }
    };
    let archive = match download_asset(&asset.download_url).await {
        Ok(archive) => archive,
        Err(err) => {
            return lifecycle_error(app, host_id, github_install_error(&target_version, err));
        }
    };
    let binary = match extract_tyde_binary(&archive, asset.kind) {
        Ok(binary) => binary,
        Err(err) => {
            return lifecycle_error(
                app,
                host_id,
                format!("failed to extract Tyde {target_version} server binary: {err}"),
            );
        }
    };

    emit_running(
        app,
        host_id,
        RemoteHostLifecycleStep::InstallBinary,
        Some(target_version.clone()),
    );
    if let Err(err) = install_binary(ssh_destination, &target_version, &binary).await {
        return lifecycle_error(
            app,
            host_id,
            format!("failed to install Tyde {target_version} on the remote host: {err}"),
        );
    }

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

fn github_install_error(target_version: &TydeReleaseVersion, error: String) -> String {
    format!("installing Tyde {target_version} requires GitHub, which is unavailable: {error}")
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
    version: TydeReleaseVersion,
    assets: Vec<ReleaseAssetInfo>,
}

#[derive(Debug, Clone)]
struct ReleaseAssetInfo {
    name: String,
    download_url: String,
}

async fn resolve_release(expected: TydeReleaseVersion) -> Result<ReleaseInfo, String> {
    let url = format!("{GITHUB_RELEASES_API}/tags/{}", expected.github_tag());

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

    let version = release
        .tag_name
        .parse::<TydeReleaseVersion>()
        .map_err(|err| {
            format!(
                "GitHub release tag {:?} is not a valid Tyde release version: {err}",
                release.tag_name
            )
        })?;
    if version != expected {
        return Err(format!(
            "GitHub release tag {} resolved to {}, expected {}",
            expected.github_tag(),
            version.github_tag(),
            expected.github_tag()
        ));
    }

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
    let (asset_name, kind) = release_asset_name(platform)?;
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

fn release_asset_name(platform: RemotePlatform) -> Result<(String, ReleaseAssetKind), String> {
    match (platform.os, platform.arch) {
        (RemoteOperatingSystem::Linux, RemoteArchitecture::X86_64) => Ok((
            "tyde-server-x86_64-unknown-linux-musl.zip".to_string(),
            ReleaseAssetKind::Zip,
        )),
        (RemoteOperatingSystem::Linux, RemoteArchitecture::Aarch64) => Ok((
            "tyde-server-aarch64-unknown-linux-musl.zip".to_string(),
            ReleaseAssetKind::Zip,
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
        ReleaseAssetKind::Zip => extract_tyde_from_zip(archive),
    }
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

/// Number of times to retry the binary upload before giving up. Streaming a
/// large binary into `ssh ... 'cat > file'` over stdin can intermittently
/// truncate, so each attempt verifies size + sha256 on the remote and we retry
/// the whole upload on any mismatch.
const INSTALL_UPLOAD_ATTEMPTS: usize = 3;

async fn install_binary(
    ssh_destination: &str,
    version: &TydeReleaseVersion,
    binary: &[u8],
) -> Result<(), String> {
    let version_sh = shell_quote(version.as_str());
    let expected_len = binary.len();
    let expected_sha = sha256_hex(binary);
    // The remote script writes stdin to a temp file, then refuses to install it
    // unless both the byte count and (when a sha256 tool is available) the
    // digest match what we sent. `cat` happily returns success on a truncated
    // stream, so without this check a partial transfer silently installs a
    // corrupt binary. The EXIT trap removes the temp file on any failure.
    let command = format!(
        r#"set -eu
version={version_sh}
expected_len={expected_len}
expected_sha={expected_sha}
install_dir="$HOME/.tyde/bin/$version"
mkdir -p "$install_dir" "$HOME/.tyde/logs" "$HOME/.tyde/run"
tmp="$install_dir/tyde-server.tmp.$$"
trap 'rm -f "$tmp"' EXIT
cat > "$tmp"
actual_len=$(wc -c < "$tmp" | tr -d ' ')
if [ "$actual_len" != "$expected_len" ]; then
  echo "tyde-server upload truncated: expected $expected_len bytes, received $actual_len" >&2
  exit 1
fi
if command -v sha256sum >/dev/null 2>&1; then
  actual_sha=$(sha256sum "$tmp" | awk '{{print $1}}')
elif command -v shasum >/dev/null 2>&1; then
  actual_sha=$(shasum -a 256 "$tmp" | awk '{{print $1}}')
else
  actual_sha=""
fi
if [ -n "$actual_sha" ] && [ "$actual_sha" != "$expected_sha" ]; then
  echo "tyde-server upload corrupted: sha256 mismatch (expected $expected_sha, received $actual_sha)" >&2
  exit 1
fi
chmod 755 "$tmp"
mv "$tmp" "$install_dir/tyde-server"
trap - EXIT
ln -sfn "$version" "$HOME/.tyde/bin/current"
"#
    );

    let mut last_err = String::new();
    for attempt in 1..=INSTALL_UPLOAD_ATTEMPTS {
        match ssh_with_stdin(ssh_destination, &command, binary).await {
            Ok(()) => return Ok(()),
            Err(err) => last_err = format!("attempt {attempt}/{INSTALL_UPLOAD_ATTEMPTS}: {err}"),
        }
    }
    Err(format!(
        "failed to upload a complete tyde-server binary after {INSTALL_UPLOAD_ATTEMPTS} attempts ({last_err})"
    ))
}

fn sha256_hex(bytes: &[u8]) -> String {
    use sha2::{Digest, Sha256};
    let digest = Sha256::digest(bytes);
    let mut out = String::with_capacity(digest.len() * 2);
    for byte in digest {
        out.push_str(&format!("{byte:02x}"));
    }
    out
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
# Remove any stale socket left by a previous server. Otherwise the readiness
# check below ([ -S "$socket" ]) can pass against the old socket and report a
# successful launch even when the new process crashed immediately, masking the
# real failure as a later "NotRunning" observation. Safe here: this path is
# only reached when no managed server is running (NotRunning, or after the old
# one was stopped).
rm -f "$socket"
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
            (
                "tyde-server-x86_64-unknown-linux-musl.zip".to_string(),
                ReleaseAssetKind::Zip
            )
        );
        assert_eq!(
            release_asset_name(mac_arm).unwrap(),
            (
                "tyde-server-aarch64-apple-darwin.zip".to_string(),
                ReleaseAssetKind::Zip
            )
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
