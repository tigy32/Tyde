//! Wasm [`MqttLink`] backend over the browser `web-sys::WebSocket` plus the
//! standalone `mqttbytes 0.6` MQTT5 codec.
//!
//! The browser terminates the `wss://` TLS, so there is no rustls here. JS
//! callbacks (`onopen`/`onmessage`/`onclose`/`onerror`) funnel signals into an
//! unbounded channel that [`poll`](MqttLink::poll) drains, reassembling MQTT5
//! packets from the (possibly fragmented/coalesced) binary frames and
//! translating them into the transport-neutral [`LinkEvent`]s the shared
//! [`protocol_driver`](crate::protocol_driver) consumes. MQTT keep-alive PINGREQ
//! is driven from inside `poll` (which the driver always has in flight), so no
//! extra task is needed.

use std::collections::VecDeque;
use std::time::Duration;

use bytes::BytesMut;
use futures_channel::mpsc::{UnboundedReceiver, unbounded};
use futures_util::StreamExt;
use mqttbytes::QoS;
use mqttbytes::v5::{
    Connect, ConnectProperties, ConnectReturnCode, Packet, PubAck, PubAckProperties, PubAckReason,
    Publish, SubAck, SubscribeReasonCode, read,
};
use rand::RngCore;
use rand::rngs::OsRng;
use wasm_bindgen::JsCast;
use wasm_bindgen::closure::Closure;
use web_sys::{BinaryType, CloseEvent, Event, MessageEvent, WebSocket};

use crate::chunking::MAX_PLAINTEXT_CHUNK_LEN;
use crate::config::ParticipantRole;
use crate::error::{MqttTransportError, PublishRejection};
use crate::link::{IncomingPublish, LinkEvent, MqttLink};
use crate::time::{Instant, Interval, interval_at};
use crate::types::{BrokerAuth, BrokerEndpoint};

const KEEP_ALIVE: Duration = Duration::from_secs(30);
const MAX_MQTT_PACKET_SIZE: usize = MAX_PLAINTEXT_CHUNK_LEN + 1024;
/// MQTT5 PUBACK reason code for "Quota exceeded" (0x97).
const PUBACK_QUOTA_EXCEEDED: u8 = 0x97;

/// A signal produced by one of the WebSocket JS callbacks.
enum WsSignal {
    Open,
    Bytes(Vec<u8>),
    Closed { reason: String },
    Error { reason: String },
}

/// Minimal boxable error for the seam (`MqttTransportError` sources are
/// `Box<dyn Error + Send + Sync>`); carries a human message.
#[derive(Debug)]
struct WasmLinkError(String);

impl std::fmt::Display for WasmLinkError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for WasmLinkError {}

fn publish_err(message: impl Into<String>) -> MqttTransportError {
    MqttTransportError::Publish {
        source: Box::new(WasmLinkError(message.into())),
    }
}

fn subscribe_err(message: impl Into<String>) -> MqttTransportError {
    MqttTransportError::Subscribe {
        source: Box::new(WasmLinkError(message.into())),
    }
}

fn connect_err(message: impl Into<String>) -> MqttTransportError {
    MqttTransportError::BrokerConnect {
        source: Box::new(WasmLinkError(message.into())),
    }
}

pub(crate) struct WasmMqttLink {
    ws: WebSocket,
    events: UnboundedReceiver<WsSignal>,
    incoming: BytesMut,
    pending: VecDeque<LinkEvent>,
    ping: Interval,
    next_pkid: u16,
    // Closures must outlive the socket; dropping them detaches the callbacks.
    _on_open: Closure<dyn FnMut(Event)>,
    _on_message: Closure<dyn FnMut(MessageEvent)>,
    _on_close: Closure<dyn FnMut(CloseEvent)>,
    _on_error: Closure<dyn FnMut(Event)>,
}

