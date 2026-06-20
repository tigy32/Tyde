//! Public connection entry points for the native build.
//!
//! This module is intentionally thin: it wires the native rumqttc backend
//! ([`NativeMqttLink`](crate::link_native::NativeMqttLink)) to the
//! transport-agnostic [`ProtocolDriver`](crate::protocol_driver::ProtocolDriver)
//! and exposes the same `connect` / `connect_ephemeral` API as before. All
//! reusable protocol logic lives in `protocol_driver`; all rumqttc/TLS specifics
//! live in `link_native`. A Phase-2 wasm build will add a parallel entry point
//! over a `web-sys::WebSocket` backend.

use std::collections::VecDeque;
use std::time::Duration;

use futures_channel::mpsc::channel;
#[cfg(test)]
use std::sync::Arc;
#[cfg(test)]
use tokio::sync::Barrier;
use tokio::sync::{mpsc, oneshot};

use crate::config::MqttConnectConfig;
use crate::error::MqttTransportError;
use crate::framing::SESSION_SALT_LEN;
use crate::link_native::NativeMqttLink;
use crate::protocol_driver::{
    EphemeralDataRoom, ProtocolDriver, PublishPacer, generate_session_salt,
    negotiate_ephemeral_data_room,
};
use crate::stream::{EnvelopeStream, InboundEvent, OutboundChunk};

const OUTBOUND_CHUNK_CAPACITY: usize = 64;
const INBOUND_EVENT_CAPACITY: usize = 64;
const RENDEZVOUS_DATA_CONNECT_TIMEOUT: Duration = Duration::from_secs(20);

#[derive(Debug, Clone, Default)]
struct ConnectOverrides {
    tls_ca_pem: Option<Vec<u8>>,
    fixed_session_salt: Option<[u8; SESSION_SALT_LEN]>,
    #[cfg(test)]
    subscribe_barrier: Option<Arc<Barrier>>,
}

pub async fn connect(config: MqttConnectConfig) -> Result<EnvelopeStream, MqttTransportError> {
    connect_inner(config, ConnectOverrides::default()).await
}

pub async fn connect_ephemeral(
    config: MqttConnectConfig,
) -> Result<EnvelopeStream, MqttTransportError> {
    connect_ephemeral_inner(config, ConnectOverrides::default()).await
}

#[cfg(test)]
pub(crate) async fn connect_with_test_overrides(
    config: MqttConnectConfig,
    tls_ca_pem: Vec<u8>,
    fixed_session_salt: Option<[u8; SESSION_SALT_LEN]>,
    subscribe_barrier: Option<Arc<Barrier>>,
) -> Result<EnvelopeStream, MqttTransportError> {
    connect_inner(
        config,
        ConnectOverrides {
            tls_ca_pem: Some(tls_ca_pem),
            fixed_session_salt,
            subscribe_barrier,
        },
    )
    .await
}

#[cfg(test)]
pub(crate) async fn connect_ephemeral_with_test_overrides(
    config: MqttConnectConfig,
    tls_ca_pem: Vec<u8>,
    fixed_session_salt: Option<[u8; SESSION_SALT_LEN]>,
    subscribe_barrier: Option<Arc<Barrier>>,
) -> Result<EnvelopeStream, MqttTransportError> {
    connect_ephemeral_inner(
        config,
        ConnectOverrides {
            tls_ca_pem: Some(tls_ca_pem),
            fixed_session_salt,
            subscribe_barrier,
        },
    )
    .await
}

