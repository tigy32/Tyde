//! Native [`MqttLink`] backend: rumqttc `AsyncClient` + `EventLoop` + rustls TLS.
//!
//! This is the only module in the protocol path that names rumqttc types. It
//! owns MQTT option/TLS construction and translates `rumqttc::v5::Event` into the
//! transport-neutral [`LinkEvent`] consumed by
//! [`protocol_driver`](crate::protocol_driver). A future wasm backend will live
//! beside this one (`link_wasm.rs`) behind `cfg(target_arch = "wasm32")`.

use std::collections::{HashMap, VecDeque};
use std::sync::Arc;
#[cfg(test)]
use std::sync::atomic::{AtomicUsize, Ordering};
#[cfg(test)]
use std::time::Instant;

use rand::RngCore;
use rand::rngs::OsRng;
use rumqttc::v5::mqttbytes::QoS;
use rumqttc::v5::mqttbytes::v5::{
    Packet, PubAckProperties, PubAckReason, SubAck, SubAckProperties, SubscribeReasonCode,
};
use rumqttc::v5::{AsyncClient, Event, EventLoop, MqttOptions};
use rumqttc::{Outgoing, TlsConfiguration, Transport};
#[cfg(feature = "test-support")]
use std::net::IpAddr;
use tokio_rustls::rustls::{ClientConfig, RootCertStore};

use crate::chunking::MAX_PLAINTEXT_CHUNK_LEN;
use crate::config::{
    LinkBrokerAuth, LinkBrokerConfig, LinkClientId, ParticipantRole, link_broker_url,
};
use crate::error::{MqttTransportError, PublishRejection};
use crate::link::{
    IncomingPublish, LinkEvent, MQTT_QOS1_WINDOW, MqttLink, PublishAck, PublishToken,
};
use crate::types::BrokerAuth;
use protocol::BrokerUrl;
use std::time::Duration;

const KEEP_ALIVE: Duration = Duration::from_secs(30);
const EVENTLOOP_REQUEST_CAPACITY: usize = 128;
const MAX_MQTT_PACKET_SIZE: u32 = (MAX_PLAINTEXT_CHUNK_LEN as u32) + 1024;

/// rumqttc-backed [`MqttLink`]. Holds the request-side `AsyncClient` and the
/// driving `EventLoop` together so the driver sees a single connection surface.
pub(crate) struct NativeMqttLink {
    client: AsyncClient,
    eventloop: EventLoop,
    next_publish_token: u64,
    pending_publish_tokens: VecDeque<PublishToken>,
    publish_tokens_by_pkid: HashMap<u16, PublishToken>,
    #[cfg(test)]
    accepted_publish_count: Option<Arc<AtomicUsize>>,
    #[cfg(test)]
    diagnostic: Option<TestConnectionDiagnostic>,
}

#[cfg(test)]
pub(crate) struct TestConnectionDiagnosticContext {
    pub(crate) label: String,
    pub(crate) phase: &'static str,
    pub(crate) started_at: Instant,
    pub(crate) role: String,
    pub(crate) inbound_topic: String,
    pub(crate) outbound_topic: String,
    pub(crate) client_identity: String,
}

#[cfg(test)]
struct TestConnectionDiagnostic {
    context: TestConnectionDiagnosticContext,
    connack_observed: bool,
}

impl NativeMqttLink {
    pub(crate) fn connect(
        broker: &LinkBrokerConfig,
        tls_ca_pem: Option<Vec<u8>>,
    ) -> Result<Self, MqttTransportError> {
        let options = mqtt_options_for_link(broker, tls_ca_pem)?;
        let (client, eventloop) = AsyncClient::new(options, EVENTLOOP_REQUEST_CAPACITY);
        Ok(Self {
            client,
            eventloop,
            next_publish_token: 0,
            pending_publish_tokens: VecDeque::new(),
            publish_tokens_by_pkid: HashMap::new(),
            #[cfg(test)]
            accepted_publish_count: None,
            #[cfg(test)]
            diagnostic: None,
        })
    }

    #[cfg(test)]
    pub(crate) fn set_accepted_publish_count_for_test(
        &mut self,
        accepted_publish_count: Option<Arc<AtomicUsize>>,
    ) {
        self.accepted_publish_count = accepted_publish_count;
    }

    #[cfg(test)]
    pub(crate) fn set_test_connection_diagnostic(
        &mut self,
        context: TestConnectionDiagnosticContext,
    ) {
        self.diagnostic = Some(TestConnectionDiagnostic {
            context,
            connack_observed: false,
        });
    }

