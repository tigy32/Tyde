use std::process::Stdio;
use std::sync::Arc;
use std::time::Duration;

use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, ChildStdin, Command};
use tokio::sync::{mpsc, Mutex};

use crate::remote::parse_remote_workspace_roots;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ImageAttachment {
    pub data: String,
    pub media_type: String,
    pub name: String,
    pub size: u64,
}

pub struct SubprocessBridge {
    stdin: Arc<Mutex<ChildStdin>>,
    child: Arc<Mutex<Option<Child>>>,
}

impl SubprocessBridge {
    pub fn spawn(
        subprocess_path: &str,
        workspace_roots: &[String],
        mcp_servers_json: Option<&str>,
        ephemeral: bool,
    ) -> Result<(Self, mpsc::UnboundedReceiver<Value>), String> {
        let remote_roots = parse_remote_workspace_roots(workspace_roots)?;

        let roots_json = match &remote_roots {
            Some((_, roots)) => serde_json::to_string(roots).map_err(|e| format!("{e:?}"))?,
            None => serde_json::to_string(workspace_roots).map_err(|e| format!("{e:?}"))?,
        };

        let mut child = if let Some((host, _)) = remote_roots {
            let remote_binary = if subprocess_path.is_empty() {
                std::env::var("TYDE_REMOTE_SUBPROCESS_PATH")
                    .unwrap_or_else(|_| "tycode-subprocess".to_string())
            } else {
                subprocess_path.to_string()
            };

            let remote_cmd =
                build_remote_ssh_command(&remote_binary, &roots_json, mcp_servers_json, ephemeral);
            Command::new("ssh")
                .arg("-T")
                .arg(host)
                .arg(remote_cmd)
                .stdin(Stdio::piped())
                .stdout(Stdio::piped())
                .stderr(Stdio::piped())
                .spawn()
                .map_err(|e| format!("Failed to spawn remote subprocess over ssh: {e:?}"))?
        } else {
            let mut cmd = Command::new(subprocess_path);
            cmd.arg("--workspace-roots").arg(&roots_json);
            if let Some(mcp_servers_json) = mcp_servers_json {
                cmd.arg("--mcp-servers").arg(mcp_servers_json);
            }
            if ephemeral {
                cmd.arg("--ephemeral");
            }
            cmd.stdin(Stdio::piped())
                .stdout(Stdio::piped())
                .stderr(Stdio::piped())
                .spawn()
                .map_err(|e| format!("Failed to spawn subprocess: {e:?}"))?
        };

        let stdin = child.stdin.take().ok_or("Failed to capture stdin")?;
        let stdout = child.stdout.take().ok_or("Failed to capture stdout")?;
        let stderr = child.stderr.take().ok_or("Failed to capture stderr")?;

        let (event_tx, event_rx) = mpsc::unbounded_channel();

        let tx = event_tx.clone();
        let child_for_reader = Arc::new(Mutex::new(Some(child)));
        let child_ref = child_for_reader.clone();
        tokio::spawn(async move {
            let mut lines = BufReader::new(stdout).lines();
            while let Ok(Some(line)) = lines.next_line().await {
                let Ok(value) = serde_json::from_str::<Value>(&line) else {
                    tracing::warn!("Failed to parse subprocess stdout: {line}");
                    continue;
                };
                let _ = tx.send(value);
            }
            let exit_code = match child_ref.lock().await.as_mut() {
                Some(c) => c.try_wait().ok().flatten().and_then(|s| s.code()),
                None => None,
            };
            let exit_event =
                serde_json::json!({"kind": "SubprocessExit", "data": {"exit_code": exit_code}});
            let _ = tx.send(exit_event);
        });

        let stderr_tx = event_tx.clone();
        tokio::spawn(async move {
            let mut lines = BufReader::new(stderr).lines();
            while let Ok(Some(line)) = lines.next_line().await {
                tracing::debug!("subprocess stderr: {line}");
                let event = serde_json::json!({"kind": "SubprocessStderr", "data": line});
                let _ = stderr_tx.send(event);
            }
        });

        Ok((
            Self {
                stdin: Arc::new(Mutex::new(stdin)),
                child: child_for_reader.clone(),
            },
            event_rx,
        ))
    }

    /// Callers must clone this before dropping a std::sync::Mutex guard
    /// to avoid holding a sync lock across an async await point.
    pub fn stdin(&self) -> Arc<Mutex<ChildStdin>> {
        self.stdin.clone()
    }

    async fn send_line(&self, line: &str) -> Result<(), String> {
        let mut stdin = self.stdin.lock().await;
        stdin
            .write_all(format!("{line}\n").as_bytes())
            .await
            .map_err(|e| format!("{e:?}"))
    }

    pub async fn is_alive(&self) -> bool {
        let mut guard = self.child.lock().await;
        let Some(child) = guard.as_mut() else {
            return false;
        };
        match child.try_wait() {
            Ok(Some(_)) => false,
            Ok(None) => true,
            Err(_) => false,
        }
    }

    pub async fn shutdown(self) {
        if self.is_alive().await {
            let _ = self
                .send_line(&serde_json::json!({"command": "quit"}).to_string())
                .await;
        }

        let mut child = self.child.lock().await;
        let Some(mut c) = child.take() else { return };

        match tokio::time::timeout(Duration::from_secs(2), c.wait()).await {
            Ok(_) => {}
            Err(_) => {
                let _ = c.kill().await;
            }
        }
        // child is taken (None) — Drop will be a no-op
    }
}

