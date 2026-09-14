use protocol::{MobileIceServer, MobileTurnUrl};
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::time::Duration;
use turn::auth::{AuthHandler, generate_auth_key};
use turn::relay::relay_static::RelayAddressGeneratorStatic;
use turn::server::Server;
use turn::server::config::{ConnConfig, ServerConfig};

struct RelayAuth;
impl AuthHandler for RelayAuth {
    fn auth_handle(
        &self,
        username: &str,
        realm: &str,
        _source: SocketAddr,
    ) -> Result<Vec<u8>, turn::Error> {
        if username != "tyde-test" || realm != "tyde" {
            return Err(turn::Error::ErrNoPermission);
        }
        Ok(generate_auth_key(username, realm, "relay-password"))
    }
}

pub async fn relay() -> RelayFixture {
    let conn = Arc::new(
        tokio::net::UdpSocket::bind("127.0.0.1:0")
            .await
            .expect("TURN socket"),
    );
    let address = conn.local_addr().expect("TURN address");
    let server = Server::new(ServerConfig {
        conn_configs: vec![ConnConfig {
            conn,
            relay_addr_generator: Box::new(RelayAddressGeneratorStatic {
                relay_address: IpAddr::V4(Ipv4Addr::LOCALHOST),
                address: "127.0.0.1".to_owned(),
                net: Arc::new(webrtc_util::vnet::net::Net::new(None)),
            }),
        }],
        realm: "tyde".to_owned(),
        auth_handler: Arc::new(RelayAuth),
        channel_bind_timeout: Duration::ZERO,
        alloc_close_notify: None,
    })
    .await
    .expect("start real TURN server");
    let browser_ice = MobileIceServer {
        urls: vec![MobileTurnUrl(format!("turn:{address}?transport=udp"))],
        username: "tyde-test".to_owned(),
        credential: "relay-password".to_owned(),
    };
    let certificate = rcgen::generate_simple_self_signed(vec!["localhost".to_owned()])
        .expect("fixture certificate");
    let cert = certificate.cert.der().clone();
    let key = tokio_rustls::rustls::pki_types::PrivatePkcs8KeyDer::from(
        certificate.signing_key.serialize_der(),
    );
    let config = tokio_rustls::rustls::ServerConfig::builder_with_provider(Arc::new(
        tokio_rustls::rustls::crypto::aws_lc_rs::default_provider(),
    ))
    .with_safe_default_protocol_versions()
    .expect("TLS protocol versions")
    .with_no_client_auth()
    .with_single_cert(vec![cert.clone()], key.into())
    .expect("fixture TLS config");
    let mut roots = tokio_rustls::rustls::RootCertStore::empty();
    roots.add(cert).expect("trust only fixture certificate");
    let localhost = tokio::net::lookup_host("localhost:0")
        .await
        .expect("resolve fixture hostname")
        .next()
        .expect("fixture hostname address");
    let listener = tokio::net::TcpListener::bind(localhost)
        .await
        .expect("TLS TURN listener");
    let tls_address = listener.local_addr().expect("TLS TURN address");
    let tls_ice = MobileIceServer {
        urls: vec![MobileTurnUrl(format!(
            "turns:localhost:{}?transport=tcp",
            tls_address.port()
        ))],
        ..browser_ice.clone()
    };
    let acceptor = tokio_rustls::TlsAcceptor::from(Arc::new(config));
    let outage = Arc::new(RelayOutage::default());
    let connection_outage = outage.clone();
    let task = tokio::spawn(async move {
        let mut connections = tokio::task::JoinSet::new();
        loop {
            tokio::select! {
                accepted = listener.accept() => {
                    let (socket, _) = accepted.expect("accept TLS TURN connection");
                    socket.set_nodelay(true).expect("TLS TURN nodelay");
                    let acceptor = acceptor.clone();
                    let outage = connection_outage.clone();
                    connections.spawn(async move {
                        if let Err(error) = tls_frontend(acceptor, socket, address, outage).await {
                            eprintln!("TLS TURN fixture connection ended: {error}");
                        }
                    });
                }
                completed = connections.join_next(), if !connections.is_empty() => {
                    completed.expect("TLS TURN connection task").expect("TLS TURN task must not panic");
                }
            }
        }
    });
    RelayFixture {
        server,
        browser_ice,
        tls_ice,
        tls_address,
        roots,
        task,
        outage,
    }
}