impl WasmMqttLink {
    /// Open the WebSocket, perform the MQTT5 CONNECT/CONNACK handshake, and return
    /// a link ready for `subscribe`/`publish`/`poll`.
    pub(crate) async fn connect(
        endpoint: &BrokerEndpoint,
        role: ParticipantRole,
    ) -> Result<Self, MqttTransportError> {
        let url = endpoint.url.as_str();
        if !url.starts_with("wss://") {
            // The wasm backend can only reach the broker over wss (mixed-content
            // and TLS-termination constraints); a stored mqtts:// host needs re-pair.
            return Err(MqttTransportError::Configuration {
                message: format!(
                    "wasm MQTT transport requires a wss:// broker URL; got {url:?} (re-pair needed)"
                ),
            });
        }

        let ws = WebSocket::new_with_str(url, "mqtt")
            .map_err(|err| connect_err(format!("failed to open WebSocket: {}", js_debug(&err))))?;
        ws.set_binary_type(BinaryType::Arraybuffer);

        let (tx, rx) = unbounded::<WsSignal>();

        let on_open = {
            let tx = tx.clone();
            Closure::<dyn FnMut(Event)>::new(move |_event: Event| {
                let _ = tx.unbounded_send(WsSignal::Open);
            })
        };
        let on_message = {
            let tx = tx.clone();
            Closure::<dyn FnMut(MessageEvent)>::new(move |event: MessageEvent| {
                if let Ok(buffer) = event.data().dyn_into::<js_sys::ArrayBuffer>() {
                    let bytes = js_sys::Uint8Array::new(&buffer).to_vec();
                    let _ = tx.unbounded_send(WsSignal::Bytes(bytes));
                }
            })
        };
        let on_close = {
            let tx = tx.clone();
            Closure::<dyn FnMut(CloseEvent)>::new(move |event: CloseEvent| {
                let _ = tx.unbounded_send(WsSignal::Closed {
                    reason: format!("code {} reason {:?}", event.code(), event.reason()),
                });
            })
        };
        let on_error = {
            let tx = tx.clone();
            Closure::<dyn FnMut(Event)>::new(move |_event: Event| {
                let _ = tx.unbounded_send(WsSignal::Error {
                    reason: "websocket error".to_string(),
                });
            })
        };

        ws.set_onopen(Some(on_open.as_ref().unchecked_ref()));
        ws.set_onmessage(Some(on_message.as_ref().unchecked_ref()));
        ws.set_onclose(Some(on_close.as_ref().unchecked_ref()));
        ws.set_onerror(Some(on_error.as_ref().unchecked_ref()));

        let ping = interval_at(Instant::now() + KEEP_ALIVE, KEEP_ALIVE);

        let mut link = Self {
            ws,
            events: rx,
            incoming: BytesMut::new(),
            pending: VecDeque::new(),
            ping,
            next_pkid: 0,
            _on_open: on_open,
            _on_message: on_message,
            _on_close: on_close,
            _on_error: on_error,
        };

        link.await_open().await?;
        link.send_connect(role, &endpoint.auth)?;
        link.await_connack().await?;
        Ok(link)
    }

    async fn await_open(&mut self) -> Result<(), MqttTransportError> {
        loop {
            match self.events.next().await {
                Some(WsSignal::Open) => return Ok(()),
                Some(WsSignal::Bytes(bytes)) => self.incoming.extend_from_slice(&bytes),
                Some(WsSignal::Closed { reason }) | Some(WsSignal::Error { reason }) => {
                    return Err(connect_err(format!(
                        "websocket closed before MQTT connect: {reason}"
                    )));
                }
                None => {
                    return Err(connect_err("websocket event channel closed before open"));
                }
            }
        }
    }

    async fn await_connack(&mut self) -> Result<(), MqttTransportError> {
        loop {
            // Drain any packets already buffered (CONNACK can arrive coalesced).
            while let Some(packet) = self.read_packet()? {
                match packet {
                    Packet::ConnAck(connack) => {
                        return match connack.code {
                            ConnectReturnCode::Success => Ok(()),
                            code => Err(connect_err(format!("MQTT connection refused: {code:?}"))),
                        };
                    }
                    // Ignore anything else arriving before CONNACK.
                    _ => continue,
                }
            }
            match self.events.next().await {
                Some(WsSignal::Bytes(bytes)) => self.incoming.extend_from_slice(&bytes),
                Some(WsSignal::Open) => {}
                Some(WsSignal::Closed { reason }) | Some(WsSignal::Error { reason }) => {
                    return Err(connect_err(format!(
                        "websocket closed during MQTT connect: {reason}"
                    )));
                }
                None => {
                    return Err(connect_err("websocket event channel closed during connect"));
                }
            }
        }
    }

