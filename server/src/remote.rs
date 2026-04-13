use std::process::Output;

use tokio::process::{Child, Command};

pub fn parse_remote_workspace_roots(
    workspace_roots: &[String],
) -> Result<Option<(String, Vec<String>)>, String> {
    let mut host: Option<String> = None;
    let mut paths = Vec::new();

    for root in workspace_roots {
        let Some(rest) = root.strip_prefix("ssh://") else {
            continue;
        };
        let mut parts = rest.splitn(2, '/');
        let parsed_host = parts.next().unwrap_or("").trim();
        let parsed_path = format!("/{}", parts.next().unwrap_or("").trim());
        if parsed_host.is_empty() || parsed_path == "/" {
            continue;
        }
        if let Some(current) = &host {
            if current != parsed_host {
                return Err("All remote workspace roots must use the same host".to_string());
            }
        } else {
            host = Some(parsed_host.to_string());
        }
        paths.push(parsed_path);
    }

    match host {
        Some(h) => Ok(Some((h, paths))),
        None => Ok(None),
    }
}

pub fn shell_quote_arg(value: &str) -> String {
    format!("'{}'", value.replace('\'', r"'\''"))
}

pub fn shell_quote_command(args: &[String]) -> String {
    args.iter()
        .map(|arg| shell_quote_arg(arg))
        .collect::<Vec<_>>()
        .join(" ")
}

pub fn ssh_control_args() -> Result<Vec<String>, String> {
    Ok(Vec::new())
}

pub async fn run_ssh_raw(host: &str, command: &str) -> Result<Output, String> {
    Command::new("ssh")
        .arg("-T")
        .arg(host)
        .arg(command)
        .output()
        .await
        .map_err(|err| format!("ssh command failed: {err}"))
}

pub async fn spawn_remote_process(
    host: &str,
    program: &str,
    args: &[String],
    cwd: Option<&str>,
) -> Result<Child, String> {
    let mut remote_parts = Vec::new();
    if let Some(path) = cwd.map(str::trim).filter(|v| !v.is_empty()) {
        remote_parts.push(format!("cd {} &&", shell_quote_arg(path)));
    }
    remote_parts.push(shell_quote_arg(program));
    if !args.is_empty() {
        remote_parts.push(shell_quote_command(args));
    }
    let remote_cmd = remote_parts.join(" ");

    Command::new("ssh")
        .arg("-T")
        .arg(host)
        .arg(remote_cmd)
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .map_err(|err| format!("Failed to spawn remote process over ssh: {err}"))
}
