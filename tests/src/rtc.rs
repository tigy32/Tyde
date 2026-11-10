use protocol::{MobileIceServer, MobileRtcSessionId, MobileSdpKind, MobileTurnUrl};
use rtc_transport::{Peer, RtcStream, authenticate_description, verify_description};
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
    oversized: AtomicUsize,
}

impl RelayOutage {
    fn drop_packet(&self, bytes: usize) -> bool {
        // Model the 1232-byte UDP budget on an IPv6 minimum-MTU path.
        if bytes > 1232 {
            if self.oversized.fetch_add(1, Ordering::SeqCst) == 0 {
                eprintln!("TURN path dropped oversized datagram: bytes={bytes} limit=1232");
            }
            true
        } else if self.active.load(Ordering::SeqCst) {
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
            if !outage.drop_packet(length) {
                udp.send(&packet[..length]).await?;
            }
        }
    };
    let mut send_tls = async || -> std::io::Result<()> {
        let mut packet = vec![0; 65536];
        loop {
            let count = udp.recv(&mut packet).await?;
            if outage.drop_packet(count) {
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

pub async fn connect(
    ice: &MobileIceServer,
    roots: &tokio_rustls::rustls::RootCertStore,
) -> (RtcStream, RtcStream) {
    let mut mobile = Peer::with_tls_roots(std::slice::from_ref(ice), roots.clone())
        .await
        .expect("mobile peer");
    let mut host = Peer::with_tls_roots(std::slice::from_ref(ice), roots.clone())
        .await
        .expect("host peer");
    let session = MobileRtcSessionId(uuid::Uuid::new_v4().to_string());
    let key = [53; 32];
    eprintln!("TURN flow: gathering mobile offer");
    let offer = mobile.offer().await.expect("gather mobile TURN candidates");
    eprintln!("TURN flow: mobile offer gathered");
    assert!(offer.lines().any(|line| line.contains(" typ relay")));
    assert!(
        !offer.lines().any(|line| line.contains(" typ host")),
        "test must require the relay"
    );
    let mut signed = authenticate_description(session.clone(), MobileSdpKind::Offer, offer, &key)
        .expect("sign offer");
    assert!(
        verify_description(&signed, &session, MobileSdpKind::Offer, &[54; 32]).is_err(),
        "another pairing cannot authenticate this real peer"
    );
    let original = signed.sdp.clone();
    signed.sdp = signed
        .sdp
        .replace("a=fingerprint:", "a=fingerprint:tampered");
    assert!(
        verify_description(&signed, &session, MobileSdpKind::Offer, &key).is_err(),
        "signaling cannot substitute a DTLS fingerprint"
    );
    signed.sdp = original;
    verify_description(&signed, &session, MobileSdpKind::Offer, &key)
        .expect("authenticate host offer");
    let answer = host
        .answer(signed.sdp)
        .await
        .expect("gather host TURN candidates");
    let signed = authenticate_description(session.clone(), MobileSdpKind::Answer, answer, &key)
        .expect("sign answer");
    verify_description(&signed, &session, MobileSdpKind::Answer, &key)
        .expect("authenticate mobile answer");
    mobile.set_answer(signed.sdp).await.expect("apply answer");
    let (mobile, host) =
        tokio::try_join!(mobile.into_stream(), host.into_stream()).expect("open relayed channels");
    eprintln!("TURN flow: channels open");
    (mobile, host)
}

#[derive(Clone)]
pub struct PushSignaling {
    commands: tokio::sync::mpsc::UnboundedSender<SignalRequest>,
    state: tokio::sync::watch::Receiver<(u64, bool)>,
    pub url: String,
}

enum SignalRequest {
    Register(
        tokio::sync::mpsc::UnboundedSender<HostRequest>,
        tokio::sync::oneshot::Sender<u64>,
    ),
    Unregister(u64),
    Offer(
        protocol::MobileRtcDescription,
        tokio::sync::oneshot::Sender<protocol::MobileSignalEvent>,
    ),
    Disconnect(tokio::sync::oneshot::Sender<()>),
}

enum HostRequest {
    Offer(
        protocol::MobileRtcDescription,
        tokio::sync::oneshot::Sender<protocol::MobileSignalEvent>,
    ),
    Close,
}

impl PushSignaling {
    pub fn router(base_url: &str) -> (Self, axum::Router) {
        use axum::routing::get;
        use tokio::sync::{mpsc, watch};
        let (commands, mut requests) = mpsc::unbounded_channel();
        let (state_tx, state) = watch::channel((0, false));
        tokio::spawn(async move {
            let mut generation = 0;
            let mut host: Option<mpsc::UnboundedSender<HostRequest>> = None;
            while let Some(request) = requests.recv().await {
                match request {
                    SignalRequest::Register(next, reply) => {
                        if let Some(old) = host.replace(next) {
                            let _ = old.send(HostRequest::Close);
                        }
                        generation += 1;
                        state_tx.send_replace((generation, true));
                        let _ = reply.send(generation);
                    }
                    SignalRequest::Unregister(id) if id == generation => {
                        host = None;
                        state_tx.send_replace((generation, false));
                    }
                    SignalRequest::Unregister(_) => {}
                    SignalRequest::Offer(description, reply) => match host.as_ref() {
                        Some(host) => {
                            if let Err(error) = host.send(HostRequest::Offer(description, reply)) {
                                let HostRequest::Offer(_, reply) = error.0 else {
                                    unreachable!()
                                };
                                let _ = reply.send(offline());
                            }
                        }
                        None => {
                            let _ = reply.send(offline());
                        }
                    },
                    SignalRequest::Disconnect(reply) => {
                        if let Some(host) = host.take() {
                            let _ = host.send(HostRequest::Close);
                        }
                        state_tx.send_replace((generation, false));
                        let _ = reply.send(());
                    }
                }
            }
        });
        let fixture = Self {
            commands,
            state,
            url: format!("{base_url}/push-signal"),
        };
        let router = axum::Router::new()
            .route(
                "/push-signal",
                get(push_host)
                    .post(push_mobile)
                    .options(|| async { axum::http::StatusCode::NO_CONTENT }),
            )
            .with_state(fixture.clone())
            .layer(axum::middleware::map_response(
                |mut response: axum::response::Response| async move {
                    use axum::http::{HeaderValue, header};
                    response.headers_mut().insert(
                        header::ACCESS_CONTROL_ALLOW_ORIGIN,
                        HeaderValue::from_static("*"),
                    );
                    response.headers_mut().insert(
                        header::ACCESS_CONTROL_ALLOW_HEADERS,
                        HeaderValue::from_static("content-type, authorization"),
                    );
                    response.headers_mut().insert(
                        header::ACCESS_CONTROL_ALLOW_METHODS,
                        HeaderValue::from_static("GET, POST, OPTIONS"),
                    );
                    response
                },
            ));
        (fixture, router)
    }

    pub fn generation(&self) -> u64 {
        self.state.borrow().0
    }

    pub async fn host_connected_after(&self, previous: u64) {
        self.state
            .clone()
            .wait_for(|(generation, connected)| *generation > previous && *connected)
            .await
            .expect("signaling actor");
    }

    pub async fn disconnect(&self) {
        let (reply, result) = tokio::sync::oneshot::channel();
        self.commands
            .send(SignalRequest::Disconnect(reply))
            .expect("signaling actor");
        result.await.expect("signaling disconnect");
    }

    pub fn credentials(
        &self,
        role: protocol::MobilePeerRole,
        ice: protocol::MobileIceServer,
    ) -> protocol::MobileRtcCredentials {
        protocol::MobileRtcCredentials {
            protocol_version: protocol::MOBILE_RTC_PROTOCOL_VERSION,
            pairing_id: protocol::MobilePairingId("push-fixture".into()),
            role,
            signaling_url: protocol::MobileSignalingUrl(self.url.clone()),
            signaling_token: "local-test-only".into(),
            ice_servers: vec![ice],
            expires_at_ms: std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .expect("clock")
                .as_millis() as u64
                + 3_600_000,
        }
    }
}

fn offline() -> protocol::MobileSignalEvent {
    protocol::MobileSignalEvent::Failed {
        message: "host offline".into(),
    }
}

fn authorize(headers: &axum::http::HeaderMap) {
    assert!(
        headers
            .get("authorization")
            .is_some_and(|value| value == "Bearer local-test-only"),
        "signaling requires bearer authorization"
    );
}

async fn push_mobile(
    axum::extract::State(fixture): axum::extract::State<PushSignaling>,
    headers: axum::http::HeaderMap,
    axum::Json(command): axum::Json<protocol::MobileSignalCommand>,
) -> axum::Json<protocol::MobileSignalEvent> {
    authorize(&headers);
    let protocol::MobileSignalCommand::Publish { description } = command;
    assert_eq!(description.kind, MobileSdpKind::Offer);
    let (reply, result) = tokio::sync::oneshot::channel();
    fixture
        .commands
        .send(SignalRequest::Offer(description, reply))
        .expect("signaling actor");
    axum::Json(
        result
            .await
            .expect("held POST must receive an explicit outcome"),
    )
}

async fn push_host(
    axum::extract::State(fixture): axum::extract::State<PushSignaling>,
    headers: axum::http::HeaderMap,
    upgrade: axum::extract::WebSocketUpgrade,
) -> axum::response::Response {
    authorize(&headers);
    upgrade.on_upgrade(move |socket| push_socket(fixture, socket))
}

async fn push_socket(fixture: PushSignaling, mut socket: axum::extract::ws::WebSocket) {
    use axum::extract::ws::Message;
    use protocol::{MobileSignalCommand, MobileSignalEvent};
    let (host, mut offers) = tokio::sync::mpsc::unbounded_channel();
    let (reply, result) = tokio::sync::oneshot::channel();
    fixture
        .commands
        .send(SignalRequest::Register(host, reply))
        .expect("signaling actor");
    let generation = result.await.expect("host registration");
    let mut pending = None;
    loop {
        tokio::select! {
            request = offers.recv() => match request {
                Some(HostRequest::Offer(description, reply)) => {
                    assert!(pending.is_none(), "one pending room per pairing");
                    pending = Some(reply);
                    let event = MobileSignalEvent::Description { description };
                    if socket.send(Message::Text(serde_json::to_string(&event).expect("offer event").into())).await.is_err() { break; }
                }
                Some(HostRequest::Close) | None => {
                    let _ = socket.send(Message::Close(None)).await;
                    break;
                }
            },
            frame = socket.recv() => match frame {
                Some(Ok(Message::Text(text))) => {
                    let MobileSignalCommand::Publish { description } = serde_json::from_str(&text).expect("typed host answer");
                    assert_eq!(description.kind, MobileSdpKind::Answer);
                    let reply = pending.take().expect("answer requires pending phone POST");
                    let _ = reply.send(MobileSignalEvent::Description { description });
                    if socket.send(Message::Text(serde_json::to_string(&MobileSignalEvent::Waiting).expect("answer ack").into())).await.is_err() { break; }
                }
                Some(Ok(Message::Ping(bytes))) => { if socket.send(Message::Pong(bytes)).await.is_err() { break; } }
                Some(Ok(Message::Pong(_))) => {}
                _ => break,
            }
        }
    }
    if let Some(reply) = pending {
        let _ = reply.send(offline());
    }
    let _ = fixture.commands.send(SignalRequest::Unregister(generation));
}
