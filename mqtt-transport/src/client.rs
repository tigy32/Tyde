use std::collections::VecDeque;
#[cfg(feature = "test-support")]
use std::net::IpAddr;
use std::str;
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
use tokio::time::{Instant, interval_at, sleep};
use tokio_rustls::rustls::{ClientConfig, RootCertStore};

use crate::chunking::MAX_PLAINTEXT_CHUNK_LEN;
use crate::config::{MqttConnectConfig, ParticipantRole};
use crate::error::{FramingError, MqttTransportError, PublishRejection};
use crate::framing::{
    SESSION_SALT_LEN, TransportFrame, decode_frame, encode_data_frame, encode_handshake_frame,
};
use crate::rendezvous::{
    ConnectionId, OpenAccept, OpenRequest, decode_open_accept, decode_open_request,
    derive_ephemeral_psk, encode_open_accept, encode_open_request, random_nonce,
};
use crate::session::SessionCipher;
use crate::stream::{EnvelopeStream, InboundEvent, OutboundChunk};
use crate::types::{BrokerAuth, PreSharedKey, RoomId};
use protocol::BrokerUrl;

const KEEP_ALIVE: Duration = Duration::from_secs(30);
const EVENTLOOP_REQUEST_CAPACITY: usize = 128;
const OUTBOUND_CHUNK_CAPACITY: usize = 64;
const INBOUND_EVENT_CAPACITY: usize = 64;
const MAX_MQTT_PACKET_SIZE: u32 = (MAX_PLAINTEXT_CHUNK_LEN as u32) + 1024;
const CLIENT_HANDSHAKE_RETRY_INTERVAL: Duration = Duration::from_millis(250);
const PUBLISH_RETRY_INITIAL: Duration = Duration::from_millis(250);
const PUBLISH_RETRY_MAX: Duration = Duration::from_secs(30);
const OUTBOUND_BOXCAR_DELAY: Duration = Duration::from_millis(100);
const RENDEZVOUS_RETRY_INTERVAL: Duration = Duration::from_millis(250);
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
    let options = mqtt_options(
        &config.endpoint.url,
        &config.endpoint.auth,
        config.role,
        overrides.tls_ca_pem,
    )?;
    let (client, eventloop) = AsyncClient::new(options, EVENTLOOP_REQUEST_CAPACITY);

    let (outbound_tx, outbound_rx) = channel::<OutboundChunk>(OUTBOUND_CHUNK_CAPACITY);
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
    let data = negotiate_ephemeral_data_room(&config, &overrides).await?;
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

struct EphemeralDataRoom {
    room: RoomId,
    psk: PreSharedKey,
}

async fn negotiate_ephemeral_data_room(
    config: &MqttConnectConfig,
    overrides: &ConnectOverrides,
) -> Result<EphemeralDataRoom, MqttTransportError> {
    let inbound_topic = config.role.inbound_topic(&config.room);
    let outbound_topic = config.role.outbound_topic(&config.room);
    let options = mqtt_options(
        &config.endpoint.url,
        &config.endpoint.auth,
        config.role,
        overrides.tls_ca_pem.clone(),
    )?;
    let (client, mut eventloop) = AsyncClient::new(options, EVENTLOOP_REQUEST_CAPACITY);

    client
        .subscribe(inbound_topic.clone(), QoS::AtLeastOnce)
        .await
        .map_err(|source| MqttTransportError::Subscribe {
            source: Box::new(source),
        })?;
    await_suback(&mut eventloop).await?;

    match config.role {
        ParticipantRole::Host => {
            await_open_and_accept(
                config,
                client,
                &mut eventloop,
                &inbound_topic,
                &outbound_topic,
            )
            .await
        }
        ParticipantRole::Client => {
            open_and_await_accept(
                config,
                client,
                &mut eventloop,
                &inbound_topic,
                &outbound_topic,
            )
            .await
        }
    }
}