async fn connect_inner(
    config: MqttConnectConfig,
    overrides: ConnectOverrides,
) -> Result<EnvelopeStream, MqttTransportError> {
    let local_salt = match overrides.fixed_session_salt {
        Some(salt) => salt,
        None => generate_session_salt(),
    };

    let inbound_topic = config.role.inbound_topic(&config.room);
    let outbound_topic = config.role.outbound_topic(&config.room);
    let link = NativeMqttLink::connect(&config.endpoint, config.role, overrides.tls_ca_pem)?;

    let (outbound_tx, outbound_rx) = channel::<OutboundChunk>(OUTBOUND_CHUNK_CAPACITY);
    let (inbound_tx, inbound_rx) = mpsc::channel::<InboundEvent>(INBOUND_EVENT_CAPACITY);
    let (ready_tx, ready_rx) = oneshot::channel::<Result<(), MqttTransportError>>();

    let actor = ProtocolDriver {
        config,
        link,
        inbound_topic,
        outbound_topic,
        local_salt,
        pending_peer_salt: None,
        established_peer_salt: None,
        pending_data_frames: VecDeque::new(),
        outbound_rx,
        inbound_tx,
        ready_tx: Some(ready_tx),
        publish_pacer: PublishPacer::new(),
        #[cfg(test)]
        subscribe_barrier: overrides.subscribe_barrier,
    };

    tokio::spawn(async move {
        actor.run().await;
    });

    match ready_rx.await {
        Ok(Ok(())) => Ok(EnvelopeStream::new(outbound_tx, inbound_rx)),
        Ok(Err(error)) => Err(error),
        Err(_) => Err(MqttTransportError::ActorClosed),
    }
}

async fn connect_ephemeral_inner(
    config: MqttConnectConfig,
    overrides: ConnectOverrides,
) -> Result<EnvelopeStream, MqttTransportError> {
    let data = negotiate_ephemeral_data_room_native(&config, &overrides).await?;
    let data_config = MqttConnectConfig {
        endpoint: config.endpoint,
        room: data.room,
        psk: data.psk,
        role: config.role,
    };
    tokio::time::timeout(
        RENDEZVOUS_DATA_CONNECT_TIMEOUT,
        connect_inner(data_config, overrides),
    )
    .await
    .map_err(|_| MqttTransportError::BrokerDisconnected {
        reason: format!(
            "timed out waiting for MQTT ephemeral data room after {:?}",
            RENDEZVOUS_DATA_CONNECT_TIMEOUT
        ),
    })?
}