    fn send_connect(
        &mut self,
        role: ParticipantRole,
        auth: &BrokerAuth,
    ) -> Result<(), MqttTransportError> {
        let mut connect = Connect::new(random_client_id(role));
        connect.keep_alive = KEEP_ALIVE.as_secs() as u16;
        connect.clean_session = true;
        connect.properties = Some(ConnectProperties {
            session_expiry_interval: Some(0),
            receive_maximum: None,
            max_packet_size: Some(MAX_MQTT_PACKET_SIZE as u32),
            topic_alias_max: None,
            request_response_info: None,
            request_problem_info: None,
            user_properties: Vec::new(),
            authentication_method: None,
            authentication_data: None,
        });
        if let BrokerAuth::UsernamePassword { username, password } = auth {
            connect.set_login(username.clone(), password.clone());
        }

        let mut buffer = BytesMut::new();
        connect
            .write(&mut buffer)
            .map_err(|err| connect_err(format!("failed to encode CONNECT: {err:?}")))?;
        self.send_bytes(&buffer)
            .map_err(|err| connect_err(format!("failed to send CONNECT: {err}")))
    }

    /// Parse the next complete MQTT packet out of the accumulator, if any.
    /// Returns `Ok(None)` when more bytes are needed.
    fn read_packet(&mut self) -> Result<Option<Packet>, MqttTransportError> {
        match read(&mut self.incoming, MAX_MQTT_PACKET_SIZE) {
            Ok(packet) => Ok(Some(packet)),
            Err(mqttbytes::Error::InsufficientBytes(_)) => Ok(None),
            Err(err) => Err(MqttTransportError::BrokerDisconnected {
                reason: format!("malformed MQTT packet from broker: {err:?}"),
            }),
        }
    }

    fn send_bytes(&self, bytes: &[u8]) -> Result<(), String> {
        self.ws
            .send_with_u8_array(bytes)
            .map_err(|err| js_debug(&err))
    }

    fn allocate_pkid(&mut self) -> u16 {
        // QoS 1 packet identifiers must be non-zero.
        self.next_pkid = self.next_pkid.wrapping_add(1);
        if self.next_pkid == 0 {
            self.next_pkid = 1;
        }
        self.next_pkid
    }

    fn send_pingreq(&self) -> Result<(), MqttTransportError> {
        let mut buffer = BytesMut::new();
        mqttbytes::v5::PingReq
            .write(&mut buffer)
            .map_err(|err| publish_err(format!("failed to encode PINGREQ: {err:?}")))?;
        self.send_bytes(&buffer)
            .map_err(|err| MqttTransportError::BrokerDisconnected {
                reason: format!("failed to send PINGREQ: {err}"),
            })
    }
}

impl MqttLink for WasmMqttLink {
    async fn subscribe(&mut self, topic: &str) -> Result<(), MqttTransportError> {
        let mut subscribe = mqttbytes::v5::Subscribe::new(topic.to_owned(), QoS::AtLeastOnce);
        subscribe.pkid = self.allocate_pkid();
        let mut buffer = BytesMut::new();
        subscribe
            .write(&mut buffer)
            .map_err(|err| subscribe_err(format!("failed to encode SUBSCRIBE: {err:?}")))?;
        self.send_bytes(&buffer)
            .map_err(|err| subscribe_err(format!("failed to send SUBSCRIBE: {err}")))
    }

    async fn publish(&mut self, topic: &str, payload: Vec<u8>) -> Result<(), MqttTransportError> {
        let mut publish = Publish::new(topic.to_owned(), QoS::AtLeastOnce, payload);
        publish.pkid = self.allocate_pkid();
        let mut buffer = BytesMut::new();
        publish
            .write(&mut buffer)
            .map_err(|err| publish_err(format!("failed to encode PUBLISH: {err:?}")))?;
        self.send_bytes(&buffer)
            .map_err(|err| publish_err(format!("failed to send PUBLISH: {err}")))
    }