    fn allocate_publish_token(&mut self) -> Result<PublishToken, MqttTransportError> {
        let token = PublishToken::new(self.next_publish_token);
        self.next_publish_token = self.next_publish_token.checked_add(1).ok_or_else(|| {
            MqttTransportError::Configuration {
                message: "MQTT publish token counter exhausted".to_string(),
            }
        })?;
        Ok(token)
    }

    fn translate_event(&mut self, event: Event) -> Result<LinkEvent, MqttTransportError> {
        match event {
            #[cfg(test)]
            Event::Incoming(Packet::ConnAck(connack)) => {
                if let Some(diagnostic) = self.diagnostic.as_mut()
                    && !diagnostic.connack_observed
                {
                    diagnostic.connack_observed = true;
                    eprintln!(
                        "mqtt transport test connack label={} phase={} elapsed_ms={} role={} client_identity={} inbound_topic={} outbound_topic={} connack={connack:?}",
                        diagnostic.context.label,
                        diagnostic.context.phase,
                        diagnostic.context.started_at.elapsed().as_millis(),
                        diagnostic.context.role,
                        diagnostic.context.client_identity,
                        diagnostic.context.inbound_topic,
                        diagnostic.context.outbound_topic,
                    );
                }
                Ok(LinkEvent::Other)
            }
            #[cfg(not(test))]
            Event::Incoming(Packet::ConnAck(_)) => Ok(LinkEvent::Other),
            Event::Incoming(Packet::Publish(publish)) => Ok(LinkEvent::Publish(IncomingPublish {
                topic: publish.topic.to_vec(),
                payload: publish.payload.to_vec(),
                retain: publish.retain,
            })),
            Event::Incoming(Packet::PubAck(puback)) => {
                match self.publish_tokens_by_pkid.remove(&puback.pkid) {
                    Some(token) => {
                        let result = validate_puback(puback.reason, puback.properties.as_ref());
                        #[cfg(test)]
                        if result.is_ok()
                            && let Some(accepted_publish_count) = &self.accepted_publish_count
                        {
                            accepted_publish_count.fetch_add(1, Ordering::SeqCst);
                        }
                        Ok(LinkEvent::PubAck(PublishAck { token, result }))
                    }
                    None => {
                        // We never retransmit, so a PUBACK for a pkid we are not
                        // awaiting is a stray or duplicate ack from the broker.
                        // Dropping it keeps a healthy link alive; the driver's
                        // own in-flight bookkeeping still catches real desync.
                        tracing::warn!(
                            pkid = puback.pkid,
                            "ignoring MQTT PUBACK for unknown packet id (stray or duplicate)"
                        );
                        Ok(LinkEvent::Other)
                    }
                }
            }
            Event::Incoming(Packet::SubAck(suback)) => {
                let debug = format!("{suback:?}");
                Ok(LinkEvent::SubAck {
                    result: validate_suback(suback),
                    debug,
                })
            }
            Event::Incoming(Packet::Disconnect(disconnect)) => Ok(LinkEvent::Disconnect {
                reason: format!("{disconnect:?}"),
            }),
            Event::Outgoing(Outgoing::Publish(pkid)) => {
                let token = self.pending_publish_tokens.pop_front().ok_or(
                    MqttTransportError::PublishAckMismatch {
                        packet_id: Some(pkid),
                        token: None,
                    },
                )?;
                if self.publish_tokens_by_pkid.insert(pkid, token).is_some() {
                    return Err(MqttTransportError::PublishAckMismatch {
                        packet_id: Some(pkid),
                        token: Some(token.value()),
                    });
                }
                Ok(LinkEvent::Other)
            }
            Event::Incoming(_) | Event::Outgoing(_) => Ok(LinkEvent::Other),
        }
    }
}

impl MqttLink for NativeMqttLink {
    async fn subscribe(&mut self, topic: &str) -> Result<(), MqttTransportError> {
        self.client
            .subscribe(topic.to_owned(), QoS::AtLeastOnce)
            .await
            .map_err(|source| MqttTransportError::Subscribe {
                source: Box::new(source),
            })
    }

    async fn publish(
        &mut self,
        topic: &str,
        payload: Vec<u8>,
    ) -> Result<PublishToken, MqttTransportError> {
        let token = self.allocate_publish_token()?;
        self.client
            .publish(topic.to_owned(), QoS::AtLeastOnce, false, payload)
            .await
            .map_err(|source| MqttTransportError::Publish {
                source: Box::new(source),
            })?;
        self.pending_publish_tokens.push_back(token);
        Ok(token)
    }

