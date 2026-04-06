use std::cmp::Ordering as CmpOrdering;
use std::path::PathBuf;
use std::process::Stdio;
#[cfg(unix)]
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use serde::Serialize;
use tauri::Emitter;
use tokio::io::AsyncReadExt;
use tokio::process::Command;

#[derive(Serialize, Clone)]
pub struct RemoteConnectionProgress {
    pub host: String,
    pub step: String,
    pub status: String,
    pub message: String,
}

#[derive(Debug, Clone)]
pub struct RemotePath {
    pub host: String,
    pub path: String,
}

pub fn parse_remote_path(raw: &str) -> Option<RemotePath> {
    let trimmed = raw.trim();
    if !trimmed.starts_with("ssh://") {
        return None;
    }

    let rest = &trimmed["ssh://".len()..];
    let slash_idx = rest.find('/')?;
    let host = rest[..slash_idx].trim();
    let path_part = rest[slash_idx..].trim();

    if host.is_empty() || path_part.is_empty() {
        return None;
    }

    Some(RemotePath {
        host: host.to_string(),
        path: normalize_remote_path(path_part),
    })
}

pub fn to_remote_uri(host: &str, path: &str) -> String {
    format!("ssh://{}{}", host, normalize_remote_path(path))
}

fn normalize_remote_path(path: &str) -> String {
    let mut normalized = if path.starts_with('/') {
        path.to_string()
    } else {
        format!("/{}", path)
    };

    while normalized.len() > 1 && normalized.ends_with('/') {
        normalized.pop();
    }

    normalized
}

pub fn parse_remote_workspace_roots(
    roots: &[String],
) -> Result<Option<(String, Vec<String>)>, String> {
    let parsed: Vec<RemotePath> = roots
        .iter()
        .filter_map(|root| parse_remote_path(root))
        .collect();

    if parsed.is_empty() {
        return Ok(None);
    }

    if parsed.len() != roots.len() {
        return Err(
            "Cannot mix local and remote workspace roots in a single conversation".to_string(),
        );
    }

    let host = parsed[0].host.clone();
    if parsed.iter().any(|p| p.host != host) {
        return Err(
            "All remote workspace roots for a conversation must use the same SSH host".to_string(),
        );
    }

    let remote_paths = parsed.into_iter().map(|p| p.path).collect();
    Ok(Some((host, remote_paths)))
}

/// Single-quotes a string for safe passage through a POSIX shell.
/// Embedded single quotes are escaped as `'\''`.
pub fn shell_quote_arg(arg: &str) -> String {
    let escaped = arg.replace("'", "'\\''");
    format!("'{}'", escaped)
}

/// Joins multiple arguments into a single shell-safe command string.
/// Each argument is single-quoted to prevent metacharacter interpretation.
pub fn shell_quote_command(args: &[String]) -> String {
    args.iter()
        .map(|a| shell_quote_arg(a))
        .collect::<Vec<_>>()
        .join(" ")
}

pub(crate) const SUBPROCESS_VERSION: &str = env!("SUBPROCESS_VERSION");
pub(crate) const SUBPROCESS_GIT_REPO: &str = "https://github.com/tigy32/Tycode";
pub(crate) const SUBPROCESS_CRATE_NAME: &str = "tycode-subprocess";
pub(crate) const TYDE_GIT_REPO: &str = "https://github.com/tigy32/Tyde";
pub(crate) const TYDE_BINARY_NAME: &str = "tyde";
pub(crate) const TYDE_REMOTE_SOCKET_SUFFIX: &str = ".tyde/tyde.sock";
pub(crate) const TYDE_REMOTE_INSTALLS_SUFFIX: &str = ".tyde/installs";
pub(crate) const TYDE_REMOTE_HEADLESS_PID_SUFFIX: &str = ".tyde/run/tyde-headless.pid";
pub(crate) const TYDE_REMOTE_HEADLESS_LOG_SUFFIX: &str = ".tyde/logs/tyde-headless.log";
#[cfg(unix)]
static NEXT_TUNNEL_ID: AtomicU64 = AtomicU64::new(1);

#[cfg(unix)]
pub(crate) type IpcStream = tokio::net::UnixStream;
#[cfg(not(unix))]
pub(crate) type IpcStream = tokio::net::TcpStream;

fn ssh_control_socket_dir() -> Result<PathBuf, String> {
    let home = std::env::var("HOME")
        .or_else(|_| std::env::var("USERPROFILE"))
        .map_err(|_| "Could not determine home directory for SSH control socket".to_string())?;
    Ok(PathBuf::from(home).join(".tycode").join("ssh"))
}

/// Returns SSH args that enable connection multiplexing via ControlMaster.
/// First connection to a host performs the full handshake and becomes the master.
/// Subsequent connections reuse the existing TCP channel via a Unix domain socket,
/// reducing per-command overhead from seconds to ~50-100ms.
pub fn ssh_control_args() -> Result<Vec<String>, String> {
    let dir = ssh_control_socket_dir()?;

    std::fs::create_dir_all(&dir)
        .map_err(|e| format!("Cannot create SSH control socket dir: {e}"))?;

    let socket_path = dir.join("%C");

    Ok(vec![
        "-o".to_string(),
        "ControlMaster=auto".to_string(),
        "-o".to_string(),
        format!("ControlPath={}", socket_path.display()),
        "-o".to_string(),
        "ControlPersist=600".to_string(),
        // Without keepalives the master can't detect a dead TCP connection
        // to the remote. It sits for hours refusing new sessions, forcing
        // every SSH call to fall back to a direct connection.
        "-o".to_string(),
        "ServerAliveInterval=60".to_string(),
        "-o".to_string(),
        "ServerAliveCountMax=3".to_string(),
    ])
}

/// SSH args that explicitly disable ControlMaster multiplexing while keeping
/// TCP keepalives. Useful for long-lived remote subprocesses where stale
/// control sockets can break session opens.
fn ssh_no_multiplex_args() -> Vec<String> {
    vec![
        "-S".to_string(),
        "none".to_string(),
        "-o".to_string(),
        "ControlMaster=no".to_string(),
        "-o".to_string(),
        "ServerAliveInterval=60".to_string(),
        "-o".to_string(),
        "ServerAliveCountMax=3".to_string(),
    ]
}

fn looks_like_mux_failure(stderr: &str) -> bool {
    stderr.contains("mux_client_request_session: session request failed")
        || stderr.contains("ControlSocket")
            && stderr.contains("already exists")
            && stderr.contains("disabling multiplexing")
        || stderr.contains("Master refused session request")
}

async fn run_ssh_raw_without_multiplex(
    host: &str,
    raw_cmd: &str,
) -> Result<std::process::Output, String> {
    let mut cmd = Command::new("ssh");
    for arg in ssh_no_multiplex_args() {
        cmd.arg(arg);
    }
    cmd.arg("-T")
        .arg(host)
        .arg(raw_cmd)
        .output()
        .await
        .map_err(|e| format!("Failed to run ssh command: {e}"))
}

pub async fn resolve_remote_shell_path(host: &str) -> Result<String, String> {
    let output = run_ssh_raw(host, "$SHELL -li -c 'echo TYDE_PATH_MARKER=$PATH'").await?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(format!("Failed to resolve remote shell PATH: {stderr}"));
    }
    let stdout = String::from_utf8_lossy(&output.stdout);
    // Login shells print banners before and cleanup commands (e.g. `clear`)
    // after our echo. Use a unique prefix to reliably extract the PATH.
    stdout
        .lines()
        .find_map(|line| line.trim().strip_prefix("TYDE_PATH_MARKER="))
        .map(|path| path.trim().to_string())
        .filter(|path| !path.is_empty())
        .ok_or_else(|| "Remote shell PATH resolution returned empty".to_string())
}

/// Runs a raw command string over SSH, allowing shell variable expansion.
/// The command string is passed directly to the remote shell without quoting.
/// Only use with commands constructed entirely from trusted strings.
pub async fn run_ssh_raw(host: &str, raw_cmd: &str) -> Result<std::process::Output, String> {
    let mut cmd = Command::new("ssh");
    for arg in ssh_control_args()? {
        cmd.arg(arg);
    }
    let output = cmd
        .arg("-T")
        .arg(host)
        .arg(raw_cmd)
        .output()
        .await
        .map_err(|e| format!("Failed to run ssh command: {e}"))?;

    if output.status.success() {
        return Ok(output);
    }

    let stderr = String::from_utf8_lossy(&output.stderr);
    if looks_like_mux_failure(&stderr) {
        tracing::warn!(
            "SSH multiplexing failed for host '{}', retrying without ControlMaster: {}",
            host,
            stderr.trim()
        );
        return run_ssh_raw_without_multiplex(host, raw_cmd).await;
    }

    Ok(output)
}

