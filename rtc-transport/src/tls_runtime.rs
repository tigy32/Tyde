use std::future::Future;
use std::io;
use std::net::SocketAddr;
use std::pin::Pin;
use std::sync::Arc;
use std::time::Duration;

use tokio::io::{AsyncReadExt, AsyncWriteExt, ReadHalf, WriteHalf};
use tokio::net::TcpStream;
use tokio::sync::{Mutex, watch};
use tokio_rustls::client::TlsStream;
use tokio_rustls::rustls::{ClientConfig, RootCertStore, pki_types::ServerName};
use tokio_rustls::{TlsConnector, rustls};
use webrtc::runtime::{
    AsyncInterval, AsyncTcpListener, AsyncTcpStream, AsyncUdpSocket, JoinHandle, Runtime,
    TokioRuntime,
};

#[derive(Debug)]
pub(crate) struct TlsRuntime {
    config: Arc<ClientConfig>,
    failure: watch::Sender<Option<String>>,
}

fn report(failure: &watch::Sender<Option<String>>, context: &str, error: &io::Error) {
    tracing::error!(%error, context, "TURN TLS transport failed");
    failure.send_if_modified(|reason| {
        if reason.is_some() {
            return false;
        }
        *reason = Some(format!("{context}: {error}"));
        true
    });
}

impl TlsRuntime {
    pub(crate) fn new(
        roots: RootCertStore,
        failure: watch::Sender<Option<String>>,
    ) -> Result<Self, crate::Error> {
        let config = ClientConfig::builder_with_provider(Arc::new(
            rustls::crypto::aws_lc_rs::default_provider(),
        ))
        .with_safe_default_protocol_versions()
        .map_err(|error| crate::failure("configure TURN TLS", error))?
        .with_root_certificates(roots)
        .with_no_client_auth();
        Ok(Self {
            config: Arc::new(config),
            failure,
        })
    }
}

impl Runtime for TlsRuntime {
    fn spawn(&self, future: Pin<Box<dyn Future<Output = ()> + Send>>) -> Box<dyn JoinHandle> {
        TokioRuntime.spawn(future)
    }

    fn wrap_udp_socket(&self, socket: std::net::UdpSocket) -> io::Result<Arc<dyn AsyncUdpSocket>> {
        Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            format!(
                "TURN TLS does not permit a UDP socket: {:?}",
                socket.local_addr()
            ),
        ))
    }

    fn wrap_tcp_listener(
        &self,
        listener: std::net::TcpListener,
    ) -> io::Result<Arc<dyn AsyncTcpListener>> {
        Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            format!(
                "TURN TLS does not permit a TCP listener: {:?}",
                listener.local_addr()
            ),
        ))
    }

    fn connect_tcp<'a>(
        &'a self,
        remote_addr: SocketAddr,
    ) -> Pin<Box<dyn Future<Output = io::Result<Arc<dyn AsyncTcpStream>>> + Send + 'a>> {
        Box::pin(async move {
            Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                format!("TURN requires verified TLS for {remote_addr}"),
            ))
        })
    }

    fn connect_tls<'a>(
        &'a self,
        remote_addr: SocketAddr,
        server_name: &'a str,
    ) -> Pin<Box<dyn Future<Output = io::Result<Arc<dyn AsyncTcpStream>>> + Send + 'a>> {
        Box::pin(async move {
            let connect = async {
                let name =
                    ServerName::try_from(server_name.to_owned()).map_err(io::Error::other)?;
                let socket = TcpStream::connect(remote_addr).await?;
                socket.set_nodelay(true)?;
                let local_addr = socket.local_addr()?;
                let stream = TlsConnector::from(self.config.clone())
                    .connect(name, socket)
                    .await?;
                let (reader, writer) = tokio::io::split(stream);
                tracing::info!(%remote_addr, server_name, "TURN TLS certificate verified and connection established");
                Ok(Arc::new(TurnTlsStream {
                    reader: Mutex::new(reader),
                    writer: Mutex::new(writer),
                    local_addr,
                    remote_addr,
                    failure: self.failure.clone(),
                }) as Arc<dyn AsyncTcpStream>)
            };
            let result = match tokio::time::timeout(Duration::from_secs(10), connect).await {
                Ok(result) => result,
                Err(error) => Err(io::Error::new(io::ErrorKind::TimedOut, error)),
            };
            if let Err(error) = &result {
                report(&self.failure, "connect to TURN over TLS", error);
            }
            result
        })
    }

    fn resolve_host<'a>(
        &'a self,
        host: &'a str,
    ) -> Pin<Box<dyn Future<Output = io::Result<Vec<SocketAddr>>> + Send + 'a>> {
        Box::pin(async move {
            let result = TokioRuntime.resolve_host(host).await;
            if let Err(error) = &result {
                report(&self.failure, "resolve TURN hostname", error);
            }
            result
        })
    }

    fn sleep(&self, duration: Duration) -> Pin<Box<dyn Future<Output = ()> + Send + 'static>> {
        TokioRuntime.sleep(duration)
    }
    fn interval(&self, period: Duration) -> Box<dyn AsyncInterval> {
        TokioRuntime.interval(period)
    }
    fn block_on(&self, future: Pin<Box<dyn Future<Output = ()> + '_>>) {
        TokioRuntime.block_on(future);
    }
    fn name(&self) -> &'static str {
        "Tyde TURN TLS"
    }
}

#[derive(Debug)]
struct TurnTlsStream {
    // Reads and writes must proceed independently under full-duplex backpressure.
    reader: Mutex<ReadHalf<TlsStream<TcpStream>>>,
    writer: Mutex<WriteHalf<TlsStream<TcpStream>>>,
    local_addr: SocketAddr,
    remote_addr: SocketAddr,
    failure: watch::Sender<Option<String>>,
}

impl AsyncTcpStream for TurnTlsStream {
    fn read<'a, 'b>(
        &'a self,
        buf: &'b mut [u8],
    ) -> Pin<Box<dyn Future<Output = io::Result<usize>> + Send + 'b>>
    where
        'a: 'b,
    {
        Box::pin(async move {
            let result = match self.reader.lock().await.read(buf).await {
                Ok(0) => Err(io::Error::new(
                    io::ErrorKind::UnexpectedEof,
                    "TURN TLS connection closed",
                )),
                result => result,
            };
            if let Err(error) = &result {
                report(&self.failure, "read TURN TLS", error);
            }
            result
        })
    }

    fn write_all<'a, 'b>(
        &'a self,
        buf: &'b [u8],
    ) -> Pin<Box<dyn Future<Output = io::Result<()>> + Send + 'b>>
    where
        'a: 'b,
    {
        Box::pin(async move {
            let write = async {
                let mut writer = self.writer.lock().await;
                writer.write_all(buf).await?;
                writer.flush().await
            };
            let result = write.await;
            if let Err(error) = &result {
                report(&self.failure, "write TURN TLS", error);
            }
            result
        })
    }

    fn local_addr(&self) -> io::Result<SocketAddr> {
        Ok(self.local_addr)
    }
    fn peer_addr(&self) -> io::Result<SocketAddr> {
        Ok(self.remote_addr)
    }
}
