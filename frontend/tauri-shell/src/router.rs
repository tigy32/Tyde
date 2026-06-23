use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use tauri::{AppHandle, Emitter};
use tokio::io::{AsyncBufRead, AsyncBufReadExt, AsyncWrite, AsyncWriteExt, BufReader};
use tokio::process::{Child, Command};
use tokio::sync::mpsc;

use crate::bridge::{
    HOST_DISCONNECTED_EVENT, HOST_ERROR_EVENT, HOST_LINE_EVENT, HostDisconnectedEvent,
    HostErrorEvent, HostLineEvent,
};
use crate::host_store::{HostTransportConfig, RemoteHostLifecycleConfig};

const DEFAULT_REMOTE_HOST_COMMAND: &str = "tyde host --bridge-uds";

/// Routes Tauri commands to per-connection writer tasks and tracks the live
/// connections.
///
/// Each connection is driven by two fully independent tasks that share no
/// channel and never coordinate across directions:
///
///   * a **reader task** that solely owns the transport's read half and emits
///     every inbound line straight to the app, and
///   * a **writer task** that solely owns the write half and drains an
///     unbounded outbound channel.
///
/// The registry below only carries control-plane state (the outbound sender
/// and the child handle used for teardown). It never sits in the inbound data
/// path, so a stalled/backpressured writer can never stop the reader from
/// draining the transport — which is what previously deadlocked the SSH proxy.
#[derive(Clone)]
pub struct ProxyRouterHandle {
    state: Arc<Mutex<RouterState>>,
}

struct RouterState {
    hosts: HashMap<String, Connection>,
    next_connection_id: u64,
}

struct Connection {
    connection_id: u64,
    /// Outbound frames are enqueued here; the writer task owns the receiver.
    /// Dropping every sender (i.e. removing this entry) makes the writer task
    /// finish on its own.
    outbound_tx: mpsc::UnboundedSender<String>,
    /// SSH child process, owned here so teardown can reap it. `None` for the
    /// in-process embedded transport.
    child: Option<Child>,
}

impl ProxyRouterHandle {
    pub fn new() -> Self {
        Self {
            state: Arc::new(Mutex::new(RouterState {
                hosts: HashMap::new(),
                next_connection_id: 1,
            })),
        }
    }

    pub async fn connect_local(
        &self,
        app: AppHandle,
        host_id: String,
        transport: HostTransportConfig,
        host: server::HostHandle,
    ) -> Result<(), String> {
        tracing::info!(host_id, "connect_duplex requested");

        // Quietly tear down any existing connection for this host before
        // establishing the new one. Removing the entry drops its outbound
        // sender (stopping the old writer); reaping the child closes the old
        // reader via EOF. The old reader's teardown will no-op because the new
        // connection carries a different connection id.
        let existing = {
            let mut guard = self.state.lock().expect("router state poisoned");
            guard.hosts.remove(&host_id)
        };
        if let Some(existing) = existing {
            tracing::info!(host_id, "replacing existing host connection");
            reap_child(existing.child).await;
        }

        let connection_id = {
            let mut guard = self.state.lock().expect("router state poisoned");
            let id = guard.next_connection_id;
            guard.next_connection_id += 1;
            id
        };

        let setup = setup_connection_transport(&host_id, app.clone(), transport, host).await?;

        let (outbound_tx, outbound_rx) = mpsc::unbounded_channel::<String>();

        // Register before spawning so that an immediate reader EOF can find the
        // entry (and thus reap the child / emit disconnect) instead of racing.
        {
            let mut guard = self.state.lock().expect("router state poisoned");
            guard.hosts.insert(
                host_id.clone(),
                Connection {
                    connection_id,
                    outbound_tx,
                    child: setup.child,
                },
            );
        }

        tokio::spawn(reader_task(
            self.state.clone(),
            app.clone(),
            host_id.clone(),
            connection_id,
            setup.reader,
        ));
        tokio::spawn(writer_task(
            self.state.clone(),
            app,
            host_id.clone(),
            connection_id,
            setup.writer,
            outbound_rx,
        ));

        tracing::info!(host_id, "connected via duplex");
        Ok(())
    }

