//! A resumable envelope transport. The protocol above it sees one uninterrupted
//! byte stream; acknowledgements mean delivery into that stream, not execution.
use std::collections::{HashMap, VecDeque};
use std::io;
#[cfg(feature = "test-support")]
use std::sync::atomic::AtomicBool;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt, BufReader, DuplexStream};
use tokio::sync::{Notify, mpsc, oneshot};
use tokio::time::{Instant, timeout};
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

pub const MAGIC: &[u8; 5] = b"\x7fTYR1";
pub const REPLAY_BYTES: usize = 128 * 1024 * 1024;
pub const RESUME_WINDOW: Duration = Duration::from_secs(300);
const RECORD_LIMIT: usize = 32 * 1024 * 1024;
const LIVENESS: Duration = Duration::from_secs(45);

fn invalid(message: &str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message)
}

struct Replay {
    frames: VecDeque<Arc<Vec<u8>>>,
    first: u64,
    next: u64,
    bytes: usize,
}

impl Replay {
    fn acknowledge(&mut self, received: u64) -> io::Result<()> {
        if received < self.first || received > self.next {
            return Err(invalid(
                "resume unavailable: acknowledgement outside replay buffer",
            ));
        }
        while self.first < received {
            self.bytes -= self.frames.pop_front().expect("retained frame").len();
            self.first += 1;
        }
        Ok(())
    }
}

struct AttachmentLease {
    session: Arc<Session>,
    generation: u64,
    token: CancellationToken,
}

impl Drop for AttachmentLease {
    fn drop(&mut self) {
        self.token.cancel();
        let mut detached = self.session.detached.lock().expect("detach time poisoned");
        if self.session.generation.load(Ordering::Acquire) == self.generation {
            *detached = Some(Instant::now());
        }
    }
}

enum Delivery {
    Fence {
        received: u64,
        token: CancellationToken,
        reply: oneshot::Sender<io::Result<AttachmentLease>>,
    },
    Record {
        generation: u64,
        kind: u8,
        seq: u64,
        bytes: Vec<u8>,
        reply: oneshot::Sender<io::Result<()>>,
    },
}

pub struct Session {
    replay: Mutex<Replay>,
    delivery: mpsc::Sender<Delivery>,
    received: AtomicU64,
    generation: AtomicU64,
    attachment: Mutex<Option<CancellationToken>>,
    detached: Mutex<Option<Instant>>,
    changed: Notify,
    closed: CancellationToken,
    limit: usize,
    #[cfg(feature = "test-support")]
    capacity_waiting: AtomicBool,
}

impl Session {
    pub fn new() -> (Arc<Self>, DuplexStream) {
        Self::with_limits(REPLAY_BYTES, RESUME_WINDOW)
    }

    pub fn with_limits(limit: usize, window: Duration) -> (Arc<Self>, DuplexStream) {
        let (logical, transport) = tokio::io::duplex(256 * 1024);
        let (reader, writer) = tokio::io::split(transport);
        let (delivery, deliveries) = mpsc::channel(1);
        let session = Arc::new(Self {
            replay: Mutex::new(Replay {
                frames: VecDeque::new(),
                first: 0,
                next: 0,
                bytes: 0,
            }),
            delivery,
            received: AtomicU64::new(0),
            generation: AtomicU64::new(0),
            attachment: Mutex::new(None),
            detached: Mutex::new(Some(Instant::now())),
            changed: Notify::new(),
            closed: CancellationToken::new(),
            limit,
            #[cfg(feature = "test-support")]
            capacity_waiting: AtomicBool::new(false),
        });
        tokio::spawn(session.clone().deliver(writer, deliveries));
        let producer = session.clone();
        tokio::spawn(async move {
            let mut reader = protocol::FrameReader::new(BufReader::new(reader));
            let result: io::Result<()> = async {
                loop {
                    let frame = tokio::select! {
                        _ = producer.closed.cancelled() => return Ok(()),
                        frame = reader.read_frame() => frame.map_err(io::Error::other)?,
                    };
                    let Some(frame) = frame else { return Ok(()) };
                    let mut bytes = Vec::new();
                    protocol::write_frame(&mut bytes, &frame)
                        .await
                        .map_err(io::Error::other)?;
                    if bytes.len() > producer.limit {
                        return Err(invalid(
                            "resume unavailable: record exceeds replay capacity",
                        ));
                    }
                    let bytes = Arc::new(bytes);
                    loop {
                        let changed = producer.changed.notified();
                        let admitted = {
                            let mut replay = producer.replay.lock().expect("replay poisoned");
                            if bytes.len() <= producer.limit.saturating_sub(replay.bytes) {
                                replay.bytes += bytes.len();
                                replay.next += 1;
                                replay.frames.push_back(bytes.clone());
                                true
                            } else {
                                false
                            }
                        };
                        if admitted {
                            #[cfg(feature = "test-support")]
                            producer.capacity_waiting.store(false, Ordering::Release);
                            producer.changed.notify_waiters();
                            break;
                        }
                        #[cfg(feature = "test-support")]
                        {
                            if !producer.capacity_waiting.swap(true, Ordering::AcqRel) {
                                producer.changed.notify_waiters();
                            }
                        }
                        tokio::select! {
                            _ = producer.closed.cancelled() => return Ok(()),
                            _ = changed => {},
                        }
                    }
                }
            }
            .await;
            if let Err(error) = result {
                tracing::warn!(%error, "closing resumable session");
            }
            producer.close();
        });
        let cleanup = session.clone();
        tokio::spawn(async move {
            loop {
                tokio::select! {
                    _ = cleanup.closed.cancelled() => break,
                    _ = tokio::time::sleep(window.min(Duration::from_secs(1))) => {
                        let expired = cleanup.detached.lock().expect("detach time poisoned")
                            .is_some_and(|at| at.elapsed() >= window);
                        if expired {
                            tracing::info!("resumable session expired after disconnect");
                            cleanup.close();
                            break;
                        }
                    }
                }
            }
            let mut replay = cleanup.replay.lock().expect("replay poisoned");
            replay.frames.clear();
            replay.bytes = 0;
        });
        (session, logical)
    }