pub(crate) async fn resolve_remote_home_dir(host: &str) -> Result<String, String> {
    let output = run_ssh_raw(host, "printf '%s' \"$HOME\"").await?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(format!("Failed to resolve remote home directory: {stderr}"));
    }
    let home = String::from_utf8_lossy(&output.stdout).trim().to_string();
    if home.is_empty() {
        return Err("Remote home directory resolved to empty path".to_string());
    }
    Ok(home)
}

pub(crate) fn tyde_socket_path_from_home(home: &str) -> String {
    let home = home.trim_end_matches('/');
    if home.is_empty() {
        format!("/{}", TYDE_REMOTE_SOCKET_SUFFIX)
    } else {
        format!("{home}/{}", TYDE_REMOTE_SOCKET_SUFFIX)
    }
}

pub(crate) fn tyde_install_path_from_home(home: &str, version: &str, target: &str) -> String {
    let home = home.trim_end_matches('/');
    if home.is_empty() {
        format!(
            "/{}/v{version}/{target}/{TYDE_BINARY_NAME}",
            TYDE_REMOTE_INSTALLS_SUFFIX
        )
    } else {
        format!(
            "{home}/{}/v{version}/{target}/{TYDE_BINARY_NAME}",
            TYDE_REMOTE_INSTALLS_SUFFIX
        )
    }
}

fn parse_numeric_version_parts(version: &str) -> Option<Vec<u64>> {
    let mut parts = Vec::new();
    for part in version.split('.') {
        if part.is_empty() {
            return None;
        }
        parts.push(part.parse::<u64>().ok()?);
    }
    if parts.is_empty() {
        return None;
    }
    Some(parts)
}

pub(crate) fn compare_numeric_versions(left: &str, right: &str) -> Option<CmpOrdering> {
    let mut left_parts = parse_numeric_version_parts(left)?;
    let mut right_parts = parse_numeric_version_parts(right)?;
    let width = left_parts.len().max(right_parts.len());
    left_parts.resize(width, 0);
    right_parts.resize(width, 0);
    for (left_part, right_part) in left_parts.iter().zip(right_parts.iter()) {
        let cmp = left_part.cmp(right_part);
        if cmp != CmpOrdering::Equal {
            return Some(cmp);
        }
    }
    Some(CmpOrdering::Equal)
}

fn is_safe_release_component(value: &str) -> bool {
    !value.is_empty()
        && value
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '.' || c == '-' || c == '_')
}

pub(crate) async fn resolve_remote_tyde_install_path(
    host: &str,
    version: &str,
    target: &str,
) -> Result<String, String> {
    let home = resolve_remote_home_dir(host).await?;
    Ok(tyde_install_path_from_home(&home, version, target))
}

pub(crate) async fn is_remote_tyde_server_running(
    host: &str,
    remote_socket_path: &str,
) -> Result<bool, String> {
    let socket_check = format!("test -S {}", shell_quote_arg(remote_socket_path));
    let socket_output = run_ssh_raw(host, &socket_check).await?;
    Ok(socket_output.status.success())
}

/// Detects the remote host's target triple for Tyde server binary downloads.
pub(crate) async fn detect_remote_tyde_target(host: &str) -> Result<String, String> {
    let output = run_ssh_raw(host, "uname -s && uname -m").await?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(format!("Failed to detect remote platform: {stderr}"));
    }

    let stdout = String::from_utf8_lossy(&output.stdout);
    let lines: Vec<&str> = stdout.trim().lines().collect();
    if lines.len() < 2 {
        return Err(format!("Unexpected uname output: {stdout}"));
    }

    let os = lines[0].trim();
    let arch = lines[1].trim();

    match (os, arch) {
        ("Linux", "x86_64") => Ok("x86_64-unknown-linux-gnu".to_string()),
        ("Linux", "aarch64") => Ok("aarch64-unknown-linux-gnu".to_string()),
        ("Darwin", "x86_64") => Ok("x86_64-apple-darwin".to_string()),
        ("Darwin", "arm64") => Ok("aarch64-apple-darwin".to_string()),
        _ => Err(format!("Unsupported remote platform: {os} {arch}")),
    }
}

pub(crate) async fn list_remote_tyde_installed_versions(host: &str) -> Result<Vec<String>, String> {
    let cmd = format!(
        "if [ -d \"$HOME/{installs}\" ]; then \
            ls -1 \"$HOME/{installs}\" | sed -n 's/^v//p'; \
         fi",
        installs = TYDE_REMOTE_INSTALLS_SUFFIX,
    );
    let output = run_ssh_raw(host, &cmd).await?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(format!("Failed to list remote Tyde installs: {stderr}"));
    }
    let mut versions: Vec<String> = String::from_utf8_lossy(&output.stdout)
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .map(str::to_string)
        .collect();
    versions.sort_by(|left, right| match compare_numeric_versions(left, right) {
        Some(cmp) => cmp.reverse(),
        None => right.cmp(left),
    });
    versions.dedup();
    Ok(versions)
}

pub(crate) async fn check_remote_tyde_install(
    host: &str,
    version: &str,
    target: &str,
) -> Result<Option<String>, String> {
    let path = resolve_remote_tyde_install_path(host, version, target).await?;
    let cmd = format!(
        "test -x {path} && printf '%s' {path}",
        path = shell_quote_arg(&path),
    );
    let output = run_ssh_raw(host, &cmd).await?;
    if !output.status.success() {
        return Ok(None);
    }
    let resolved = String::from_utf8_lossy(&output.stdout).trim().to_string();
    if resolved.is_empty() {
        return Ok(None);
    }
    Ok(Some(resolved))
}

/// Resolve the `tyde` executable from the remote login shell PATH.
pub(crate) async fn resolve_remote_tyde_from_path(host: &str) -> Result<Option<String>, String> {
    let remote_path = resolve_remote_shell_path(host).await?;
    let cmd = format!(
        "PATH={} command -v {}",
        shell_quote_arg(&remote_path),
        shell_quote_arg(TYDE_BINARY_NAME)
    );
    let output = run_ssh_raw(host, &cmd).await?;
    if !output.status.success() {
        return Ok(None);
    }
    let path = String::from_utf8_lossy(&output.stdout).trim().to_string();
    if path.is_empty() {
        return Ok(None);
    }
    Ok(Some(path))
}

/// Downloads a pre-built Tyde binary from GitHub releases.
/// Returns the resolved absolute binary path on success.
pub(crate) async fn install_remote_tyde_binary(
    host: &str,
    version: &str,
) -> Result<String, String> {
    let target = detect_remote_tyde_target(host).await?;
    if !is_safe_release_component(version) {
        return Err(format!(
            "Invalid Tyde version for remote install: '{version}'"
        ));
    }
    if !is_safe_release_component(&target) {
        return Err(format!(
            "Invalid remote target for install command: '{target}'"
        ));
    }
    let archive = format!("{TYDE_BINARY_NAME}-{target}.tar.xz");
    let url = format!(
        "{repo}/releases/download/v{version}/{archive}",
        repo = TYDE_GIT_REPO
    );
    let quoted_url = shell_quote_arg(&url);
    let quoted_version = shell_quote_arg(version);
    let quoted_target = shell_quote_arg(&target);

    let cmd = format!(
        "VERSION={version} && \
         TARGET={target} && \
         TMP=$(mktemp -d) && \
         curl -sSfL {url} | tar -xJ -C \"$TMP\" && \
         BIN=$(find \"$TMP\" -type f -name \"{bin}\" | head -n 1) && \
         test -n \"$BIN\" && \
         INSTALL_DIR=\"$HOME/{installs}/v$VERSION/$TARGET\" && \
         mkdir -p \"$INSTALL_DIR\" && \
         mv \"$BIN\" \"$INSTALL_DIR/{bin}\" && \
         chmod +x \"$INSTALL_DIR/{bin}\" && \
         rm -rf \"$TMP\" && \
         printf '%s' \"$INSTALL_DIR/{bin}\"",
        version = quoted_version,
        target = quoted_target,
        url = quoted_url,
        bin = TYDE_BINARY_NAME,
        installs = TYDE_REMOTE_INSTALLS_SUFFIX,
    );
    let output = run_ssh_raw(host, &cmd).await?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(format!(
            "Failed to install Tyde v{version} ({target}) on '{host}': {stderr}"
        ));
    }
    let path = String::from_utf8_lossy(&output.stdout).trim().to_string();
    if path.is_empty() {
        return Err(format!(
            "Tyde v{version} install completed but no binary path was returned"
        ));
    }
    Ok(path)
}

