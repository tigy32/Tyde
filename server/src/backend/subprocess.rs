use std::process::Stdio;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use command_group::{AsyncCommandGroup, AsyncGroupChild};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::{ChildStdin, Command};
use tokio::sync::{Mutex, mpsc};
use tokio::task::JoinHandle;

use crate::process_env;
use crate::remote::parse_remote_workspace_roots;

/// Best-effort kill-and-reap of a grouped child that the caller already owns.
///
/// An `AsyncGroupChild` (unlike a bare tokio `Child`) is not registered with
/// tokio's orphan reaper, so dropping it without `wait()` leaves a zombie in
/// the OS process table. A synchronous `Drop` can't await, so when a tokio
/// runtime is available we spawn a detached reaper that kills the process group
/// and awaits it — reclaiming the process-table slot. With no runtime handle we
/// fall back to a bounded blocking `start_kill` + `try_wait` loop. Never blocks
/// the async runtime.
///
/// Note: the parent-side stdio read fds are NOT held by the child here — they
/// were `.take()`n out at spawn and live in the reader tasks; those fds are
/// released when the aborted reader tasks drop, not by this reap. This reap
/// reclaims the process/zombie.
pub(crate) fn reap_group_child(mut child: AsyncGroupChild) {
    match tokio::runtime::Handle::try_current() {
        Ok(handle) => {
            handle.spawn(async move {
                let _ = child.start_kill();
                let _ = child.wait().await;
            });
        }
        Err(_) => {
            let _ = child.start_kill();
            for _ in 0..20 {
                match child.try_wait() {
                    Ok(Some(_)) | Err(_) => break,
                    Ok(None) => std::thread::sleep(Duration::from_millis(100)),
                }
            }
        }
    }
}

/// Take a grouped child out of a shared slot and reap it. Idempotent: a no-op
/// when the slot is already empty, e.g. an explicit `shutdown()`/`kill()`
/// already reaped it.
///
/// The locking is done INSIDE the spawned reaper with the async `Mutex` (never
/// `try_lock`): a stdout-EOF reader task can momentarily hold this same `child`
/// slot when the process exits, and a synchronous `try_lock` in `Drop` would
/// lose that race, log, and return without reaping — re-leaking the zombie.
/// Awaiting the lock on a detached task removes the race entirely. Only the
/// no-runtime fallback uses `try_lock` (there is no place to await, and
/// `blocking_lock` would panic on a runtime worker thread).
pub(crate) fn reap_group_child_slot(child: &Arc<Mutex<Option<AsyncGroupChild>>>) {
    match tokio::runtime::Handle::try_current() {
        Ok(handle) => {
            let slot = Arc::clone(child);
            handle.spawn(async move {
                if let Some(mut c) = slot.lock().await.take() {
                    let _ = c.start_kill();
                    let _ = c.wait().await;
                }
            });
        }
        Err(_) => {
            let Ok(mut guard) = child.try_lock() else {
                tracing::warn!(
                    "reap_group_child_slot: could not acquire child lock without a runtime; child may be leaked"
                );
                return;
            };
            if let Some(mut c) = guard.take() {
                let _ = c.start_kill();
                for _ in 0..20 {
                    match c.try_wait() {
                        Ok(Some(_)) | Err(_) => break,
                        Ok(None) => std::thread::sleep(Duration::from_millis(100)),
                    }
                }
            }
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ImageAttachment {
    pub data: String,
    pub media_type: String,
    pub name: String,
    pub size: u64,
}

pub struct SubprocessBridge {
    stdin: Arc<Mutex<ChildStdin>>,
    child: Arc<Mutex<Option<AsyncGroupChild>>>,
    shutting_down: Arc<AtomicBool>,
    stdout_task: JoinHandle<()>,
    stderr_task: JoinHandle<()>,
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
            if let Some(path) = process_env::resolved_child_process_path() {
                cmd.env("PATH", path);
            }
            cmd.stdin(Stdio::piped())
                .stdout(Stdio::piped())
                .stderr(Stdio::piped())
                .group_spawn()
                .map_err(|e| format!("Failed to spawn subprocess: {e:?}"))?
        };

        let stdin = child
            .inner()
            .stdin
            .take()
            .ok_or("Failed to capture stdin")?;
        let stdout = child
            .inner()
            .stdout
            .take()
            .ok_or("Failed to capture stdout")?;
        let stderr = child
            .inner()
            .stderr
            .take()
            .ok_or("Failed to capture stderr")?;

        let (event_tx, event_rx) = mpsc::unbounded_channel();

        let tx = event_tx.clone();
        let child_for_reader = Arc::new(Mutex::new(Some(child)));
        let child_ref = child_for_reader.clone();
        let shutting_down = Arc::new(AtomicBool::new(false));
        let shutting_down_reader = Arc::clone(&shutting_down);
        let stdout_task = tokio::spawn(async move {
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
        let stderr_task = tokio::spawn(async move {
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
                stdout_task,
                stderr_task,
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
    /// Safety net for disconnect/panic/teardown when `shutdown()` didn't run.
    /// Aborts the reader tasks (closing the parent-side pipe fds they hold) and
    /// reaps the child so it doesn't linger as a zombie. A no-op once the child
    /// has already been taken by `shutdown()`.
    fn drop(&mut self) {
        self.stdout_task.abort();
        self.stderr_task.abort();
        reap_group_child_slot(&self.child);
    }
}
