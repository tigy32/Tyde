use std::collections::HashMap;

use tauri::{AppHandle, Emitter};
use tokio::io::{AsyncBufRead, AsyncBufReadExt, AsyncWrite, AsyncWriteExt, BufReader};
use tokio::process::{Child, Command};
use tokio::sync::{mpsc, oneshot};

use crate::bridge::{
    HOST_DISCONNECTED_EVENT, HOST_ERROR_EVENT, HOST_LINE_EVENT, HostDisconnectedEvent,
    HostErrorEvent, HostLineEvent,
};
use crate::host_store::{HostTransportConfig, RemoteHostLifecycleConfig};

const DEFAULT_REMOTE_HOST_COMMAND: &str = "tyde host --bridge-uds";

#[derive(Clone)]
pub struct ProxyRouterHandle {
    tx: mpsc::Sender<RouterCommand>,
}

struct ConnectedHost {
    connection_id: u64,
    tx: mpsc::Sender<ConnectionCommand>,
}

enum RouterCommand {
    ConnectDuplex {
        app: AppHandle,
        host_id: String,
        transport: HostTransportConfig,
        host: server::HostHandle,
        reply: oneshot::Sender<Result<(), String>>,
    },
    Disconnect {
        host_id: String,
        reply: oneshot::Sender<Result<(), String>>,
    },
    SendLine {
        host_id: String,
        line: String,
        reply: oneshot::Sender<Result<(), String>>,
    },
    ConnectionClosed {
        host_id: String,
        connection_id: u64,
    },
}

enum ConnectionCommand {
    SendLine {
        line: String,
        reply: oneshot::Sender<Result<(), String>>,
    },
    Disconnect,
}

impl ProxyRouterHandle {
    pub fn new() -> Self {
        let (tx, rx) = mpsc::channel(64);
        let handle = Self { tx: tx.clone() };
        tauri::async_runtime::spawn(router_actor(tx, rx));
        handle
    }

    pub async fn connect_local(
        &self,
        app: AppHandle,
        host_id: String,
        transport: HostTransportConfig,
        host: server::HostHandle,
    ) -> Result<(), String> {
        let (reply_tx, reply_rx) = oneshot::channel();
        self.tx
            .send(RouterCommand::ConnectDuplex {
                app,
                host_id,
                transport,
                host,
                reply: reply_tx,
            })
            .await
            .map_err(|_| "proxy router is unavailable".to_owned())?;
        reply_rx
            .await
            .map_err(|_| "proxy router dropped connect reply".to_owned())?
    }

    pub async fn disconnect(&self, host_id: String) -> Result<(), String> {
        let (reply_tx, reply_rx) = oneshot::channel();
        self.tx
            .send(RouterCommand::Disconnect {
                host_id,
                reply: reply_tx,
            })
            .await
            .map_err(|_| "proxy router is unavailable".to_owned())?;
        reply_rx
            .await
            .map_err(|_| "proxy router dropped disconnect reply".to_owned())?
    }

    pub async fn send_line(&self, host_id: String, line: String) -> Result<(), String> {
        let (reply_tx, reply_rx) = oneshot::channel();
        self.tx
            .send(RouterCommand::SendLine {
                host_id,
                line,
                reply: reply_tx,
            })
            .await
            .map_err(|_| "proxy router is unavailable".to_owned())?;
        reply_rx
            .await
            .map_err(|_| "proxy router dropped send reply".to_owned())?
    }
}

async fn router_actor(
    router_tx: mpsc::Sender<RouterCommand>,
    mut rx: mpsc::Receiver<RouterCommand>,
) {
    let mut hosts: HashMap<String, ConnectedHost> = HashMap::new();
    let mut next_connection_id = 1_u64;

    while let Some(command) = rx.recv().await {
        match command {
            RouterCommand::ConnectDuplex {
                app,
                host_id,
                transport,
                host,
                reply,
            } => {
                tracing::info!(host_id, "connect_duplex requested");
                if let Some(existing) = hosts.remove(&host_id) {
                    tracing::info!(host_id, "replacing existing host connection");
                    let _ = existing.tx.send(ConnectionCommand::Disconnect).await;
                }

                let connection_id = next_connection_id;
                next_connection_id += 1;

                let setup = match setup_connection_transport(&app, &host_id, transport, host).await
                {
                    Ok(setup) => setup,
                    Err(err) => {
                        let _ = reply.send(Err(err));
                        continue;
                    }
                };
                let (connection_tx, connection_rx) = mpsc::channel(64);
                tokio::spawn(connection_actor(
                    ConnectionActorContext {
                        app,
                        host_id: host_id.clone(),
                        connection_id,
                        rx: connection_rx,
                        router_tx: router_tx.clone(),
                        cleanup: setup.cleanup,
                    },
                    setup.reader,
                    setup.writer,
                ));
                hosts.insert(
                    host_id.clone(),
                    ConnectedHost {
                        connection_id,
                        tx: connection_tx,
                    },
                );
                tracing::info!(host_id, "connected via duplex");
                let _ = reply.send(Ok(()));
            }
            RouterCommand::Disconnect { host_id, reply } => {
                let Some(connected) = hosts.remove(&host_id) else {
                    let _ = reply.send(Err(format!("host {host_id} is not connected")));
                    continue;
                };

                connected
                    .tx
                    .send(ConnectionCommand::Disconnect)
                    .await
                    .expect("connection task channel closed before disconnect was sent");
                let _ = reply.send(Ok(()));
            }
            RouterCommand::SendLine {
                host_id,
                line,
                reply,
            } => {
                let Some(connected) = hosts.get(&host_id) else {
                    let _ = reply.send(Err(format!("host {host_id} is not connected")));
                    continue;
                };

                let _ = connected
                    .tx
                    .send(ConnectionCommand::SendLine { line, reply })
                    .await;
            }
            RouterCommand::ConnectionClosed {
                host_id,
                connection_id,
            } => {
                let should_remove = hosts
                    .get(&host_id)
                    .map(|connected| connected.connection_id == connection_id)
                    .unwrap_or(false);
                tracing::info!(host_id, connection_id, should_remove, "connection closed");
                if should_remove {
                    hosts.remove(&host_id);
                }
            }
        }
    }
}

