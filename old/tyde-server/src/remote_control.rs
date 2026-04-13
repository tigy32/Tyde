#[cfg(unix)]
use std::collections::HashMap;
use std::future::Future;
#[cfg(unix)]
use std::os::unix::net::UnixListener as StdUnixListener;
use std::path::PathBuf;
use std::pin::Pin;
#[cfg(unix)]
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
#[cfg(unix)]
use std::thread;

#[cfg(unix)]
use tokio::io::{AsyncBufRead, AsyncBufReadExt, AsyncWrite, AsyncWriteExt, BufReader};
#[cfg(unix)]
use tokio::net::UnixListener;
#[cfg(unix)]
use tokio::runtime::{Builder as RuntimeBuilder, Handle as RuntimeHandle};
use tokio::sync::{broadcast, Mutex};
#[cfg(unix)]
use tokio::sync::{mpsc, oneshot, Semaphore};
#[cfg(unix)]
use tokio::task::{JoinHandle, JoinSet};
use tyde_protocol::protocol::{ChatEventPayload, ClientFrame, ServerFrame, PROTOCOL_VERSION};

use crate::chat_buffer::ChatEventBuffer;
use crate::invoke::InvokeRequest;

#[cfg(unix)]
const MAX_IN_FLIGHT_INVOKES: usize = 1024;

#[cfg(unix)]
enum AcceptTask {
    Tokio {
        handle: JoinHandle<()>,
        shutdown_tx: Option<oneshot::Sender<()>>,
    },
    Thread {
        handle: thread::JoinHandle<()>,
        shutdown_tx: Option<oneshot::Sender<()>>,
    },
}

#[cfg(unix)]
impl AcceptTask {
    fn stop(self) {
        match self {
            Self::Tokio {
                handle,
                mut shutdown_tx,
            } => {
                if let Some(tx) = shutdown_tx.take() {
                    let _ = tx.send(());
                }
                handle.abort();
            }
            Self::Thread {
                handle,
                mut shutdown_tx,
            } => {
                if let Some(tx) = shutdown_tx.take() {
                    let _ = tx.send(());
                }
                let _ = handle.join();
            }
        }
    }

    fn is_finished(&self) -> bool {
        match self {
            Self::Tokio { handle, .. } => handle.is_finished(),
            Self::Thread { handle, .. } => handle.is_finished(),
        }
    }
}

pub type BoxFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

pub trait RemoteControlDelegate: Clone + Send + Sync + 'static {
    fn handshake_result<'a>(
        &'a self,
        instance_id: &'a str,
        chat_buffer: Arc<parking_lot::Mutex<ChatEventBuffer>>,
    ) -> BoxFuture<'a, Result<serde_json::Value, String>>;

    fn replay_agent_frames<'a>(
        &'a self,
        last_agent_seq: u64,
    ) -> BoxFuture<'a, Result<Vec<ServerFrame>, String>>;

    fn dispatch_invoke<'a>(
        &'a self,
        request: InvokeRequest,
    ) -> BoxFuture<'a, Result<serde_json::Value, String>>;
}

pub struct RemoteControlServer<D> {
    socket_path: PathBuf,
    #[cfg(unix)]
    instance_id: String,
    #[cfg(unix)]
    accept_task: parking_lot::Mutex<Option<AcceptTask>>,
    pub event_broadcast: broadcast::Sender<ServerFrame>,
    pub chat_buffer: Arc<parking_lot::Mutex<ChatEventBuffer>>,
    #[cfg(unix)]
    clients: Arc<Mutex<Vec<u64>>>,
    #[cfg(unix)]
    next_client_id: Arc<AtomicU64>,
    delegate: D,
    tyde_version: String,
}

