use std::collections::HashSet;
use std::ffi::{OsStr, OsString};
#[cfg(unix)]
use std::io::Read;
use std::path::{Path, PathBuf};
#[cfg(unix)]
use std::process::{Command, Stdio};
use std::sync::OnceLock;
#[cfg(unix)]
use std::time::{Duration, Instant};

static RESOLVED_CHILD_PROCESS_PATH: OnceLock<Option<OsString>> = OnceLock::new();
#[cfg(unix)]
const LOGIN_SHELL_TIMEOUT: Duration = Duration::from_secs(60);
#[cfg(unix)]
const PROBE_SENTINEL_BEGIN: &str = "TYDE_SHELL_PROBE_BEGIN_7f3c9a2e=";
#[cfg(unix)]
const PROBE_SENTINEL_END: &str = "=TYDE_SHELL_PROBE_END_7f3c9a2e";

pub(crate) fn resolved_child_process_path() -> Option<&'static OsStr> {
    RESOLVED_CHILD_PROCESS_PATH
        .get_or_init(compute_resolved_child_process_path)
        .as_deref()
}

pub(crate) fn find_executable_in_path(binary: &str) -> Option<PathBuf> {
    let trimmed = binary.trim();
    if trimmed.is_empty() {
        return None;
    }

    let explicit_path = Path::new(trimmed);
    if explicit_path.components().count() > 1 {
        return explicit_path.exists().then(|| explicit_path.to_path_buf());
    }

    let resolved_path = resolved_child_process_path()?;
    for dir in std::env::split_paths(resolved_path) {
        if let Some(candidate) = find_matching_executable_in_dir(&dir, trimmed) {
            return Some(candidate);
        }
    }
    None
}

fn compute_resolved_child_process_path() -> Option<OsString> {
    let mut segments = Vec::<PathBuf>::new();
    #[cfg(unix)]
    extend_login_shell_path(&mut segments);
    #[cfg(not(unix))]
    extend_from_path_value(&mut segments, std::env::var_os("PATH"));

    let mut seen = HashSet::<PathBuf>::new();
    let mut deduped = Vec::<PathBuf>::new();
    for path in segments {
        if path.as_os_str().is_empty() {
            continue;
        }
        if seen.insert(path.clone()) {
            deduped.push(path);
        }
    }

    if deduped.is_empty() {
        return None;
    }

    match std::env::join_paths(deduped) {
        Ok(path) => Some(path),
        Err(err) => {
            tracing::error!("failed to build resolved child process PATH: {err}");
            None
        }
    }
}

fn extend_from_path_value(segments: &mut Vec<PathBuf>, path_value: Option<OsString>) {
    let Some(path_value) = path_value else {
        return;
    };
    segments.extend(std::env::split_paths(&path_value));
}

#[cfg(unix)]
fn extend_login_shell_path(segments: &mut Vec<PathBuf>) {
    let Some(shell) = default_login_shell() else {
        tracing::error!("failed to determine default login shell for PATH query");
        return;
    };

    let script = format!(
        "printf '{begin}%s{end}\\n' \"$PATH\"",
        begin = PROBE_SENTINEL_BEGIN,
        end = PROBE_SENTINEL_END,
    );
    let mut child = match Command::new(&shell)
        .arg("-ilc")
        .arg(&script)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
    {
        Ok(child) => child,
        Err(err) => {
            tracing::error!("failed to query login-shell PATH via {}: {err}", shell);
            return;
        }
    };

    let started = Instant::now();
    let timeout = LOGIN_SHELL_TIMEOUT;
    loop {
        match child.try_wait() {
            Ok(Some(status)) => {
                if !status.success() {
                    tracing::error!(
                        "login-shell PATH query via {} exited with status {}",
                        shell,
                        status
                    );
                    return;
                }
                let stdout = match read_child_stdout(child, "login-shell PATH query") {
                    Some(stdout) => stdout,
                    None => return,
                };
                let text = String::from_utf8_lossy(&stdout);
                let Some(value) = extract_probe_value(&text) else {
                    tracing::error!(
                        "login-shell PATH probe output missing sentinels via {}",
                        shell
                    );
                    return;
                };
                if value.is_empty() {
                    return;
                }
                extend_from_path_value(segments, Some(OsString::from(value)));
                return;
            }
            Ok(None) => {
                if started.elapsed() >= timeout {
                    let _ = child.kill();
                    let _ = child.wait();
                    tracing::error!(
                        "timed out querying login-shell PATH via {} (deadline 60s)",
                        shell
                    );
                    return;
                }
                std::thread::sleep(Duration::from_millis(10));
            }
            Err(err) => {
                tracing::error!(
                    "failed to wait for login-shell PATH query via {}: {err}",
                    shell
                );
                return;
            }
        }
    }
}

#[cfg(unix)]
fn default_login_shell() -> Option<String> {
    if let Ok(shell) = std::env::var("SHELL") {
        let trimmed = shell.trim();
        if !trimmed.is_empty() {
            return Some(trimmed.to_string());
        }
    }

    if cfg!(target_os = "macos") {
        return Some("/bin/zsh".to_string());
    }
    Some("/bin/bash".to_string())
}

#[cfg(unix)]
fn extract_probe_value(output: &str) -> Option<&str> {
    let start = output.rfind(PROBE_SENTINEL_BEGIN)? + PROBE_SENTINEL_BEGIN.len();
    let rest = output.get(start..)?;
    let end = rest.find(PROBE_SENTINEL_END)?;
    Some(rest[..end].trim())
}

#[cfg(unix)]
fn read_child_stdout(mut child: std::process::Child, context: &str) -> Option<Vec<u8>> {
    let mut stdout = Vec::new();
    if let Some(mut pipe) = child.stdout.take()
        && let Err(err) = pipe.read_to_end(&mut stdout)
    {
        tracing::debug!("failed to read stdout for {context}: {err}");
        return None;
    }
    Some(stdout)
}

fn find_matching_executable_in_dir(dir: &Path, binary: &str) -> Option<PathBuf> {
    #[cfg(windows)]
    {
        let candidate = dir.join(binary);
        if candidate.is_file() {
            return Some(candidate);
        }

        let pathext =
            std::env::var_os("PATHEXT").unwrap_or_else(|| OsString::from(".COM;.EXE;.BAT;.CMD"));
        for ext in pathext
            .to_string_lossy()
            .split(';')
            .map(str::trim)
            .filter(|ext| !ext.is_empty())
        {
            let candidate = dir.join(format!("{binary}{ext}"));
            if candidate.is_file() {
                return Some(candidate);
            }
        }
        None
    }

    #[cfg(not(windows))]
    {
        let candidate = dir.join(binary);
        candidate.is_file().then_some(candidate)
    }
}
