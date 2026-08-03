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
use futures_util::future::{AbortHandle, Abortable};
use tokio::sync::{mpsc, oneshot};
use wasm_bindgen_futures::spawn_local;

use crate::config::{ConnectionPlan, ManagedMqttConnectConfig, MqttConnectConfig};
use crate::error::MqttTransportError;
use crate::link_wasm::WasmMqttLink;
use crate::protocol_driver::{
    EphemeralDataRoom, OutboundByteBudget, ProtocolDriver, PublishPacer,
    accept_ephemeral_data_connections, generate_session_salt, negotiate_ephemeral_data_room,
};
use crate::stream::{EnvelopeStream, InboundEvent, OutboundChunk};

const OUTBOUND_CHUNK_CAPACITY: usize = 64;
const INBOUND_EVENT_CAPACITY: usize = 64;
const RENDEZVOUS_DATA_CONNECT_TIMEOUT: Duration = Duration::from_secs(20);

struct ActorAbortGuard(Option<AbortHandle>);

impl ActorAbortGuard {
    fn disarm(&mut self) -> AbortHandle {
        self.0.take().expect("actor abort handle")
    }
}

impl Drop for ActorAbortGuard {
    fn drop(&mut self) {
        if let Some(handle) = self.0.take() {
            handle.abort();
        }
    }
}

pub async fn connect(config: MqttConnectConfig) -> Result<EnvelopeStream, MqttTransportError> {
    connect_plan(ConnectionPlan::legacy(config)).await
}

pub async fn connect_managed(
    config: ManagedMqttConnectConfig,
) -> Result<EnvelopeStream, MqttTransportError> {
    connect_plan(ConnectionPlan::managed(config)?).await
}

async fn connect_plan(plan: ConnectionPlan) -> Result<EnvelopeStream, MqttTransportError> {
    let session_renewal_after = plan.session_renewal_after(crate::time::unix_time_ms());
    let ConnectionPlan {
        config,
        broker,
        topics,
        session_expires_at_ms: _,
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
        outbound_budget: OutboundByteBudget::new(),
        session_renewal_after,
    };

    let (actor_abort, actor_registration) = AbortHandle::new_pair();
    let mut actor_abort_guard = ActorAbortGuard(Some(actor_abort));
    spawn_local(async move {
        let _aborted = Abortable::new(actor.run(), actor_registration).await;
    });

    match ready_rx.await {
        Ok(Ok(())) => Ok(EnvelopeStream::new(outbound_tx, inbound_rx)
            .with_actor_abort(actor_abort_guard.disarm())),
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
    if plan.config.role == crate::config::ParticipantRole::Host && plan.can_open_parallel_links() {
        return connect_ephemeral_host(plan).await;
    }
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
        session_expires_at_ms: plan.session_expires_at_ms,
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

async fn connect_ephemeral_host(
    plan: ConnectionPlan,
) -> Result<EnvelopeStream, MqttTransportError> {
    let inbound_topic = plan
        .topics
        .inbound_topic(plan.config.role, &plan.config.room)?;
    let outbound_topic = plan
        .topics
        .outbound_topic(plan.config.role, &plan.config.room)?;
    let mut link = WasmMqttLink::connect(&plan.broker).await?;
    let endpoint = plan.config.endpoint.clone();
    let broker = plan.broker.clone();
    let topics = plan.topics.clone();
    let session_expires_at_ms = plan.session_expires_at_ms;

    accept_ephemeral_data_connections(
        &plan.config,
        &inbound_topic,
        &outbound_topic,
        &mut link,
        move |connection_id, data| {
            let data_plan = ConnectionPlan {
                config: MqttConnectConfig {
                    endpoint: endpoint.clone(),
                    room: data.room,
                    psk: data.psk,
                    role: crate::config::ParticipantRole::Host,
                },
                broker: broker.clone(),
                topics: topics.clone(),
                session_expires_at_ms,
            };
            async move {
                let result = wasmtimer::tokio::timeout(
                    RENDEZVOUS_DATA_CONNECT_TIMEOUT,
                    connect_plan(data_plan),
                )
                .await
                .map_err(|_| MqttTransportError::BrokerDisconnected {
                    reason: format!(
                        "timed out waiting for MQTT ephemeral data room after {:?}",
                        RENDEZVOUS_DATA_CONNECT_TIMEOUT
                    ),
                })
                .and_then(|result| result);
                (connection_id, result)
            }
        },
    )
    .await
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