struct ConnectionSetup {
    reader: Box<dyn AsyncBufRead + Unpin + Send>,
    writer: Box<dyn AsyncWrite + Unpin + Send>,
    cleanup: ConnectionCleanup,
}

struct ConnectionActorContext {
    app: AppHandle,
    host_id: String,
    connection_id: u64,
    rx: mpsc::Receiver<ConnectionCommand>,
    router_tx: mpsc::Sender<RouterCommand>,
    cleanup: ConnectionCleanup,
}

enum ConnectionCleanup {
    None,
    Child(Child),
}

async fn connection_actor<R, W>(context: ConnectionActorContext, mut reader: R, mut writer: W)
where
    R: AsyncBufRead + Unpin + Send,
    W: AsyncWrite + Unpin + Send,
{
    let ConnectionActorContext {
        app,
        host_id,
        connection_id,
        mut rx,
        router_tx,
        mut cleanup,
    } = context;

    loop {
        let mut incoming = String::new();
        tokio::select! {
            read_result = reader.read_line(&mut incoming) => {
                match read_result {
                    Ok(0) => break,
                    Ok(_) => {
                        let line = trim_line_ending(incoming);
                        tracing::info!(
                            host_id,
                            line_len = line.len(),
                            "proxy router received line from host"
                        );
                        if let Err(error) = app.emit(
                            HOST_LINE_EVENT,
                            HostLineEvent {
                                host_id: host_id.clone(),
                                line,
                            },
                        ) {
                            let _ = emit_error(&app, &host_id, format!("failed to emit host line: {error}"));
                        }
                    }
                    Err(error) => {
                        let _ = emit_error(&app, &host_id, format!("failed to read from host connection: {error}"));
                        break;
                    }
                }
            }
            command = rx.recv() => {
                match command {
                    Some(ConnectionCommand::SendLine { line, reply }) => {
                        let result = write_line(&mut writer, line).await;
                        let _ = reply.send(result);
                    }
                    Some(ConnectionCommand::Disconnect) | None => break,
                }
            }
        }
    }

    match &mut cleanup {
        ConnectionCleanup::None => {}
        ConnectionCleanup::Child(child) => {
            let _ = child.kill().await;
            let _ = child.wait().await;
        }
    }

    let _ = app.emit(
        HOST_DISCONNECTED_EVENT,
        HostDisconnectedEvent {
            host_id: host_id.clone(),
        },
    );
    let _ = router_tx
        .send(RouterCommand::ConnectionClosed {
            host_id,
            connection_id,
        })
        .await;
}

async fn write_line<W: AsyncWriteExt + Unpin>(writer: &mut W, line: String) -> Result<(), String> {
    if line.contains('\n') {
        return Err("host line must not contain a newline".to_owned());
    }

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
    app: &AppHandle,
    host_id: &str,
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
                cleanup: ConnectionCleanup::None,
            })
        }
        HostTransportConfig::SshStdio {
            ssh_destination,
            remote_command,
            lifecycle,
        } => {
            let command = match lifecycle {
                RemoteHostLifecycleConfig::Manual => {
                    remote_command.unwrap_or_else(|| DEFAULT_REMOTE_HOST_COMMAND.to_string())
                }
                RemoteHostLifecycleConfig::ManagedTyde { .. } => managed_remote_bridge_command(),
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
                                tracing::warn!(host_id, "{line}");
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
                cleanup: ConnectionCleanup::Child(child),
            })
        }
    }
}

fn managed_remote_bridge_command() -> String {
    r#"set -eu
mkdir -p "$HOME/.tyde/logs"
bin="$HOME/.tyde/bin/current/tyde"
if [ ! -x "$bin" ]; then
  echo "managed Tyde bridge binary is not executable: $bin" >&2
  exit 1
fi
export TYDE_SOCKET_PATH="$HOME/.tyde/tyde.sock"
exec "$bin" host --bridge-uds 2>> "$HOME/.tyde/logs/tyde-host-bridge-uds.log"
"#
    .to_string()
}