#[cfg(unix)]
impl<D: RemoteControlDelegate> RemoteControlServer<D> {
    pub fn start(delegate: D, tyde_version: &str) -> Result<Self, String> {
        let socket_path = resolve_socket_path()?;

        if socket_path.exists() {
            std::fs::remove_file(&socket_path)
                .map_err(|e| format!("Failed to remove stale socket: {e}"))?;
        }

        let (event_tx, _) = broadcast::channel::<ServerFrame>(1024);
        let chat_buffer = Arc::new(parking_lot::Mutex::new(ChatEventBuffer::new()));
        let clients: Arc<Mutex<Vec<u64>>> = Arc::new(Mutex::new(Vec::new()));

        let server = Self {
            socket_path,
            instance_id: uuid::Uuid::new_v4().to_string(),
            accept_task: parking_lot::Mutex::new(None),
            event_broadcast: event_tx,
            chat_buffer,
            clients,
            next_client_id: Arc::new(AtomicU64::new(1)),
            delegate,
            tyde_version: tyde_version.to_string(),
        };

        server.start_listening()?;
        Ok(server)
    }

    pub fn start_listening(&self) -> Result<(), String> {
        if self.is_running() {
            return Ok(());
        }

        if self.socket_path.exists() {
            std::fs::remove_file(&self.socket_path)
                .map_err(|e| format!("Failed to remove stale socket: {e}"))?;
        }

        let listener = StdUnixListener::bind(&self.socket_path)
            .map_err(|e| format!("Failed to bind UDS at {}: {e}", self.socket_path.display()))?;
        listener
            .set_nonblocking(true)
            .map_err(|e| format!("Failed to set UDS listener nonblocking: {e}"))?;

        tracing::info!("Remote control listening on {}", self.socket_path.display());

        let socket_path = self.socket_path.clone();
        let instance_id = self.instance_id.clone();
        let event_broadcast = self.event_broadcast.clone();
        let chat_buffer = self.chat_buffer.clone();
        let clients = self.clients.clone();
        let next_client_id = self.next_client_id.clone();
        let delegate = self.delegate.clone();
        let tyde_version = self.tyde_version.clone();
        let (shutdown_tx, shutdown_rx) = oneshot::channel();
        let accept_handle = if let Ok(handle) = RuntimeHandle::try_current() {
            AcceptTask::Tokio {
                handle: handle.spawn(async move {
                    run_accept_loop(
                        listener,
                        socket_path,
                        instance_id,
                        event_broadcast,
                        chat_buffer,
                        clients,
                        next_client_id,
                        delegate,
                        tyde_version,
                        shutdown_rx,
                    )
                    .await;
                }),
                shutdown_tx: Some(shutdown_tx),
            }
        } else {
            AcceptTask::Thread {
                handle: thread::spawn(move || {
                    let runtime = match RuntimeBuilder::new_current_thread().enable_all().build() {
                        Ok(runtime) => runtime,
                        Err(err) => {
                            tracing::warn!(
                                "Remote control server failed to create tokio runtime: {err}"
                            );
                            let _ = std::fs::remove_file(socket_path);
                            return;
                        }
                    };
                    runtime.block_on(async move {
                        run_accept_loop(
                            listener,
                            socket_path,
                            instance_id,
                            event_broadcast,
                            chat_buffer,
                            clients,
                            next_client_id,
                            delegate,
                            tyde_version,
                            shutdown_rx,
                        )
                        .await;
                    });
                }),
                shutdown_tx: Some(shutdown_tx),
            }
        };
        *self.accept_task.lock() = Some(accept_handle);
        Ok(())
    }

    pub fn socket_path(&self) -> &PathBuf {
        &self.socket_path
    }

    pub async fn connected_client_count(&self) -> usize {
        self.clients.lock().await.len()
    }