impl Drop for SubprocessBridge {
    fn drop(&mut self) {
        let Ok(mut guard) = self.child.try_lock() else {
            tracing::warn!(
                "SubprocessBridge::drop: could not acquire lock, child process may be leaked"
            );
            return;
        };
        if let Some(child) = guard.as_mut() {
            if let Err(err) = child.start_kill() {
                tracing::warn!("SubprocessBridge::drop: failed to kill child: {err}");
            }
        }
    }
}

fn build_remote_ssh_command(
    binary: &str,
    roots_json: &str,
    mcp_servers_json: Option<&str>,
    ephemeral: bool,
) -> String {
    use crate::remote::shell_quote_arg;
    // Prepend common binary directories to PATH. Sourcing profile files is
    // unsafe — they can contain `exec` statements that replace the shell
    // process, preventing our command from ever running.
    let mut cmd = format!(
        "PATH=\"$HOME/.cargo/bin:$HOME/.local/bin:/usr/local/bin:$PATH\" {} --workspace-roots {}",
        binary,
        shell_quote_arg(roots_json)
    );
    if let Some(mcp_servers_json) = mcp_servers_json {
        cmd.push_str(" --mcp-servers ");
        cmd.push_str(&shell_quote_arg(mcp_servers_json));
    }
    if ephemeral {
        cmd.push_str(" --ephemeral");
    }
    cmd
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ssh_args_survive_shell_interpretation() {
        let roots_json = r#"["/home/user/project","/tmp/other"]"#;
        let remote_cmd = build_remote_ssh_command("echo", roots_json, None, false);

        let sh_result = std::process::Command::new("sh")
            .arg("-c")
            .arg(&remote_cmd)
            .output()
            .expect("failed to spawn sh");

        let sh_stdout = String::from_utf8_lossy(&sh_result.stdout);
        let sh_stderr = String::from_utf8_lossy(&sh_result.stderr);
        assert!(
            sh_result.status.success(),
            "sh failed with stderr: {sh_stderr}"
        );
        assert!(
            sh_stdout.contains(roots_json),
            "sh output didn't preserve JSON: got {sh_stdout}"
        );

        // Run through zsh if available (the shell that triggered the original bug)
        if let Ok(zsh_result) = std::process::Command::new("zsh")
            .arg("-c")
            .arg(&remote_cmd)
            .output()
        {
            let zsh_stdout = String::from_utf8_lossy(&zsh_result.stdout);
            let zsh_stderr = String::from_utf8_lossy(&zsh_result.stderr);
            assert!(
                zsh_result.status.success(),
                "zsh failed with stderr: {zsh_stderr}"
            );
            assert!(
                zsh_stdout.contains(roots_json),
                "zsh output didn't preserve JSON: got {zsh_stdout}"
            );
        }
    }

    #[test]
    fn remote_cmd_prepends_common_path_dirs() {
        let cmd = build_remote_ssh_command("/usr/bin/tycode-subprocess", "[]", None, false);
        assert!(
            cmd.contains(".cargo/bin"),
            "command missing .cargo/bin in PATH: {cmd}"
        );
        assert!(
            cmd.contains(".local/bin"),
            "command missing .local/bin in PATH: {cmd}"
        );
        assert!(
            cmd.contains("/usr/local/bin"),
            "command missing /usr/local/bin in PATH: {cmd}"
        );
    }

    #[test]
    fn remote_cmd_path_prepend_doesnt_block_execution() {
        let roots_json = r#"["/home/user/project"]"#;
        let remote_cmd = build_remote_ssh_command("echo", roots_json, None, false);

        for shell in ["sh", "zsh"] {
            let result = std::process::Command::new(shell)
                .arg("-c")
                .arg(&remote_cmd)
                .output();

            let Ok(output) = result else {
                continue;
            };

            let stdout = String::from_utf8_lossy(&output.stdout);
            let stderr = String::from_utf8_lossy(&output.stderr);
            assert!(
                output.status.success(),
                "{shell}: PATH prepend blocked execution. stderr: {stderr}"
            );
            assert!(
                stdout.contains(roots_json),
                "{shell}: command didn't produce expected output. stdout: {stdout}"
            );
        }
    }

    #[test]
    fn remote_cmd_includes_ephemeral_flag_when_set() {
        let roots_json = r#"["/home/user/project"]"#;

        let without = build_remote_ssh_command("tycode-subprocess", roots_json, None, false);
        assert!(
            !without.contains("--ephemeral"),
            "non-ephemeral command should not contain --ephemeral: {without}"
        );

        let with = build_remote_ssh_command("tycode-subprocess", roots_json, None, true);
        assert!(
            with.contains("--ephemeral"),
            "ephemeral command should contain --ephemeral: {with}"
        );
    }

    #[test]
    fn ssh_args_handle_embedded_single_quotes() {
        let roots_json = r#"["/home/user/it's a path"]"#;
        let remote_cmd = build_remote_ssh_command("echo", roots_json, None, false);

        let result = std::process::Command::new("sh")
            .arg("-c")
            .arg(&remote_cmd)
            .output()
            .expect("failed to spawn sh");

        let stdout = String::from_utf8_lossy(&result.stdout);
        let stderr = String::from_utf8_lossy(&result.stderr);
        assert!(result.status.success(), "sh failed with stderr: {stderr}");
        assert!(
            stdout.contains(roots_json),
            "sh output didn't preserve JSON with quotes: got {stdout}"
        );
    }
}