    pub fn close(&self) {
        self.closed.cancel();
    }
    pub fn is_closed(&self) -> bool {
        self.closed.is_cancelled()
    }
    pub async fn closed(&self) {
        self.closed.cancelled().await;
    }
    pub fn received(&self) -> u64 {
        self.received.load(Ordering::Acquire)
    }
    pub fn tail(&self) -> u64 {
        self.replay.lock().expect("replay poisoned").next
    }

    pub async fn caught_up(&self, tail: u64) {
        loop {
            let changed = self.changed.notified();
            if self.received() >= tail || self.is_closed() {
                return;
            }
            tokio::select! {
                _ = changed => {},
                _ = self.closed.cancelled() => return,
            }
        }
    }

    pub async fn attach<R, W>(
        self: Arc<Self>,
        reader: R,
        writer: W,
        received: u64,
    ) -> io::Result<()>
    where
        R: AsyncRead + Unpin + Send,
        W: AsyncWrite + Unpin + Send,
    {
        let token = CancellationToken::new();
        let (reply, fenced) = oneshot::channel();
        self.delivery
            .send(Delivery::Fence {
                received,
                token: token.clone(),
                reply,
            })
            .await
            .map_err(|_| invalid("recovery delivery actor stopped"))?;
        let lease = fenced
            .await
            .map_err(|_| invalid("recovery delivery fence lost"))??;
        let generation = lease.generation;
        self.changed.notify_waiters();
        let reader = self.read_wire(reader, generation, &token);
        let writer = self.write_wire(writer, received, &token);
        tokio::pin!(reader, writer);
        let result = tokio::select! {
            _ = self.closed.cancelled() => Err(invalid("resume unavailable: session closed")),
            result = &mut reader => result,
            result = &mut writer => {
                token.cancel();
                // Finish an envelope already being delivered before detaching.
                tokio::select! {
                    _ = self.closed.cancelled() => {},
                    _ = &mut reader => {},
                }
                result
            },
        };
        token.cancel();
        if self.generation.load(Ordering::Acquire) == generation
            && result
                .as_ref()
                .is_err_and(|error| error.kind() == io::ErrorKind::InvalidData)
        {
            self.close();
        }
        result
    }