    pub async fn connect_in_memory(&self) -> tokio::io::DuplexStream {
        let (server_stream, client_stream) = tokio::io::duplex(65_536);
        let client_id = self.next_client_id.fetch_add(1, Ordering::Relaxed);
        tracing::info!("Remote client {client_id} connected");
        self.clients.lock().await.push(client_id);

        let event_rx = self.event_broadcast.subscribe();
        let chat_buffer = self.chat_buffer.clone();
        let clients = self.clients.clone();
        let instance_id = self.instance_id.clone();
        let delegate = self.delegate.clone();
        let tyde_version = self.tyde_version.clone();

        tokio::spawn(async move {
            let (read_half, write_half) = tokio::io::split(server_stream);
            let reader = BufReader::new(read_half);
            if let Err(err) = handle_client(
                reader,
                write_half,
                client_id,
                event_rx,
                chat_buffer,
                instance_id,
                delegate,
                tyde_version,
            )
            .await
            {
                tracing::warn!("Remote client {client_id} error: {err}");
            }
            tracing::info!("Remote client {client_id} disconnected");
            clients.lock().await.retain(|id| *id != client_id);
        });

        client_stream
    }

    pub fn shutdown(&self) {
        if let Some(handle) = self.accept_task.lock().take() {
            handle.stop();
        }
        let _ = std::fs::remove_file(&self.socket_path);
        tracing::info!("Remote control server stopped");
    }

    pub fn is_running(&self) -> bool {
        self.accept_task
            .lock()
            .as_ref()
            .is_some_and(|h| !h.is_finished())
            && self.socket_path.exists()
    }
}

#[cfg(unix)]
fn resolve_socket_path() -> Result<PathBuf, String> {
    if let Ok(path) = std::env::var("TYDE_SOCKET_PATH") {
        let socket_path = PathBuf::from(path);
        let parent = socket_path
            .parent()
            .ok_or("TYDE_SOCKET_PATH must include a parent directory".to_string())?;
        std::fs::create_dir_all(parent)
            .map_err(|e| format!("Failed to create {}: {e}", parent.display()))?;
        return Ok(socket_path);
    }

    let home = dirs::home_dir().ok_or("Cannot determine home directory")?;
    let socket_dir = home.join(".tyde");
    std::fs::create_dir_all(&socket_dir).map_err(|e| format!("Failed to create ~/.tyde: {e}"))?;
    Ok(socket_dir.join("tyde.sock"))
}

#[cfg(not(unix))]
impl<D> RemoteControlServer<D> {
    pub fn start(_delegate: D, _tyde_version: &str) -> Result<Self, String> {
        Err("Remote control requires Unix domain sockets (not available on this platform)".into())
    }

    pub fn start_listening(&self) -> Result<(), String> {
        Err("Remote control requires Unix domain sockets (not available on this platform)".into())
    }

    pub fn socket_path(&self) -> &PathBuf {
        &self.socket_path
    }

    pub async fn connected_client_count(&self) -> usize {
        0
    }

    pub async fn connect_in_memory(&self) -> tokio::io::DuplexStream {
        panic!("Remote control requires Unix domain sockets (not available on this platform)")
    }

    pub fn shutdown(&self) {}

    pub fn is_running(&self) -> bool {
        false
    }
}

#[cfg(unix)]
impl<D> Drop for RemoteControlServer<D> {
    fn drop(&mut self) {
        if let Some(handle) = self.accept_task.get_mut().take() {
            handle.stop();
        }
        let _ = std::fs::remove_file(&self.socket_path);
    }
}

#[cfg(unix)]
#[allow(clippy::too_many_arguments)]
async fn run_accept_loop<D: RemoteControlDelegate>(
    listener: StdUnixListener,
    socket_path: PathBuf,
    instance_id: String,
    event_tx: broadcast::Sender<ServerFrame>,
    chat_buffer: Arc<parking_lot::Mutex<ChatEventBuffer>>,
    clients: Arc<Mutex<Vec<u64>>>,
    next_client_id: Arc<AtomicU64>,
    delegate: D,
    tyde_version: String,
    mut shutdown_rx: oneshot::Receiver<()>,
) {
    let listener = match UnixListener::from_std(listener) {
        Ok(listener) => listener,
        Err(err) => {
            tracing::warn!("Remote control server failed to create async listener: {err}");
            let _ = std::fs::remove_file(socket_path);
            return;
        }
    };

    tokio::select! {
        _ = accept_loop(
            listener,
            instance_id,
            event_tx,
            chat_buffer,
            clients,
            next_client_id,
            delegate,
            tyde_version,
        ) => {}
        _ = &mut shutdown_rx => {}
    }

    let _ = std::fs::remove_file(socket_path);
}

