//! Transport-agnostic Tyde protocol driver.
//!
//! This module holds the reusable protocol logic that was previously baked into
//! the rumqttc-specific `MqttActor`: salt-handshake session establishment, the
//! rendezvous open/accept exchange, outbound boxcar batching, the publish
//! retry/pacing policy, deferred-data-frame handling, and the duplicate-frame
//! validators. It is generic over [`MqttLink`] and never names the underlying
//! MQTT library, so the same logic compiles against the native rumqttc backend
//! today and a `web-sys::WebSocket` backend in Phase 2.
//!
//! It still uses tokio timers/`select!` for now; replacing those with
//! wasm-friendly equivalents is deferred to Phase 2 per the design doc.

use std::collections::VecDeque;
use std::str;
#[cfg(test)]
use std::sync::Arc;
use std::time::Duration;

use futures_channel::mpsc::Receiver as OutboundReceiver;
use futures_util::StreamExt;
#[cfg(test)]
use tokio::sync::Barrier;
use tokio::sync::{mpsc, oneshot};
use tokio::time::{Instant, interval_at, sleep};

use crate::chunking::MAX_PLAINTEXT_CHUNK_LEN;
use crate::config::{MqttConnectConfig, ParticipantRole};
use crate::error::{FramingError, MqttTransportError};
use crate::framing::{
    SESSION_SALT_LEN, TransportFrame, decode_frame, encode_data_frame, encode_handshake_frame,
};
use crate::link::{IncomingPublish, LinkEvent, MqttLink};
use crate::rendezvous::{
    ConnectionId, OpenAccept, OpenRequest, decode_open_accept, decode_open_request,
    derive_ephemeral_psk, encode_open_accept, encode_open_request, random_nonce,
};
use crate::session::SessionCipher;
use crate::stream::{InboundEvent, OutboundChunk};
use crate::types::{PreSharedKey, RoomId};

const CLIENT_HANDSHAKE_RETRY_INTERVAL: Duration = Duration::from_millis(250);
const PUBLISH_RETRY_INITIAL: Duration = Duration::from_millis(250);
const PUBLISH_RETRY_MAX: Duration = Duration::from_secs(30);
const OUTBOUND_BOXCAR_DELAY: Duration = Duration::from_millis(100);
const RENDEZVOUS_RETRY_INTERVAL: Duration = Duration::from_millis(250);

/// Drives the Tyde transport protocol over an [`MqttLink`]. Field-for-field the
/// former `MqttActor`, with the rumqttc `client`/`eventloop` pair replaced by a
/// single `link`.
pub(crate) struct ProtocolDriver<L: MqttLink> {
    pub(crate) config: MqttConnectConfig,
    pub(crate) link: L,
    pub(crate) inbound_topic: String,
    pub(crate) outbound_topic: String,
    pub(crate) local_salt: [u8; SESSION_SALT_LEN],
    pub(crate) pending_peer_salt: Option<[u8; SESSION_SALT_LEN]>,
    pub(crate) established_peer_salt: Option<[u8; SESSION_SALT_LEN]>,
    pub(crate) pending_data_frames: VecDeque<PendingDataFrame>,
    pub(crate) outbound_rx: OutboundReceiver<OutboundChunk>,
    pub(crate) inbound_tx: mpsc::Sender<InboundEvent>,
    pub(crate) ready_tx: Option<oneshot::Sender<Result<(), MqttTransportError>>>,
    pub(crate) publish_pacer: PublishPacer,
    #[cfg(test)]
    pub(crate) subscribe_barrier: Option<Arc<Barrier>>,
}

