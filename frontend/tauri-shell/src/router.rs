use std::collections::HashMap;

use tauri::{AppHandle, Emitter};
use tokio::io::{AsyncBufRead, AsyncBufReadExt, AsyncWrite, AsyncWriteExt, BufReader};
use tokio::sync::{mpsc, oneshot};

use crate::bridge::{
    HOST_DISCONNECTED_EVENT, HOST_ERROR_EVENT, HOST_LINE_EVENT, HostDisconnectedEvent,
    HostErrorEvent, HostLineEvent,
};

#[derive(Clone)]
pub struct ProxyRouterHandle {
    tx: mpsc::Sender<RouterCommand>,
}

struct ConnectedHost {
    tx: mpsc::Sender<ConnectionCommand>,
}

enum RouterCommand {
    ConnectDuplex {
        app: AppHandle,
        host_id: String,
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
        host: server::HostHandle,
    ) -> Result<(), String> {
        let (reply_tx, reply_rx) = oneshot::channel();
        self.tx
            .send(RouterCommand::ConnectDuplex {
                app,
                host_id,
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

    while let Some(command) = rx.recv().await {
        match command {
            RouterCommand::ConnectDuplex {
                app,
                host_id,
                host,
                reply,
            } => {
                tracing::info!(host_id, "connect_duplex requested");
                if hosts.contains_key(&host_id) {
                    tracing::warn!(host_id, "host already connected");
                    let _ = reply.send(Err(format!("host {host_id} is already connected")));
                    continue;
                }

                // Create an in-process duplex stream — one end for the shell,
                // one end for the server. Exactly like the test fixture.
                let (client_stream, server_stream) = tokio::io::duplex(8192);
                let config = server::ServerConfig::current();

                // Hand the server end to the server's accept + run_connection loop.
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

                // Use the client end for our connection actor.
                let (read_half, write_half) = tokio::io::split(client_stream);
                let (connection_tx, connection_rx) = mpsc::channel(64);
                tokio::spawn(connection_actor(
                    app,
                    host_id.clone(),
                    BufReader::new(read_half),
                    write_half,
                    connection_rx,
                    router_tx.clone(),
                ));
                hosts.insert(host_id.clone(), ConnectedHost { tx: connection_tx });
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
                    .map_err(|_| "connection task is unavailable".to_owned())
                    .unwrap_or(());
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
            RouterCommand::ConnectionClosed { host_id } => {
                tracing::info!(host_id, "connection closed");
                hosts.remove(&host_id);
            }
        }
    }
}

async fn connection_actor<R, W>(
    app: AppHandle,
    host_id: String,
    mut reader: R,
    mut writer: W,
    mut rx: mpsc::Receiver<ConnectionCommand>,
    router_tx: mpsc::Sender<RouterCommand>,
) where
    R: AsyncBufRead + Unpin + Send,
    W: AsyncWrite + Unpin + Send,
{
    loop {
        let mut incoming = String::new();
        tokio::select! {
            read_result = reader.read_line(&mut incoming) => {
                match read_result {
                    Ok(0) => break,
                    Ok(_) => {
                        let line = trim_line_ending(incoming);
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

    let _ = app.emit(
        HOST_DISCONNECTED_EVENT,
        HostDisconnectedEvent {
            host_id: host_id.clone(),
        },
    );
    let _ = router_tx
        .send(RouterCommand::ConnectionClosed { host_id })
        .await;
}

async fn write_line<W: AsyncWriteExt + Unpin>(writer: &mut W, line: String) -> Result<(), String> {
    if line.contains('\n') {
        return Err("host line must not contain a newline".to_owned());
    }

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