#[cfg(unix)]
#[allow(clippy::too_many_arguments)]
async fn accept_loop<D: RemoteControlDelegate>(
    listener: UnixListener,
    instance_id: String,
    event_tx: broadcast::Sender<ServerFrame>,
    chat_buffer: Arc<parking_lot::Mutex<ChatEventBuffer>>,
    clients: Arc<Mutex<Vec<u64>>>,
    next_client_id: Arc<AtomicU64>,
    delegate: D,
    tyde_version: String,
) {
    loop {
        let (stream, _) = match listener.accept().await {
            Ok(client) => client,
            Err(err) => {
                tracing::warn!("UDS accept error: {err}");
                continue;
            }
        };

        let client_id = next_client_id.fetch_add(1, Ordering::Relaxed);
        tracing::info!("Remote client {client_id} connected");
        clients.lock().await.push(client_id);

        let event_rx = event_tx.subscribe();
        let chat_buffer = chat_buffer.clone();
        let clients = clients.clone();
        let instance_id_for_client = instance_id.clone();
        let delegate = delegate.clone();
        let tyde_version = tyde_version.clone();

        tokio::spawn(async move {
            let (read_half, write_half) = stream.into_split();
            let reader = BufReader::new(read_half);
            if let Err(err) = handle_client(
                reader,
                write_half,
                client_id,
                event_rx,
                chat_buffer,
                instance_id_for_client,
                delegate,
                tyde_version,
            )
            .await
            {
                tracing::warn!("Remote client {client_id} error: {err}");
            }
            tracing::info!("Remote client {client_id} disconnected");
            clients.lock().await.retain(|id| *id != client_id);
        });
    }
}

#[cfg(unix)]
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
enum InvokeLaneKey {
    Agent(String),
    Terminal(u64),
}

#[cfg(unix)]
#[derive(Clone)]
struct LaneInvoke {
    req_id: u64,
    request: InvokeRequest,
}

#[cfg(unix)]
struct LaneWorker {
    tx: mpsc::UnboundedSender<LaneInvoke>,
    handle: tokio::task::JoinHandle<()>,
}

#[cfg(unix)]
fn parse_session_lane(request: &InvokeRequest) -> Option<InvokeLaneKey> {
    let agent_id = request.session_lane_id()?.trim();
    if agent_id.is_empty() {
        return None;
    }
    Some(InvokeLaneKey::Agent(agent_id.to_string()))
}

#[cfg(unix)]
fn parse_agent_lane(request: &InvokeRequest) -> Option<InvokeLaneKey> {
    let agent_id = request.agent_lane_id()?.trim();
    if agent_id.is_empty() {
        return None;
    }
    Some(InvokeLaneKey::Agent(agent_id.to_string()))
}

#[cfg(unix)]
fn parse_terminal_lane(request: &InvokeRequest) -> Option<InvokeLaneKey> {
    Some(InvokeLaneKey::Terminal(request.terminal_lane_id()?))
}

#[cfg(unix)]
fn invoke_lane_key(request: &InvokeRequest) -> Option<InvokeLaneKey> {
    parse_session_lane(request)
        .or_else(|| parse_agent_lane(request))
        .or_else(|| parse_terminal_lane(request))
}