/// Starts Tyde in headless mode on the remote host and waits for the socket.
pub(crate) async fn launch_remote_tyde_headless(
    host: &str,
    binary_path: &str,
) -> Result<(), String> {
    let cmd = format!(
        "BIN={bin} && \
         PIDFILE=\"$HOME/{pid}\" && \
         LOGFILE=\"$HOME/{log}\" && \
         SOCKET=\"$HOME/{socket}\" && \
         mkdir -p \"$(dirname \"$PIDFILE\")\" \"$(dirname \"$LOGFILE\")\" && \
         if [ -f \"$PIDFILE\" ]; then \
           PID=$(cat \"$PIDFILE\" 2>/dev/null || true); \
           if [ -n \"$PID\" ] && kill -0 \"$PID\" 2>/dev/null; then \
             if [ -S \"$SOCKET\" ]; then \
               printf '%s' \"already-running\"; \
               exit 0; \
             fi; \
             kill \"$PID\" 2>/dev/null || true; \
             i=0; \
             while [ \"$i\" -lt 50 ]; do \
               if ! kill -0 \"$PID\" 2>/dev/null; then break; fi; \
               sleep 0.1; \
               i=$((i+1)); \
             done; \
             if kill -0 \"$PID\" 2>/dev/null; then \
               kill -9 \"$PID\" 2>/dev/null || true; \
             fi; \
           fi; \
           rm -f \"$PIDFILE\"; \
         fi && \
         nohup \"$BIN\" --headless >>\"$LOGFILE\" 2>&1 < /dev/null & \
         PID=$! && \
         printf '%s' \"$PID\" > \"$PIDFILE\" && \
         i=0; \
         while [ \"$i\" -lt 120 ]; do \
           if [ -S \"$SOCKET\" ]; then \
             printf '%s' \"started\"; \
             exit 0; \
           fi; \
           if ! kill -0 \"$PID\" 2>/dev/null; then \
             printf '%s' \"Tyde headless process exited early\"; \
             exit 1; \
           fi; \
           sleep 0.1; \
           i=$((i+1)); \
         done; \
         printf '%s' \"Timed out waiting for Tyde socket\"; \
         exit 1",
        bin = shell_quote_arg(binary_path),
        pid = TYDE_REMOTE_HEADLESS_PID_SUFFIX,
        log = TYDE_REMOTE_HEADLESS_LOG_SUFFIX,
        socket = TYDE_REMOTE_SOCKET_SUFFIX,
    );
    let output = run_ssh_raw(host, &cmd).await?;
    if output.status.success() {
        return Ok(());
    }
    let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
    let stdout = String::from_utf8_lossy(&output.stdout).trim().to_string();
    let detail = if !stderr.is_empty() {
        stderr
    } else if !stdout.is_empty() {
        stdout
    } else {
        "unknown error".to_string()
    };
    Err(format!(
        "Failed to launch Tyde headless on '{host}': {detail}"
    ))
}

/// Stops Tyde headless on the remote host.
pub(crate) async fn stop_remote_tyde_headless(host: &str) -> Result<(), String> {
    let cmd = format!(
        "PIDFILE=\"$HOME/{pid}\" && \
         SOCKET=\"$HOME/{socket}\" && \
         if [ -f \"$PIDFILE\" ]; then \
           PID=$(cat \"$PIDFILE\" 2>/dev/null || true); \
           if [ -n \"$PID\" ] && kill -0 \"$PID\" 2>/dev/null; then \
             kill \"$PID\" 2>/dev/null || true; \
             i=0; \
             while [ \"$i\" -lt 50 ]; do \
               if ! kill -0 \"$PID\" 2>/dev/null; then break; fi; \
               sleep 0.1; \
               i=$((i+1)); \
             done; \
             if kill -0 \"$PID\" 2>/dev/null; then \
               kill -9 \"$PID\" 2>/dev/null || true; \
             fi; \
           fi; \
           rm -f \"$PIDFILE\"; \
         fi && \
         if [ -S \"$SOCKET\" ]; then rm -f \"$SOCKET\" 2>/dev/null || true; fi && \
         printf '%s' \"stopped\"",
        pid = TYDE_REMOTE_HEADLESS_PID_SUFFIX,
        socket = TYDE_REMOTE_SOCKET_SUFFIX,
    );
    let output = run_ssh_raw(host, &cmd).await?;
    if output.status.success() {
        return Ok(());
    }
    let stderr = String::from_utf8_lossy(&output.stderr);
    Err(format!("Failed to stop remote Tyde headless: {stderr}"))
}

/// Queries the running remote Tyde server version using `tyde connect`.
pub(crate) async fn query_remote_tyde_server_version(
    host: &str,
    connect_binary: &str,
    client_tyde_version: &str,
) -> Result<String, String> {
    let handshake = serde_json::json!({
        "type": "Handshake",
        "req_id": 0,
        "protocol_version": crate::protocol::PROTOCOL_VERSION,
        "tyde_version": client_tyde_version,
        "last_agent_event_seq": 0,
        "last_chat_event_seqs": {},
    })
    .to_string();
    let server_status = serde_json::json!({
        "type": "Invoke",
        "req_id": 1,
        "command": "server_status",
        "params": {},
    })
    .to_string();
    let cmd = format!(
        "printf '%s\\n%s\\n' {handshake} {status} | {binary} connect",
        handshake = shell_quote_arg(&handshake),
        status = shell_quote_arg(&server_status),
        binary = shell_quote_arg(connect_binary),
    );
    let output = tokio::time::timeout(Duration::from_secs(15), run_ssh_raw(host, &cmd))
        .await
        .map_err(|_| format!("Timed out querying remote server version on '{host}'"))??;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(format!("Failed to query remote server version: {stderr}"));
    }

    let stdout = String::from_utf8_lossy(&output.stdout);
    for line in stdout.lines() {
        let parsed: serde_json::Value = match serde_json::from_str(line.trim()) {
            Ok(v) => v,
            Err(_) => continue,
        };
        let req_id = parsed.get("req_id").and_then(|v| v.as_u64());
        let frame_type = parsed.get("type").and_then(|v| v.as_str()).unwrap_or("");

        if frame_type == "Error" && req_id == Some(1) {
            let error = parsed
                .get("error")
                .and_then(|v| v.as_str())
                .unwrap_or("unknown error");
            return Err(format!("Remote server status query failed: {error}"));
        }
        if frame_type == "Result" && req_id == Some(1) {
            let version = parsed
                .get("data")
                .and_then(|data| data.get("tyde_version"))
                .and_then(|v| v.as_str())
                .ok_or("Remote server_status response missing tyde_version")?;
            return Ok(version.to_string());
        }
    }

    Err("Did not receive server_status response from remote Tyde server".to_string())
}

#[cfg(unix)]
fn make_local_tunnel_socket_path(host: &str) -> PathBuf {
    let sanitized_host: String = host
        .chars()
        .filter(|c| c.is_ascii_alphanumeric())
        .take(24)
        .collect();
    let id = NEXT_TUNNEL_ID.fetch_add(1, Ordering::Relaxed);
    std::env::temp_dir().join(format!(
        "tyde-tunnel-{}-{}-{}.sock",
        std::process::id(),
        if sanitized_host.is_empty() {
            "host"
        } else {
            &sanitized_host
        },
        id
    ))
}

#[cfg(unix)]
async fn read_child_stderr(child: &mut tokio::process::Child) -> String {
    let mut stderr = String::new();
    if let Some(mut reader) = child.stderr.take() {
        let mut buf = Vec::new();
        if reader.read_to_end(&mut buf).await.is_ok() {
            stderr = String::from_utf8_lossy(&buf).trim().to_string();
        }
    }
    stderr
}