async fn await_open_and_accept(
    config: &MqttConnectConfig,
    client: AsyncClient,
    eventloop: &mut EventLoop,
    inbound_topic: &str,
    outbound_topic: &str,
) -> Result<EphemeralDataRoom, MqttTransportError> {
    loop {
        let event = eventloop
            .poll()
            .await
            .map_err(|source| MqttTransportError::BrokerConnect {
                source: Box::new(source),
            })?;
        match event {
            Event::Incoming(Packet::Publish(publish)) => {
                if publish_topic_string(&publish)? != inbound_topic {
                    return Err(MqttTransportError::Framing(
                        crate::error::FramingError::InvalidTopic {
                            message: format!(
                                "received publish for topic {:?}; expected {inbound_topic:?}",
                                publish_topic_string(&publish)?
                            ),
                        },
                    ));
                }
                let request =
                    decode_open_request(&config.room, &config.psk, publish.payload.as_ref())?;
                let server_nonce = random_nonce();
                let accept = OpenAccept {
                    connection_id: request.connection_id,
                    client_nonce: request.client_nonce,
                    server_nonce,
                    data_room: request.proposed_data_room,
                };
                let frame = encode_open_accept(&config.room, &config.psk, &accept)?;
                publish_control_frame(&client, eventloop, outbound_topic, frame).await?;
                let psk = derive_ephemeral_psk(
                    &config.psk,
                    &config.room,
                    accept.connection_id,
                    &accept.client_nonce,
                    &accept.server_nonce,
                    &accept.data_room,
                )?;
                let _disconnect_result = client.disconnect().await;
                return Ok(EphemeralDataRoom {
                    room: accept.data_room,
                    psk,
                });
            }
            Event::Incoming(Packet::PubAck(puback)) => {
                validate_puback(puback.reason, puback.properties.as_ref())?;
            }
            Event::Incoming(Packet::Disconnect(disconnect)) => {
                return Err(MqttTransportError::BrokerDisconnected {
                    reason: format!("disconnect during rendezvous accept: {disconnect:?}"),
                });
            }
            Event::Incoming(_) | Event::Outgoing(_) => {}
        }
    }
}

async fn open_and_await_accept(
    config: &MqttConnectConfig,
    client: AsyncClient,
    eventloop: &mut EventLoop,
    inbound_topic: &str,
    outbound_topic: &str,
) -> Result<EphemeralDataRoom, MqttTransportError> {
    let request = OpenRequest {
        connection_id: ConnectionId::random(),
        client_nonce: random_nonce(),
        proposed_data_room: RoomId::random(),
    };
    let open_frame = encode_open_request(&config.room, &config.psk, &request)?;
    enqueue_control_frame(&client, outbound_topic, open_frame.clone()).await?;
    let mut retry = interval_at(
        Instant::now() + RENDEZVOUS_RETRY_INTERVAL,
        RENDEZVOUS_RETRY_INTERVAL,
    );

    loop {
        tokio::select! {
            _ = retry.tick() => {
                enqueue_control_frame(&client, outbound_topic, open_frame.clone()).await?;
            }
            event = eventloop.poll() => {
                let event = event.map_err(|source| MqttTransportError::BrokerConnect {
                    source: Box::new(source),
                })?;
                match event {
                    Event::Incoming(Packet::Publish(publish)) => {
                        if publish_topic_string(&publish)? != inbound_topic {
                            return Err(MqttTransportError::Framing(
                                crate::error::FramingError::InvalidTopic {
                                    message: format!(
                                        "received publish for topic {:?}; expected {inbound_topic:?}",
                                        publish_topic_string(&publish)?
                                    ),
                                },
                            ));
                        }
                        let accept = match decode_open_accept(
                            &config.room,
                            &config.psk,
                            publish.payload.as_ref(),
                        ) {
                            Ok(accept) => accept,
                            Err(crate::error::FramingError::UnknownTag { .. }) => continue,
                            Err(error) => return Err(MqttTransportError::Framing(error)),
                        };
                        if accept.connection_id != request.connection_id
                            || accept.client_nonce != request.client_nonce
                        {
                            continue;
                        }
                        let psk = derive_ephemeral_psk(
                            &config.psk,
                            &config.room,
                            accept.connection_id,
                            &accept.client_nonce,
                            &accept.server_nonce,
                            &accept.data_room,
                        )?;
                        let _disconnect_result = client.disconnect().await;
                        return Ok(EphemeralDataRoom {
                            room: accept.data_room,
                            psk,
                        });
                    }
                    Event::Incoming(Packet::PubAck(puback)) => {
                        validate_puback(puback.reason, puback.properties.as_ref())?;
                    }
                    Event::Incoming(Packet::Disconnect(disconnect)) => {
                        return Err(MqttTransportError::BrokerDisconnected {
                            reason: format!("disconnect during rendezvous open: {disconnect:?}"),
                        });
                    }
                    Event::Incoming(_) | Event::Outgoing(_) => {}
                }
            }
        }
    }
}