#[cfg(unix)]
fn spawn_lane_worker<D, W>(
    delegate: D,
    writer: Arc<Mutex<W>>,
    client_id: u64,
    lane: InvokeLaneKey,
    invoke_permits: Arc<Semaphore>,
) -> LaneWorker
where
    D: RemoteControlDelegate,
    W: AsyncWrite + Unpin + Send + 'static,
{
    let (tx, mut rx) = mpsc::unbounded_channel::<LaneInvoke>();
    let handle = tokio::spawn(async move {
        while let Some(job) = rx.recv().await {
            let permit = match invoke_permits.clone().acquire_owned().await {
                Ok(permit) => permit,
                Err(_) => break,
            };
            let _permit = permit;

            let response = dispatch(&delegate, job.req_id, job.request).await;

            if let Err(err) = send(&writer, &response).await {
                tracing::debug!(
                    "Client {client_id}: failed to send response from lane {:?}: {}",
                    lane,
                    err
                );
                break;
            }
        }
    });
    LaneWorker { tx, handle }
}

#[cfg(unix)]
#[allow(clippy::too_many_arguments)]
async fn handle_client<D, R, W>(
    mut reader: R,
    writer: W,
    client_id: u64,
    mut event_rx: broadcast::Receiver<ServerFrame>,
    chat_buffer: Arc<parking_lot::Mutex<ChatEventBuffer>>,
    instance_id: String,
    delegate: D,
    tyde_version: String,
) -> Result<(), String>
where
    D: RemoteControlDelegate,
    R: AsyncBufRead + Unpin + Send + 'static,
    W: AsyncWrite + Unpin + Send + 'static,
{
    let writer = Arc::new(Mutex::new(writer));

    let mut line = String::new();
    reader
        .read_line(&mut line)
        .await
        .map_err(|e| format!("Read handshake: {e}"))?;

    let handshake: ClientFrame =
        serde_json::from_str(line.trim()).map_err(|e| format!("Invalid handshake: {e}"))?;
    tracing::info!(
        target: "tyde_server::remote_control::protocol",
        client_id,
        direction = "in",
        frame = ?handshake,
        "Remote control frame received"
    );

    let (req_id, client_tyde_version, last_agent_seq, last_chat_seqs_raw) = match handshake {
        ClientFrame::Handshake {
            req_id,
            protocol_version,
            tyde_version: client_tyde_version,
            last_agent_event_seq,
            last_chat_event_seqs,
        } => {
            if protocol_version != PROTOCOL_VERSION {
                send(
                    &writer,
                    &ServerFrame::Error {
                        req_id,
                        error: format!(
                            "Protocol mismatch: client={protocol_version} server={PROTOCOL_VERSION}"
                        ),
                    },
                )
                .await?;
                return Err("Protocol version mismatch".into());
            }
            (
                req_id,
                client_tyde_version,
                last_agent_event_seq,
                last_chat_event_seqs,
            )
        }
        _ => return Err("First message must be Handshake".into()),
    };
    if client_tyde_version != tyde_version {
        tracing::warn!(
            "Tyde client/server version mismatch: client={} server={}",
            client_tyde_version,
            tyde_version
        );
    }
    let last_chat_seqs = last_chat_seqs_raw;

    let handshake_result = delegate
        .handshake_result(&instance_id, chat_buffer.clone())
        .await?;
    send(
        &writer,
        &ServerFrame::Result {
            req_id,
            data: handshake_result,
        },
    )
    .await?;

    for frame in delegate.replay_agent_frames(last_agent_seq).await? {
        send(&writer, &frame).await?;
    }

    {
        let replay_frames: Vec<ServerFrame> = {
            let buf = chat_buffer.lock();
            buf.all_events_since(&last_chat_seqs)
                .into_iter()
                .map(|entry| ServerFrame::Event {
                    event: "chat-event".into(),
                    seq: Some(entry.seq),
                    payload: serde_json::to_value(ChatEventPayload {
                        conversation_id: entry.agent_id.clone(),
                        event: entry.event.clone(),
                    })
                    .unwrap_or_default(),
                })
                .collect()
        };
        for frame in &replay_frames {
            send(&writer, frame).await?;
        }
    }

    let w2 = writer.clone();
    let forwarder = tokio::spawn(async move {
        loop {
            match event_rx.recv().await {
                Ok(frame) => {
                    if send(&w2, &frame).await.is_err() {
                        break;
                    }
                }
                Err(broadcast::error::RecvError::Lagged(n)) => {
                    tracing::warn!("Client {client_id} lagged {n} events");
                }
                Err(broadcast::error::RecvError::Closed) => break,
            }
        }
    });

    let invoke_permits = Arc::new(Semaphore::new(MAX_IN_FLIGHT_INVOKES));
    let mut lane_workers: HashMap<InvokeLaneKey, LaneWorker> = HashMap::new();
    let mut direct_invoke_tasks: JoinSet<()> = JoinSet::new();

    let mut buf = String::new();
    loop {
        while let Some(joined) = direct_invoke_tasks.try_join_next() {
            if let Err(err) = joined {
                tracing::warn!("Client {client_id}: direct invoke task failed: {err}");
            }
        }

        buf.clear();
        let n = reader
            .read_line(&mut buf)
            .await
            .map_err(|e| format!("Read: {e}"))?;
        if n == 0 {
            break;
        }

        let frame: ClientFrame = match serde_json::from_str(buf.trim()) {
            Ok(frame) => frame,
            Err(err) => {
                tracing::warn!("Client {client_id}: bad frame: {err}");
                tracing::info!(
                    target: "tyde_server::remote_control::protocol",
                    client_id,
                    direction = "in",
                    raw = %buf.trim(),
                    error = %err,
                    "Invalid remote control frame received"
                );
                continue;
            }
        };
        tracing::info!(
            target: "tyde_server::remote_control::protocol",
            client_id,
            direction = "in",
            frame = ?frame,
            "Remote control frame received"
        );

        match frame {
            ClientFrame::Handshake { req_id, .. } => {
                send(
                    &writer,
                    &ServerFrame::Error {
                        req_id,
                        error: "Handshake already completed".into(),
                    },
                )
                .await?;
            }
            ClientFrame::Invoke {
                req_id,
                command,
                params,
            } => {
                let request = match InvokeRequest::parse(&command, params) {
                    Ok(request) => request,
                    Err(error) => {
                        send(&writer, &ServerFrame::Error { req_id, error }).await?;
                        continue;
                    }
                };

                if let Some(lane) = invoke_lane_key(&request) {
                    let job = LaneInvoke { req_id, request };

                    let mut sent = false;
                    if let Some(existing) = lane_workers.get(&lane) {
                        if existing.tx.send(job.clone()).is_ok() {
                            sent = true;
                        } else {
                            tracing::debug!(
                                "Client {client_id}: lane {:?} worker channel closed; recreating",
                                lane
                            );
                        }
                    }
                    if !sent {
                        let worker = spawn_lane_worker(
                            delegate.clone(),
                            writer.clone(),
                            client_id,
                            lane.clone(),
                            invoke_permits.clone(),
                        );
                        if worker.tx.send(job).is_err() {
                            return Err(format!(
                                "Client {client_id}: failed to enqueue invoke in lane {:?}",
                                lane
                            ));
                        }
                        lane_workers.insert(lane, worker);
                    }
                    continue;
                }

                let delegate_for_invoke = delegate.clone();
                let writer_for_invoke = writer.clone();
                let permits_for_invoke = invoke_permits.clone();
                direct_invoke_tasks.spawn(async move {
                    let permit = match permits_for_invoke.acquire_owned().await {
                        Ok(permit) => permit,
                        Err(_) => return,
                    };
                    let _permit = permit;
                    let response = dispatch(&delegate_for_invoke, req_id, request).await;
                    let _ = send(&writer_for_invoke, &response).await;
                });
            }
        }
    }

    direct_invoke_tasks.abort_all();
    while let Some(joined) = direct_invoke_tasks.join_next().await {
        if let Err(err) = joined {
            tracing::debug!("Client {client_id}: direct invoke task join error: {err}");
        }
    }
    for (_, worker) in lane_workers.drain() {
        worker.handle.abort();
    }
    forwarder.abort();
    Ok(())
}

