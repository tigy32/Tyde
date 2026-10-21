pub mod launch;

use std::collections::HashSet;
use std::ffi::{OsStr, OsString};
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

static RESOLVED_CHILD_PROCESS_PATH: OnceLock<Result<OsString, String>> = OnceLock::new();
#[cfg(unix)]
const PROBE_SENTINEL_BEGIN: &str = "TYDE_SHELL_PROBE_BEGIN_7f3c9a2e=";
#[cfg(unix)]
const PROBE_SENTINEL_END: &str = "=TYDE_SHELL_PROBE_END_7f3c9a2e";

pub fn init_process_env() -> Result<(), String> {
    initialize_process_env().map(|_| ())
}

pub fn initialize_process_env() -> Result<&'static OsStr, String> {
    RESOLVED_CHILD_PROCESS_PATH
        .get_or_init(compute_resolved_child_process_path)
        .as_ref()
        .map(OsString::as_os_str)
        .map_err(Clone::clone)
}

pub fn command(program: impl AsRef<OsStr>) -> Result<launch::GroupCommand, String> {
    launch::command(program)
}

pub fn resolved_child_process_path() -> Option<&'static OsStr> {
    initialize_process_env().ok()
}

pub fn find_host_executable_in_path(binary: &str) -> Option<PathBuf> {
    find_executable_on_path(binary, initialize_process_env().ok()?, false)
}

pub fn find_executable_in_path(binary: &str) -> Option<PathBuf> {
    find_executable_on_path(binary, &launch::process_path().ok()?, true)
}

fn find_executable_on_path(binary: &str, path: &OsStr, execution: bool) -> Option<PathBuf> {
    let trimmed = binary.trim();
    if trimmed.is_empty() {
        return None;
    }

    let explicit_path = Path::new(trimmed);
    if explicit_path.components().count() > 1 {
        let host = if execution {
            launch::WorkspacePath::execution(explicit_path)
                .ok()?
                .on_host()
                .to_path_buf()
        } else {
            explicit_path.to_path_buf()
        };
        return host.exists().then(|| explicit_path.to_path_buf());
    }

    for dir in std::env::split_paths(path) {
        let host_dir = if execution {
            launch::WorkspacePath::execution(&dir)
                .ok()?
                .on_host()
                .to_path_buf()
        } else {
            dir
        };
        if let Some(candidate) = find_matching_executable_in_dir(&host_dir, trimmed) {
            return if execution {
                Some(
                    launch::WorkspacePath::host(candidate)
                        .ok()?
                        .in_execution()
                        .to_path_buf(),
                )
            } else {
                Some(candidate)
            };
        }
    }
    None
}

fn compute_resolved_child_process_path() -> Result<OsString, String> {
    let mut segments = Vec::<PathBuf>::new();
    #[cfg(unix)]
    extend_login_shell_path(&mut segments)?;
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
        return Err("resolved child process PATH contains no valid directory entries".to_string());
    }

    std::env::join_paths(deduped)
        .map_err(|err| format!("failed to build resolved child process PATH: {err}"))
}

fn extend_from_path_value(segments: &mut Vec<PathBuf>, path_value: Option<OsString>) {
    let Some(path_value) = path_value else {
        return;
    };
    segments.extend(std::env::split_paths(&path_value));
}

#[cfg(unix)]
fn extend_login_shell_path(segments: &mut Vec<PathBuf>) -> Result<(), String> {
    let Some(shell) = default_login_shell() else {
        let err = "failed to determine default login shell for PATH query".to_string();
        tracing::error!("{err}");
        return Err(err);
    };

    let script = format!(
        "printf '{begin}%s{end}\\n' \"$PATH\"",
        begin = PROBE_SENTINEL_BEGIN,
        end = PROBE_SENTINEL_END,
    );
    let mut command = launch::raw_command(&shell);
    command
        .on_host()
        .arg("-ilc")
        .arg(&script)
        .new_session()
        .stdin(launch::ProcessStdio::Null)
        .stdout(launch::ProcessStdio::Piped)
        .stderr(launch::ProcessStdio::Null);
    let output = command
        .output_blocking()
        .map_err(|error| format!("failed to query login-shell PATH via {shell}: {error}"))?;
    if !output.status.success() {
        return Err(format!(
            "login-shell PATH query via {shell} exited with status {}",
            output.status
        ));
    }
    let text = String::from_utf8_lossy(&output.stdout);
    let value = extract_probe_value(&text)
        .ok_or_else(|| format!("login-shell PATH probe output missing sentinels via {shell}"))?;
    if value.is_empty() {
        return Err(format!("login-shell PATH via {shell} was empty"));
    }
    extend_from_path_value(segments, Some(OsString::from(value)));
    Ok(())
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