async fn publish_control_frame(
    client: &AsyncClient,
    eventloop: &mut EventLoop,
    topic: &str,
    frame: Vec<u8>,
) -> Result<(), MqttTransportError> {
    enqueue_control_frame(client, topic, frame).await?;
    await_publish_ack_before_stream(eventloop).await
}

async fn enqueue_control_frame(
    client: &AsyncClient,
    topic: &str,
    frame: Vec<u8>,
) -> Result<(), MqttTransportError> {
    client
        .publish(topic.to_owned(), QoS::AtLeastOnce, false, frame)
        .await
        .map_err(|source| MqttTransportError::Publish {
            source: Box::new(source),
        })
}

async fn await_suback(eventloop: &mut EventLoop) -> Result<(), MqttTransportError> {
    loop {
        let event = eventloop
            .poll()
            .await
            .map_err(|source| MqttTransportError::BrokerConnect {
                source: Box::new(source),
            })?;
        match event {
            Event::Incoming(Packet::SubAck(suback)) => {
                let mut codes = suback.return_codes.into_iter();
                let first = codes
                    .next()
                    .ok_or_else(|| MqttTransportError::SubscribeRejected {
                        reason: "SUBACK contained no reason codes".to_string(),
                    })?;
                if codes.next().is_some() {
                    return Err(MqttTransportError::SubscribeRejected {
                        reason: "SUBACK contained more reason codes than requested subscriptions"
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
                    reason: format!("disconnect during rendezvous subscribe: {disconnect:?}"),
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

async fn await_publish_ack_before_stream(
    eventloop: &mut EventLoop,
) -> Result<(), MqttTransportError> {
    loop {
        let event = eventloop
            .poll()
            .await
            .map_err(|source| MqttTransportError::BrokerConnect {
                source: Box::new(source),
            })?;
        match event {
            Event::Incoming(Packet::PubAck(puback)) => {
                validate_puback(puback.reason, puback.properties.as_ref())?;
                return Ok(());
            }
            Event::Incoming(Packet::Disconnect(disconnect)) => {
                return Err(MqttTransportError::BrokerDisconnected {
                    reason: format!("disconnect while publishing rendezvous frame: {disconnect:?}"),
                });
            }
            Event::Incoming(Packet::SubAck(suback)) => {
                return Err(MqttTransportError::SubscribeRejected {
                    reason: format!("unexpected duplicate SUBACK during rendezvous: {suback:?}"),
                });
            }
            Event::Incoming(Packet::Publish(_)) | Event::Incoming(_) | Event::Outgoing(_) => {}
        }
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
    pending_data_frames: VecDeque<PendingDataFrame>,
    outbound_rx: OutboundReceiver<OutboundChunk>,
    inbound_tx: mpsc::Sender<InboundEvent>,
    ready_tx: Option<oneshot::Sender<Result<(), MqttTransportError>>>,
    publish_pacer: PublishPacer,
    #[cfg(test)]
    subscribe_barrier: Option<Arc<Barrier>>,
}

impl MqttActor {
    async fn run(mut self) {
        match self.establish_session().await {
            Ok(mut cipher) => {
                if let Err(error) = self.flush_pending_data_frames(&mut cipher).await {
                    let _sent = self.send_ready(Err(error));
                    return;
                }
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
                self.await_peer_salt_with_client_retries().await?
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
                        TransportFrame::Data {
                            counter,
                            ciphertext_with_tag,
                        } => {
                            self.defer_data_frame(counter, ciphertext_with_tag);
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

    async fn await_peer_salt_with_client_retries(
        &mut self,
    ) -> Result<[u8; SESSION_SALT_LEN], MqttTransportError> {
        if let Some(salt) = self.pending_peer_salt.take() {
            return Ok(salt);
        }

        let mut retry = interval_at(
            Instant::now() + CLIENT_HANDSHAKE_RETRY_INTERVAL,
            CLIENT_HANDSHAKE_RETRY_INTERVAL,
        );
        loop {
            tokio::select! {
                _ = retry.tick() => {
                    self.publish_local_salt().await?;
                    if let Some(salt) = self.pending_peer_salt.take() {
                        return Ok(salt);
                    }
                }
                event = self.eventloop.poll() => {
                    let event = event.map_err(|source| MqttTransportError::BrokerConnect {
                        source: Box::new(source),
                    })?;
                    if let Some(salt) = self.handle_peer_salt_event(event)? {
                        return Ok(salt);
                    }
                }
            }
        }
    }

    fn handle_peer_salt_event(
        &mut self,
        event: Event,
    ) -> Result<Option<[u8; SESSION_SALT_LEN]>, MqttTransportError> {
        match event {
            Event::Incoming(Packet::Publish(publish)) => {
                let frame = self.decode_publish(publish)?;
                match frame {
                    TransportFrame::Handshake { salt } => Ok(Some(salt)),
                    TransportFrame::Data {
                        counter,
                        ciphertext_with_tag,
                    } => {
                        self.defer_data_frame(counter, ciphertext_with_tag);
                        Ok(None)
                    }
                }
            }
            Event::Incoming(Packet::PubAck(puback)) => {
                validate_puback(puback.reason, puback.properties.as_ref())?;
                Ok(None)
            }
            Event::Incoming(Packet::Disconnect(disconnect)) => {
                Err(MqttTransportError::BrokerDisconnected {
                    reason: format!("disconnect during salt exchange: {disconnect:?}"),
                })
            }
            Event::Incoming(Packet::SubAck(suback)) => Err(MqttTransportError::SubscribeRejected {
                reason: format!("unexpected duplicate SUBACK during salt exchange: {suback:?}"),
            }),
            Event::Incoming(_) | Event::Outgoing(_) => Ok(None),
        }
    }

    async fn run_stream(mut self, mut cipher: SessionCipher) {
        let mut deferred_outbound: Option<OutboundChunk> = None;
        loop {
            if let Some(outbound) = deferred_outbound.take() {
                match self
                    .boxcar_outbound(outbound, &mut cipher, &mut deferred_outbound)
                    .await
                {
                    Ok(batch) => {
                        if let Err(error) =
                            self.publish_plaintext(&mut cipher, &batch.plaintext).await
                        {
                            batch.ack_error(&error);
                            send_inbound_error(self.inbound_tx.clone(), error).await;
                            return;
                        }
                        batch.ack_success();
                    }
                    Err(error) => {
                        send_inbound_error(self.inbound_tx.clone(), error).await;
                        return;
                    }
                }
                continue;
            }

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
                        Some(outbound) => {
                            let batch = match self.boxcar_outbound(
                                outbound,
                                &mut cipher,
                                &mut deferred_outbound,
                            ).await {
                                Ok(batch) => batch,
                                Err(error) => {
                                    send_inbound_error(self.inbound_tx.clone(), error).await;
                                    return;
                                }
                            };
                            if let Err(error) = self.publish_plaintext(&mut cipher, &batch.plaintext).await {
                                batch.ack_error(&error);
                                send_inbound_error(self.inbound_tx.clone(), error).await;
                                return;
                            }
                            batch.ack_success();
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

    async fn boxcar_outbound(
        &mut self,
        first: OutboundChunk,
        cipher: &mut SessionCipher,
        deferred_outbound: &mut Option<OutboundChunk>,
    ) -> Result<BoxcarBatch, MqttTransportError> {
        let mut batch = BoxcarBatch::new(first);
        let delay = sleep(OUTBOUND_BOXCAR_DELAY);
        tokio::pin!(delay);

        loop {
            while batch.plaintext.len() < MAX_PLAINTEXT_CHUNK_LEN {
                match self.outbound_rx.try_recv() {
                    Ok(next) => {
                        if !append_or_defer(&mut batch, next, deferred_outbound) {
                            return Ok(batch);
                        }
                    }
                    Err(_) => break,
                }
            }

            if batch.plaintext.len() >= MAX_PLAINTEXT_CHUNK_LEN {
                return Ok(batch);
            }

            tokio::select! {
                _ = &mut delay => return Ok(batch),
                event = self.eventloop.poll() => {
                    let event = event.map_err(|source| MqttTransportError::BrokerConnect {
                        source: Box::new(source),
                    })?;
                    self.handle_ready_event(event, cipher).await?;
                }
                outbound = self.outbound_rx.next() => {
                    match outbound {
                        Some(next) => {
                            if !append_or_defer(&mut batch, next, deferred_outbound) {
                                return Ok(batch);
                            }
                        }
                        None => return Ok(batch),
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
                    } => {
                        self.handle_data_frame(counter, ciphertext_with_tag, cipher)
                            .await
                    }
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
        let counter = encrypted.counter;
        let plaintext_len = plaintext.len();
        let frame = encode_data_frame(encrypted.counter, &encrypted.ciphertext_with_tag);
        let mut retry = PublishRetryBackoff::new();
        loop {
            if let Err(error) = self.publish_frame(frame.clone()).await {
                if retryable_publish_error(&error) {
                    retry.sleep_after("enqueue data publish", &error).await;
                    continue;
                }
                return Err(error);
            }

            // This actor owns the rumqttc event loop, so drive it until the QoS 1
            // publish is acknowledged instead of leaving queued chunks behind a
            // keep-alive wakeup. Reuse the same encrypted frame and counter on
            // broker rejections; the receiver's counter validator deduplicates
            // any frame that was actually forwarded before the rejection surfaced.
            match self.await_publish_ack(cipher).await {
                Ok(()) => {
                    self.publish_pacer.record_success();
                    tracing::info!(
                        role = ?self.config.role,
                        counter,
                        plaintext_len,
                        "MQTT data publish accepted"
                    );
                    return Ok(());
                }
                Err(error) if retryable_publish_error(&error) => {
                    self.publish_pacer.record_rejection(&error);
                    retry.sleep_after("ack data publish", &error).await;
                }
                Err(error) => return Err(error),
            }
        }
    }

    async fn publish_local_salt(&mut self) -> Result<(), MqttTransportError> {
        let handshake = encode_handshake_frame(&self.local_salt);
        let mut retry = PublishRetryBackoff::new();
        loop {
            if let Err(error) = self.publish_frame(handshake.clone()).await {
                if retryable_publish_error(&error) {
                    retry.sleep_after("enqueue handshake publish", &error).await;
                    continue;
                }
                return Err(error);
            }

            // Keep session readiness behind the handshake PUBACK so the first data
            // chunk does not race an outstanding handshake publish.
            match self.await_publish_ack_before_session().await {
                Ok(()) => {
                    self.publish_pacer.record_success();
                    return Ok(());
                }
                Err(error) if retryable_publish_error(&error) => {
                    self.publish_pacer.record_rejection(&error);
                    retry.sleep_after("ack handshake publish", &error).await;
                }
                Err(error) => return Err(error),
            }
        }
    }

    async fn publish_frame(&mut self, frame: Vec<u8>) -> Result<(), MqttTransportError> {
        self.publish_pacer.wait_until_ready().await;
        self.client
            .publish(self.outbound_topic.clone(), QoS::AtLeastOnce, false, frame)
            .await
            .map_err(|source| MqttTransportError::Publish {
                source: Box::new(source),
            })
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
                    TransportFrame::Data {
                        counter,
                        ciphertext_with_tag,
                    } => {
                        self.defer_data_frame(counter, ciphertext_with_tag);
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

    fn defer_data_frame(&mut self, counter: u64, ciphertext_with_tag: Vec<u8>) {
        tracing::info!(
            role = ?self.config.role,
            counter,
            ciphertext_len = ciphertext_with_tag.len(),
            "MQTT data frame arrived before session was ready; deferring"
        );
        self.pending_data_frames.push_back(PendingDataFrame {
            counter,
            ciphertext_with_tag,
        });
    }

    async fn flush_pending_data_frames(
        &mut self,
        cipher: &mut SessionCipher,
    ) -> Result<(), MqttTransportError> {
        while let Some(frame) = self.pending_data_frames.pop_front() {
            self.handle_data_frame(frame.counter, frame.ciphertext_with_tag, cipher)
                .await?;
        }
        Ok(())
    }

    async fn handle_data_frame(
        &mut self,
        counter: u64,
        ciphertext_with_tag: Vec<u8>,
        cipher: &mut SessionCipher,
    ) -> Result<(), MqttTransportError> {
        match cipher.decrypt_received(counter, &ciphertext_with_tag)? {
            Some(plaintext) => {
                tracing::info!(
                    role = ?self.config.role,
                    counter,
                    plaintext_len = plaintext.len(),
                    "MQTT data frame decrypted"
                );
                self.inbound_tx
                    .send(InboundEvent::Data(plaintext))
                    .await
                    .map_err(|_| MqttTransportError::ActorClosed)?;
            }
            None => {
                tracing::info!(
                    role = ?self.config.role,
                    counter,
                    "MQTT duplicate data frame ignored"
                );
            }
        }
        Ok(())
    }
}

struct PendingDataFrame {
    counter: u64,
    ciphertext_with_tag: Vec<u8>,
}

struct PublishRetryBackoff {
    next: Duration,
}

impl PublishRetryBackoff {
    fn new() -> Self {
        Self {
            next: PUBLISH_RETRY_INITIAL,
        }
    }

    async fn sleep_after(&mut self, operation: &'static str, error: &MqttTransportError) {
        let delay = self.next;
        tracing::warn!(
            operation,
            delay_ms = delay.as_millis(),
            error = %error,
            "retrying MQTT publish"
        );
        sleep(delay).await;
        self.next = self.next.saturating_mul(2).min(PUBLISH_RETRY_MAX);
    }
}

struct PublishPacer {
    next_publish_at: Option<Instant>,
    paced_delay: Option<Duration>,
    successes_since_quota: u8,
}

impl PublishPacer {
    fn new() -> Self {
        Self {
            next_publish_at: None,
            paced_delay: None,
            successes_since_quota: 0,
        }
    }

    async fn wait_until_ready(&mut self) {
        let Some(next_publish_at) = self.next_publish_at else {
            return;
        };
        let now = Instant::now();
        if next_publish_at > now {
            let delay = next_publish_at - now;
            tracing::info!(
                delay_ms = delay.as_millis(),
                "pacing MQTT publish after broker quota rejection"
            );
            sleep(delay).await;
        }
        self.next_publish_at = None;
    }

    fn record_success(&mut self) {
        let Some(delay) = self.paced_delay else {
            return;
        };

        self.next_publish_at = Some(Instant::now() + delay);
        self.successes_since_quota = self.successes_since_quota.saturating_add(1);
        if self.successes_since_quota < 8 {
            return;
        }

        self.successes_since_quota = 0;
        let next_delay = delay / 2;
        if next_delay < PUBLISH_RETRY_INITIAL {
            self.paced_delay = None;
            self.next_publish_at = None;
            tracing::info!("cleared MQTT publish pacing after successful publishes");
        } else {
            self.paced_delay = Some(next_delay);
        }
    }

    fn record_rejection(&mut self, error: &MqttTransportError) {
        if !publish_error_is_quota_exceeded(error) {
            return;
        }

        let delay = self
            .paced_delay
            .map(|delay| delay.saturating_mul(2).min(PUBLISH_RETRY_MAX))
            .unwrap_or(PUBLISH_RETRY_INITIAL);
        self.paced_delay = Some(delay);
        self.successes_since_quota = 0;
        self.next_publish_at = Some(Instant::now() + delay);
        tracing::warn!(
            delay_ms = delay.as_millis(),
            "MQTT broker quota exceeded; pacing subsequent publishes"
        );
    }
}

fn publish_error_is_quota_exceeded(error: &MqttTransportError) -> bool {
    matches!(
        error,
        MqttTransportError::PublishRejected { reason } if reason.is_quota_exceeded()
    )
}

fn retryable_publish_error(error: &MqttTransportError) -> bool {
    matches!(
        error,
        MqttTransportError::BrokerConnect { .. }
            | MqttTransportError::Publish { .. }
            | MqttTransportError::PublishRejected { .. }
            | MqttTransportError::BrokerDisconnected { .. }
    )
}

struct BoxcarBatch {
    plaintext: Vec<u8>,
    acks: Vec<oneshot::Sender<Result<(), String>>>,
}

impl BoxcarBatch {
    fn new(first: OutboundChunk) -> Self {
        Self {
            plaintext: first.bytes,
            acks: vec![first.ack],
        }
    }

    fn ack_success(self) {
        for ack in self.acks {
            let _send_result = ack.send(Ok(()));
        }
    }

    fn ack_error(self, error: &MqttTransportError) {
        let message = error.to_string();
        for ack in self.acks {
            let _send_result = ack.send(Err(message.clone()));
        }
    }
}

fn append_or_defer(
    batch: &mut BoxcarBatch,
    next: OutboundChunk,
    deferred_outbound: &mut Option<OutboundChunk>,
) -> bool {
    if batch.plaintext.len().saturating_add(next.bytes.len()) <= MAX_PLAINTEXT_CHUNK_LEN {
        batch.plaintext.extend_from_slice(&next.bytes);
        batch.acks.push(next.ack);
        true
    } else {
        debug_assert!(deferred_outbound.is_none());
        *deferred_outbound = Some(next);
        false
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
        "mqtt" | "tcp" if loopback_plaintext_allowed(&parsed) => {
            mqtt_plaintext_options(&parsed, client_id)?
        }
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

fn mqtt_plaintext_options(
    parsed: &url::Url,
    client_id: String,
) -> Result<MqttOptions, MqttTransportError> {
    if parsed.path() != "/" && !parsed.path().is_empty() {
        return Err(MqttTransportError::Configuration {
            message: "mqtt:// broker URLs must not include a path".to_string(),
        });
    }

    let host = parsed
        .host_str()
        .ok_or_else(|| MqttTransportError::Configuration {
            message: "mqtt:// broker URL is missing a host".to_string(),
        })?;
    Ok(MqttOptions::new(
        client_id,
        host,
        parsed.port().unwrap_or(1883),
    ))
}

fn loopback_plaintext_allowed(parsed: &url::Url) -> bool {
    #[cfg(feature = "test-support")]
    {
        parsed.host_str().is_some_and(|host| {
            host.eq_ignore_ascii_case("localhost") || {
                host.parse::<IpAddr>()
                    .map(|addr| addr.is_loopback())
                    .unwrap_or(false)
            }
        })
    }
    #[cfg(not(feature = "test-support"))]
    {
        let _ = parsed;
        false
    }
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
        None => Transport::tls_with_config(default_tls_configuration()),
    };
    options.set_transport(transport);
    Ok(options)
}

fn wss_options(parsed: &url::Url, client_id: String, tls_ca_pem: Option<Vec<u8>>) -> MqttOptions {
    let mut options = MqttOptions::new(client_id, parsed.as_str(), parsed.port().unwrap_or(443));
    let transport = match tls_ca_pem {
        Some(ca) => Transport::wss(ca, None, None),
        None => Transport::wss_with_config(default_tls_configuration()),
    };
    options.set_transport(transport);
    options
}

fn default_tls_configuration() -> TlsConfiguration {
    TlsConfiguration::Rustls(Arc::new(default_rustls_client_config()))
}

fn default_rustls_client_config() -> ClientConfig {
    ClientConfig::builder()
        .with_root_certificates(default_root_cert_store())
        .with_no_client_auth()
}

fn default_root_cert_store() -> RootCertStore {
    let mut roots = RootCertStore::empty();
    let native = rustls_native_certs::load_native_certs();
    let native_error_count = native.errors.len();
    let (native_added, native_ignored) = roots.add_parsable_certificates(native.certs);
    let before_webpki = roots.len();

    // rustls-native-certs does not load the iOS trust store; keep Mozilla
    // roots compiled in so public MQTT brokers verify on real phones.
    roots.extend(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());

    tracing::debug!(
        native_added,
        native_ignored,
        native_error_count,
        webpki_added = roots.len().saturating_sub(before_webpki),
        total_roots = roots.len(),
        "configured MQTT TLS root store"
    );

    roots
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
        PubAckReason::Success => Ok(()),
        reason => Err(MqttTransportError::PublishRejected {
            reason: puback_rejection(reason, properties),
        }),
    }
}

fn puback_rejection(
    reason: PubAckReason,
    properties: Option<&rumqttc::v5::mqttbytes::v5::PubAckProperties>,
) -> PublishRejection {
    PublishRejection {
        code: reason,
        reason_string: properties.and_then(|properties| properties.reason_string.clone()),
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
    use crate::framing::{SESSION_SALT_LEN, encode_data_frame, encode_handshake_frame};
    use crate::session::SessionCipher;
    use crate::topic::{client_to_host_topic, host_to_client_topic};
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

    fn bogus_data_frame_before_handshake() -> Vec<u8> {
        encode_data_frame(0, &[0_u8; 16])
    }
}