#[cfg(not(unix))]
pub(crate) async fn open_ssh_unix_socket_tunnel(
    _host: &str,
    _remote_socket_path: &str,
) -> Result<(tokio::process::Child, PathBuf, IpcStream), String> {
    Err(
        "SSH Unix socket tunnels require Unix domain sockets (not available on this platform)"
            .into(),
    )
}

#[cfg(unix)]
pub(crate) async fn open_ssh_unix_socket_tunnel(
    host: &str,
    remote_socket_path: &str,
) -> Result<(tokio::process::Child, PathBuf, IpcStream), String> {
    let local_socket_path = make_local_tunnel_socket_path(host);
    let _ = std::fs::remove_file(&local_socket_path);

    let forward_arg = format!("{}:{}", local_socket_path.display(), remote_socket_path);
    let mut cmd = Command::new("ssh");
    for arg in ssh_no_multiplex_args() {
        cmd.arg(arg);
    }
    let mut child = cmd
        .arg("-T")
        .arg("-N")
        .arg("-o")
        .arg("ConnectTimeout=10")
        .arg("-o")
        .arg("ExitOnForwardFailure=yes")
        .arg("-L")
        .arg(forward_arg)
        .arg(host)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| format!("Failed to start SSH socket tunnel: {e}"))?;

    for _ in 0..120 {
        if let Ok(stream) = tokio::net::UnixStream::connect(&local_socket_path).await {
            return Ok((child, local_socket_path, stream));
        }

        if let Some(status) = child
            .try_wait()
            .map_err(|e| format!("Failed to check SSH tunnel status: {e}"))?
        {
            let stderr = read_child_stderr(&mut child).await;
            let _ = std::fs::remove_file(&local_socket_path);
            let msg = if stderr.is_empty() {
                format!("SSH socket tunnel exited early with status {status}")
            } else {
                format!("SSH socket tunnel failed: {stderr}")
            };
            return Err(msg);
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }

    let _ = child.start_kill();
    let _ = std::fs::remove_file(&local_socket_path);
    Err(format!(
        "Timed out waiting for SSH socket tunnel to become ready at {}",
        local_socket_path.display()
    ))
}

/// Checks whether the versioned subprocess binary exists on the remote host.
/// Returns the resolved absolute path if it exists, None otherwise.
async fn check_remote_subprocess(host: &str) -> Result<Option<String>, String> {
    let cmd = format!(
        "test -x \"$HOME/.tycode/v{v}/bin/{crate_name}\" && echo \"$HOME/.tycode/v{v}/bin/{crate_name}\"",
        v = SUBPROCESS_VERSION,
        crate_name = SUBPROCESS_CRATE_NAME,
    );
    let output = run_ssh_raw(host, &cmd).await?;
    if output.status.success() {
        let path = String::from_utf8_lossy(&output.stdout).trim().to_string();
        if path.is_empty() {
            return Ok(None);
        }
        return Ok(Some(path));
    }
    Ok(None)
}

/// Detects the remote host's target triple for binary downloads.
async fn detect_remote_target(host: &str) -> Result<String, String> {
    let output = run_ssh_raw(host, "uname -s && uname -m").await?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(format!("Failed to detect remote platform: {stderr}"));
    }

    let stdout = String::from_utf8_lossy(&output.stdout);
    let lines: Vec<&str> = stdout.trim().lines().collect();
    if lines.len() < 2 {
        return Err(format!("Unexpected uname output: {stdout}"));
    }

    let os = lines[0].trim();
    let arch = lines[1].trim();

    match (os, arch) {
        ("Linux", "x86_64") => Ok("x86_64-unknown-linux-musl".to_string()),
        ("Linux", "aarch64") => Ok("aarch64-unknown-linux-musl".to_string()),
        ("Darwin", "x86_64") => Ok("x86_64-apple-darwin".to_string()),
        ("Darwin", "arm64") => Ok("aarch64-apple-darwin".to_string()),
        _ => Err(format!("Unsupported remote platform: {os} {arch}")),
    }
}

/// Downloads a pre-built tycode-subprocess binary from GitHub releases.
/// Returns the resolved absolute binary path on success.
async fn install_remote_subprocess(host: &str) -> Result<String, String> {
    let target = detect_remote_target(host).await?;
    let archive = format!(
        "{crate_name}-{target}.tar.xz",
        crate_name = SUBPROCESS_CRATE_NAME,
    );
    let url = format!(
        "{repo}/releases/download/v{v}/{archive}",
        repo = SUBPROCESS_GIT_REPO,
        v = SUBPROCESS_VERSION,
    );

    let cmd = format!(
        "TMP=$(mktemp -d) && \
         curl -sSfL \"{url}\" | tar -xJ -C \"$TMP\" && \
         mkdir -p \"$HOME/.tycode/v{v}/bin\" && \
         mv \"$TMP\"/*/{crate_name} \"$HOME/.tycode/v{v}/bin/{crate_name}\" && \
         chmod +x \"$HOME/.tycode/v{v}/bin/{crate_name}\" && \
         rm -rf \"$TMP\" && \
         echo \"$HOME/.tycode/v{v}/bin/{crate_name}\"",
        v = SUBPROCESS_VERSION,
        crate_name = SUBPROCESS_CRATE_NAME,
    );
    let output = run_ssh_raw(host, &cmd).await?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(format!(
            "Failed to download {SUBPROCESS_CRATE_NAME} v{SUBPROCESS_VERSION} \
             ({target}) on remote host '{host}':\n{stderr}"
        ));
    }

    let path = String::from_utf8_lossy(&output.stdout).trim().to_string();

    if path.is_empty() {
        return Err("Failed to resolve remote subprocess binary path".to_string());
    }
    Ok(path)
}

/// Performs a multi-step remote connection with granular progress events emitted
/// via Tauri so the frontend can display a connection dialog.
pub async fn connect_remote_with_progress(
    app: &tauri::AppHandle,
    host: &str,
) -> Result<String, String> {
    let emit_progress = |step: &str, status: &str, message: &str| {
        let payload = RemoteConnectionProgress {
            host: host.to_string(),
            step: step.to_string(),
            status: status.to_string(),
            message: message.to_string(),
        };
        let _ = app.emit("remote-connection-progress", payload.clone());
        crate::record_debug_event_from_app(
            app,
            "remote_connection_progress",
            serde_json::json!({
                "host": payload.host,
                "step": payload.step,
                "status": payload.status,
                "message": payload.message,
            }),
        );
    };

    emit_progress(
        "validating_connection",
        "in_progress",
        "Testing SSH connection...",
    );
    let mut ssh_cmd = Command::new("ssh");
    for arg in ssh_control_args()? {
        ssh_cmd.arg(arg);
    }
    let ssh_output = ssh_cmd
        .arg("-T")
        .arg("-o")
        .arg("ConnectTimeout=10")
        .arg(host)
        .arg("echo tycode-ok")
        .output()
        .await
        .map_err(|e| format!("Failed to run ssh: {e}"))?;

    if !ssh_output.status.success() {
        let stderr = String::from_utf8_lossy(&ssh_output.stderr);
        let msg = format!("SSH connection failed: {stderr}");
        emit_progress("validating_connection", "failed", &msg);
        return Err(msg);
    }
    emit_progress(
        "validating_connection",
        "completed",
        "SSH connection established",
    );

    emit_progress(
        "checking_environment",
        "in_progress",
        "Checking remote environment...",
    );
    if let Some(path) = check_remote_subprocess(host).await? {
        emit_progress(
            "checking_environment",
            "completed",
            "tycode-subprocess found",
        );
        emit_progress("installing_subprocess", "skipped", "Already installed");
        emit_progress("ready", "completed", &format!("Connected to {host}"));
        return Ok(path);
    }
    emit_progress(
        "checking_environment",
        "completed",
        "tycode-subprocess not installed",
    );

    emit_progress(
        "installing_subprocess",
        "in_progress",
        "Downloading tycode-subprocess...",
    );
    let path = match install_remote_subprocess(host).await {
        Ok(p) => {
            emit_progress(
                "installing_subprocess",
                "completed",
                "tycode-subprocess installed",
            );
            p
        }
        Err(e) => {
            emit_progress("installing_subprocess", "failed", &e);
            return Err(e);
        }
    };

    emit_progress("ready", "completed", &format!("Connected to {host}"));
    Ok(path)
}