    pub async fn disconnect(&self, host_id: String) -> Result<(), String> {
        let connection = {
            let mut guard = self.state.lock().expect("router state poisoned");
            guard.hosts.remove(&host_id)
        };
        let Some(connection) = connection else {
            return Err(format!("host {host_id} is not connected"));
        };

        // Quiet teardown: dropping `connection` drops the outbound sender (the
        // writer task ends), and reaping the child closes the reader via EOF.
        // The reader's own teardown no-ops because the entry is already gone,
        // so no `HOST_DISCONNECTED_EVENT` is emitted for an explicit disconnect.
        reap_child(connection.child).await;
        Ok(())
    }

    pub async fn send_line(&self, host_id: String, line: String) -> Result<(), String> {
        if line.contains('\n') {
            return Err("host line must not contain a newline".to_owned());
        }

        let outbound_tx = {
            let guard = self.state.lock().expect("router state poisoned");
            guard.hosts.get(&host_id).map(|c| c.outbound_tx.clone())
        };
        let Some(outbound_tx) = outbound_tx else {
            return Err(format!("host {host_id} is not connected"));
        };

        // Enqueue and return immediately. The unbounded channel never applies
        // backpressure, so this can't block on the writer. If the writer task
        // has already exited (dead connection), the send fails and we surface
        // an explicit error rather than silently dropping the frame.
        outbound_tx
            .send(line)
            .map_err(|_| format!("host {host_id} connection is no longer available"))
    }
}

/// Inbound half: solely owns the transport reader and pushes every line to the
/// app. It never touches the registry on the hot path and never waits on
/// anything the writer owns, so it always drains the transport.
async fn reader_task(
    state: Arc<Mutex<RouterState>>,
    app: AppHandle,
    host_id: String,
    connection_id: u64,
    mut reader: Box<dyn AsyncBufRead + Unpin + Send>,
) {
    loop {
        let mut incoming = String::new();
        match reader.read_line(&mut incoming).await {
            Ok(0) => break,
            Ok(_) => {
                let line = trim_line_ending(incoming);
                tracing::info!(
                    host_id,
                    connection_id,
                    line_len = line.len(),
                    "proxy router received line from host"
                );
                if let Err(error) = app.emit(
                    HOST_LINE_EVENT,
                    HostLineEvent {
                        host_id: host_id.clone(),
                        line,
                        connection_instance_id: None,
                        delivery_id: None,
                    },
                ) {
                    let _ =
                        emit_error(&app, &host_id, format!("failed to emit host line: {error}"));
                }
            }
            Err(error) => {
                let _ = emit_error(
                    &app,
                    &host_id,
                    format!("failed to read from host connection: {error}"),
                );
                break;
            }
        }
    }

    close_connection(&state, &app, &host_id, connection_id).await;
}

/// Outbound half: solely owns the transport writer and drains the unbounded
/// command channel. It never touches the reader. When every outbound sender is
/// dropped (the connection was torn down elsewhere) `recv` returns `None` and
/// the task ends quietly.
async fn writer_task(
    state: Arc<Mutex<RouterState>>,
    app: AppHandle,
    host_id: String,
    connection_id: u64,
    mut writer: Box<dyn AsyncWrite + Unpin + Send>,
    mut outbound_rx: mpsc::UnboundedReceiver<String>,
) {
    while let Some(line) = outbound_rx.recv().await {
        if let Err(error) = write_line(&mut writer, line).await {
            tracing::warn!(
                host_id,
                connection_id,
                %error,
                "closing host connection after write failed"
            );
            // Surface the write failure and tear the connection down so a dead
            // pipe becomes a visible error instead of silently swallowing sends.
            let _ = emit_error(&app, &host_id, error);
            close_connection(&state, &app, &host_id, connection_id).await;
            return;
        }
    }
}

