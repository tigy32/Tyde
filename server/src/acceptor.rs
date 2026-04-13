use std::io;
use std::io::ErrorKind;
use std::path::Path;
use std::time::Duration;

use protocol::{
    BootstrapData, Envelope, FrameError, FrameKind, HelloPayload, RejectCode, RejectPayload,
    SeqValidator, StreamPath, WelcomePayload, read_envelope, write_envelope,
};
use std::collections::HashMap;
use tokio::io::{AsyncRead, AsyncWrite, BufReader};
use tokio::net::UnixListener;
use tokio::time::timeout;

use crate::connection::run_connection;
use crate::{Connection, HostHandle, ServerConfig};

const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(10);

#[derive(Debug)]
pub enum HandshakeError {
    Frame(FrameError),
    UnexpectedKind { expected: FrameKind, got: FrameKind },
    InvalidHandshake(String),
    IncompatibleProtocol { client: u32, server: u32 },
    InvalidPayload(serde_json::Error),
    Timeout,
}

impl From<FrameError> for HandshakeError {
    fn from(value: FrameError) -> Self {
        Self::Frame(value)
    }
}

pub async fn accept<S>(config: &ServerConfig, stream: S) -> Result<Connection, HandshakeError>
where
    S: AsyncRead + AsyncWrite + Unpin + Send + 'static,
{
    let (read_half, mut write_half) = tokio::io::split(stream);
    let mut reader = BufReader::new(read_half);

    let first = timeout(HANDSHAKE_TIMEOUT, read_envelope(&mut reader))
        .await
        .map_err(|_| HandshakeError::Timeout)??;

    let first = match first {
        Some(envelope) => envelope,
        None => {
            let io_err = io::Error::new(
                ErrorKind::UnexpectedEof,
                "connection closed before handshake hello",
            );
            return Err(HandshakeError::Frame(FrameError::Io(io_err)));
        }
    };

    let mut incoming_seq = SeqValidator::new();
    incoming_seq.validate(&first.stream, first.seq, first.kind);

    if first.kind != FrameKind::Hello {
        let reject = RejectPayload {
            code: RejectCode::InvalidHandshake,
            message: format!("first frame must be hello, received {}", first.kind),
            server_protocol_version: config.protocol_version,
            server_tyde_version: config.tyde_version,
        };
        send_reject(&mut write_half, first.stream.clone(), reject).await?;
        return Err(HandshakeError::UnexpectedKind {
            expected: FrameKind::Hello,
            got: first.kind,
        });
    }

    if !first.stream.0.starts_with("/host/") {
        let message = format!(
            "first frame must use /host/<uuid> stream, received {}",
            first.stream
        );
        let reject = RejectPayload {
            code: RejectCode::InvalidHandshake,
            message: message.clone(),
            server_protocol_version: config.protocol_version,
            server_tyde_version: config.tyde_version,
        };
        send_reject(&mut write_half, first.stream.clone(), reject).await?;
        return Err(HandshakeError::InvalidHandshake(message));
    }

    let hello: HelloPayload = match first.parse_payload() {
        Ok(payload) => payload,
        Err(err) => {
            let reject = RejectPayload {
                code: RejectCode::InvalidHandshake,
                message: format!("invalid hello payload: {err}"),
                server_protocol_version: config.protocol_version,
                server_tyde_version: config.tyde_version,
            };
            send_reject(&mut write_half, first.stream.clone(), reject).await?;
            return Err(HandshakeError::InvalidPayload(err));
        }
    };

    if hello.protocol_version != config.protocol_version {
        let reject = RejectPayload {
            code: RejectCode::IncompatibleProtocol,
            message: format!(
                "server requires protocol version {}, client sent {}",
                config.protocol_version, hello.protocol_version
            ),
            server_protocol_version: config.protocol_version,
            server_tyde_version: config.tyde_version,
        };
        send_reject(&mut write_half, first.stream.clone(), reject).await?;
        return Err(HandshakeError::IncompatibleProtocol {
            client: hello.protocol_version,
            server: config.protocol_version,
        });
    }

    let welcome = WelcomePayload {
        protocol_version: config.protocol_version,
        tyde_version: config.tyde_version,
        bootstrap: BootstrapData::default(),
    };
    let envelope = Envelope::from_payload(first.stream.clone(), FrameKind::Welcome, 0, &welcome)
        .map_err(HandshakeError::InvalidPayload)?;
    write_envelope(&mut write_half, &envelope).await?;

    let mut outgoing_seq = HashMap::new();
    outgoing_seq.insert(first.stream, 1);

    Ok(Connection {
        reader: Box::new(reader),
        writer: Box::new(write_half),
        incoming_seq,
        outgoing_seq,
    })
}

pub async fn listen_uds(
    path: impl AsRef<Path>,
    config: ServerConfig,
    host: HostHandle,
) -> io::Result<()> {
    let path = path.as_ref();
    if path.exists() {
        std::fs::remove_file(path)?;
    }

    let listener = UnixListener::bind(path)?;
    loop {
        let (stream, _) = listener.accept().await?;
        let config = config;
        let host = host.clone();

        tokio::spawn(async move {
            let connection = match accept(&config, stream).await {
                Ok(connection) => connection,
                Err(err) => {
                    tracing::error!("handshake failed: {err:?}");
                    return;
                }
            };

            if let Err(err) = run_connection(connection, host).await {
                tracing::error!("connection loop failed: {err:?}");
            }
        });
    }
}

async fn send_reject<W: AsyncWrite + Unpin>(
    writer: &mut W,
    stream: StreamPath,
    payload: RejectPayload,
) -> Result<(), HandshakeError> {
    let envelope = Envelope::from_payload(stream, FrameKind::Reject, 0, &payload)
        .map_err(HandshakeError::InvalidPayload)?;
    write_envelope(writer, &envelope).await?;
    Ok(())
}
