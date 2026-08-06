use std::io;
use std::io::ErrorKind;
#[cfg(unix)]
use std::os::unix::fs::FileTypeExt;
use std::path::Path;
#[cfg(unix)]
use std::path::PathBuf;
use std::time::Duration;

use protocol::{
    Envelope, FrameError, FrameKind, HelloPayload, RejectCode, RejectPayload, SeqValidator,
    StreamPath, WelcomePayload, read_envelope, write_envelope,
};
use std::collections::HashMap;
use tokio::io::{AsyncRead, AsyncWrite, BufReader};
#[cfg(unix)]
use tokio::net::{UnixListener, UnixStream};
use tokio::time::timeout;

#[cfg(unix)]
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
    if let Err(error) = incoming_seq.validate(&first.stream, first.seq, first.kind) {
        let message = format!("handshake frame failed protocol validation: {error}");
        let reject = RejectPayload {
            code: RejectCode::InvalidHandshake,
            message: message.clone(),
            server_protocol_version: config.protocol_version,
            server_tyde_version: config.tyde_version,
            release_version: crate::host_release_version(),
        };
        send_reject(&mut write_half, first.stream.clone(), reject).await?;
        return Err(HandshakeError::InvalidHandshake(message));
    }

    if first.kind != FrameKind::Hello {
        let reject = RejectPayload {
            code: RejectCode::InvalidHandshake,
            message: format!("first frame must be hello, received {}", first.kind),
            server_protocol_version: config.protocol_version,
            server_tyde_version: config.tyde_version,
            release_version: crate::host_release_version(),
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
            release_version: crate::host_release_version(),
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
                release_version: crate::host_release_version(),
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
            release_version: crate::host_release_version(),
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
        release_version: crate::host_release_version(),
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
    #[cfg(unix)]
    {
        let listener = match bind_uds(path).await {
            Ok(listener) => listener,
            Err(err) => {
                host.shutdown_spawn_operations().await;
                return Err(err);
            }
        };
        serve_uds(listener, config, host).await
    }

    #[cfg(not(unix))]
    {
        let _ = path;
        let _ = config;
        host.shutdown_spawn_operations().await;
        Err(io::Error::new(
            ErrorKind::Unsupported,
            "Unix domain sockets are not supported on this platform",
        ))
    }
}

#[cfg(unix)]
#[derive(Debug)]
pub struct BoundUdsListener {
    listener: UnixListener,
    _cleanup: UdsPathCleanup,
}

#[cfg(unix)]
pub async fn bind_uds(path: impl AsRef<Path>) -> io::Result<BoundUdsListener> {
    let path = path.as_ref();
    prepare_uds_path(path).await?;
    let listener = UnixListener::bind(path)?;
    Ok(BoundUdsListener {
        listener,
        _cleanup: UdsPathCleanup {
            path: path.to_path_buf(),
        },
    })
}

#[cfg(unix)]
pub async fn serve_uds(
    listener: BoundUdsListener,
    config: ServerConfig,
    host: HostHandle,
) -> io::Result<()> {
    let result = async {
        loop {
            let (stream, _) = listener.listener.accept().await?;
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
    .await;
    host.shutdown_spawn_operations().await;
    result
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

#[cfg(unix)]
async fn prepare_uds_path(path: &Path) -> io::Result<()> {
    let parent = path.parent().ok_or_else(|| {
        io::Error::new(
            ErrorKind::InvalidInput,
            format!("UDS path has no parent: {}", path.display()),
        )
    })?;
    std::fs::create_dir_all(parent)?;

    if !path.exists() {
        return Ok(());
    }

    let metadata = std::fs::symlink_metadata(path)?;
    if !metadata.file_type().is_socket() {
        return Err(io::Error::new(
            ErrorKind::AlreadyExists,
            format!("refusing to replace non-socket path at {}", path.display()),
        ));
    }

    match UnixStream::connect(path).await {
        Ok(_) => Err(io::Error::new(
            ErrorKind::AddrInUse,
            format!("UDS path is already in use: {}", path.display()),
        )),
        Err(err) if err.kind() == ErrorKind::ConnectionRefused => {
            std::fs::remove_file(path)?;
            Ok(())
        }
        Err(err) if err.kind() == ErrorKind::NotFound => Ok(()),
        Err(err) => Err(io::Error::new(
            err.kind(),
            format!(
                "failed to probe existing UDS path {}: {err}",
                path.display()
            ),
        )),
    }
}

#[cfg(unix)]
#[derive(Debug)]
struct UdsPathCleanup {
    path: PathBuf,
}

#[cfg(unix)]
impl Drop for UdsPathCleanup {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
}

#[cfg(test)]
mod handshake_contract_tests {
    use super::{HandshakeError, accept};
    use crate::ServerConfig;
    use protocol::{
        Envelope, FrameKind, HelloPayload, RejectCode, RejectPayload, StreamPath, Version,
        read_envelope, write_envelope,
    };
    use std::time::Duration;
    use tokio::time::timeout;

    // The hello -> reject exchange is the only way a version-mismatched
    // mobile/web client learns which host version it is talking to: the
    // reject's `release_version` drives the PWA loader's self-heal reboot
    // into the host's published bundle (mobile-frontend `apply_reject`,
    // web/loader `onRepairVersion`). This test pins that contract at the
    // `accept()` level: a mismatched hello must be ANSWERED with a parseable
    // reject carrying the host release version — promptly, never left to
    // hang. When the beta.52 framing change silently broke this exchange
    // between adjacent versions, mismatched pairs wedged forever on
    // "Loading host…" (2026-08-06 incident). If this test fails, restore the
    // answer path; do not relax the assertions.
    #[tokio::test]
    async fn version_mismatch_is_answered_with_a_reject_carrying_release_version() {
        crate::set_host_release_version("0.8.19-beta.55");

        let tyde_version = Version {
            major: 0,
            minor: 8,
            patch: 19,
        };
        let config = ServerConfig {
            protocol_version: 47,
            tyde_version,
        };
        let (mut client, server) = tokio::io::duplex(64 * 1024);
        let server_task = tokio::spawn(async move { accept(&config, server).await });

        let hello = HelloPayload {
            protocol_version: 45,
            tyde_version,
            client_name: "handshake-contract-test".to_owned(),
            platform: "test".to_owned(),
        };
        let envelope = Envelope::from_payload(
            StreamPath("/host/11111111-1111-1111-1111-111111111111".into()),
            FrameKind::Hello,
            0,
            &hello,
        )
        .unwrap();

        let exchange = async {
            write_envelope(&mut client, &envelope).await.unwrap();
            read_envelope(&mut client)
                .await
                .expect("the reject must arrive in a framing this client can decode")
                .expect("host must answer a mismatched hello, not close silently")
        };
        let reject = timeout(Duration::from_secs(5), exchange)
            .await
            .expect("a version-mismatched hello must be answered, never left to hang");

        assert_eq!(reject.kind, FrameKind::Reject);
        let payload: RejectPayload = reject.parse_payload().unwrap();
        assert_eq!(payload.code, RejectCode::IncompatibleProtocol);
        assert_eq!(payload.server_protocol_version, 47);
        let advertised = payload
            .release_version
            .expect("version-mismatch rejects must carry the host release version");
        assert_eq!(
            advertised,
            crate::host_release_version().expect("recorded above"),
            "the reject must advertise the recorded host release version"
        );

        let error = server_task
            .await
            .unwrap()
            .expect_err("accept must fail after rejecting a mismatched client");
        assert!(matches!(
            error,
            HandshakeError::IncompatibleProtocol {
                client: 45,
                server: 47
            }
        ));
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::bind_uds;
    use std::io::ErrorKind;

    #[tokio::test]
    async fn concurrent_uds_binding_has_exactly_one_owner() {
        let dir = tempfile::tempdir().expect("socket tempdir");
        let path = dir.path().join("tyde.sock");

        let (left, right) = tokio::join!(bind_uds(&path), bind_uds(&path));
        let (owner, rejected) = match (left, right) {
            (Ok(owner), Err(rejected)) | (Err(rejected), Ok(owner)) => (owner, rejected),
            (left, right) => panic!("exactly one bind must win: left={left:?}, right={right:?}"),
        };
        assert_eq!(rejected.kind(), ErrorKind::AddrInUse);
        assert!(path.exists(), "the winning owner keeps the socket path");

        drop(owner);
        assert!(!path.exists(), "dropping the owner removes its socket path");
    }

    #[tokio::test]
    async fn stale_uds_path_is_recovered_before_binding() {
        let dir = tempfile::tempdir().expect("socket tempdir");
        let path = dir.path().join("tyde.sock");
        let stale = tokio::net::UnixListener::bind(&path).expect("stale listener");
        drop(stale);
        assert!(path.exists(), "fixture leaves a stale socket path");

        let owner = bind_uds(&path).await.expect("replace stale socket");
        assert!(path.exists());
        drop(owner);
        assert!(!path.exists());
    }
}
