//! Public connection entry points for the wasm build.
//!
//! Mirrors the native `client` module: it wires the browser WebSocket backend
//! ([`WasmMqttLink`](crate::link_wasm::WasmMqttLink)) to the transport-agnostic
//! [`ProtocolDriver`](crate::protocol_driver::ProtocolDriver) and exposes the
//! same `connect` / `connect_ephemeral` API. The driver task is spawned with
//! `wasm_bindgen_futures::spawn_local` (no `Send` requirement) instead of
//! `tokio::spawn`.

use std::collections::VecDeque;
use std::time::Duration;

use futures_channel::mpsc::channel;
use tokio::sync::{mpsc, oneshot};
use wasm_bindgen_futures::spawn_local;

use crate::config::{ConnectionPlan, ManagedMqttConnectConfig, MqttConnectConfig};
use crate::error::MqttTransportError;
use crate::link_wasm::WasmMqttLink;
use crate::protocol_driver::{
    EphemeralDataRoom, ProtocolDriver, PublishPacer, generate_session_salt,
    negotiate_ephemeral_data_room,
};
use crate::stream::{EnvelopeStream, InboundEvent, OutboundChunk};

const OUTBOUND_CHUNK_CAPACITY: usize = 64;
const INBOUND_EVENT_CAPACITY: usize = 64;
const RENDEZVOUS_DATA_CONNECT_TIMEOUT: Duration = Duration::from_secs(20);

pub async fn connect(config: MqttConnectConfig) -> Result<EnvelopeStream, MqttTransportError> {
    connect_plan(ConnectionPlan::legacy(config)).await
}

pub async fn connect_managed(
    config: ManagedMqttConnectConfig,
) -> Result<EnvelopeStream, MqttTransportError> {
    connect_plan(ConnectionPlan::managed(config)?).await
}

async fn connect_plan(plan: ConnectionPlan) -> Result<EnvelopeStream, MqttTransportError> {
    let ConnectionPlan {
        config,
        broker,
        topics,
    } = plan;
    let local_salt = generate_session_salt();
    let inbound_topic = topics.inbound_topic(config.role, &config.room)?;
    let outbound_topic = topics.outbound_topic(config.role, &config.room)?;
    let link = WasmMqttLink::connect(&broker).await?;

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
        pending_credit_frames: VecDeque::new(),
        outbound_rx,
        inbound_tx,
        ready_tx: Some(ready_tx),
        publish_pacer: PublishPacer::new(),
    };

    spawn_local(async move {
        actor.run().await;
    });

    match ready_rx.await {
        Ok(Ok(())) => Ok(EnvelopeStream::new(outbound_tx, inbound_rx)),
        Ok(Err(error)) => Err(error),
        Err(_) => Err(MqttTransportError::ActorClosed),
    }
}

pub async fn connect_ephemeral(
    config: MqttConnectConfig,
) -> Result<EnvelopeStream, MqttTransportError> {
    connect_ephemeral_plan(ConnectionPlan::legacy(config)).await
}

pub async fn connect_managed_ephemeral(
    config: ManagedMqttConnectConfig,
) -> Result<EnvelopeStream, MqttTransportError> {
    connect_ephemeral_plan(ConnectionPlan::managed_ephemeral(config)?).await
}

async fn connect_ephemeral_plan(
    plan: ConnectionPlan,
) -> Result<EnvelopeStream, MqttTransportError> {
    let data = negotiate_ephemeral_data_room_wasm(&plan).await?;
    let data_config = MqttConnectConfig {
        endpoint: plan.config.endpoint,
        room: data.room,
        psk: data.psk,
        role: plan.config.role,
    };
    let data_plan = ConnectionPlan {
        config: data_config,
        broker: plan.broker,
        topics: plan.topics,
    };
    wasmtimer::tokio::timeout(RENDEZVOUS_DATA_CONNECT_TIMEOUT, connect_plan(data_plan))
        .await
        .map_err(|_| MqttTransportError::BrokerDisconnected {
            reason: format!(
                "timed out waiting for MQTT ephemeral data room after {:?}",
                RENDEZVOUS_DATA_CONNECT_TIMEOUT
            ),
        })?
}

/// Construct the wasm link for the main (rendezvous) room and run the
/// transport-agnostic negotiation over it.
async fn negotiate_ephemeral_data_room_wasm(
    plan: &ConnectionPlan,
) -> Result<EphemeralDataRoom, MqttTransportError> {
    let config = &plan.config;
    let inbound_topic = plan.topics.inbound_topic(config.role, &config.room)?;
    let outbound_topic = plan.topics.outbound_topic(config.role, &config.room)?;
    let mut link = WasmMqttLink::connect(&plan.broker).await?;
    negotiate_ephemeral_data_room(config, &inbound_topic, &outbound_topic, &mut link).await
}
