//! Transport-neutral seam between the pure MQTT protocol driver and a concrete
//! MQTT connection backend.
//!
//! [`protocol_driver`](crate::protocol_driver) is generic over [`MqttLink`] and
//! never names the underlying MQTT library. The native backend
//! ([`link_native`](crate::link_native)) wraps rumqttc's `AsyncClient` +
//! `EventLoop` + TLS. A future wasm backend (Phase 2) will implement the same
//! trait over `web-sys::WebSocket` plus the standalone `mqttbytes 0.6.0` codec
//! crate — rumqttc's own `v5::mqttbytes` module does **not** compile to
//! `wasm32-unknown-unknown` (it pulls in tokio/mio/native sockets), which is why
//! the wasm codec must come from the standalone crate.

use crate::error::MqttTransportError;

/// Maximum QoS 1 publishes the Tyde driver will keep in flight on one MQTT
/// connection. Keep this well below broker caps such as AWS IoT's 100
/// in-flight publishes per connection.
pub(crate) const MAX_QOS1_INFLIGHT: usize = 16;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) struct PublishToken(u64);

impl PublishToken {
    pub(crate) const fn new(value: u64) -> Self {
        Self(value)
    }

    pub(crate) const fn value(self) -> u64 {
        self.0
    }
}

pub(crate) struct PublishAck {
    pub(crate) token: PublishToken,
    pub(crate) result: Result<(), MqttTransportError>,
}

/// A protocol-relevant event surfaced by an [`MqttLink`].
///
/// The backend translates its library-specific incoming packets into these
/// neutral variants. Anything the driver ignores (CONNACK, PINGRESP, outgoing
/// notifications, …) collapses to [`LinkEvent::Other`]. Reason-code
/// interpretation that is inherently specific to the MQTT library (PUBACK /
/// SUBACK reason codes) is performed by the backend so the driver stays free of
/// library types; the already-validated outcomes are carried here.
pub(crate) enum LinkEvent {
    /// An application PUBLISH arrived.
    Publish(IncomingPublish),
    /// A PUBACK arrived; the backend has already matched the packet identifier
    /// back to the publish token returned by [`MqttLink::publish`] and mapped
    /// its reason code to a neutral result (`Ok` on success,
    /// `Err(PublishRejected)` otherwise).
    PubAck(PublishAck),
    /// A SUBACK arrived. `result` is the validated outcome, used where a SUBACK
    /// is expected; `debug` is the backend's debug rendering of the SUBACK, used
    /// to describe an *unexpected* duplicate SUBACK.
    SubAck {
        result: Result<(), MqttTransportError>,
        debug: String,
    },
    /// The broker sent a DISCONNECT. `reason` is the backend's debug rendering of
    /// the disconnect packet.
    Disconnect { reason: String },
    /// Any other event the protocol driver does not act on.
    Other,
}

/// The parts of an incoming PUBLISH the protocol driver needs. The topic is
/// carried as raw bytes so the driver performs its own UTF-8 validation and
/// produces identical framing errors regardless of backend.
pub(crate) struct IncomingPublish {
    pub topic: Vec<u8>,
    pub payload: Vec<u8>,
    pub retain: bool,
}

/// Minimal MQTT connection surface the protocol driver drives.
///
/// All methods mirror what the driver already required of rumqttc:
/// [`subscribe`](MqttLink::subscribe) and [`publish`](MqttLink::publish) enqueue
/// requests (QoS 1, retain=false) without awaiting their acknowledgement, and
/// [`poll`](MqttLink::poll) drives the connection and yields the next
/// [`LinkEvent`]. Acknowledgements are observed by polling.
///
/// The driver is generic over this trait (never `dyn`), so `async fn` in the
/// trait is intentional and zero-cost here.
#[allow(async_fn_in_trait)]
pub(crate) trait MqttLink {
    /// Enqueue a SUBSCRIBE for `topic` at QoS 1. Does not await the SUBACK.
    async fn subscribe(&mut self, topic: &str) -> Result<(), MqttTransportError>;

    /// Enqueue a PUBLISH of `payload` to `topic` at QoS 1, retain=false and
    /// return the token that will be carried by its eventual PUBACK. Does not
    /// await the PUBACK.
    async fn publish(
        &mut self,
        topic: &str,
        payload: Vec<u8>,
    ) -> Result<PublishToken, MqttTransportError>;

    /// Drive the connection and return the next protocol-relevant event.
    async fn poll(&mut self) -> Result<LinkEvent, MqttTransportError>;

    /// Request a graceful disconnect. Errors are intentionally ignored, matching
    /// the prior `let _ = client.disconnect().await;` behavior.
    async fn disconnect(&mut self);
}
