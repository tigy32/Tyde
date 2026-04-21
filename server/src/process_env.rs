use std::collections::HashSet;
use std::ffi::{OsStr, OsString};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::OnceLock;
use std::time::{Duration, Instant};

static RESOLVED_CHILD_PROCESS_PATH: OnceLock<Option<OsString>> = OnceLock::new();

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
    extend_from_path_value(&mut segments, std::env::var_os("PATH"));
    #[cfg(target_os = "macos")]
    extend_path_helper_path(&mut segments);
    #[cfg(unix)]
    extend_login_shell_path(&mut segments);
    extend_common_user_bin_dirs(&mut segments);
    extend_common_system_bin_dirs(&mut segments);

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
        return std::env::var_os("PATH");
    }

    match std::env::join_paths(deduped) {
        Ok(path) => Some(path),
        Err(err) => {
            tracing::warn!("failed to build resolved child process PATH: {err}");
            std::env::var_os("PATH")
        }
    }
}

fn extend_from_path_value(segments: &mut Vec<PathBuf>, path_value: Option<OsString>) {
    let Some(path_value) = path_value else {
        return;
    };
    segments.extend(std::env::split_paths(&path_value));
}

#[cfg(target_os = "macos")]
fn extend_path_helper_path(segments: &mut Vec<PathBuf>) {
    let output = Command::new("/usr/libexec/path_helper")
        .arg("-s")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .output();
    let Ok(output) = output else {
        return;
    };
    if !output.status.success() {
        return;
    }
    let stdout = String::from_utf8_lossy(&output.stdout);
    if let Some(path_value) = parse_exported_path(stdout.trim()) {
        extend_from_path_value(segments, Some(OsString::from(path_value)));
    }
}

#[cfg(target_os = "macos")]
fn parse_exported_path(shell_output: &str) -> Option<String> {
    let start = shell_output.find("PATH=")? + "PATH=".len();
    let rest = shell_output.get(start..)?.trim_start();
    if let Some(rest) = rest.strip_prefix('"') {
        let end = rest.find('"')?;
        return Some(rest[..end].to_string());
    }
    let end = rest.find(';').unwrap_or(rest.len());
    let value = rest[..end].trim();
    (!value.is_empty()).then(|| value.to_string())
}

#[cfg(unix)]
fn extend_login_shell_path(segments: &mut Vec<PathBuf>) {
    let Some(shell) = default_login_shell() else {
        return;
    };

    let mut child = match Command::new(&shell)
        .arg("-lc")
        .arg("printf %s \"$PATH\"")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
    {
        Ok(child) => child,
        Err(err) => {
            tracing::debug!("failed to query login-shell PATH via {}: {err}", shell);
            return;
        }
    };

    let started = Instant::now();
    let timeout = Duration::from_millis(750);
    loop {
        match child.try_wait() {
            Ok(Some(status)) => {
                if !status.success() {
                    return;
                }
                let output = match child.wait_with_output() {
                    Ok(output) => output,
                    Err(err) => {
                        tracing::debug!("failed to collect login-shell PATH output: {err}");
                        return;
                    }
                };
                let trimmed = String::from_utf8_lossy(&output.stdout).trim().to_string();
                if trimmed.is_empty() {
                    return;
                }
                extend_from_path_value(segments, Some(OsString::from(trimmed)));
                return;
            }
            Ok(None) => {
                if started.elapsed() >= timeout {
                    let _ = child.kill();
                    let _ = child.wait();
                    tracing::debug!("timed out querying login-shell PATH via {}", shell);
                    return;
                }
                std::thread::sleep(Duration::from_millis(10));
            }
            Err(err) => {
                tracing::debug!(
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

fn extend_common_user_bin_dirs(segments: &mut Vec<PathBuf>) {
    let home = std::env::var_os("HOME").map(PathBuf::from);
    if let Some(home) = home {
        segments.push(home.join(".cargo").join("bin"));
        segments.push(home.join(".local").join("bin"));
        segments.push(home.join(".npm-global").join("bin"));
    }
}

fn extend_common_system_bin_dirs(segments: &mut Vec<PathBuf>) {
    #[cfg(target_os = "macos")]
    {
        segments.push(PathBuf::from("/opt/homebrew/bin"));
    }
    segments.push(PathBuf::from("/usr/local/bin"));
    segments.push(PathBuf::from("/usr/bin"));
    segments.push(PathBuf::from("/bin"));
    segments.push(PathBuf::from("/usr/sbin"));
    segments.push(PathBuf::from("/sbin"));
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