    async fn poll(&mut self) -> Result<LinkEvent, MqttTransportError> {
        let event = match self.eventloop.poll().await {
            Ok(event) => event,
            Err(source) => {
                #[cfg(test)]
                if let Some(diagnostic) = self.diagnostic.as_ref() {
                    eprintln!(
                        "mqtt transport test poll failed label={} phase={} elapsed_ms={} role={} client_identity={} inbound_topic={} outbound_topic={} rumqttc_error={source:?}",
                        diagnostic.context.label,
                        diagnostic.context.phase,
                        diagnostic.context.started_at.elapsed().as_millis(),
                        diagnostic.context.role,
                        diagnostic.context.client_identity,
                        diagnostic.context.inbound_topic,
                        diagnostic.context.outbound_topic,
                    );
                }
                return Err(MqttTransportError::BrokerConnect {
                    source: Box::new(source),
                });
            }
        };
        self.translate_event(event)
    }

    async fn disconnect(&mut self) {
        let _disconnect_result = self.client.disconnect().await;
    }
}

fn validate_suback(suback: SubAck) -> Result<(), MqttTransportError> {
    let mut codes = suback.return_codes.into_iter();
    let first = codes
        .next()
        .ok_or_else(|| MqttTransportError::SubscribeRejected {
            reason: "SUBACK contained no reason codes".to_string(),
        })?;
    if codes.next().is_some() {
        return Err(MqttTransportError::SubscribeRejected {
            reason: "SUBACK contained more reason codes than requested subscriptions".to_string(),
        });
    }
    match first {
        SubscribeReasonCode::Success(QoS::AtLeastOnce) => Ok(()),
        SubscribeReasonCode::Success(qos) => Err(MqttTransportError::SubscribeRejected {
            reason: format!("broker granted unsupported QoS {qos:?}"),
        }),
        reason => Err(MqttTransportError::SubscribeRejected {
            reason: suback_reason(reason, suback.properties.as_ref()),
        }),
    }
}

pub(crate) fn validate_puback(
    reason: PubAckReason,
    properties: Option<&PubAckProperties>,
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
    properties: Option<&PubAckProperties>,
) -> PublishRejection {
    PublishRejection {
        code: puback_reason_code(reason),
        code_name: format!("{reason:?}"),
        reason_string: properties.and_then(|properties| properties.reason_string.clone()),
    }
}

/// Map rumqttc's (positionally-discriminated) `PubAckReason` to its canonical
/// MQTT5 numeric reason code. rumqttc does not assign the wire values to the
/// enum, so a `reason as u8` cast would be wrong (e.g. `QuotaExceeded` is
/// variant 7, not 0x97); the driver classifies quota rejections on this code.
fn puback_reason_code(reason: PubAckReason) -> u8 {
    match reason {
        PubAckReason::Success => 0x00,
        PubAckReason::NoMatchingSubscribers => 0x10,
        PubAckReason::UnspecifiedError => 0x80,
        PubAckReason::ImplementationSpecificError => 0x83,
        PubAckReason::NotAuthorized => 0x87,
        PubAckReason::TopicNameInvalid => 0x90,
        PubAckReason::PacketIdentifierInUse => 0x91,
        PubAckReason::QuotaExceeded => 0x97,
        PubAckReason::PayloadFormatInvalid => 0x99,
    }
}

fn suback_reason(reason: SubscribeReasonCode, properties: Option<&SubAckProperties>) -> String {
    match properties.and_then(|properties| properties.reason_string.as_deref()) {
        Some(reason_string) => format!("{reason:?}: {reason_string}"),
        None => format!("{reason:?}"),
    }
}

#[cfg(test)]
pub(crate) fn mqtt_options(
    broker_url: &BrokerUrl,
    auth: &BrokerAuth,
    role: ParticipantRole,
    tls_ca_pem: Option<Vec<u8>>,
) -> Result<MqttOptions, MqttTransportError> {
    mqtt_options_inner(
        broker_url,
        &LinkBrokerAuth::Legacy(auth.clone()),
        LinkClientId::Random(role),
        tls_ca_pem,
    )
}

pub(crate) fn mqtt_options_for_link(
    broker: &LinkBrokerConfig,
    tls_ca_pem: Option<Vec<u8>>,
) -> Result<MqttOptions, MqttTransportError> {
    mqtt_options_inner(
        link_broker_url(broker),
        &broker.auth,
        broker.client_id.clone(),
        tls_ca_pem,
    )
}