/// Construct the native link for the main (rendezvous) room and run the
/// transport-agnostic negotiation over it.
async fn negotiate_ephemeral_data_room_native(
    config: &MqttConnectConfig,
    overrides: &ConnectOverrides,
) -> Result<EphemeralDataRoom, MqttTransportError> {
    let inbound_topic = config.role.inbound_topic(&config.room);
    let outbound_topic = config.role.outbound_topic(&config.room);
    let mut link =
        NativeMqttLink::connect(&config.endpoint, config.role, overrides.tls_ca_pem.clone())?;
    negotiate_ephemeral_data_room(config, &inbound_topic, &outbound_topic, &mut link).await
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;
    use std::error::Error;
    use std::fs;
    use std::net::{SocketAddr, TcpListener, TcpStream};
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::thread;
    use std::time::Duration;

    use rcgen::{CertifiedKey, generate_simple_self_signed};
    use rumqttc::v5::mqttbytes::QoS;
    use rumqttc::v5::mqttbytes::v5::Packet;
    use rumqttc::v5::{AsyncClient, Event};
    use rumqttd::{
        Broker, Config, ConnectionSettings, Notification, RouterConfig, ServerSettings, TlsConfig,
    };
    use tempfile::TempDir;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::time::timeout;

    use super::*;
    use crate::config::ParticipantRole;
    use crate::error::{CryptoError, FramingError};
    use crate::framing::{SESSION_SALT_LEN, encode_data_frame, encode_handshake_frame};
    use crate::link_native::{default_root_cert_store, mqtt_options, validate_puback};
    use crate::protocol_driver::validate_post_session_handshake;
    use crate::session::SessionCipher;
    use crate::topic::{client_to_host_topic, host_to_client_topic};
    use crate::types::{BrokerAuth, BrokerEndpoint, PreSharedKey, RoomId};
    use protocol::BrokerUrl;

    const HOST_SALT: [u8; SESSION_SALT_LEN] = [0x11; SESSION_SALT_LEN];
    const CLIENT_SALT: [u8; SESSION_SALT_LEN] = [0x22; SESSION_SALT_LEN];
    static BROKER_TEST_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

    struct TestBroker {
        endpoint: BrokerEndpoint,
        ca_pem: Vec<u8>,
        publish_count: Arc<AtomicUsize>,
        _temp_dir: TempDir,
        _broker_thread: thread::JoinHandle<()>,
        _observer_thread: thread::JoinHandle<()>,
    }

    #[test]
    fn duplicate_same_salt_after_session_is_ignored() -> Result<(), Box<dyn Error>> {
        validate_post_session_handshake(Some(CLIENT_SALT), CLIENT_SALT)?;
        Ok(())
    }

    #[test]
    fn different_salt_after_session_fails() -> Result<(), Box<dyn Error>> {
        let err = validate_post_session_handshake(Some(CLIENT_SALT), [0x23; SESSION_SALT_LEN])
            .err()
            .ok_or("different salt unexpectedly accepted")?;
        assert!(matches!(
            err,
            MqttTransportError::Framing(FramingError::HandshakeAfterSession)
        ));
        Ok(())
    }

    #[test]
    fn missing_established_salt_after_session_fails() -> Result<(), Box<dyn Error>> {
        let err = validate_post_session_handshake(None, CLIENT_SALT)
            .err()
            .ok_or("missing established salt unexpectedly accepted")?;
        assert!(matches!(
            err,
            MqttTransportError::Framing(FramingError::HandshakeAfterSession)
        ));
        Ok(())
    }

    #[test]
    fn default_tls_roots_include_static_webpki_roots() {
        let roots = default_root_cert_store();
        assert!(
            roots.len() >= webpki_roots::TLS_SERVER_ROOTS.len(),
            "expected static webpki roots in MQTT TLS store, got {} roots",
            roots.len()
        );
    }

    #[tokio::test]
    async fn real_broker_happy_path() -> Result<(), Box<dyn Error>> {
        let _broker_guard = BROKER_TEST_LOCK.lock().await;
        let broker = start_tls_broker(None)?;
        let room = RoomId([0x31; 16]);
        let psk = PreSharedKey([0x41; 32]);
        let (mut host, mut client) = connect_pair(&broker, room, psk.clone(), psk).await?;

        let host_to_client = patterned_bytes(8192, 7);
        let client_to_host = patterned_bytes(6144, 19);
        host.write_all(&host_to_client).await?;
        host.flush().await?;
        client.write_all(&client_to_host).await?;
        client.flush().await?;

        let mut client_read = vec![0_u8; host_to_client.len()];
        client.read_exact(&mut client_read).await?;
        assert_eq!(client_read, host_to_client);

        let mut host_read = vec![0_u8; client_to_host.len()];
        host.read_exact(&mut host_read).await?;
        assert_eq!(host_read, client_to_host);
        Ok(())
    }

    #[tokio::test]
    async fn ephemeral_connections_share_main_room_without_cross_talk() -> Result<(), Box<dyn Error>>
    {
        let _broker_guard = BROKER_TEST_LOCK.lock().await;
        let broker = start_tls_broker(None)?;
        let main_room = RoomId([0x3c; 16]);
        let psk = PreSharedKey([0xc1; 32]);
        let (mut host_a, mut client_a) =
            connect_ephemeral_pair(&broker, main_room, psk.clone(), psk.clone()).await?;
        let (mut host_b, mut client_b) =
            connect_ephemeral_pair(&broker, main_room, psk.clone(), psk).await?;

        host_a.write_all(b"first session").await?;
        host_a.flush().await?;
        host_b.write_all(b"second session").await?;
        host_b.flush().await?;

        let mut first = vec![0_u8; b"first session".len()];
        let mut second = vec![0_u8; b"second session".len()];
        timeout(Duration::from_secs(5), client_a.read_exact(&mut first)).await??;
        timeout(Duration::from_secs(5), client_b.read_exact(&mut second)).await??;
        assert_eq!(first, b"first session");
        assert_eq!(second, b"second session");
        Ok(())
    }

    #[tokio::test]
    async fn buffered_small_writes_are_boxcarred_on_flush() -> Result<(), Box<dyn Error>> {
        let _broker_guard = BROKER_TEST_LOCK.lock().await;
        let broker = start_tls_broker(None)?;
        let room = RoomId([0x3a; 16]);
        let psk = PreSharedKey([0xb1; 32]);
        let (mut host, mut client) = connect_pair(&broker, room, psk.clone(), psk).await?;

        let before = broker.publish_count.load(Ordering::SeqCst);
        for index in 0_u8..10 {
            host.write_all(&[index]).await?;
        }
        host.flush().await?;

        let mut received = vec![0_u8; 10];
        timeout(Duration::from_secs(5), client.read_exact(&mut received)).await??;
        assert_eq!(received, (0_u8..10).collect::<Vec<_>>());

        let published = broker.publish_count.load(Ordering::SeqCst) - before;
        assert!(
            published <= 2,
            "expected boxcarred writes to use at most 2 publishes, got {published}"
        );
        Ok(())
    }

    #[tokio::test]
    async fn client_retries_handshake_until_delayed_host_subscribes() -> Result<(), Box<dyn Error>>
    {
        let _broker_guard = BROKER_TEST_LOCK.lock().await;
        let broker = start_tls_broker(None)?;
        let room = RoomId([0x38; 16]);
        let psk = PreSharedKey([0xa1; 32]);
        let host_config = MqttConnectConfig {
            endpoint: broker.endpoint.clone(),
            room,
            psk: psk.clone(),
            role: ParticipantRole::Host,
        };
        let client_config = MqttConnectConfig {
            endpoint: broker.endpoint.clone(),
            room,
            psk,
            role: ParticipantRole::Client,
        };

        let client = tokio::spawn(connect_with_test_overrides(
            client_config,
            broker.ca_pem.clone(),
            Some(CLIENT_SALT),
            None,
        ));
        wait_for_publish_count(&broker, 1).await?;

        let host =
            connect_with_test_overrides(host_config, broker.ca_pem.clone(), Some(HOST_SALT), None);
        let (mut host, mut client) = timeout(Duration::from_secs(10), async {
            let host = host.await?;
            let client = client.await??;
            Ok::<_, Box<dyn Error>>((host, client))
        })
        .await??;

        client.write_all(b"hello after delayed host").await?;
        client.flush().await?;
        let mut received = vec![0_u8; "hello after delayed host".len()];
        host.read_exact(&mut received).await?;
        assert_eq!(received, b"hello after delayed host");
        Ok(())
    }

    #[tokio::test]
    async fn duplicate_client_handshake_after_ready_preserves_data_stream()
    -> Result<(), Box<dyn Error>> {
        let _broker_guard = BROKER_TEST_LOCK.lock().await;
        let broker = start_tls_broker(None)?;
        let room = RoomId([0x3d; 16]);
        let psk = PreSharedKey([0xd1; 32]);
        let (mut host, mut client) = connect_pair(&broker, room, psk.clone(), psk).await?;

        publish_raw(
            &broker,
            &client_to_host_topic(&room),
            encode_handshake_frame(&CLIENT_SALT),
        )
        .await?;
        tokio::time::sleep(Duration::from_millis(100)).await;

        client.write_all(b"data after duplicate handshake").await?;
        client.flush().await?;
        let mut received = vec![0_u8; b"data after duplicate handshake".len()];
        timeout(Duration::from_secs(5), host.read_exact(&mut received)).await??;
        assert_eq!(received, b"data after duplicate handshake");
        Ok(())
    }

    #[tokio::test]
    async fn different_client_handshake_after_ready_fails_stream() -> Result<(), Box<dyn Error>> {
        let _broker_guard = BROKER_TEST_LOCK.lock().await;
        let broker = start_tls_broker(None)?;
        let room = RoomId([0x3e; 16]);
        let psk = PreSharedKey([0xd2; 32]);
        let (mut host, _client) = connect_pair(&broker, room, psk.clone(), psk).await?;

        publish_raw(
            &broker,
            &client_to_host_topic(&room),
            encode_handshake_frame(&[0x23; SESSION_SALT_LEN]),
        )
        .await?;

        let mut buf = [0_u8; 1];
        let read_result = timeout(Duration::from_secs(5), host.read(&mut buf)).await?;
        assert_handshake_after_session(read_result.err())?;
        Ok(())
    }

    #[tokio::test]
    async fn valid_pre_ready_data_frame_is_delivered_after_session_key()
    -> Result<(), Box<dyn Error>> {
        let _broker_guard = BROKER_TEST_LOCK.lock().await;
        let broker = start_tls_broker(None)?;
        let room = RoomId([0x39; 16]);
        let psk = PreSharedKey([0xa2; 32]);
        let host_config = MqttConnectConfig {
            endpoint: broker.endpoint.clone(),
            room,
            psk,
            role: ParticipantRole::Host,
        };
        let host_subscribed = Arc::new(Barrier::new(2));
        let host_task = tokio::spawn(connect_with_test_overrides(
            host_config,
            broker.ca_pem.clone(),
            Some(HOST_SALT),
            Some(host_subscribed.clone()),
        ));

        host_subscribed.wait().await;
        publish_raw(
            &broker,
            &client_to_host_topic(&room),
            encode_handshake_frame(&CLIENT_SALT),
        )
        .await?;
        let mut client_cipher = SessionCipher::new(
            &room,
            &PreSharedKey([0xa2; 32]),
            ParticipantRole::Client,
            &HOST_SALT,
            &CLIENT_SALT,
        )?;
        let early = client_cipher.encrypt_next(b"early-session-data")?;
        publish_raw(
            &broker,
            &client_to_host_topic(&room),
            encode_data_frame(early.counter, &early.ciphertext_with_tag),
        )
        .await?;

        let mut host = timeout(Duration::from_secs(10), host_task).await???;
        let mut received = vec![0_u8; b"early-session-data".len()];
        host.read_exact(&mut received).await?;
        assert_eq!(received, b"early-session-data");
        Ok(())
    }

    #[tokio::test]
    async fn invalid_pre_ready_data_frame_fails_after_session_key() -> Result<(), Box<dyn Error>> {
        let _broker_guard = BROKER_TEST_LOCK.lock().await;
        let broker = start_tls_broker(None)?;
        let room = RoomId([0x3b; 16]);
        let psk = PreSharedKey([0xa3; 32]);
        let host_config = MqttConnectConfig {
            endpoint: broker.endpoint.clone(),
            room,
            psk,
            role: ParticipantRole::Host,
        };
        let host_subscribed = Arc::new(Barrier::new(2));
        let host_task = tokio::spawn(connect_with_test_overrides(
            host_config,
            broker.ca_pem.clone(),
            Some(HOST_SALT),
            Some(host_subscribed.clone()),
        ));

        host_subscribed.wait().await;
        publish_raw(
            &broker,
            &client_to_host_topic(&room),
            bogus_data_frame_before_handshake(),
        )
        .await?;
        publish_raw(
            &broker,
            &client_to_host_topic(&room),
            encode_handshake_frame(&CLIENT_SALT),
        )
        .await?;

        let err = match timeout(Duration::from_secs(10), host_task).await?? {
            Ok(_) => return Err("invalid deferred data unexpectedly connected".into()),
            Err(error) => error,
        };
        assert!(matches!(
            err,
            MqttTransportError::Crypto(CryptoError::AeadFailure)
        ));
        Ok(())
    }

    #[tokio::test]
    async fn real_broker_wrong_psk_fails_with_aead() -> Result<(), Box<dyn Error>> {
        let _broker_guard = BROKER_TEST_LOCK.lock().await;
        let broker = start_tls_broker(None)?;
        let room = RoomId([0x32; 16]);
        let host_psk = PreSharedKey([0x51; 32]);
        let client_psk = PreSharedKey([0x52; 32]);
        let (mut host, mut client) = connect_pair(&broker, room, host_psk, client_psk).await?;

        client.write_all(b"wrong psk payload").await?;
        client.flush().await?;

        let mut buf = [0_u8; 64];
        let read_result = timeout(Duration::from_secs(5), host.read(&mut buf)).await?;
        assert_aead_failure(read_result.err())?;
        Ok(())
    }

    #[tokio::test]
    async fn real_broker_chunking_transparent_for_one_megabyte() -> Result<(), Box<dyn Error>> {
        let _broker_guard = BROKER_TEST_LOCK.lock().await;
        let broker = start_tls_broker(None)?;
        let room = RoomId([0x33; 16]);
        let psk = PreSharedKey([0x61; 32]);
        let (mut host, mut client) = connect_pair(&broker, room, psk.clone(), psk).await?;

        let payload = patterned_bytes(1024 * 1024, 23);
        client.write_all(&payload).await?;
        client.flush().await?;

        let mut received = vec![0_u8; payload.len()];
        let mut offset = 0;
        while offset < received.len() {
            let read = timeout(Duration::from_secs(5), host.read(&mut received[offset..]))
                .await
                .map_err(|_| {
                    format!(
                        "timed out after receiving {offset} of {} bytes; publishes observed: {}",
                        received.len(),
                        broker.publish_count.load(Ordering::SeqCst)
                    )
                })??;
            if read == 0 {
                return Err(format!("stream closed after receiving {offset} bytes").into());
            }
            offset += read;
        }

        assert_eq!(received, payload);
        assert!(
            broker.publish_count.load(Ordering::SeqCst) > 2,
            "expected more than one MQTT publish"
        );
        Ok(())
    }

    #[tokio::test]
    async fn real_broker_cross_room_misroute_fails_aead() -> Result<(), Box<dyn Error>> {
        let _broker_guard = BROKER_TEST_LOCK.lock().await;
        let broker = start_tls_broker(None)?;
        let room_a = RoomId([0x34; 16]);
        let room_b = RoomId([0x35; 16]);
        let psk = PreSharedKey([0x71; 32]);
        let (_host_b, mut client_b) =
            connect_pair(&broker, room_b, psk.clone(), psk.clone()).await?;

        let mut malicious_cipher = SessionCipher::new(
            &room_a,
            &psk,
            ParticipantRole::Host,
            &HOST_SALT,
            &CLIENT_SALT,
        )?;
        let encrypted = malicious_cipher.encrypt_next(b"room-a ciphertext on room-b topic")?;
        let frame = encode_data_frame(encrypted.counter, &encrypted.ciphertext_with_tag);
        publish_raw(&broker, &host_to_client_topic(&room_b), frame).await?;

        let mut buf = [0_u8; 64];
        let read_result = timeout(Duration::from_secs(5), client_b.read(&mut buf)).await?;
        assert_aead_failure(read_result.err())?;
        Ok(())
    }

    #[tokio::test]
    async fn insecure_url_rejected() -> Result<(), Box<dyn Error>> {
        for url in [
            "mqtt://broker.example.test:1883",
            "ws://broker.example.test/mqtt",
        ] {
            let config = MqttConnectConfig {
                endpoint: BrokerEndpoint {
                    url: BrokerUrl::new(url)?,
                    auth: BrokerAuth::Anonymous,
                },
                room: RoomId([0x36; 16]),
                psk: PreSharedKey([0x81; 32]),
                role: ParticipantRole::Client,
            };
            let err = connect(config).await.err();
            assert!(matches!(
                err,
                Some(MqttTransportError::Configuration { .. })
            ));
        }
        Ok(())
    }

    #[tokio::test]
    async fn mqtt5_connection_rejection_is_surfaced() -> Result<(), Box<dyn Error>> {
        let _broker_guard = BROKER_TEST_LOCK.lock().await;
        let mut auth = HashMap::new();
        auth.insert("allowed".to_string(), "secret".to_string());
        let broker = start_tls_broker(Some(auth))?;
        let config = MqttConnectConfig {
            endpoint: broker.endpoint.clone(),
            room: RoomId([0x37; 16]),
            psk: PreSharedKey([0x91; 32]),
            role: ParticipantRole::Client,
        };

        let err =
            connect_with_test_overrides(config, broker.ca_pem.clone(), Some(CLIENT_SALT), None)
                .await
                .err();
        match err {
            Some(MqttTransportError::BrokerConnect { source }) => {
                let reason = source.to_string();
                assert!(!reason.is_empty(), "expected non-empty broker failure");
            }
            other => return Err(format!("expected BrokerConnect error, got {other:?}").into()),
        }
        Ok(())
    }

    #[test]
    fn mqtt5_connection_reason_code_display_is_preserved() {
        use rumqttc::v5::ConnectionError;
        use rumqttc::v5::mqttbytes::v5::ConnectReturnCode;

        let error = MqttTransportError::BrokerConnect {
            source: Box::new(ConnectionError::ConnectionRefused(
                ConnectReturnCode::NotAuthorized,
            )),
        };
        let message = error.to_string();
        assert!(message.contains("NotAuthorized"));
    }

    async fn connect_pair(
        broker: &TestBroker,
        room: RoomId,
        host_psk: PreSharedKey,
        client_psk: PreSharedKey,
    ) -> Result<(EnvelopeStream, EnvelopeStream), Box<dyn Error>> {
        let host_config = MqttConnectConfig {
            endpoint: broker.endpoint.clone(),
            room,
            psk: host_psk,
            role: ParticipantRole::Host,
        };
        let client_config = MqttConnectConfig {
            endpoint: broker.endpoint.clone(),
            room,
            psk: client_psk,
            role: ParticipantRole::Client,
        };
        let barrier = Arc::new(Barrier::new(2));
        let host = connect_with_test_overrides(
            host_config,
            broker.ca_pem.clone(),
            Some(HOST_SALT),
            Some(barrier.clone()),
        );
        let client = connect_with_test_overrides(
            client_config,
            broker.ca_pem.clone(),
            Some(CLIENT_SALT),
            Some(barrier),
        );
        let connected = timeout(Duration::from_secs(10), async {
            tokio::try_join!(host, client)
        })
        .await??;
        Ok(connected)
    }

    async fn connect_ephemeral_pair(
        broker: &TestBroker,
        room: RoomId,
        host_psk: PreSharedKey,
        client_psk: PreSharedKey,
    ) -> Result<(EnvelopeStream, EnvelopeStream), Box<dyn Error>> {
        let host_config = MqttConnectConfig {
            endpoint: broker.endpoint.clone(),
            room,
            psk: host_psk,
            role: ParticipantRole::Host,
        };
        let client_config = MqttConnectConfig {
            endpoint: broker.endpoint.clone(),
            room,
            psk: client_psk,
            role: ParticipantRole::Client,
        };
        let connected = timeout(Duration::from_secs(10), async {
            tokio::try_join!(
                connect_ephemeral_with_test_overrides(
                    host_config,
                    broker.ca_pem.clone(),
                    Some(HOST_SALT),
                    None,
                ),
                connect_ephemeral_with_test_overrides(
                    client_config,
                    broker.ca_pem.clone(),
                    Some(CLIENT_SALT),
                    None,
                )
            )
        })
        .await??;
        Ok(connected)
    }

    async fn publish_raw(
        broker: &TestBroker,
        topic: &str,
        frame: Vec<u8>,
    ) -> Result<(), Box<dyn Error>> {
        let options = mqtt_options(
            &broker.endpoint.url,
            &BrokerAuth::Anonymous,
            ParticipantRole::Host,
            Some(broker.ca_pem.clone()),
        )?;
        let (client, mut eventloop) = AsyncClient::new(options, 16);
        client
            .publish(topic.to_string(), QoS::AtLeastOnce, false, frame)
            .await?;
        timeout(Duration::from_secs(5), async {
            loop {
                match eventloop.poll().await.map_err(|source| {
                    MqttTransportError::BrokerConnect {
                        source: Box::new(source),
                    }
                })? {
                    Event::Incoming(Packet::PubAck(puback)) => {
                        validate_puback(puback.reason, puback.properties.as_ref())?;
                        return Ok::<(), MqttTransportError>(());
                    }
                    Event::Incoming(Packet::Disconnect(disconnect)) => {
                        return Err(MqttTransportError::BrokerDisconnected {
                            reason: format!("raw publisher disconnected: {disconnect:?}"),
                        });
                    }
                    Event::Incoming(_) | Event::Outgoing(_) => {}
                }
            }
        })
        .await??;
        Ok(())
    }

    fn start_tls_broker(
        auth: Option<HashMap<String, String>>,
    ) -> Result<TestBroker, Box<dyn Error>> {
        let temp_dir = TempDir::new()?;
        let CertifiedKey { cert, signing_key } =
            generate_simple_self_signed(vec!["localhost".to_string()])?;
        let cert_pem = cert.pem();
        let key_pem = signing_key.serialize_pem();
        let cert_path = temp_dir.path().join("server.cert.pem");
        let key_path = temp_dir.path().join("server.key.pem");
        fs::write(&cert_path, &cert_pem)?;
        fs::write(&key_path, key_pem)?;

        let listener = TcpListener::bind(("127.0.0.1", 0))?;
        let port = listener.local_addr()?.port();
        drop(listener);

        let listen = SocketAddr::from(([127, 0, 0, 1], port));
        let mut v5 = HashMap::new();
        v5.insert(
            "test".to_string(),
            ServerSettings {
                name: format!("mqtt-transport-test-{port}"),
                listen,
                tls: Some(TlsConfig::Rustls {
                    capath: None,
                    certpath: cert_path.to_string_lossy().to_string(),
                    keypath: key_path.to_string_lossy().to_string(),
                }),
                next_connection_delay_ms: 1,
                connections: ConnectionSettings {
                    connection_timeout_ms: 60_000,
                    max_payload_size: 2 * 1024 * 1024,
                    max_inflight_count: 1_000,
                    auth,
                    external_auth: None,
                    dynamic_filters: true,
                },
            },
        );

        let config = Config {
            id: port as usize,
            router: RouterConfig {
                max_connections: 1_000,
                max_outgoing_packet_count: 10_000,
                max_segment_size: 2 * 1024 * 1024,
                max_segment_count: 16,
                ..RouterConfig::default()
            },
            v5: Some(v5),
            ..Config::default()
        };

        let broker = Broker::new(config);
        let (mut link_tx, mut link_rx) = broker.link("mqtt-transport-test-observer")?;
        link_tx.subscribe("#")?;
        let publish_count = Arc::new(AtomicUsize::new(0));
        let observer_count = publish_count.clone();
        let observer_thread = thread::spawn(move || {
            loop {
                match link_rx.recv() {
                    Ok(Some(Notification::Forward(_))) => {
                        observer_count.fetch_add(1, Ordering::SeqCst);
                    }
                    Ok(Some(_)) | Ok(None) => {}
                    Err(_) => return,
                }
            }
        });

        let broker_thread = thread::spawn(move || {
            let mut broker = broker;
            let _result = broker.start();
        });

        wait_for_port(port)?;

        Ok(TestBroker {
            endpoint: BrokerEndpoint {
                url: BrokerUrl::new(format!("mqtts://localhost:{port}"))?,
                auth: BrokerAuth::Anonymous,
            },
            ca_pem: cert_pem.into_bytes(),
            publish_count,
            _temp_dir: temp_dir,
            _broker_thread: broker_thread,
            _observer_thread: observer_thread,
        })
    }

    fn wait_for_port(port: u16) -> Result<(), Box<dyn Error>> {
        for _ in 0..100 {
            match TcpStream::connect(("127.0.0.1", port)) {
                Ok(_) => return Ok(()),
                Err(_) => std::thread::sleep(Duration::from_millis(20)),
            }
        }
        Err(format!("broker did not listen on port {port}").into())
    }

    async fn wait_for_publish_count(
        broker: &TestBroker,
        minimum: usize,
    ) -> Result<(), Box<dyn Error>> {
        timeout(Duration::from_secs(2), async {
            loop {
                if broker.publish_count.load(Ordering::SeqCst) >= minimum {
                    return Ok::<(), Box<dyn Error>>(());
                }
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await?
    }

    fn patterned_bytes(len: usize, multiplier: u8) -> Vec<u8> {
        (0..len)
            .map(|index| ((index as u8).wrapping_mul(multiplier)).wrapping_add(3))
            .collect()
    }

    fn assert_aead_failure(err: Option<std::io::Error>) -> Result<(), Box<dyn Error>> {
        let err = err.ok_or("expected read error")?;
        let inner = err
            .into_inner()
            .ok_or("expected MqttTransportError inside io::Error")?;
        let transport = inner
            .downcast::<MqttTransportError>()
            .map_err(|inner| format!("expected MqttTransportError, got {inner:?}"))?;
        assert!(matches!(
            *transport,
            MqttTransportError::Crypto(CryptoError::AeadFailure)
        ));
        Ok(())
    }

    fn assert_handshake_after_session(err: Option<std::io::Error>) -> Result<(), Box<dyn Error>> {
        let err = err.ok_or("expected handshake-after-session read error")?;
        let inner = err
            .into_inner()
            .ok_or("expected MqttTransportError inside io::Error")?;
        let transport = inner
            .downcast::<MqttTransportError>()
            .map_err(|inner| format!("expected MqttTransportError, got {inner:?}"))?;
        assert!(matches!(
            *transport,
            MqttTransportError::Framing(FramingError::HandshakeAfterSession)
        ));
        Ok(())
    }

    fn bogus_data_frame_before_handshake() -> Vec<u8> {
        encode_data_frame(0, &[0_u8; 16])
    }
}
