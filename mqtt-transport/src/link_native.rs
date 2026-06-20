//! Native [`MqttLink`] backend: rumqttc `AsyncClient` + `EventLoop` + rustls TLS.
//!
//! This is the only module in the protocol path that names rumqttc types. It
//! owns MQTT option/TLS construction and translates `rumqttc::v5::Event` into the
//! transport-neutral [`LinkEvent`] consumed by
//! [`protocol_driver`](crate::protocol_driver). A future wasm backend will live
//! beside this one (`link_wasm.rs`) behind `cfg(target_arch = "wasm32")`.

use std::sync::Arc;

use rand::RngCore;
use rand::rngs::OsRng;
use rumqttc::v5::mqttbytes::QoS;
use rumqttc::v5::mqttbytes::v5::{
    Packet, PubAckProperties, PubAckReason, SubAck, SubAckProperties, SubscribeReasonCode,
};
use rumqttc::v5::{AsyncClient, Event, EventLoop, MqttOptions};
use rumqttc::{TlsConfiguration, Transport};
#[cfg(feature = "test-support")]
use std::net::IpAddr;
use tokio_rustls::rustls::{ClientConfig, RootCertStore};

use crate::chunking::MAX_PLAINTEXT_CHUNK_LEN;
use crate::config::ParticipantRole;
use crate::error::{MqttTransportError, PublishRejection};
use crate::link::{IncomingPublish, LinkEvent, MqttLink};
use crate::types::{BrokerAuth, BrokerEndpoint};
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
}

impl NativeMqttLink {
    pub(crate) fn connect(
        endpoint: &BrokerEndpoint,
        role: ParticipantRole,
        tls_ca_pem: Option<Vec<u8>>,
    ) -> Result<Self, MqttTransportError> {
        let options = mqtt_options(&endpoint.url, &endpoint.auth, role, tls_ca_pem)?;
        let (client, eventloop) = AsyncClient::new(options, EVENTLOOP_REQUEST_CAPACITY);
        Ok(Self { client, eventloop })
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
    ) -> Result<(), MqttTransportError> {
        self.client
            .publish(topic.to_owned(), QoS::AtLeastOnce, false, payload)
            .await
            .map_err(|source| MqttTransportError::Publish {
                source: Box::new(source),
            })
    }

    async fn poll(&mut self) -> Result<LinkEvent, MqttTransportError> {
        let event = self
            .eventloop
            .poll()
            .await
            .map_err(|source| MqttTransportError::BrokerConnect {
                source: Box::new(source),
            })?;
        Ok(translate_event(event))
    }

    async fn disconnect(&mut self) {
        let _disconnect_result = self.client.disconnect().await;
    }
}

/// Translate a rumqttc event into the transport-neutral [`LinkEvent`]. The
/// reason-code validation that depends on rumqttc enums happens here so the
/// driver never sees them.
fn translate_event(event: Event) -> LinkEvent {
    match event {
        Event::Incoming(Packet::Publish(publish)) => LinkEvent::Publish(IncomingPublish {
            topic: publish.topic.to_vec(),
            payload: publish.payload.to_vec(),
            retain: publish.retain,
        }),
        Event::Incoming(Packet::PubAck(puback)) => {
            LinkEvent::PubAck(validate_puback(puback.reason, puback.properties.as_ref()))
        }
        Event::Incoming(Packet::SubAck(suback)) => {
            let debug = format!("{suback:?}");
            LinkEvent::SubAck {
                result: validate_suback(suback),
                debug,
            }
        }
        Event::Incoming(Packet::Disconnect(disconnect)) => LinkEvent::Disconnect {
            reason: format!("{disconnect:?}"),
        },
        Event::Incoming(_) | Event::Outgoing(_) => LinkEvent::Other,
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
        code: reason,
        reason_string: properties.and_then(|properties| properties.reason_string.clone()),
    }
}

fn suback_reason(reason: SubscribeReasonCode, properties: Option<&SubAckProperties>) -> String {
    match properties.and_then(|properties| properties.reason_string.as_deref()) {
        Some(reason_string) => format!("{reason:?}: {reason_string}"),
        None => format!("{reason:?}"),
    }
}

pub(crate) fn mqtt_options(
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
