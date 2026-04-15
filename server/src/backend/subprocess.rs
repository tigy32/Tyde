use std::process::Stdio;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, ChildStdin, Command};
use tokio::sync::{Mutex, mpsc};

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
    shutting_down: Arc<AtomicBool>,
}

impl SubprocessBridge {
    pub async fn spawn(
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

            let mut remote_args = vec!["--workspace-roots".to_string(), roots_json.clone()];
            if let Some(mcp) = mcp_servers_json {
                remote_args.push("--mcp-servers".to_string());
                remote_args.push(mcp.to_string());
            }
            if ephemeral {
                remote_args.push("--ephemeral".to_string());
            }
            crate::remote::spawn_remote_process(&host, &remote_binary, &remote_args, None).await?
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
        let shutting_down = Arc::new(AtomicBool::new(false));
        let shutting_down_reader = Arc::clone(&shutting_down);
        tokio::spawn(async move {
            let mut lines = BufReader::new(stdout).lines();
            while let Ok(Some(line)) = lines.next_line().await {
                let Ok(value) = serde_json::from_str::<Value>(&line) else {
                    tracing::warn!("Failed to parse subprocess stdout: {line}");
                    continue;
                };
                let _ = tx.send(value);
            }
            let exit_code = if shutting_down_reader.load(Ordering::Acquire) {
                Some(0)
            } else {
                match child_ref.lock().await.as_mut() {
                    Some(c) => c.try_wait().ok().flatten().and_then(|s| s.code()),
                    None => None,
                }
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
                shutting_down,
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
        self.shutting_down.store(true, Ordering::Release);
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
        if let Some(child) = guard.as_mut()
            && let Err(err) = child.start_kill()
        {
            tracing::warn!("SubprocessBridge::drop: failed to kill child: {err}");
        }
    }
}