#[cfg(unix)]
async fn dispatch<D: RemoteControlDelegate>(
    delegate: &D,
    req_id: u64,
    request: InvokeRequest,
) -> ServerFrame {
    tracing::info!(
        target: "tyde_server::remote_control::dispatch",
        req_id,
        request = ?request,
        "Dispatching remote control invoke"
    );
    let response = match delegate.dispatch_invoke(request).await {
        Ok(data) => ServerFrame::Result { req_id, data },
        Err(error) => ServerFrame::Error { req_id, error },
    };
    tracing::info!(
        target: "tyde_server::remote_control::dispatch",
        req_id,
        response = ?response,
        "Remote control invoke completed"
    );
    response
}

#[cfg(unix)]
async fn send<W>(writer: &Arc<Mutex<W>>, frame: &ServerFrame) -> Result<(), String>
where
    W: AsyncWrite + Unpin + Send + 'static,
{
    tracing::info!(
        target: "tyde_server::remote_control::protocol",
        direction = "out",
        frame = ?frame,
        "Remote control frame sent"
    );
    let mut line = serde_json::to_string(frame).map_err(|e| format!("Serialize: {e}"))?;
    line.push('\n');
    let mut w = writer.lock().await;
    w.write_all(line.as_bytes())
        .await
        .map_err(|e| format!("Write: {e}"))?;
    w.flush().await.map_err(|e| format!("Flush: {e}"))?;
    Ok(())
}