/// Validates SSH connectivity and checks that a CLI binary (e.g. "claude" or
/// "codex") is available in PATH on the remote host.  Emits the same
/// `remote-connection-progress` events as `connect_remote_with_progress` so
/// the frontend connection dialog works unchanged.
pub async fn validate_remote_cli(
    app: &tauri::AppHandle,
    host: &str,
    cli_name: &str,
) -> Result<(), String> {
    let emit_progress = |step: &str, status: &str, message: &str| {
        let payload = RemoteConnectionProgress {
            host: host.to_string(),
            step: step.to_string(),
            status: status.to_string(),
            message: message.to_string(),
        };
        let _ = app.emit("remote-connection-progress", payload.clone());
        crate::record_debug_event_from_app(
            app,
            "remote_connection_progress",
            serde_json::json!({
                "host": payload.host,
                "step": payload.step,
                "status": payload.status,
                "message": payload.message,
            }),
        );
    };

    emit_progress(
        "validating_connection",
        "in_progress",
        "Testing SSH connection...",
    );
    let mut ssh_cmd = Command::new("ssh");
    for arg in ssh_control_args()? {
        ssh_cmd.arg(arg);
    }
    let ssh_output = ssh_cmd
        .arg("-T")
        .arg("-o")
        .arg("ConnectTimeout=10")
        .arg(host)
        .arg("echo tycode-ok")
        .output()
        .await
        .map_err(|e| format!("Failed to run ssh: {e}"))?;

    if !ssh_output.status.success() {
        let stderr = String::from_utf8_lossy(&ssh_output.stderr);
        let msg = format!("SSH connection failed: {stderr}");
        emit_progress("validating_connection", "failed", &msg);
        return Err(msg);
    }
    emit_progress(
        "validating_connection",
        "completed",
        "SSH connection established",
    );

    emit_progress(
        "checking_environment",
        "in_progress",
        &format!("Checking for {cli_name} CLI on remote host..."),
    );
    // Bare SSH sessions use a minimal PATH that excludes ~/.local/bin,
    // ~/.toolbox/bin, etc. Resolve the user's login shell PATH first.
    let remote_path = resolve_remote_shell_path(host).await.map_err(|err| {
        let msg = format!("Failed to resolve remote PATH: {err}");
        emit_progress("checking_environment", "failed", &msg);
        msg
    })?;
    let check_cmd = format!(
        "PATH={} command -v {}",
        shell_quote_arg(&remote_path),
        shell_quote_arg(cli_name)
    );
    let check_output = run_ssh_raw(host, &check_cmd).await?;
    if !check_output.status.success() {
        let msg = format!("{cli_name} CLI not found on remote host '{host}'");
        emit_progress("checking_environment", "failed", &msg);
        return Err(msg);
    }
    emit_progress(
        "checking_environment",
        "completed",
        &format!("{cli_name} CLI found"),
    );

    emit_progress("ready", "completed", &format!("Connected to {host}"));
    Ok(())
}

/// Multi-step connection to a remote Tyde server. Validates SSH, resolves the
/// remote socket path, and verifies the remote control socket exists.
/// Does not install or launch Tyde.
pub async fn connect_tyde_server_with_progress(
    app: &tauri::AppHandle,
    host: &str,
) -> Result<String, String> {
    let emit_progress = |step: &str, status: &str, message: &str| {
        let payload = RemoteConnectionProgress {
            host: host.to_string(),
            step: step.to_string(),
            status: status.to_string(),
            message: message.to_string(),
        };
        let _ = app.emit("remote-connection-progress", payload.clone());
        crate::record_debug_event_from_app(
            app,
            "remote_connection_progress",
            serde_json::json!({
                "host": payload.host,
                "step": payload.step,
                "status": payload.status,
                "message": payload.message,
            }),
        );
    };

    // Step 1: Validate SSH
    emit_progress(
        "validating_connection",
        "in_progress",
        "Testing SSH connection...",
    );
    let mut ssh_cmd = Command::new("ssh");
    for arg in ssh_control_args()? {
        ssh_cmd.arg(arg);
    }
    let ssh_output = ssh_cmd
        .arg("-T")
        .arg("-o")
        .arg("ConnectTimeout=10")
        .arg(host)
        .arg("echo tycode-ok")
        .output()
        .await
        .map_err(|e| format!("Failed to run ssh: {e}"))?;

    if !ssh_output.status.success() {
        let stderr = String::from_utf8_lossy(&ssh_output.stderr);
        let msg = format!("SSH connection failed: {stderr}");
        emit_progress("validating_connection", "failed", &msg);
        return Err(msg);
    }
    emit_progress(
        "validating_connection",
        "completed",
        "SSH connection established",
    );

    // Step 2: Resolve remote home and socket path.
    emit_progress(
        "checking_tyde",
        "in_progress",
        "Resolving Tyde socket path...",
    );
    let remote_home = resolve_remote_home_dir(host).await.map_err(|err| {
        let msg = format!("Failed to resolve remote home path: {err}");
        emit_progress("checking_tyde", "failed", &msg);
        msg
    })?;
    let remote_socket_path = tyde_socket_path_from_home(&remote_home);
    emit_progress(
        "checking_tyde",
        "completed",
        &format!("Using socket path {remote_socket_path}"),
    );

    // Step 3: Verify remote control server is running.
    emit_progress(
        "checking_server",
        "in_progress",
        "Checking if Tyde server is running...",
    );
    if !is_remote_tyde_server_running(host, &remote_socket_path).await? {
        let msg = format!(
            "Tyde server is not running on '{host}' (missing {remote_socket_path}). \
Open Settings → Remote Server → Install & Manage to install/launch it, or \
start Tyde on the remote host with '--headless' and retry."
        );
        emit_progress("checking_server", "failed", &msg);
        return Err(msg);
    }
    emit_progress("checking_server", "completed", "Tyde server is running");

    // Step 4: Ready
    emit_progress("ready", "completed", &format!("Connected to {host}"));
    Ok(remote_socket_path)
}

pub async fn run_ssh_command(host: &str, args: &[String]) -> Result<std::process::Output, String> {
    let remote_cmd = shell_quote_command(args);
    let mut cmd = Command::new("ssh");
    for arg in ssh_control_args()? {
        cmd.arg(arg);
    }
    cmd.arg("-T")
        .arg(host)
        .arg(remote_cmd)
        .output()
        .await
        .map_err(|e| format!("Failed to run ssh command: {e}"))
}

/// Spawn a command either locally or through SSH, with stdin/stdout/stderr
/// piped in both modes.
pub async fn spawn_local_or_remote_process(
    ssh_host: Option<&str>,
    program: &str,
    args: &[String],
    cwd: Option<&str>,
) -> Result<tokio::process::Child, String> {
    if let Some(host) = ssh_host {
        return spawn_remote_process(host, program, args, cwd).await;
    }

    let mut cmd = Command::new(program);
    for arg in args {
        cmd.arg(arg);
    }
    if let Some(dir) = cwd.filter(|d| !d.trim().is_empty()) {
        cmd.current_dir(dir);
    }
    cmd.stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    cmd.spawn()
        .map_err(|err| format!("Failed to spawn local process '{program}': {err}"))
}

/// Find `count` available TCP ports on a remote host by binding to :0.
/// Returns the port numbers. Requires python3 on the remote.
pub(crate) async fn find_remote_free_ports(host: &str, count: usize) -> Result<Vec<u16>, String> {
    let script = format!(
        "python3 -c '\
import socket\n\
ports = []\n\
for _ in range({count}):\n\
    s = socket.socket()\n\
    s.bind((\"127.0.0.1\", 0))\n\
    ports.append(s.getsockname()[1])\n\
    s.close()\n\
print(\" \".join(str(p) for p in ports))\n\
'"
    );
    let output = run_ssh_raw(host, &script).await?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(format!(
            "Failed to find free ports on remote host: {stderr}"
        ));
    }
    let stdout = String::from_utf8_lossy(&output.stdout);
    stdout
        .split_whitespace()
        .map(|s| {
            s.parse::<u16>()
                .map_err(|e| format!("Invalid port number '{s}': {e}"))
        })
        .collect()
}