/// Drop a connection from the registry and notify the app. Idempotent and
/// connection-id guarded: only the task that still matches the live entry wins
/// the removal, so the disconnect event fires exactly once and a stale task can
/// never tear down a newer connection that reused the same host id.
async fn close_connection(
    state: &Arc<Mutex<RouterState>>,
    app: &AppHandle,
    host_id: &str,
    connection_id: u64,
) {
    let connection = {
        let mut guard = state.lock().expect("router state poisoned");
        match guard.hosts.get(host_id) {
            Some(existing) if existing.connection_id == connection_id => {
                guard.hosts.remove(host_id)
            }
            _ => None,
        }
    };
    let Some(connection) = connection else {
        return;
    };

    reap_child(connection.child).await;
    tracing::info!(host_id, connection_id, "connection closed");
    let _ = app.emit(
        HOST_DISCONNECTED_EVENT,
        HostDisconnectedEvent {
            host_id: host_id.to_owned(),
        },
    );
}

async fn reap_child(child: Option<Child>) {
    let Some(mut child) = child else {
        return;
    };
    let _ = child.kill().await;
    match child.wait().await {
        Ok(status) => {
            tracing::info!(%status, "ssh transport exited");
        }
        Err(error) => {
            tracing::warn!(%error, "failed to wait for ssh transport exit");
        }
    }
}

struct ConnectionSetup {
    reader: Box<dyn AsyncBufRead + Unpin + Send>,
    writer: Box<dyn AsyncWrite + Unpin + Send>,
    child: Option<Child>,
}

async fn write_line<W: AsyncWriteExt + Unpin>(writer: &mut W, line: String) -> Result<(), String> {
    tracing::info!(line_len = line.len(), "proxy router sending line to host");

    writer
        .write_all(line.as_bytes())
        .await
        .map_err(|error| format!("failed to write host line: {error}"))?;
    writer
        .write_all(b"\n")
        .await
        .map_err(|error| format!("failed to terminate host line: {error}"))?;
    writer
        .flush()
        .await
        .map_err(|error| format!("failed to flush host line: {error}"))?;
    Ok(())
}

fn emit_error(app: &AppHandle, host_id: &str, message: String) -> tauri::Result<()> {
    app.emit(
        HOST_ERROR_EVENT,
        HostErrorEvent {
            host_id: host_id.to_owned(),
            message,
        },
    )
}

fn trim_line_ending(mut line: String) -> String {
    while line.ends_with('\n') || line.ends_with('\r') {
        line.pop();
    }
    line
}

async fn setup_connection_transport(
    host_id: &str,
    app: AppHandle,
    transport: HostTransportConfig,
    host: server::HostHandle,
) -> Result<ConnectionSetup, String> {
    match transport {
        HostTransportConfig::LocalEmbedded => {
            let (client_stream, server_stream) = tokio::io::duplex(8192);
            let config = server::ServerConfig::current();

            tokio::spawn(async move {
                match server::accept(&config, server_stream).await {
                    Ok(conn) => {
                        if let Err(e) = server::run_connection(conn, host).await {
                            tracing::error!(?e, "server connection loop failed");
                        }
                    }
                    Err(e) => {
                        tracing::error!(?e, "server handshake failed");
                    }
                }
            });

            let (read_half, write_half) = tokio::io::split(client_stream);
            Ok(ConnectionSetup {
                reader: Box::new(BufReader::new(read_half)),
                writer: Box::new(write_half),
                child: None,
            })
        }
        HostTransportConfig::SshStdio {
            ssh_destination,
            remote_command,
            lifecycle,
        } => {
            if ssh_destination.trim_start().starts_with('-') {
                return Err(format!(
                    "ssh destination for host {host_id} must not start with '-'"
                ));
            }
            let command = match lifecycle {
                RemoteHostLifecycleConfig::Manual => {
                    remote_command.unwrap_or_else(|| DEFAULT_REMOTE_HOST_COMMAND.to_string())
                }
                RemoteHostLifecycleConfig::ManagedTyde => managed_remote_bridge_command(),
            };
            let mut child = Command::new("ssh");
            child
                .arg("-T")
                .arg(&ssh_destination)
                .arg(&command)
                .stdin(std::process::Stdio::piped())
                .stdout(std::process::Stdio::piped())
                .stderr(std::process::Stdio::piped());

            let mut child = child.spawn().map_err(|err| {
                format!("failed to start ssh transport for host {host_id}: {err}")
            })?;

            let stdout = child
                .stdout
                .take()
                .ok_or_else(|| format!("ssh transport for host {host_id} has no stdout"))?;
            let stdin = child
                .stdin
                .take()
                .ok_or_else(|| format!("ssh transport for host {host_id} has no stdin"))?;

            if let Some(stderr) = child.stderr.take() {
                let app = app.clone();
                let host_id = host_id.to_string();
                tokio::spawn(async move {
                    let mut stderr = BufReader::new(stderr);
                    loop {
                        let mut line = String::new();
                        match stderr.read_line(&mut line).await {
                            Ok(0) => break,
                            Ok(_) => {
                                let line = trim_line_ending(line);
                                if line.is_empty() {
                                    continue;
                                }
                                let _ = emit_error(&app, &host_id, format!("ssh: {line}"));
                            }
                            Err(error) => {
                                let _ = emit_error(
                                    &app,
                                    &host_id,
                                    format!("failed to read ssh stderr: {error}"),
                                );
                                break;
                            }
                        }
                    }
                });
            }

            Ok(ConnectionSetup {
                reader: Box::new(BufReader::new(stdout)),
                writer: Box::new(stdin),
                child: Some(child),
            })
        }
    }
}