#[cfg(all(test, unix))]
mod tests {
    use serde_json::json;

    use crate::invoke::InvokeRequest;

    use super::{invoke_lane_key, InvokeLaneKey};

    #[test]
    fn invoke_lane_key_routes_session_mutations() {
        let request = InvokeRequest::parse(
            "send_message",
            json!({
                "conversationId": "42",
                "message": "hello",
            }),
        )
        .expect("request should parse");
        let lane = invoke_lane_key(&request);
        assert_eq!(lane, Some(InvokeLaneKey::Agent("42".to_string())));
    }

    #[test]
    fn invoke_lane_key_routes_agent_mutations() {
        let request = InvokeRequest::parse(
            "interrupt_agent",
            json!({
                "agent_id": "agent-123",
            }),
        )
        .expect("request should parse");
        let lane = invoke_lane_key(&request);
        assert_eq!(lane, Some(InvokeLaneKey::Agent("agent-123".to_string())));
    }

    #[test]
    fn invoke_lane_key_routes_terminal_mutations() {
        let request = InvokeRequest::parse(
            "write_terminal",
            json!({
                "terminal_id": 7,
                "data": "ls\n",
            }),
        )
        .expect("request should parse");
        let lane = invoke_lane_key(&request);
        assert_eq!(lane, Some(InvokeLaneKey::Terminal(7)));
    }

    #[test]
    fn invoke_lane_key_does_not_lane_wait_calls() {
        let request = InvokeRequest::parse(
            "wait_for_agent",
            json!({
                "agent_id": "agent-123",
            }),
        )
        .expect("request should parse");
        let lane = invoke_lane_key(&request);
        assert_eq!(lane, None);
    }

    #[test]
    fn invoke_request_parse_rejects_invalid_params() {
        let err = InvokeRequest::parse("send_message", json!({ "message": "missing id" }))
            .expect_err("missing conversation_id should fail");
        assert!(err.contains("conversation_id"));
    }

    #[test]
    fn invoke_request_parse_accepts_optional_spawn_agent_name() {
        let request = InvokeRequest::parse(
            "spawn_agent",
            json!({
                "workspaceRoots": ["/tmp/work"],
                "prompt": "inspect repo",
            }),
        )
        .expect("spawn_agent should parse without an explicit name");

        match request {
            InvokeRequest::Agent(crate::invoke::AgentInvoke::SpawnAgent { name, .. }) => {
                assert_eq!(name, "Sub-agent");
            }
            other => panic!("unexpected request: {other:?}"),
        }
    }
}
