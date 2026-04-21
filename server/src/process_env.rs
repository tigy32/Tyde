use std::collections::HashSet;
use std::ffi::{OsStr, OsString};
#[cfg(unix)]
use std::io::Read;
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

pub(crate) fn resolve_login_shell_command_path(binary: &str) -> Option<PathBuf> {
    let trimmed = binary.trim();
    if trimmed.is_empty() {
        return None;
    }

    let explicit_path = Path::new(trimmed);
    if explicit_path.components().count() > 1 {
        return explicit_path.is_file().then(|| explicit_path.to_path_buf());
    }

    #[cfg(unix)]
    {
        let shell = default_login_shell()?;
        resolve_login_shell_command_path_with_shell(&shell, trimmed)
    }

    #[cfg(not(unix))]
    {
        None
    }
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
                let stdout = match read_child_stdout(child, "login-shell PATH query") {
                    Some(stdout) => stdout,
                    None => return,
                };
                let trimmed = String::from_utf8_lossy(&stdout).trim().to_string();
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

#[cfg(unix)]
fn resolve_login_shell_command_path_with_shell(shell: &str, binary: &str) -> Option<PathBuf> {
    let mut child = match Command::new(shell)
        .arg("-lc")
        .arg("command -v -- \"$TYDE_LOOKUP_BINARY\"")
        .env("TYDE_LOOKUP_BINARY", binary)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
    {
        Ok(child) => child,
        Err(err) => {
            tracing::debug!(
                "failed to resolve {} via login shell {}: {err}",
                binary,
                shell
            );
            return None;
        }
    };

    let started = Instant::now();
    let timeout = Duration::from_millis(750);
    loop {
        match child.try_wait() {
            Ok(Some(status)) => {
                if !status.success() {
                    return None;
                }
                let stdout =
                    String::from_utf8_lossy(&read_child_stdout(child, "login-shell resolution")?)
                        .into_owned();
                let resolved = stdout
                    .lines()
                    .map(str::trim)
                    .find(|line| !line.is_empty())?;
                let resolved_path = PathBuf::from(resolved);
                if !resolved_path.is_absolute() || !resolved_path.is_file() {
                    return None;
                }
                return Some(resolved_path);
            }
            Ok(None) => {
                if started.elapsed() >= timeout {
                    let _ = child.kill();
                    let _ = child.wait();
                    tracing::debug!("timed out resolving {} via login shell {}", binary, shell);
                    return None;
                }
                std::thread::sleep(Duration::from_millis(10));
            }
            Err(err) => {
                tracing::debug!(
                    "failed waiting for login-shell resolution of {} via {}: {err}",
                    binary,
                    shell
                );
                return None;
            }
        }
    }
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

#[cfg(test)]
mod tests {
    use std::fs;
    use std::path::PathBuf;

    use super::*;

    #[cfg(unix)]
    fn write_executable(path: &Path, body: &str) {
        fs::write(path, body).expect("write executable");
        use std::os::unix::fs::PermissionsExt;
        let mut perms = fs::metadata(path).expect("stat executable").permissions();
        perms.set_mode(0o755);
        fs::set_permissions(path, perms).expect("chmod executable");
    }

    #[cfg(unix)]
    fn temp_test_dir() -> PathBuf {
        let path =
            std::env::temp_dir().join(format!("tyde-process-env-test-{}", uuid::Uuid::new_v4()));
        fs::create_dir_all(&path).expect("create temp test dir");
        path
    }

    #[cfg(unix)]
    #[test]
    fn resolve_login_shell_command_path_uses_shell_reported_absolute_path() {
        let dir = temp_test_dir();
        let binary = dir.join("tycode-subprocess");
        write_executable(&binary, "#!/bin/sh\nexit 0\n");

        let shell = dir.join("fake-shell");
        write_executable(
            &shell,
            &format!("#!/bin/sh\nprintf '%s\\n' \"{}\"\n", binary.display()),
        );

        let resolved = resolve_login_shell_command_path_with_shell(
            shell.to_str().expect("shell path utf-8"),
            "tycode-subprocess",
        );

        assert_eq!(resolved, Some(binary));
        fs::remove_dir_all(dir).expect("remove temp test dir");
    }

    #[cfg(unix)]
    #[test]
    fn resolve_login_shell_command_path_rejects_non_absolute_output() {
        let dir = temp_test_dir();
        let shell = dir.join("fake-shell");
        write_executable(&shell, "#!/bin/sh\nprintf 'tycode-subprocess\\n'\n");

        let resolved = resolve_login_shell_command_path_with_shell(
            shell.to_str().expect("shell path utf-8"),
            "tycode-subprocess",
        );

        assert_eq!(resolved, None);
        fs::remove_dir_all(dir).expect("remove temp test dir");
    }
}
