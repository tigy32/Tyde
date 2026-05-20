use std::str;
#[cfg(test)]
use std::sync::Arc;
use std::time::Duration;

use futures_channel::mpsc::{Receiver as OutboundReceiver, channel};
use futures_util::StreamExt;
use rand::RngCore;
use rand::rngs::OsRng;
use rumqttc::v5::mqttbytes::QoS;
use rumqttc::v5::mqttbytes::v5::{Packet, PubAckReason, Publish, SubscribeReasonCode};
use rumqttc::v5::{AsyncClient, Event, EventLoop, MqttOptions};
use rumqttc::{TlsConfiguration, Transport};
#[cfg(test)]
use tokio::sync::Barrier;
use tokio::sync::{mpsc, oneshot};

use crate::chunking::MAX_PLAINTEXT_CHUNK_LEN;
use crate::config::{MqttConnectConfig, ParticipantRole};
use crate::error::{FramingError, MqttTransportError};
use crate::framing::{
    SESSION_SALT_LEN, TransportFrame, decode_frame, encode_data_frame, encode_handshake_frame,
};
use crate::session::SessionCipher;
use crate::stream::{EnvelopeStream, InboundEvent};
use crate::types::BrokerAuth;
use protocol::BrokerUrl;

const KEEP_ALIVE: Duration = Duration::from_secs(30);
const EVENTLOOP_REQUEST_CAPACITY: usize = 128;
const OUTBOUND_CHUNK_CAPACITY: usize = 64;
const INBOUND_EVENT_CAPACITY: usize = 64;
const MAX_MQTT_PACKET_SIZE: u32 = (MAX_PLAINTEXT_CHUNK_LEN as u32) + 1024;

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
    let options = mqtt_options(
        &config.endpoint.url,
        &config.endpoint.auth,
        config.role,
        overrides.tls_ca_pem,
    )?;
    let (client, eventloop) = AsyncClient::new(options, EVENTLOOP_REQUEST_CAPACITY);

    let (outbound_tx, outbound_rx) = channel::<Vec<u8>>(OUTBOUND_CHUNK_CAPACITY);
    let (inbound_tx, inbound_rx) = mpsc::channel::<InboundEvent>(INBOUND_EVENT_CAPACITY);
    let (ready_tx, ready_rx) = oneshot::channel::<Result<(), MqttTransportError>>();

    let actor = MqttActor {
        config,
        client,
        eventloop,
        inbound_topic,
        outbound_topic,
        local_salt,
        pending_peer_salt: None,
        outbound_rx,
        inbound_tx,
        ready_tx: Some(ready_tx),
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

struct MqttActor {
    config: MqttConnectConfig,
    client: AsyncClient,
    eventloop: EventLoop,
    inbound_topic: String,
    outbound_topic: String,
    local_salt: [u8; SESSION_SALT_LEN],
    pending_peer_salt: Option<[u8; SESSION_SALT_LEN]>,
    outbound_rx: OutboundReceiver<Vec<u8>>,
    inbound_tx: mpsc::Sender<InboundEvent>,
    ready_tx: Option<oneshot::Sender<Result<(), MqttTransportError>>>,
    #[cfg(test)]
    subscribe_barrier: Option<Arc<Barrier>>,
}

impl MqttActor {
    async fn run(mut self) {
        match self.establish_session().await {
            Ok(cipher) => {
                if !self.send_ready(Ok(())) {
                    return;
                }
                self.run_stream(cipher).await;
            }
            Err(error) => {
                let _sent = self.send_ready(Err(error));
            }
        }
    }

    fn send_ready(&mut self, result: Result<(), MqttTransportError>) -> bool {
        match self.ready_tx.take() {
            Some(sender) => sender.send(result).is_ok(),
            None => false,
        }
    }

    async fn establish_session(&mut self) -> Result<SessionCipher, MqttTransportError> {
        self.client
            .subscribe(self.inbound_topic.clone(), QoS::AtLeastOnce)
            .await
            .map_err(|source| MqttTransportError::Subscribe {
                source: Box::new(source),
            })?;

        self.await_suback().await?;
        #[cfg(test)]
        if let Some(barrier) = self.configured_subscribe_barrier() {
            barrier.wait().await;
        }

        // The product lifecycle makes the host subscription the accept signal:
        // a host can be listening before the phone exists. With clean-session
        // and retained=false, a host salt published before the client
        // subscription would be lost. Therefore the host receives the client
        // salt first and then replies; the client publishes after its SUBACK.
        // This keeps the required subscription-before-publish invariant while
        // avoiding broker-side retained messages or transport fallbacks.
        let peer_salt = match self.config.role {
            ParticipantRole::Host => {
                let peer_salt = self.await_peer_salt().await?;
                self.publish_local_salt().await?;
                peer_salt
            }
            ParticipantRole::Client => {
                self.publish_local_salt().await?;
                self.await_peer_salt().await?
            }
        };
        let (host_salt, client_salt) = match self.config.role {
            ParticipantRole::Host => (self.local_salt, peer_salt),
            ParticipantRole::Client => (peer_salt, self.local_salt),
        };

        SessionCipher::new(
            &self.config.room,
            &self.config.psk,
            self.config.role,
            &host_salt,
            &client_salt,
        )
        .map_err(MqttTransportError::Crypto)
    }

    #[cfg(test)]
    fn configured_subscribe_barrier(&self) -> Option<Arc<Barrier>> {
        self.subscribe_barrier.clone()
    }

    async fn await_suback(&mut self) -> Result<(), MqttTransportError> {
        loop {
            let event = self.eventloop.poll().await.map_err(|source| {
                MqttTransportError::BrokerConnect {
                    source: Box::new(source),
                }
            })?;
            match event {
                Event::Incoming(Packet::SubAck(suback)) => {
                    let mut codes = suback.return_codes.into_iter();
                    let first =
                        codes
                            .next()
                            .ok_or_else(|| MqttTransportError::SubscribeRejected {
                                reason: "SUBACK contained no reason codes".to_string(),
                            })?;
                    if codes.next().is_some() {
                        return Err(MqttTransportError::SubscribeRejected {
                            reason:
                                "SUBACK contained more reason codes than requested subscriptions"
                                    .to_string(),
                        });
                    }
                    match first {
                        SubscribeReasonCode::Success(QoS::AtLeastOnce) => return Ok(()),
                        SubscribeReasonCode::Success(qos) => {
                            return Err(MqttTransportError::SubscribeRejected {
                                reason: format!("broker granted unsupported QoS {qos:?}"),
                            });
                        }
                        reason => {
                            return Err(MqttTransportError::SubscribeRejected {
                                reason: suback_reason(reason, suback.properties.as_ref()),
                            });
                        }
                    }
                }
                Event::Incoming(Packet::Disconnect(disconnect)) => {
                    return Err(MqttTransportError::BrokerDisconnected {
                        reason: format!("disconnect during subscribe: {disconnect:?}"),
                    });
                }
                Event::Incoming(Packet::Publish(publish)) => {
                    return Err(MqttTransportError::Framing(
                        unexpected_publish_before_suback(&publish),
                    ));
                }
                Event::Incoming(Packet::PubAck(puback)) => {
                    validate_puback(puback.reason, puback.properties.as_ref())?;
                }
                Event::Incoming(_) | Event::Outgoing(_) => {}
            }
        }
    }

    async fn await_peer_salt(&mut self) -> Result<[u8; SESSION_SALT_LEN], MqttTransportError> {
        if let Some(salt) = self.pending_peer_salt.take() {
            return Ok(salt);
        }

        loop {
            let event = self.eventloop.poll().await.map_err(|source| {
                MqttTransportError::BrokerConnect {
                    source: Box::new(source),
                }
            })?;
            match event {
                Event::Incoming(Packet::Publish(publish)) => {
                    let frame = self.decode_publish(publish)?;
                    match frame {
                        TransportFrame::Handshake { salt } => return Ok(salt),
                        TransportFrame::Data { .. } => {
                            return Err(MqttTransportError::Framing(
                                FramingError::DataBeforeHandshake,
                            ));
                        }
                    }
                }
                Event::Incoming(Packet::PubAck(puback)) => {
                    validate_puback(puback.reason, puback.properties.as_ref())?;
                }
                Event::Incoming(Packet::Disconnect(disconnect)) => {
                    return Err(MqttTransportError::BrokerDisconnected {
                        reason: format!("disconnect during salt exchange: {disconnect:?}"),
                    });
                }
                Event::Incoming(Packet::SubAck(suback)) => {
                    return Err(MqttTransportError::SubscribeRejected {
                        reason: format!(
                            "unexpected duplicate SUBACK during salt exchange: {suback:?}"
                        ),
                    });
                }
                Event::Incoming(_) | Event::Outgoing(_) => {}
            }
        }
    }

    async fn run_stream(mut self, mut cipher: SessionCipher) {
        loop {
            tokio::select! {
                event = self.eventloop.poll() => {
                    match event {
                        Ok(event) => {
                            if let Err(error) = self.handle_ready_event(event, &mut cipher).await {
                                send_inbound_error(self.inbound_tx.clone(), error).await;
                                return;
                            }
                        }
                        Err(error) => {
                            send_inbound_error(self.inbound_tx.clone(), MqttTransportError::BrokerDisconnected {
                                reason: error.to_string(),
                            }).await;
                            return;
                        }
                    }
                }
                outbound = self.outbound_rx.next() => {
                    match outbound {
                        Some(plaintext) => {
                            if let Err(error) = self.publish_plaintext(&mut cipher, &plaintext).await {
                                send_inbound_error(self.inbound_tx.clone(), error).await;
                                return;
                            }
                        }
                        None => {
                            let _disconnect_result = self.client.disconnect().await;
                            let _send_result = self.inbound_tx.send(InboundEvent::Eof).await;
                            return;
                        }
                    }
                }
            }
        }
    }

    async fn handle_ready_event(
        &mut self,
        event: Event,
        cipher: &mut SessionCipher,
    ) -> Result<(), MqttTransportError> {
        match event {
            Event::Incoming(Packet::Publish(publish)) => {
                let frame = self.decode_publish(publish)?;
                match frame {
                    TransportFrame::Handshake { .. } => Err(MqttTransportError::Framing(
                        FramingError::HandshakeAfterSession,
                    )),
                    TransportFrame::Data {
                        counter,
                        ciphertext_with_tag,
                    } => match cipher.decrypt_received(counter, &ciphertext_with_tag)? {
                        Some(plaintext) => {
                            self.inbound_tx
                                .send(InboundEvent::Data(plaintext))
                                .await
                                .map_err(|_| MqttTransportError::ActorClosed)?;
                            Ok(())
                        }
                        None => Ok(()),
                    },
                }
            }
            Event::Incoming(Packet::PubAck(puback)) => {
                validate_puback(puback.reason, puback.properties.as_ref())
            }
            Event::Incoming(Packet::Disconnect(disconnect)) => {
                Err(MqttTransportError::BrokerDisconnected {
                    reason: format!("disconnect after session established: {disconnect:?}"),
                })
            }
            Event::Incoming(_) | Event::Outgoing(_) => Ok(()),
        }
    }

    fn decode_publish(&self, publish: Publish) -> Result<TransportFrame, MqttTransportError> {
        if publish.retain {
            let topic = publish_topic_string(&publish)?;
            return Err(MqttTransportError::RetainedMessage { topic });
        }

        let topic = publish_topic_string(&publish)?;
        if topic != self.inbound_topic {
            return Err(MqttTransportError::Framing(FramingError::InvalidTopic {
                message: format!(
                    "received publish for topic {topic:?}; expected {:?}",
                    self.inbound_topic
                ),
            }));
        }

        decode_frame(publish.payload.as_ref()).map_err(MqttTransportError::Framing)
    }

    async fn publish_plaintext(
        &mut self,
        cipher: &mut SessionCipher,
        plaintext: &[u8],
    ) -> Result<(), MqttTransportError> {
        let encrypted = cipher.encrypt_next(plaintext)?;
        let frame = encode_data_frame(encrypted.counter, &encrypted.ciphertext_with_tag);
        self.client
            .publish(self.outbound_topic.clone(), QoS::AtLeastOnce, false, frame)
            .await
            .map_err(|source| MqttTransportError::Publish {
                source: Box::new(source),
            })?;
        // This actor owns the rumqttc event loop, so drive it until the QoS 1
        // publish is acknowledged instead of leaving queued chunks behind a
        // keep-alive wakeup.
        self.await_publish_ack(cipher).await
    }

    async fn publish_local_salt(&mut self) -> Result<(), MqttTransportError> {
        let handshake = encode_handshake_frame(&self.local_salt);
        self.client
            .publish(
                self.outbound_topic.clone(),
                QoS::AtLeastOnce,
                false,
                handshake,
            )
            .await
            .map_err(|source| MqttTransportError::Publish {
                source: Box::new(source),
            })?;
        // Keep session readiness behind the handshake PUBACK so the first data
        // chunk does not race an outstanding handshake publish.
        self.await_publish_ack_before_session().await
    }

    async fn await_publish_ack(
        &mut self,
        cipher: &mut SessionCipher,
    ) -> Result<(), MqttTransportError> {
        loop {
            let event = self.eventloop.poll().await.map_err(|source| {
                MqttTransportError::BrokerConnect {
                    source: Box::new(source),
                }
            })?;
            match event {
                Event::Incoming(Packet::PubAck(puback)) => {
                    validate_puback(puback.reason, puback.properties.as_ref())?;
                    return Ok(());
                }
                event => self.handle_ready_event(event, cipher).await?,
            }
        }
    }

    async fn await_publish_ack_before_session(&mut self) -> Result<(), MqttTransportError> {
        loop {
            let event = self.eventloop.poll().await.map_err(|source| {
                MqttTransportError::BrokerConnect {
                    source: Box::new(source),
                }
            })?;
            match event {
                Event::Incoming(Packet::PubAck(puback)) => {
                    validate_puback(puback.reason, puback.properties.as_ref())?;
                    return Ok(());
                }
                Event::Incoming(Packet::Disconnect(disconnect)) => {
                    return Err(MqttTransportError::BrokerDisconnected {
                        reason: format!("disconnect while publishing handshake: {disconnect:?}"),
                    });
                }
                Event::Incoming(Packet::Publish(publish)) => match self.decode_publish(publish)? {
                    TransportFrame::Handshake { salt } => {
                        self.pending_peer_salt = Some(salt);
                    }
                    TransportFrame::Data { .. } => {
                        return Err(MqttTransportError::Framing(
                            FramingError::DataBeforeHandshake,
                        ));
                    }
                },
                Event::Incoming(Packet::SubAck(suback)) => {
                    return Err(MqttTransportError::SubscribeRejected {
                        reason: format!("unexpected duplicate SUBACK during publish: {suback:?}"),
                    });
                }
                Event::Incoming(_) | Event::Outgoing(_) => {}
            }
        }
    }
}

async fn send_inbound_error(inbound_tx: mpsc::Sender<InboundEvent>, error: MqttTransportError) {
    let _send_result = inbound_tx.send(InboundEvent::Error(Box::new(error))).await;
}

fn mqtt_options(
    broker_url: &BrokerUrl,
    auth: &BrokerAuth,
    role: ParticipantRole,
    tls_ca_pem: Option<Vec<u8>>,
) -> Result<MqttOptions, MqttTransportError> {
    let parsed =
        url::Url::parse(broker_url.as_str()).map_err(|err| MqttTransportError::Configuration {
            message: format!("broker URL {:?} is invalid: {err}", broker_url.as_str()),
        })?;

    if !parsed.username().is_empty() || parsed.password().is_some() {
        return Err(MqttTransportError::Configuration {
            message:
                "broker credentials must be supplied through BrokerAuth, not embedded in the URL"
                    .to_string(),
        });
    }

    if parsed.fragment().is_some() {
        return Err(MqttTransportError::Configuration {
            message: "broker URL fragments are not valid MQTT transport configuration".to_string(),
        });
    }

    let client_id = random_client_id(role);
    let mut options = match parsed.scheme() {
        "mqtts" => mqtts_options(&parsed, client_id, tls_ca_pem)?,
        "wss" => wss_options(&parsed, client_id, tls_ca_pem),
        "mqtt" | "tcp" | "ws" => {
            return Err(MqttTransportError::Configuration {
                message: format!(
                    "broker URL scheme {:?} is insecure; only mqtts:// and wss:// are allowed",
                    parsed.scheme()
                ),
            });
        }
        scheme => {
            return Err(MqttTransportError::Configuration {
                message: format!(
                    "broker URL scheme {scheme:?} is unsupported; expected mqtts:// or wss://"
                ),
            });
        }
    };

    options.set_keep_alive(KEEP_ALIVE);
    options.set_clean_start(true);
    options.set_session_expiry_interval(Some(0));
    options.set_max_packet_size(Some(MAX_MQTT_PACKET_SIZE));

    match auth {
        BrokerAuth::Anonymous => {}
        BrokerAuth::UsernamePassword { username, password } => {
            options.set_credentials(username.clone(), password.clone());
        }
    }

    Ok(options)
}

fn mqtts_options(
    parsed: &url::Url,
    client_id: String,
    tls_ca_pem: Option<Vec<u8>>,
) -> Result<MqttOptions, MqttTransportError> {
    if parsed.path() != "/" && !parsed.path().is_empty() {
        return Err(MqttTransportError::Configuration {
            message: "mqtts:// broker URLs must not include a path".to_string(),
        });
    }

    let host = parsed
        .host_str()
        .ok_or_else(|| MqttTransportError::Configuration {
            message: "mqtts:// broker URL is missing a host".to_string(),
        })?;
    let port = parsed.port().unwrap_or(8883);
    let mut options = MqttOptions::new(client_id, host, port);
    let transport = match tls_ca_pem {
        Some(ca) => Transport::tls(ca, None, None),
        None => Transport::tls_with_config(TlsConfiguration::default()),
    };
    options.set_transport(transport);
    Ok(options)
}

fn wss_options(parsed: &url::Url, client_id: String, tls_ca_pem: Option<Vec<u8>>) -> MqttOptions {
    let mut options = MqttOptions::new(client_id, parsed.as_str(), parsed.port().unwrap_or(443));
    let transport = match tls_ca_pem {
        Some(ca) => Transport::wss(ca, None, None),
        None => Transport::wss_with_config(TlsConfiguration::default()),
    };
    options.set_transport(transport);
    options
}

fn generate_session_salt() -> [u8; SESSION_SALT_LEN] {
    let mut salt = [0_u8; SESSION_SALT_LEN];
    OsRng.fill_bytes(&mut salt);
    salt
}

fn random_client_id(role: ParticipantRole) -> String {
    let mut random = [0_u8; 16];
    OsRng.fill_bytes(&mut random);
    format!("{}-{}", role.client_id_prefix(), lower_hex(&random))
}

fn lower_hex(bytes: &[u8]) -> String {
    const DIGITS: &[u8; 16] = b"0123456789abcdef";
    let mut output = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        output.push(DIGITS[(byte >> 4) as usize] as char);
        output.push(DIGITS[(byte & 0x0f) as usize] as char);
    }
    output
}