/// Spawns a long-lived process on a remote host via SSH with the user's
/// full login shell PATH. Backends describe WHAT to run — this handles HOW.
pub async fn spawn_remote_process(
    host: &str,
    program: &str,
    args: &[String],
    cwd: Option<&str>,
) -> Result<tokio::process::Child, String> {
    let remote_path = resolve_remote_shell_path(host).await?;
    let cwd_prefix = match cwd {
        Some(dir) if !dir.trim().is_empty() => format!("cd {} && ", shell_quote_arg(dir)),
        _ => String::new(),
    };
    let mut cmd_parts = vec![program.to_string()];
    cmd_parts.extend(args.iter().cloned());
    let remote_exec = format!(
        "{}PATH={} {}",
        cwd_prefix,
        shell_quote_arg(&remote_path),
        shell_quote_command(&cmd_parts),
    );
    let mut cmd = Command::new("ssh");
    for arg in ssh_no_multiplex_args() {
        cmd.arg(arg);
    }
    cmd.arg("-T")
        .arg(host)
        .arg(&remote_exec)
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped());
    cmd.spawn()
        .map_err(|err| format!("Failed to spawn remote process '{program}' on '{host}': {err}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::Command as StdCommand;

    fn run_through_shell(shell: &str, cmd: &str) -> std::process::Output {
        StdCommand::new(shell)
            .arg("-c")
            .arg(cmd)
            .output()
            .unwrap_or_else(|e| panic!("failed to spawn {shell}: {e}"))
    }

    fn available_shells() -> Vec<&'static str> {
        let mut v = vec!["sh"];
        if StdCommand::new("zsh")
            .arg("--version")
            .output()
            .map(|o| o.status.success())
            .unwrap_or(false)
        {
            v.push("zsh");
        }
        v
    }

    /// Round-trips args through shell quoting + real shell interpretation.
    /// Uses null-delimited printf to recover exact arg values.
    fn verify_args_roundtrip(shell: &str, args: &[String]) {
        let quoted_args: Vec<String> = args.iter().map(|a| shell_quote_arg(a)).collect();
        let cmd = format!("printf '%s\\0' {}", quoted_args.join(" "));

        let output = run_through_shell(shell, &cmd);
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(output.status.success(), "{shell} failed: {stderr}");

        let stdout = String::from_utf8(output.stdout.clone()).expect("non-utf8 output");
        let recovered: Vec<&str> = stdout.split('\0').filter(|s| !s.is_empty()).collect();

        assert_eq!(
            recovered.len(),
            args.len(),
            "{shell}: expected {} args, got {}: {recovered:?}",
            args.len(),
            recovered.len()
        );
        for (i, (expected, actual)) in args.iter().zip(recovered.iter()).enumerate() {
            assert_eq!(expected.as_str(), *actual, "{shell}: arg {i} mismatch");
        }
    }

    // -- Unit tests for shell_quote_arg --

    #[test]
    fn quote_simple_string() {
        assert_eq!(shell_quote_arg("hello"), "'hello'");
    }

    #[test]
    fn quote_string_with_spaces() {
        assert_eq!(shell_quote_arg("hello world"), "'hello world'");
    }

    #[test]
    fn quote_string_with_single_quotes() {
        assert_eq!(shell_quote_arg("it's"), "'it'\\''s'");
    }

    #[test]
    fn quote_string_with_braces() {
        assert_eq!(shell_quote_arg("{a,b}"), "'{a,b}'");
    }

    #[test]
    fn quote_string_with_dollars() {
        assert_eq!(shell_quote_arg("$HOME"), "'$HOME'");
    }

    #[test]
    fn quote_empty_string() {
        assert_eq!(shell_quote_arg(""), "''");
    }

    #[test]
    fn compare_numeric_versions_handles_semver_ordering() {
        assert_eq!(
            compare_numeric_versions("0.10.0", "0.9.0"),
            Some(std::cmp::Ordering::Greater)
        );
        assert_eq!(
            compare_numeric_versions("1.2", "1.2.0"),
            Some(std::cmp::Ordering::Equal)
        );
        assert_eq!(
            compare_numeric_versions("1.2.3", "1.2.3.1"),
            Some(std::cmp::Ordering::Less)
        );
    }

    #[test]
    fn compare_numeric_versions_rejects_non_numeric() {
        assert_eq!(compare_numeric_versions("1.2.x", "1.2.3"), None);
    }

    #[test]
    fn safe_release_component_validation() {
        assert!(is_safe_release_component("0.7.0"));
        assert!(is_safe_release_component("x86_64-unknown-linux-gnu"));
        assert!(!is_safe_release_component("1.0$(rm -rf ~)"));
        assert!(!is_safe_release_component(""));
    }

    // -- Round-trip tests through real shells --

    #[test]
    fn roundtrip_basic_args() {
        let args: Vec<String> = vec!["echo", "hello", "world"]
            .into_iter()
            .map(String::from)
            .collect();

        for shell in available_shells() {
            verify_args_roundtrip(shell, &args);
        }
    }

    #[test]
    fn roundtrip_args_with_braces() {
        let args: Vec<String> = vec![
            "echo",
            "{\"name\": \"test\"}",
            "[1, 2, 3]",
            "a{b,c}d",
            "{",
            "}",
        ]
        .into_iter()
        .map(String::from)
        .collect();

        for shell in available_shells() {
            verify_args_roundtrip(shell, &args);
        }
    }

    #[test]
    fn roundtrip_args_with_shell_metacharacters() {
        let args: Vec<String> = vec![
            "cmd",
            "$HOME",
            "$(whoami)",
            "`id`",
            "a && b",
            "x || y",
            "foo | bar",
            "test > /dev/null",
            "hello; rm -rf /",
            "back\\slash",
            "double\"quote",
            "single'quote",
            "tab\there",
            "newline\nhere",
            "glob*pattern",
            "question?mark",
            "hash#comment",
            "exclaim!bang",
            "tilde~home",
        ]
        .into_iter()
        .map(String::from)
        .collect();

        for shell in available_shells() {
            verify_args_roundtrip(shell, &args);
        }
    }

    #[test]
    fn roundtrip_json_workspace_roots() {
        let args: Vec<String> = vec![
            "tycode-subprocess",
            "--workspace-roots",
            r#"["/home/user/project","/tmp/other dir"]"#,
        ]
        .into_iter()
        .map(String::from)
        .collect();

        for shell in available_shells() {
            verify_args_roundtrip(shell, &args);
        }
    }

    #[test]
    fn roundtrip_paths_with_special_characters() {
        let args: Vec<String> = vec![
            "cmd",
            "/home/user/my project",
            "/home/user/it's a path",
            "/home/user/path with \"quotes\"",
            "/home/user/$HOME",
            "/home/user/file{1,2}.txt",
        ]
        .into_iter()
        .map(String::from)
        .collect();

        for shell in available_shells() {
            verify_args_roundtrip(shell, &args);
        }
    }

    // -- E2E: file_service Python scripts through real shells --

    #[test]
    fn file_service_list_dir_script_survives_shells() {
        let script = r#"
import json, os, sys
root = sys.argv[1]
show_hidden = sys.argv[2] == "1"
skip = {"node_modules", "target", ".git"}
entries = []
with os.scandir(root) as it:
    for entry in it:
        name = entry.name
        if not show_hidden and (name.startswith('.') or name in skip):
            continue
        try:
            st = entry.stat(follow_symlinks=False)
            is_dir = entry.is_dir(follow_symlinks=False)
        except OSError:
            continue
        entries.append({
            "name": name,
            "path": os.path.join(root, name),
            "is_directory": is_dir,
            "size": None if is_dir else st.st_size
        })
entries.sort(key=lambda e: (0 if e["is_directory"] else 1, e["name"].lower()))
print(json.dumps(entries))
"#;

        let tmp = std::env::temp_dir().join("tycode_test_list_dir");
        let _ = std::fs::create_dir_all(&tmp);
        let _ = std::fs::write(tmp.join("test.txt"), "hello");
        let _ = std::fs::create_dir_all(tmp.join("subdir"));

        let tmp_path = tmp.to_string_lossy().to_string();

        let args: Vec<String> = vec![
            "python3".to_string(),
            "-c".to_string(),
            script.to_string(),
            tmp_path.clone(),
            "0".to_string(),
        ];

        let quoted = shell_quote_command(&args);

        for shell in available_shells() {
            let output = run_through_shell(shell, &quoted);
            let stderr = String::from_utf8_lossy(&output.stderr);
            assert!(
                output.status.success(),
                "{shell} failed running list_dir script:\nstderr: {stderr}\ncmd: {quoted}"
            );

            let stdout = String::from_utf8_lossy(&output.stdout);
            let parsed: serde_json::Value =
                serde_json::from_str(stdout.trim()).unwrap_or_else(|e| {
                    panic!("{shell}: invalid JSON output: {e}\nstdout: {stdout}\nstderr: {stderr}")
                });
            assert!(
                parsed.is_array(),
                "{shell}: expected JSON array, got: {parsed}"
            );

            let entries = parsed.as_array().unwrap();
            let names: Vec<&str> = entries
                .iter()
                .filter_map(|e| e.get("name").and_then(|n| n.as_str()))
                .collect();
            assert!(
                names.contains(&"test.txt"),
                "{shell}: expected test.txt in output, got: {names:?}"
            );
            assert!(
                names.contains(&"subdir"),
                "{shell}: expected subdir in output, got: {names:?}"
            );
        }

        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn file_service_read_file_script_survives_shells() {
        let script = r#"
import json, os, sys
path = sys.argv[1]
max_bytes = 1024 * 1024

size = os.path.getsize(path)
with open(path, "rb") as f:
    data = f.read(max_bytes + 1)

if b"\x00" in data[:512]:
    print(json.dumps({
        "content": "Binary file",
        "size": size,
        "truncated": False
    }))
else:
    truncated = len(data) > max_bytes or size > max_bytes
    usable = data[:max_bytes]
    print(json.dumps({
        "content": usable.decode("utf-8", errors="replace"),
        "size": size,
        "truncated": truncated
    }))
"#;

        let tmp = std::env::temp_dir().join("tycode_test_read_file");
        let _ = std::fs::create_dir_all(&tmp);
        let test_file = tmp.join("hello.txt");
        std::fs::write(&test_file, "hello world").unwrap();

        let file_path = test_file.to_string_lossy().to_string();

        let args: Vec<String> = vec![
            "python3".to_string(),
            "-c".to_string(),
            script.to_string(),
            file_path,
        ];

        let quoted = shell_quote_command(&args);

        for shell in available_shells() {
            let output = run_through_shell(shell, &quoted);
            let stderr = String::from_utf8_lossy(&output.stderr);
            assert!(
                output.status.success(),
                "{shell} failed running read_file script:\nstderr: {stderr}\ncmd: {quoted}"
            );

            let stdout = String::from_utf8_lossy(&output.stdout);
            let parsed: serde_json::Value =
                serde_json::from_str(stdout.trim()).unwrap_or_else(|e| {
                    panic!("{shell}: invalid JSON output: {e}\nstdout: {stdout}\nstderr: {stderr}")
                });

            let content = parsed.get("content").and_then(|v| v.as_str()).unwrap();
            assert_eq!(content, "hello world", "{shell}: content mismatch");

            let size = parsed.get("size").and_then(|v| v.as_u64()).unwrap();
            assert_eq!(size, 11, "{shell}: size mismatch");
        }

        let _ = std::fs::remove_dir_all(&tmp);
    }

    // -- E2E: git command patterns through real shells --

    #[test]
    fn git_status_command_survives_shells() {
        let args: Vec<String> = vec![
            "git",
            "-C",
            "/home/user/project",
            "status",
            "--porcelain=v1",
            "--untracked-files=all",
        ]
        .into_iter()
        .map(String::from)
        .collect();

        let quoted = shell_quote_command(&args);
        for shell in available_shells() {
            // Just verify parsing — git will fail because path doesn't exist,
            // but the shell must not choke on the command syntax itself
            let output = run_through_shell(shell, &quoted);
            let stderr = String::from_utf8_lossy(&output.stderr);
            // The error should be from git (not finding the repo), not from shell parsing
            assert!(
                !stderr.contains("parse error"),
                "{shell}: shell parse error in git status command: {stderr}"
            );
        }
    }

    #[test]
    fn git_diff_command_survives_shells() {
        let args: Vec<String> = vec![
            "git",
            "-C",
            "/tmp/test repo",
            "diff",
            "--no-index",
            "--",
            "/dev/null",
            "file with spaces.txt",
        ]
        .into_iter()
        .map(String::from)
        .collect();

        let quoted = shell_quote_command(&args);
        for shell in available_shells() {
            let output = run_through_shell(shell, &quoted);
            let stderr = String::from_utf8_lossy(&output.stderr);
            assert!(
                !stderr.contains("parse error"),
                "{shell}: shell parse error in git diff command: {stderr}"
            );
        }
    }

    #[test]
    fn git_commit_message_with_special_chars_survives_shells() {
        let args: Vec<String> = vec![
            "git",
            "-C",
            "/tmp/project",
            "commit",
            "-m",
            "fix: handle {braces} and $variables in paths",
        ]
        .into_iter()
        .map(String::from)
        .collect();

        let quoted = shell_quote_command(&args);
        for shell in available_shells() {
            let output = run_through_shell(shell, &quoted);
            let stderr = String::from_utf8_lossy(&output.stderr);
            assert!(
                !stderr.contains("parse error"),
                "{shell}: shell parse error in git commit command: {stderr}"
            );
        }
    }

    #[test]
    fn git_ls_files_command_survives_shells() {
        let args: Vec<String> = vec![
            "git",
            "-C",
            "/tmp/project",
            "ls-files",
            "--others",
            "--exclude-standard",
            "-z",
            "--",
            "some/path",
        ]
        .into_iter()
        .map(String::from)
        .collect();

        let quoted = shell_quote_command(&args);
        for shell in available_shells() {
            let output = run_through_shell(shell, &quoted);
            let stderr = String::from_utf8_lossy(&output.stderr);
            assert!(
                !stderr.contains("parse error"),
                "{shell}: shell parse error in git ls-files command: {stderr}"
            );
        }
    }

    // -- E2E: subprocess spawn command through real shells --

    #[test]
    fn subprocess_spawn_command_survives_shells() {
        // Simulates what build_remote_ssh_command produces
        let roots_json = r#"["/home/user/my project","/tmp/other"]"#;
        let cmd = format!("echo --workspace-roots {}", shell_quote_arg(roots_json));

        for shell in available_shells() {
            let output = run_through_shell(shell, &cmd);
            let stderr = String::from_utf8_lossy(&output.stderr);
            assert!(
                output.status.success(),
                "{shell} failed running subprocess spawn cmd: {stderr}"
            );
            let stdout = String::from_utf8_lossy(&output.stdout);
            assert!(
                stdout.contains(roots_json),
                "{shell}: JSON not preserved: {stdout}"
            );
        }
    }

    // -- E2E: full shell_quote_command used exactly as run_ssh_command does --

    #[test]
    fn shell_quote_command_full_pipeline() {
        // Simulate exactly what run_ssh_command does: joins all args into one
        // string, then the remote shell interprets it
        let args: Vec<String> = vec![
            "printf".to_string(),
            "%s\\n".to_string(),
            "arg with {braces}".to_string(),
            "arg with $dollar".to_string(),
            "arg with 'quotes'".to_string(),
        ];

        let quoted = shell_quote_command(&args);

        for shell in available_shells() {
            let output = run_through_shell(shell, &quoted);
            let stderr = String::from_utf8_lossy(&output.stderr);
            assert!(
                output.status.success(),
                "{shell} failed: {stderr}\ncmd: {quoted}"
            );

            let stdout = String::from_utf8_lossy(&output.stdout);
            let lines: Vec<&str> = stdout.lines().collect();
            assert_eq!(lines.len(), 3, "{shell}: expected 3 lines, got {lines:?}");
            assert_eq!(lines[0], "arg with {braces}", "{shell}: braces mismatch");
            assert_eq!(lines[1], "arg with $dollar", "{shell}: dollar mismatch");
            assert_eq!(lines[2], "arg with 'quotes'", "{shell}: quotes mismatch");
        }
    }

    // -- E2E: remote subprocess deployment commands through real shells --

    #[test]
    fn check_remote_subprocess_command_survives_shells() {
        // Reproduces the exact command string from check_remote_subprocess
        let cmd = format!(
            "test -x \"$HOME/.tycode/v{v}/bin/{crate_name}\" && echo \"$HOME/.tycode/v{v}/bin/{crate_name}\"",
            v = SUBPROCESS_VERSION,
            crate_name = SUBPROCESS_CRATE_NAME,
        );

        for shell in available_shells() {
            let output = run_through_shell(shell, &cmd);
            let stderr = String::from_utf8_lossy(&output.stderr);
            // test -x will fail (path doesn't exist), but shell must not choke
            assert!(
                !stderr.contains("parse error") && !stderr.contains("syntax error"),
                "{shell}: shell parse error in check command: {stderr}"
            );
        }
    }

    #[test]
    fn check_remote_subprocess_returns_expanded_path_when_exists() {
        let tmp = std::env::temp_dir().join(".tycode_test_check");
        let tycode_dir = tmp.join(format!(".tycode/v{}/bin", SUBPROCESS_VERSION));
        let _ = std::fs::create_dir_all(&tycode_dir);
        let bin = tycode_dir.join(SUBPROCESS_CRATE_NAME);
        std::fs::write(&bin, "#!/bin/sh\n").unwrap();

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&bin, std::fs::Permissions::from_mode(0o755)).unwrap();
        }

        let tmp_path = tmp.to_string_lossy();
        // export HOME so it persists across && chain
        let cmd = format!(
            "export HOME={tmp}; test -x \"$HOME/.tycode/v{v}/bin/{cn}\" && echo \"$HOME/.tycode/v{v}/bin/{cn}\"",
            tmp = tmp_path,
            v = SUBPROCESS_VERSION,
            cn = SUBPROCESS_CRATE_NAME,
        );

        for shell in available_shells() {
            let output = run_through_shell(shell, &cmd);
            assert!(
                output.status.success(),
                "{shell}: check command failed when binary exists"
            );
            let stdout = String::from_utf8_lossy(&output.stdout);
            let expected_path = format!(
                "{}/.tycode/v{}/bin/{}",
                tmp_path, SUBPROCESS_VERSION, SUBPROCESS_CRATE_NAME
            );
            assert_eq!(
                stdout.trim(),
                expected_path,
                "{shell}: path expansion mismatch"
            );
        }

        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn install_remote_subprocess_download_command_survives_shells() {
        // Verifies the download+extract command structure parses correctly in all shells
        let target = "x86_64-unknown-linux-musl";
        let archive = format!(
            "{crate_name}-{target}.tar.xz",
            crate_name = SUBPROCESS_CRATE_NAME,
        );
        let url = format!(
            "{repo}/releases/download/v{v}/{archive}",
            repo = SUBPROCESS_GIT_REPO,
            v = SUBPROCESS_VERSION,
        );

        // Test the mkdir + echo portion (can't test curl/tar without network)
        let cmd = format!(
            "mkdir -p \"$HOME/.tycode/v{v}/bin\" && \
             echo \"$HOME/.tycode/v{v}/bin/{crate_name}\"",
            v = SUBPROCESS_VERSION,
            crate_name = SUBPROCESS_CRATE_NAME,
        );

        for shell in available_shells() {
            let output = run_through_shell(shell, &cmd);
            let stderr = String::from_utf8_lossy(&output.stderr);
            assert!(
                !stderr.contains("parse error") && !stderr.contains("syntax error"),
                "{shell}: shell parse error in download command: {stderr}"
            );
            let stdout = String::from_utf8_lossy(&output.stdout);
            let path = stdout.trim();
            assert!(
                !path.contains("$HOME"),
                "{shell}: $HOME was not expanded: {path}"
            );
            assert!(
                path.contains(&format!(
                    ".tycode/v{}/bin/{}",
                    SUBPROCESS_VERSION, SUBPROCESS_CRATE_NAME
                )),
                "{shell}: path structure wrong: {path}"
            );
        }

        assert!(url.starts_with("https://"), "URL should be HTTPS: {url}");
        assert!(
            url.contains(&archive),
            "URL should contain archive name: {url}"
        );
    }

    #[test]
    fn path_prepend_survives_shells() {
        // Verify that the PATH prepend pattern used by
        // build_remote_ssh_command works across shells.
        let cmd = "PATH=\"$HOME/.cargo/bin:$HOME/.local/bin:/usr/local/bin:$PATH\" echo ok";
        for shell in available_shells() {
            let output = run_through_shell(shell, cmd);
            let stdout = String::from_utf8_lossy(&output.stdout);
            let stderr = String::from_utf8_lossy(&output.stderr);
            assert!(
                !stderr.contains("parse error") && !stderr.contains("syntax error"),
                "{shell}: shell parse error in PATH prepend: {stderr}"
            );
            assert!(
                stdout.contains("ok"),
                "{shell}: command didn't run after PATH prepend: stdout={stdout}"
            );
        }
    }

    #[test]
    fn version_path_isolation() {
        // Verify that different versions produce different paths
        let v1 = "0.5.2";
        let v2 = "0.6.0";
        let path1 = format!("$HOME/.tycode/v{v1}/bin/{SUBPROCESS_CRATE_NAME}");
        let path2 = format!("$HOME/.tycode/v{v2}/bin/{SUBPROCESS_CRATE_NAME}");
        assert_ne!(
            path1, path2,
            "Different versions must produce different paths"
        );
    }

    #[test]
    fn subprocess_version_is_valid_semver() {
        // Verify the compiled-in version is valid semver
        let parts: Vec<&str> = SUBPROCESS_VERSION.split('.').collect();
        assert!(
            parts.len() == 3,
            "SUBPROCESS_VERSION '{SUBPROCESS_VERSION}' is not semver (expected 3 parts)"
        );
        for part in &parts {
            assert!(
                part.parse::<u32>().is_ok(),
                "SUBPROCESS_VERSION '{SUBPROCESS_VERSION}' has non-numeric component '{part}'"
            );
        }
    }

    // -- parse_remote_path tests --

    #[test]
    fn parse_remote_path_valid() {
        let result = parse_remote_path("ssh://myhost/home/user/project").unwrap();
        assert_eq!(result.host, "myhost");
        assert_eq!(result.path, "/home/user/project");
    }

    #[test]
    fn parse_remote_path_with_port() {
        let result = parse_remote_path("ssh://user@host:2222/home/user/project").unwrap();
        assert_eq!(result.host, "user@host:2222");
        assert_eq!(result.path, "/home/user/project");
    }

    #[test]
    fn parse_remote_path_local_returns_none() {
        assert!(parse_remote_path("/home/user/project").is_none());
        assert!(parse_remote_path("./relative").is_none());
        assert!(parse_remote_path("C:\\Windows\\path").is_none());
    }

    #[test]
    fn parse_remote_path_strips_trailing_slash() {
        let result = parse_remote_path("ssh://host/path/to/dir/").unwrap();
        assert_eq!(result.path, "/path/to/dir");
    }

    // -- to_remote_uri tests --

    #[test]
    fn to_remote_uri_roundtrip() {
        let uri = to_remote_uri("myhost", "/home/user/project");
        assert_eq!(uri, "ssh://myhost/home/user/project");

        let parsed = parse_remote_path(&uri).unwrap();
        assert_eq!(parsed.host, "myhost");
        assert_eq!(parsed.path, "/home/user/project");
    }

    // -- parse_remote_workspace_roots tests --

    #[test]
    fn parse_workspace_roots_all_local() {
        let roots = vec!["/home/user/a".to_string(), "/home/user/b".to_string()];
        let result = parse_remote_workspace_roots(&roots).unwrap();
        assert!(result.is_none());
    }

    #[test]
    fn parse_workspace_roots_all_remote_same_host() {
        let roots = vec![
            "ssh://myhost/home/user/a".to_string(),
            "ssh://myhost/home/user/b".to_string(),
        ];
        let result = parse_remote_workspace_roots(&roots).unwrap().unwrap();
        assert_eq!(result.0, "myhost");
        assert_eq!(result.1, vec!["/home/user/a", "/home/user/b"]);
    }

    #[test]
    fn parse_workspace_roots_mixed_errors() {
        let roots = vec![
            "ssh://myhost/home/user/a".to_string(),
            "/local/path".to_string(),
        ];
        assert!(parse_remote_workspace_roots(&roots).is_err());
    }

    #[test]
    fn parse_workspace_roots_different_hosts_errors() {
        let roots = vec![
            "ssh://host1/path/a".to_string(),
            "ssh://host2/path/b".to_string(),
        ];
        assert!(parse_remote_workspace_roots(&roots).is_err());
    }
}