fn managed_remote_bridge_command() -> String {
    r#"set -eu
mkdir -p "$HOME/.tyde/logs"
bin="$HOME/.tyde/bin/current/tyde-server"
if [ ! -x "$bin" ]; then
  echo "managed Tyde bridge binary is not executable: $bin" >&2
  exit 1
fi
export TYDE_SOCKET_PATH="$HOME/.tyde/tyde.sock"
exec "$bin" host --bridge-uds 2>> "$HOME/.tyde/logs/tyde-host-bridge-uds.log"
"#
    .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn router() -> ProxyRouterHandle {
        ProxyRouterHandle::new()
    }

    #[test]
    fn trim_line_ending_strips_crlf() {
        assert_eq!(trim_line_ending("hello\r\n".to_string()), "hello");
        assert_eq!(trim_line_ending("hello\n".to_string()), "hello");
        assert_eq!(trim_line_ending("hello".to_string()), "hello");
    }

    #[tokio::test]
    async fn send_line_to_unknown_host_errors() {
        let router = router();
        let err = router
            .send_line("ghost".to_string(), "hi".to_string())
            .await
            .unwrap_err();
        assert!(err.contains("ghost"));
        assert!(err.contains("not connected"));
    }

    #[tokio::test]
    async fn send_line_rejects_embedded_newline() {
        let router = router();
        // Register a live connection by hand so we exercise the newline guard
        // rather than the "not connected" path.
        let (tx, mut rx) = mpsc::unbounded_channel();
        router.state.lock().unwrap().hosts.insert(
            "host".to_string(),
            Connection {
                connection_id: 1,
                outbound_tx: tx,
                child: None,
            },
        );

        let err = router
            .send_line("host".to_string(), "a\nb".to_string())
            .await
            .unwrap_err();
        assert!(err.contains("must not contain a newline"));
        // The bad frame must not have been enqueued.
        assert!(rx.try_recv().is_err());
    }

    #[tokio::test]
    async fn send_line_enqueues_to_writer_channel() {
        let router = router();
        let (tx, mut rx) = mpsc::unbounded_channel();
        router.state.lock().unwrap().hosts.insert(
            "host".to_string(),
            Connection {
                connection_id: 1,
                outbound_tx: tx,
                child: None,
            },
        );

        router
            .send_line("host".to_string(), "frame".to_string())
            .await
            .unwrap();
        assert_eq!(rx.recv().await.unwrap(), "frame");
    }

    #[tokio::test]
    async fn send_line_errors_when_writer_gone() {
        let router = router();
        let (tx, rx) = mpsc::unbounded_channel::<String>();
        router.state.lock().unwrap().hosts.insert(
            "host".to_string(),
            Connection {
                connection_id: 1,
                outbound_tx: tx,
                child: None,
            },
        );
        // Simulate the writer task having exited: its receiver is dropped.
        drop(rx);

        let err = router
            .send_line("host".to_string(), "frame".to_string())
            .await
            .unwrap_err();
        assert!(err.contains("no longer available"));
    }

    #[tokio::test]
    async fn disconnect_unknown_host_errors() {
        let router = router();
        let err = router.disconnect("ghost".to_string()).await.unwrap_err();
        assert!(err.contains("not connected"));
    }
}