    async fn read_wire<R: AsyncRead + Unpin>(
        &self,
        mut reader: R,
        generation: u64,
        token: &CancellationToken,
    ) -> io::Result<()> {
        loop {
            let record = async {
                let (kind, seq, len) = timeout(LIVENESS, async {
                    Ok::<_, io::Error>((
                        reader.read_u8().await?,
                        reader.read_u64().await?,
                        reader.read_u32().await? as usize,
                    ))
                })
                .await
                .map_err(|_| {
                    io::Error::new(io::ErrorKind::TimedOut, "recovery peer heartbeat timed out")
                })??;
                if len > RECORD_LIMIT
                    || kind > 1
                    || (kind == 0 && len != 0)
                    || (kind == 1 && len == 0)
                {
                    return Err(invalid("invalid recovery record"));
                }
                let mut bytes = vec![0; len];
                let mut offset = 0;
                while offset < len {
                    let count = timeout(LIVENESS, reader.read(&mut bytes[offset..]))
                        .await
                        .map_err(|_| {
                            io::Error::new(io::ErrorKind::TimedOut, "recovery record stalled")
                        })??;
                    if count == 0 {
                        return Err(io::Error::new(
                            io::ErrorKind::UnexpectedEof,
                            "SSH dropped during recovery record",
                        ));
                    }
                    offset += count;
                }
                Ok::<_, io::Error>((kind, seq, bytes))
            };
            let (kind, seq, bytes) = tokio::select! {
                _ = token.cancelled() => return Ok(()),
                record = record => record?,
            };
            let (reply, delivered) = oneshot::channel();
            self.delivery
                .send(Delivery::Record {
                    generation,
                    kind,
                    seq,
                    bytes,
                    reply,
                })
                .await
                .map_err(|_| invalid("recovery delivery actor stopped"))?;
            delivered
                .await
                .map_err(|_| invalid("recovery delivery completion lost"))??;
        }
    }

    async fn deliver(
        self: Arc<Self>,
        mut writer: tokio::io::WriteHalf<DuplexStream>,
        mut requests: mpsc::Receiver<Delivery>,
    ) {
        loop {
            let request = tokio::select! {
                _ = self.closed.cancelled() => return,
                request = requests.recv() => match request { Some(request) => request, None => return },
            };
            match request {
                Delivery::Fence {
                    received,
                    token,
                    reply,
                } => {
                    if reply.is_closed() {
                        continue;
                    }
                    let result = (|| {
                        if self.is_closed() {
                            return Err(invalid("resume unavailable: session closed"));
                        }
                        self.replay
                            .lock()
                            .expect("replay poisoned")
                            .acknowledge(received)?;
                        let mut detached = self.detached.lock().expect("detach time poisoned");
                        let generation = self.generation.fetch_add(1, Ordering::AcqRel) + 1;
                        if let Some(old) = self
                            .attachment
                            .lock()
                            .expect("attachment poisoned")
                            .replace(token.clone())
                        {
                            old.cancel();
                        }
                        *detached = None;
                        self.changed.notify_waiters();
                        Ok(AttachmentLease {
                            session: self.clone(),
                            generation,
                            token,
                        })
                    })();
                    let _ = reply.send(result);
                }
                Delivery::Record {
                    generation,
                    kind,
                    seq,
                    bytes,
                    reply,
                } => {
                    let result = async {
                        if self.generation.load(Ordering::Acquire) != generation {
                            return Ok(());
                        }
                        if kind == 0 {
                            self.replay
                                .lock()
                                .expect("replay poisoned")
                                .acknowledge(seq)?;
                        } else {
                            let received = self.received();
                            if seq > received {
                                return Err(invalid("gap in recovery replay"));
                            }
                            if seq == received {
                                writer.write_all(&bytes).await?;
                                self.received.store(received + 1, Ordering::Release);
                            }
                        }
                        self.changed.notify_waiters();
                        Ok(())
                    };
                    let result = tokio::select! {
                        _ = self.closed.cancelled() => return,
                        result = result => result,
                    };
                    let _ = reply.send(result);
                }
            }
        }
    }

    async fn write_wire<W: AsyncWrite + Unpin>(
        &self,
        mut writer: W,
        mut next: u64,
        token: &CancellationToken,
    ) -> io::Result<()> {
        let mut acknowledged = u64::MAX;
        loop {
            let changed = self.changed.notified();
            let received = self.received();
            if acknowledged != received {
                write_record(&mut writer, 0, received, &[]).await?;
                acknowledged = received;
            }
            let frame = {
                let replay = self.replay.lock().expect("replay poisoned");
                if next < replay.first {
                    return Err(invalid("replay was superseded"));
                }
                replay.frames.get((next - replay.first) as usize).cloned()
            };
            if let Some(frame) = frame {
                write_record(&mut writer, 1, next, &frame).await?;
                next += 1;
                continue;
            }
            tokio::select! {
                _ = token.cancelled() => return Ok(()),
                _ = changed => {},
                _ = tokio::time::sleep(Duration::from_secs(10)) => { acknowledged = u64::MAX; },
            }
        }
    }
}

async fn write_record<W: AsyncWrite + Unpin>(
    writer: &mut W,
    kind: u8,
    seq: u64,
    bytes: &[u8],
) -> io::Result<()> {
    writer.write_u8(kind).await?;
    writer.write_u64(seq).await?;
    writer.write_u32(bytes.len() as u32).await?;
    writer.write_all(bytes).await?;
    writer.flush().await
}