pub struct RelayFixture {
    server: Server,
    pub browser_ice: MobileIceServer,
    pub tls_ice: MobileIceServer,
    pub tls_address: SocketAddr,
    pub roots: tokio_rustls::rustls::RootCertStore,
    task: tokio::task::JoinHandle<()>,
    outage: Arc<RelayOutage>,
}

impl RelayFixture {
    pub async fn interrupt_traffic(&self, duration: Duration) -> usize {
        self.outage.dropped.store(0, Ordering::SeqCst);
        self.outage.active.store(true, Ordering::SeqCst);
        tokio::time::sleep(duration).await;
        self.outage.active.store(false, Ordering::SeqCst);
        self.outage.dropped.load(Ordering::SeqCst)
    }

    pub async fn close(&self) {
        self.task.abort();
        self.server.close().await.expect("close TURN server");
    }
}

#[derive(Default)]
struct RelayOutage {
    active: AtomicBool,
    dropped: AtomicUsize,
}

impl RelayOutage {
    fn drop_packet(&self) -> bool {
        if self.active.load(Ordering::SeqCst) {
            self.dropped.fetch_add(1, Ordering::SeqCst);
            true
        } else {
            false
        }
    }
}

impl Drop for RelayFixture {
    fn drop(&mut self) {
        self.task.abort();
    }
}

// Terminate real TLS and pass unmodified TURN messages to the real relay server.
// Each TLS connection owns its own TURN allocation tuple on the UDP side.
async fn tls_frontend(
    acceptor: tokio_rustls::TlsAcceptor,
    socket: tokio::net::TcpStream,
    turn: SocketAddr,
    outage: Arc<RelayOutage>,
) -> std::io::Result<()> {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let tls = acceptor.accept(socket).await?;
    let udp = tokio::net::UdpSocket::bind("127.0.0.1:0").await?;
    udp.connect(turn).await?;
    let (mut reader, mut writer) = tokio::io::split(tls);
    let mut receive_tls = async || -> std::io::Result<()> {
        loop {
            let mut header = [0; 4];
            reader.read_exact(&mut header).await?;
            let payload = u16::from_be_bytes([header[2], header[3]]) as usize;
            let (length, padding) = if header[0] & 0xc0 == 0x40 {
                (payload + 4, (4 - payload % 4) % 4)
            } else if header[0] & 0xc0 == 0 && payload.is_multiple_of(4) {
                (payload + 20, 0)
            } else {
                return Err(std::io::Error::other("invalid TURN TLS header"));
            };
            let mut packet = vec![0; length + padding];
            packet[..4].copy_from_slice(&header);
            reader.read_exact(&mut packet[4..]).await?;
            if !outage.drop_packet() {
                udp.send(&packet[..length]).await?;
            }
        }
    };
    let mut send_tls = async || -> std::io::Result<()> {
        let mut packet = vec![0; 65536];
        loop {
            let count = udp.recv(&mut packet).await?;
            if outage.drop_packet() {
                continue;
            }
            writer.write_all(&packet[..count]).await?;
            if packet[0] & 0xc0 == 0x40 {
                let padding = (4 - count % 4) % 4;
                writer.write_all(&[0; 3][..padding]).await?;
            }
            writer.flush().await?;
        }
    };
    let result: std::io::Result<((), ())> = tokio::try_join!(receive_tls(), send_tls());
    result.map(|_| ())
}