fn mqtt_options_inner(
    broker_url: &BrokerUrl,
    auth: &LinkBrokerAuth,
    client_id: LinkClientId,
    tls_ca_pem: Option<Vec<u8>>,
) -> Result<MqttOptions, MqttTransportError> {
    let parsed =
        url::Url::parse(broker_url.as_str()).map_err(|err| MqttTransportError::Configuration {
            message: format!("broker URL is invalid: {err}"),
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

    let client_id = match client_id {
        LinkClientId::Random(role) => random_client_id(role),
        LinkClientId::Exact(client_id) => client_id.as_str().to_owned(),
    };
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
    options.set_outgoing_inflight_upper_limit(MQTT_QOS1_WINDOW as u16);
    options.set_receive_maximum(Some(MQTT_QOS1_WINDOW as u16));

    match auth {
        LinkBrokerAuth::Legacy(BrokerAuth::Anonymous) => {}
        LinkBrokerAuth::Legacy(BrokerAuth::UsernamePassword { username, password }) => {
            options.set_credentials(username.clone(), password.clone());
        }
        LinkBrokerAuth::Managed(_) => {}
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

pub(crate) fn default_root_cert_store() -> RootCertStore {
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

/// Codec-parity tests for the Phase-2 wasm backend. These run natively (no wasm
/// runtime, no broker) and prove that the standalone `mqttbytes 0.6` codec the
/// wasm backend uses (a) round-trips its own PUBLISH/SUBSCRIBE encoding and
/// (b) produces packets the native rumqttc stack decodes identically — the
/// cross-implementation interop guarantee the two transports rely on.
#[cfg(test)]
mod codec_parity_tests {
    use bytes::BytesMut;

    #[test]
    fn mqttbytes_publish_round_trips_and_rumqttc_decodes_it() {
        use mqttbytes::QoS as MbQoS;
        use mqttbytes::v5::{Packet as MbPacket, Publish as MbPublish, read as mb_read};

        let mut publish =
            MbPublish::new("tyde/parity/topic", MbQoS::AtLeastOnce, b"frame".to_vec());
        publish.pkid = 7;

        let mut bytes = BytesMut::new();
        publish.write(&mut bytes).expect("mqttbytes encode PUBLISH");
        let wire = bytes.clone();

        // (a) mqttbytes self round-trip.
        match mb_read(&mut bytes, 64 * 1024).expect("mqttbytes decode") {
            MbPacket::Publish(decoded) => {
                assert_eq!(decoded.topic, "tyde/parity/topic");
                assert_eq!(decoded.payload.as_ref(), b"frame");
                assert_eq!(decoded.qos, MbQoS::AtLeastOnce);
                assert_eq!(decoded.pkid, 7);
            }
            other => panic!("mqttbytes decoded unexpected packet: {other:?}"),
        }

        // (b) rumqttc decodes the very same bytes the wasm backend would send.
        use rumqttc::v5::mqttbytes::QoS as RcQoS;
        use rumqttc::v5::mqttbytes::v5::Packet as RcPacket;
        let mut wire = wire;
        match RcPacket::read(&mut wire, Some(64 * 1024))
            .expect("rumqttc decode of mqttbytes PUBLISH")
        {
            RcPacket::Publish(decoded) => {
                assert_eq!(decoded.topic.as_ref(), b"tyde/parity/topic");
                assert_eq!(decoded.payload.as_ref(), b"frame");
                assert_eq!(decoded.qos, RcQoS::AtLeastOnce);
                assert_eq!(decoded.pkid, 7);
            }
            other => panic!("rumqttc decoded unexpected packet: {other:?}"),
        }
    }

    #[test]
    fn mqttbytes_subscribe_round_trips_and_rumqttc_decodes_it() {
        use mqttbytes::QoS as MbQoS;
        use mqttbytes::v5::{Packet as MbPacket, Subscribe as MbSubscribe, read as mb_read};

        let mut subscribe = MbSubscribe::new("tyde/parity/topic", MbQoS::AtLeastOnce);
        subscribe.pkid = 9;

        let mut bytes = BytesMut::new();
        subscribe
            .write(&mut bytes)
            .expect("mqttbytes encode SUBSCRIBE");
        let wire = bytes.clone();

        match mb_read(&mut bytes, 64 * 1024).expect("mqttbytes decode") {
            MbPacket::Subscribe(decoded) => {
                assert_eq!(decoded.pkid, 9);
                assert_eq!(decoded.filters.len(), 1);
                assert_eq!(decoded.filters[0].path, "tyde/parity/topic");
                assert_eq!(decoded.filters[0].qos, MbQoS::AtLeastOnce);
            }
            other => panic!("mqttbytes decoded unexpected packet: {other:?}"),
        }

        use rumqttc::v5::mqttbytes::v5::Packet as RcPacket;
        let mut wire = wire;
        match RcPacket::read(&mut wire, Some(64 * 1024))
            .expect("rumqttc decode of mqttbytes SUBSCRIBE")
        {
            RcPacket::Subscribe(decoded) => {
                assert_eq!(decoded.filters.len(), 1);
                assert_eq!(decoded.filters[0].path.as_str(), "tyde/parity/topic");
            }
            other => panic!("rumqttc decoded unexpected packet: {other:?}"),
        }
    }
}