#[derive(Clone)]
pub struct Registry {
    sessions: Arc<Mutex<HashMap<Uuid, Arc<Session>>>>,
    limit: usize,
    window: Duration,
}

impl Default for Registry {
    fn default() -> Self {
        Self::with_limits(REPLAY_BYTES, RESUME_WINDOW)
    }
}

impl Registry {
    #[cfg(feature = "test-support")]
    pub async fn wait_for_output_capacity_pressure(&self, id: Uuid) -> io::Result<()> {
        let session = self
            .sessions
            .lock()
            .expect("session registry poisoned")
            .get(&id)
            .cloned()
            .ok_or_else(|| invalid("recovery session missing"))?;
        loop {
            let changed = session.changed.notified();
            if session.is_closed() {
                return Err(invalid("recovery session closed before capacity wait"));
            }
            if session.capacity_waiting.load(Ordering::Acquire) {
                return Ok(());
            }
            tokio::select! {
                _ = changed => {},
                _ = session.closed.cancelled() => return Err(invalid("recovery session closed before capacity wait")),
            }
        }
    }

    pub fn with_limits(limit: usize, window: Duration) -> Self {
        Self {
            sessions: Arc::default(),
            limit,
            window,
        }
    }

    pub async fn accept<R, W>(
        &self,
        mut reader: R,
        mut writer: W,
    ) -> io::Result<Option<DuplexStream>>
    where
        R: AsyncRead + Unpin + Send + 'static,
        W: AsyncWrite + Unpin + Send + 'static,
    {
        let (id, received) = timeout(Duration::from_secs(10), async {
            let mut magic = [0; 5];
            reader.read_exact(&mut magic).await?;
            if magic != *MAGIC {
                return Err(invalid("invalid recovery handshake"));
            }
            let mut id = [0; 16];
            reader.read_exact(&mut id).await?;
            Ok::<_, io::Error>((Uuid::from_bytes(id), reader.read_u64().await?))
        })
        .await
        .map_err(io::Error::other)??;
        let (id, session, logical) = {
            let mut sessions = self.sessions.lock().expect("session registry poisoned");
            sessions.retain(|_, session| !session.is_closed());
            if id.is_nil() {
                let id = Uuid::new_v4();
                let (session, logical) = Session::with_limits(self.limit, self.window);
                sessions.insert(id, session.clone());
                (id, Some(session), Some(logical))
            } else {
                (id, sessions.get(&id).cloned(), None)
            }
        };
        let Some(session) = session.filter(|session| {
            session
                .replay
                .lock()
                .expect("replay poisoned")
                .acknowledge(received)
                .is_ok()
        }) else {
            writer.write_u8(0).await?;
            writer.flush().await?;
            return Ok(None);
        };
        writer.write_u8(1).await?;
        writer.write_all(id.as_bytes()).await?;
        writer.write_u64(session.received()).await?;
        writer.write_u64(session.tail()).await?;
        writer.flush().await?;
        tokio::spawn(async move {
            if let Err(error) = session.attach(reader, writer, received).await {
                tracing::info!(%error, "resumable transport detached");
            }
        });
        Ok(logical)
    }
}

pub async fn connect<R: AsyncRead + Unpin, W: AsyncWrite + Unpin>(
    reader: &mut R,
    writer: &mut W,
    id: Option<Uuid>,
    session: &Session,
) -> io::Result<(Uuid, u64, u64)> {
    timeout(Duration::from_secs(15), async {
        writer.write_all(MAGIC).await?;
        writer
            .write_all(id.unwrap_or(Uuid::nil()).as_bytes())
            .await?;
        writer.write_u64(session.received()).await?;
        writer.flush().await?;
        if reader.read_u8().await? != 1 {
            return Err(io::Error::new(
                io::ErrorKind::NotFound,
                "resume unavailable: session expired or replay lost",
            ));
        }
        let mut returned_id = [0; 16];
        reader.read_exact(&mut returned_id).await?;
        let returned_id = Uuid::from_bytes(returned_id);
        if returned_id.is_nil() || id.is_some_and(|requested| requested != returned_id) {
            return Err(invalid("resume returned a different logical session"));
        }
        let received = reader.read_u64().await?;
        let tail = reader.read_u64().await?;
        if let Err(error) = session
            .replay
            .lock()
            .expect("replay poisoned")
            .acknowledge(received)
        {
            session.close();
            return Err(error);
        }
        Ok((returned_id, received, tail))
    })
    .await
    .map_err(io::Error::other)?
}