    async fn poll(&mut self) -> Result<LinkEvent, MqttTransportError> {
        loop {
            if let Some(event) = self.pending.pop_front() {
                return Ok(event);
            }
            if let Some(packet) = self.read_packet()? {
                return Ok(translate_packet(packet));
            }

            tokio::select! {
                _ = self.ping.tick() => {
                    self.send_pingreq()?;
                }
                signal = self.events.next() => {
                    match signal {
                        Some(WsSignal::Bytes(bytes)) => self.incoming.extend_from_slice(&bytes),
                        Some(WsSignal::Open) => {}
                        Some(WsSignal::Closed { reason }) | Some(WsSignal::Error { reason }) => {
                            return Ok(LinkEvent::Disconnect { reason });
                        }
                        None => {
                            return Err(MqttTransportError::BrokerDisconnected {
                                reason: "websocket event channel closed".to_string(),
                            });
                        }
                    }
                }
            }
        }
    }

    async fn disconnect(&mut self) {
        let _close_result = self.ws.close();
    }
}

/// Translate a decoded MQTT packet into a [`LinkEvent`]. Packets the driver does
/// not act on (PINGRESP, a stray CONNACK, …) map to [`LinkEvent::Other`], the
/// same way the native backend reports ignored incoming events.
fn translate_packet(packet: Packet) -> LinkEvent {
    match packet {
        Packet::Publish(publish) => LinkEvent::Publish(IncomingPublish {
            topic: publish.topic.into_bytes(),
            payload: publish.payload.to_vec(),
            retain: publish.retain,
        }),
        Packet::PubAck(puback) => LinkEvent::PubAck(validate_puback(puback)),
        Packet::SubAck(suback) => {
            let debug = format!("{suback:?}");
            LinkEvent::SubAck {
                result: validate_suback(suback),
                debug,
            }
        }
        Packet::Disconnect(disconnect) => LinkEvent::Disconnect {
            reason: format!("{disconnect:?}"),
        },
        _ => LinkEvent::Other,
    }
}

fn validate_puback(puback: PubAck) -> Result<(), MqttTransportError> {
    match puback.reason {
        PubAckReason::Success => Ok(()),
        reason => Err(MqttTransportError::PublishRejected {
            reason: PublishRejection {
                code: puback_reason_code(reason),
                code_name: format!("{reason:?}"),
                reason_string: puback_reason_string(puback.properties.as_ref()),
            },
        }),
    }
}

fn puback_reason_string(properties: Option<&PubAckProperties>) -> Option<String> {
    properties.and_then(|properties| properties.reason_string.clone())
}

/// Map mqttbytes' `PubAckReason` to its canonical MQTT5 numeric reason code (the
/// driver classifies quota rejections on this code).
fn puback_reason_code(reason: PubAckReason) -> u8 {
    match reason {
        PubAckReason::Success => 0x00,
        PubAckReason::NoMatchingSubscribers => 0x10,
        PubAckReason::UnspecifiedError => 0x80,
        PubAckReason::ImplementationSpecificError => 0x83,
        PubAckReason::NotAuthorized => 0x87,
        PubAckReason::TopicNameInvalid => 0x90,
        PubAckReason::PacketIdentifierInUse => 0x91,
        PubAckReason::QuotaExceeded => PUBACK_QUOTA_EXCEEDED,
        PubAckReason::PayloadFormatInvalid => 0x99,
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
        // mqttbytes encodes a granted QoS as QoS0/QoS1/QoS2; we always request
        // QoS1, so only QoS1 is a successful grant.
        SubscribeReasonCode::QoS1 => Ok(()),
        SubscribeReasonCode::QoS0 | SubscribeReasonCode::QoS2 => {
            Err(MqttTransportError::SubscribeRejected {
                reason: format!("broker granted unsupported QoS: {first:?}"),
            })
        }
        reason => Err(MqttTransportError::SubscribeRejected {
            reason: format!("{reason:?}"),
        }),
    }
}

fn random_client_id(role: ParticipantRole) -> String {
    let mut random = [0_u8; 16];
    OsRng.fill_bytes(&mut random);
    let mut hex = String::with_capacity(random.len() * 2);
    const DIGITS: &[u8; 16] = b"0123456789abcdef";
    for byte in random {
        hex.push(DIGITS[(byte >> 4) as usize] as char);
        hex.push(DIGITS[(byte & 0x0f) as usize] as char);
    }
    format!("{}-{}", role.client_id_prefix(), hex)
}

fn js_debug(value: &wasm_bindgen::JsValue) -> String {
    format!("{value:?}")
}