impl<L: MqttLink> ProtocolDriver<L> {
    pub(crate) async fn run(mut self) {
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
        self.link.subscribe(&self.inbound_topic).await?;

        await_suback(&mut self.link, "subscribe").await?;
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
                self.established_peer_salt = Some(peer_salt);
                self.publish_local_salt().await?;
                peer_salt
            }
            ParticipantRole::Client => {
                self.publish_local_salt().await?;
                let peer_salt = self.await_peer_salt_with_client_retries().await?;
                self.established_peer_salt = Some(peer_salt);
                peer_salt
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

    async fn await_peer_salt(&mut self) -> Result<[u8; SESSION_SALT_LEN], MqttTransportError> {
        if let Some(salt) = self.pending_peer_salt.take() {
            return Ok(salt);
        }

        loop {
            match self.link.poll().await? {
                LinkEvent::Publish(publish) => {
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
                LinkEvent::PubAck(result) => result?,
                LinkEvent::Disconnect { reason } => {
                    return Err(MqttTransportError::BrokerDisconnected {
                        reason: format!("disconnect during salt exchange: {reason}"),
                    });
                }
                LinkEvent::SubAck { debug, .. } => {
                    return Err(MqttTransportError::SubscribeRejected {
                        reason: format!(
                            "unexpected duplicate SUBACK during salt exchange: {debug}"
                        ),
                    });
                }
                LinkEvent::Other => {}
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
                event = self.link.poll() => {
                    if let Some(salt) = self.handle_peer_salt_event(event?)? {
                        return Ok(salt);
                    }
                }
            }
        }
    }

    fn handle_peer_salt_event(
        &mut self,
        event: LinkEvent,
    ) -> Result<Option<[u8; SESSION_SALT_LEN]>, MqttTransportError> {
        match event {
            LinkEvent::Publish(publish) => {
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
            LinkEvent::PubAck(result) => {
                result?;
                Ok(None)
            }
            LinkEvent::Disconnect { reason } => Err(MqttTransportError::BrokerDisconnected {
                reason: format!("disconnect during salt exchange: {reason}"),
            }),
            LinkEvent::SubAck { debug, .. } => Err(MqttTransportError::SubscribeRejected {
                reason: format!("unexpected duplicate SUBACK during salt exchange: {debug}"),
            }),
            LinkEvent::Other => Ok(None),
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
                event = self.link.poll() => {
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
                            self.link.disconnect().await;
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
                event = self.link.poll() => {
                    self.handle_ready_event(event?, cipher).await?;
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
        event: LinkEvent,
        cipher: &mut SessionCipher,
    ) -> Result<(), MqttTransportError> {
        match event {
            LinkEvent::Publish(publish) => {
                let frame = self.decode_publish(publish)?;
                match frame {
                    TransportFrame::Handshake { salt } => self.handle_post_session_handshake(salt),
                    TransportFrame::Data {
                        counter,
                        ciphertext_with_tag,
                    } => {
                        self.handle_data_frame(counter, ciphertext_with_tag, cipher)
                            .await
                    }
                }
            }
            LinkEvent::PubAck(result) => result,
            LinkEvent::Disconnect { reason } => Err(MqttTransportError::BrokerDisconnected {
                reason: format!("disconnect after session established: {reason}"),
            }),
            LinkEvent::SubAck { .. } | LinkEvent::Other => Ok(()),
        }
    }

    fn handle_handshake_before_session(
        &mut self,
        salt: [u8; SESSION_SALT_LEN],
    ) -> Result<(), MqttTransportError> {
        if self.established_peer_salt.is_some() {
            return self.handle_post_session_handshake(salt);
        }

        self.pending_peer_salt = Some(salt);
        Ok(())
    }

    fn handle_post_session_handshake(
        &self,
        salt: [u8; SESSION_SALT_LEN],
    ) -> Result<(), MqttTransportError> {
        validate_post_session_handshake(self.established_peer_salt, salt)?;
        tracing::debug!(
            role = ?self.config.role,
            "MQTT duplicate peer handshake ignored after session established"
        );
        Ok(())
    }

    fn decode_publish(
        &self,
        publish: IncomingPublish,
    ) -> Result<TransportFrame, MqttTransportError> {
        if publish.retain {
            let topic = publish_topic_string(&publish.topic)?;
            return Err(MqttTransportError::RetainedMessage { topic });
        }

        let topic = publish_topic_string(&publish.topic)?;
        if topic != self.inbound_topic {
            return Err(MqttTransportError::Framing(FramingError::InvalidTopic {
                message: format!(
                    "received publish for topic {topic:?}; expected {:?}",
                    self.inbound_topic
                ),
            }));
        }

        decode_frame(&publish.payload).map_err(MqttTransportError::Framing)
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

            // This driver owns the connection event loop, so drive it until the
            // QoS 1 publish is acknowledged instead of leaving queued chunks
            // behind a keep-alive wakeup. Reuse the same encrypted frame and
            // counter on broker rejections; the receiver's counter validator
            // deduplicates any frame that was actually forwarded before the
            // rejection surfaced.
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
        let topic = self.outbound_topic.clone();
        self.link.publish(&topic, frame).await
    }

    async fn await_publish_ack(
        &mut self,
        cipher: &mut SessionCipher,
    ) -> Result<(), MqttTransportError> {
        loop {
            match self.link.poll().await? {
                LinkEvent::PubAck(result) => {
                    result?;
                    return Ok(());
                }
                event => self.handle_ready_event(event, cipher).await?,
            }
        }
    }

    async fn await_publish_ack_before_session(&mut self) -> Result<(), MqttTransportError> {
        loop {
            match self.link.poll().await? {
                LinkEvent::PubAck(result) => {
                    result?;
                    return Ok(());
                }
                LinkEvent::Disconnect { reason } => {
                    return Err(MqttTransportError::BrokerDisconnected {
                        reason: format!("disconnect while publishing handshake: {reason}"),
                    });
                }
                LinkEvent::Publish(publish) => match self.decode_publish(publish)? {
                    TransportFrame::Handshake { salt } => {
                        self.handle_handshake_before_session(salt)?;
                    }
                    TransportFrame::Data {
                        counter,
                        ciphertext_with_tag,
                    } => {
                        self.defer_data_frame(counter, ciphertext_with_tag);
                    }
                },
                LinkEvent::SubAck { debug, .. } => {
                    return Err(MqttTransportError::SubscribeRejected {
                        reason: format!("unexpected duplicate SUBACK during publish: {debug}"),
                    });
                }
                LinkEvent::Other => {}
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

pub(crate) struct PendingDataFrame {
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

pub(crate) struct PublishPacer {
    next_publish_at: Option<Instant>,
    paced_delay: Option<Duration>,
    successes_since_quota: u8,
}

impl PublishPacer {
    pub(crate) fn new() -> Self {
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

pub(crate) fn validate_post_session_handshake(
    established_peer_salt: Option<[u8; SESSION_SALT_LEN]>,
    salt: [u8; SESSION_SALT_LEN],
) -> Result<(), MqttTransportError> {
    if established_peer_salt == Some(salt) {
        Ok(())
    } else {
        Err(MqttTransportError::Framing(
            FramingError::HandshakeAfterSession,
        ))
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

fn publish_topic_string(topic: &[u8]) -> Result<String, MqttTransportError> {
    str::from_utf8(topic)
        .map(|topic| topic.to_string())
        .map_err(|err| {
            MqttTransportError::Framing(FramingError::InvalidTopicUtf8 {
                message: err.to_string(),
            })
        })
}

fn unexpected_publish_before_suback(topic: &[u8]) -> FramingError {
    match publish_topic_string(topic) {
        Ok(topic) => FramingError::InvalidTopic {
            message: format!("received publish for topic {topic:?} before SUBACK"),
        },
        Err(_) => FramingError::InvalidTopicUtf8 {
            message: "received publish with non-UTF-8 topic before SUBACK".to_string(),
        },
    }
}

// ---------------------------------------------------------------------------
// Rendezvous (ephemeral data-room negotiation), generic over the link.
// ---------------------------------------------------------------------------

pub(crate) struct EphemeralDataRoom {
    pub(crate) room: RoomId,
    pub(crate) psk: PreSharedKey,
}

pub(crate) async fn negotiate_ephemeral_data_room<L: MqttLink>(
    config: &MqttConnectConfig,
    inbound_topic: &str,
    outbound_topic: &str,
    link: &mut L,
) -> Result<EphemeralDataRoom, MqttTransportError> {
    link.subscribe(inbound_topic).await?;
    await_suback(link, "rendezvous subscribe").await?;

    match config.role {
        ParticipantRole::Host => {
            await_open_and_accept(config, link, inbound_topic, outbound_topic).await
        }
        ParticipantRole::Client => {
            open_and_await_accept(config, link, inbound_topic, outbound_topic).await
        }
    }
}

async fn await_open_and_accept<L: MqttLink>(
    config: &MqttConnectConfig,
    link: &mut L,
    inbound_topic: &str,
    outbound_topic: &str,
) -> Result<EphemeralDataRoom, MqttTransportError> {
    loop {
        match link.poll().await? {
            LinkEvent::Publish(publish) => {
                let topic = publish_topic_string(&publish.topic)?;
                if topic != inbound_topic {
                    return Err(MqttTransportError::Framing(FramingError::InvalidTopic {
                        message: format!(
                            "received publish for topic {topic:?}; expected {inbound_topic:?}"
                        ),
                    }));
                }
                let request =
                    decode_open_request(&config.room, &config.psk, &publish.payload)?;
                let server_nonce = random_nonce();
                let accept = OpenAccept {
                    connection_id: request.connection_id,
                    client_nonce: request.client_nonce,
                    server_nonce,
                    data_room: request.proposed_data_room,
                };
                let frame = encode_open_accept(&config.room, &config.psk, &accept)?;
                publish_control_frame(link, outbound_topic, frame).await?;
                let psk = derive_ephemeral_psk(
                    &config.psk,
                    &config.room,
                    accept.connection_id,
                    &accept.client_nonce,
                    &accept.server_nonce,
                    &accept.data_room,
                )?;
                link.disconnect().await;
                return Ok(EphemeralDataRoom {
                    room: accept.data_room,
                    psk,
                });
            }
            LinkEvent::PubAck(result) => result?,
            LinkEvent::Disconnect { reason } => {
                return Err(MqttTransportError::BrokerDisconnected {
                    reason: format!("disconnect during rendezvous accept: {reason}"),
                });
            }
            LinkEvent::SubAck { .. } | LinkEvent::Other => {}
        }
    }
}

async fn open_and_await_accept<L: MqttLink>(
    config: &MqttConnectConfig,
    link: &mut L,
    inbound_topic: &str,
    outbound_topic: &str,
) -> Result<EphemeralDataRoom, MqttTransportError> {
    let request = OpenRequest {
        connection_id: ConnectionId::random(),
        client_nonce: random_nonce(),
        proposed_data_room: RoomId::random(),
    };
    let open_frame = encode_open_request(&config.room, &config.psk, &request)?;
    link.publish(outbound_topic, open_frame.clone()).await?;
    let mut retry = interval_at(
        Instant::now() + RENDEZVOUS_RETRY_INTERVAL,
        RENDEZVOUS_RETRY_INTERVAL,
    );

    loop {
        tokio::select! {
            _ = retry.tick() => {
                link.publish(outbound_topic, open_frame.clone()).await?;
            }
            event = link.poll() => {
                match event? {
                    LinkEvent::Publish(publish) => {
                        let topic = publish_topic_string(&publish.topic)?;
                        if topic != inbound_topic {
                            return Err(MqttTransportError::Framing(FramingError::InvalidTopic {
                                message: format!(
                                    "received publish for topic {topic:?}; expected {inbound_topic:?}"
                                ),
                            }));
                        }
                        let accept = match decode_open_accept(
                            &config.room,
                            &config.psk,
                            &publish.payload,
                        ) {
                            Ok(accept) => accept,
                            Err(FramingError::UnknownTag { .. }) => continue,
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
                        link.disconnect().await;
                        return Ok(EphemeralDataRoom {
                            room: accept.data_room,
                            psk,
                        });
                    }
                    LinkEvent::PubAck(result) => result?,
                    LinkEvent::Disconnect { reason } => {
                        return Err(MqttTransportError::BrokerDisconnected {
                            reason: format!("disconnect during rendezvous open: {reason}"),
                        });
                    }
                    LinkEvent::SubAck { .. } | LinkEvent::Other => {}
                }
            }
        }
    }
}

async fn publish_control_frame<L: MqttLink>(
    link: &mut L,
    topic: &str,
    frame: Vec<u8>,
) -> Result<(), MqttTransportError> {
    link.publish(topic, frame).await?;
    await_publish_ack_before_stream(link).await
}

async fn await_publish_ack_before_stream<L: MqttLink>(
    link: &mut L,
) -> Result<(), MqttTransportError> {
    loop {
        match link.poll().await? {
            LinkEvent::PubAck(result) => {
                result?;
                return Ok(());
            }
            LinkEvent::Disconnect { reason } => {
                return Err(MqttTransportError::BrokerDisconnected {
                    reason: format!("disconnect while publishing rendezvous frame: {reason}"),
                });
            }
            LinkEvent::SubAck { debug, .. } => {
                return Err(MqttTransportError::SubscribeRejected {
                    reason: format!("unexpected duplicate SUBACK during rendezvous: {debug}"),
                });
            }
            LinkEvent::Publish(_) | LinkEvent::Other => {}
        }
    }
}

/// SUBACK wait shared by session establishment and rendezvous. `disconnect_context`
/// names the phase for the broker-disconnect error message ("subscribe" vs
/// "rendezvous subscribe"), preserving the prior per-call-site wording.
async fn await_suback<L: MqttLink>(
    link: &mut L,
    disconnect_context: &str,
) -> Result<(), MqttTransportError> {
    loop {
        match link.poll().await? {
            LinkEvent::SubAck { result, .. } => return result,
            LinkEvent::Disconnect { reason } => {
                return Err(MqttTransportError::BrokerDisconnected {
                    reason: format!("disconnect during {disconnect_context}: {reason}"),
                });
            }
            LinkEvent::Publish(publish) => {
                return Err(MqttTransportError::Framing(unexpected_publish_before_suback(
                    &publish.topic,
                )));
            }
            LinkEvent::PubAck(result) => result?,
            LinkEvent::Other => {}
        }
    }
}