fn publish_topic_string(publish: &Publish) -> Result<String, MqttTransportError> {
    str::from_utf8(publish.topic.as_ref())
        .map(|topic| topic.to_string())
        .map_err(|err| {
            MqttTransportError::Framing(FramingError::InvalidTopicUtf8 {
                message: err.to_string(),
            })
        })
}

fn unexpected_publish_before_suback(publish: &Publish) -> FramingError {
    match publish_topic_string(publish) {
        Ok(topic) => FramingError::InvalidTopic {
            message: format!("received publish for topic {topic:?} before SUBACK"),
        },
        Err(_) => FramingError::InvalidTopicUtf8 {
            message: "received publish with non-UTF-8 topic before SUBACK".to_string(),
        },
    }
}

fn validate_puback(
    reason: PubAckReason,
    properties: Option<&rumqttc::v5::mqttbytes::v5::PubAckProperties>,
) -> Result<(), MqttTransportError> {
    match reason {
        PubAckReason::Success | PubAckReason::NoMatchingSubscribers => Ok(()),
        reason => Err(MqttTransportError::PublishRejected {
            reason: puback_reason(reason, properties),
        }),
    }
}

fn puback_reason(
    reason: PubAckReason,
    properties: Option<&rumqttc::v5::mqttbytes::v5::PubAckProperties>,
) -> String {
    match properties.and_then(|properties| properties.reason_string.as_deref()) {
        Some(reason_string) => format!("{reason:?}: {reason_string}"),
        None => format!("{reason:?}"),
    }
}

fn suback_reason(
    reason: SubscribeReasonCode,
    properties: Option<&rumqttc::v5::mqttbytes::v5::SubAckProperties>,
) -> String {
    match properties.and_then(|properties| properties.reason_string.as_deref()) {
        Some(reason_string) => format!("{reason:?}: {reason_string}"),
        None => format!("{reason:?}"),
    }
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
    use rumqttd::{
        Broker, Config, ConnectionSettings, Notification, RouterConfig, ServerSettings, TlsConfig,
    };
    use tempfile::TempDir;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::time::timeout;

    use super::*;
    use crate::error::CryptoError;
    use crate::framing::{SESSION_SALT_LEN, encode_data_frame};
    use crate::session::SessionCipher;
    use crate::topic::host_to_client_topic;
    use crate::types::{BrokerEndpoint, PreSharedKey, RoomId};

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
        for url in ["mqtt://localhost:1883", "ws://localhost:8083/mqtt"] {
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
}
