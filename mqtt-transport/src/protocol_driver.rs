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
//! Timers come from [`crate::time`] (tokio on native, wasmtimer on wasm) so this
//! module names no runtime-specific timer; `tokio::select!` is just a macro and
//! is used directly on both targets. tokio's `sync` channels are portable to
//! wasm32, so they are used directly as well.

use std::collections::{HashMap, VecDeque};
use std::str;
#[cfg(test)]
use std::sync::Arc;
use std::time::Duration;

use futures_channel::mpsc::Receiver as OutboundReceiver;
use futures_util::StreamExt;
use rand::RngCore;
use rand::rngs::OsRng;
#[cfg(test)]
use tokio::sync::Barrier;
use tokio::sync::{mpsc, oneshot};

use crate::time::{Instant, interval_at, sleep};

use crate::chunking::MAX_PLAINTEXT_CHUNK_LEN;
use crate::config::{MqttConnectConfig, ParticipantRole};
use crate::error::{FramingError, MqttTransportError};
use crate::framing::{
    SESSION_SALT_LEN, TransportFrame, decode_frame, encode_credit_frame, encode_data_frame,
    encode_handshake_frame,
};
use crate::link::{
    DATA_CREDIT_WINDOW, IncomingPublish, LinkEvent, MAX_DATA_QOS1_INFLIGHT, MQTT_QOS1_WINDOW,
    MqttLink, PublishAck, PublishToken,
};
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
const PUBLISH_RETRY_ATTEMPTS: u8 = 5;
const OUTBOUND_BOXCAR_DELAY: Duration = Duration::from_millis(100);
const RENDEZVOUS_RETRY_INTERVAL: Duration = Duration::from_millis(250);
const CREDIT_EMIT_THRESHOLD: u64 = (DATA_CREDIT_WINDOW / 2) as u64;
const CREDIT_DEBOUNCE: Duration = Duration::from_millis(25);
#[cfg(not(test))]
const CREDIT_BLOCK_TIMEOUT: Duration = Duration::from_secs(10);
#[cfg(test)]
const CREDIT_BLOCK_TIMEOUT: Duration = Duration::from_millis(100);

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
    pub(crate) pending_credit_frames: VecDeque<PendingCreditFrame>,
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
                let mut credit = ReceiverCreditState::new();
                if let Err(error) = self
                    .flush_pending_data_frames(&mut cipher, &mut credit)
                    .await
                {
                    let _sent = self.send_ready(Err(error));
                    return;
                }
                if let Err(error) = self.flush_pending_credit_frames(&mut cipher).await {
                    let _sent = self.send_ready(Err(error));
                    return;
                }
                if !self.send_ready(Ok(())) {
                    return;
                }
                self.run_stream(cipher, credit).await;
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
                        TransportFrame::Credit {
                            control_counter,
                            ciphertext_with_tag,
                        } => {
                            self.defer_credit_frame(control_counter, ciphertext_with_tag);
                        }
                    }
                }
                LinkEvent::PubAck(ack) => ack.result?,
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
                    TransportFrame::Credit {
                        control_counter,
                        ciphertext_with_tag,
                    } => {
                        self.defer_credit_frame(control_counter, ciphertext_with_tag);
                        Ok(None)
                    }
                }
            }
            LinkEvent::PubAck(ack) => {
                ack.result?;
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

    async fn run_stream(mut self, mut cipher: SessionCipher, mut credit: ReceiverCreditState) {
        let mut deferred_outbound: Option<OutboundChunk> = None;
        let mut in_flight = InflightPublishes::new();
        let mut outbound_closed = false;
        let mut credit_blocked_since: Option<Instant> = None;
        loop {
            if let Err(error) = self
                .publish_due_credit(&mut cipher, &mut in_flight, &mut credit)
                .await
            {
                ack_deferred_outbound(&mut deferred_outbound, &error);
                self.fail_stream(&mut in_flight, error).await;
                return;
            }

            if can_publish_data(&cipher, &in_flight)
                && let Some(outbound) = deferred_outbound.take()
            {
                credit_blocked_since = None;
                match self
                    .boxcar_outbound(
                        outbound,
                        &mut cipher,
                        &mut deferred_outbound,
                        &mut in_flight,
                        &mut credit,
                    )
                    .await
                {
                    Ok(batch) => {
                        if let Err(failure) = self
                            .publish_boxcar_batch(&mut cipher, batch, &mut in_flight)
                            .await
                        {
                            failure.batch.ack_error(&failure.error);
                            ack_deferred_outbound(&mut deferred_outbound, &failure.error);
                            self.fail_stream(&mut in_flight, failure.error).await;
                            return;
                        }
                    }
                    Err(failure) => {
                        failure.batch.ack_error(&failure.error);
                        ack_deferred_outbound(&mut deferred_outbound, &failure.error);
                        self.fail_stream(&mut in_flight, failure.error).await;
                        return;
                    }
                }
                continue;
            }

            if outbound_closed && in_flight.is_empty() {
                self.link.disconnect().await;
                let _send_result = self.inbound_tx.send(InboundEvent::Eof).await;
                return;
            }

            let receiver_credit_blocked =
                deferred_outbound.is_some() && !has_receiver_credit(&cipher);
            match (receiver_credit_blocked, credit_blocked_since) {
                (true, None) => credit_blocked_since = Some(Instant::now()),
                (false, Some(_)) => credit_blocked_since = None,
                _ => {}
            }
            let credit_block_delay = credit_blocked_since.map(|since| {
                CREDIT_BLOCK_TIMEOUT
                    .checked_sub(Instant::now().duration_since(since))
                    .unwrap_or(Duration::ZERO)
            });
            let credit_block_timer = sleep(credit_block_delay.unwrap_or(CREDIT_BLOCK_TIMEOUT));
            tokio::pin!(credit_block_timer);

            let credit_debounce_delay = credit.next_publish_delay();
            let credit_debounce_timer = sleep(credit_debounce_delay.unwrap_or(CREDIT_DEBOUNCE));
            tokio::pin!(credit_debounce_timer);

            let can_accept_outbound = !outbound_closed
                && deferred_outbound.is_none()
                && in_flight.has_data_slot()
                && in_flight.has_broker_capacity();
            tokio::select! {
                _ = &mut credit_block_timer, if credit_block_delay.is_some() => {
                    let error = MqttTransportError::ReceiverCreditTimeout {
                        data_counter: cipher.next_send_data_counter(),
                        timeout_ms: CREDIT_BLOCK_TIMEOUT.as_millis() as u64,
                    };
                    ack_deferred_outbound(&mut deferred_outbound, &error);
                    self.fail_stream(&mut in_flight, error).await;
                    return;
                }
                _ = &mut credit_debounce_timer, if credit_debounce_delay.is_some() => {
                    continue;
                }
                event = self.link.poll() => {
                    match event {
                        Ok(event) => {
                            if let Err(error) = self.handle_stream_event(
                                event,
                                &mut cipher,
                                &mut in_flight,
                                &mut credit,
                            ).await {
                                ack_deferred_outbound(&mut deferred_outbound, &error);
                                self.fail_stream(&mut in_flight, error).await;
                                return;
                            }
                        }
                        Err(error) => {
                            let error = poll_error_to_disconnect(error);
                            ack_deferred_outbound(&mut deferred_outbound, &error);
                            self.fail_stream(&mut in_flight, error).await;
                            return;
                        }
                    }
                }
                outbound = self.outbound_rx.next(), if can_accept_outbound => {
                    match outbound {
                        Some(outbound) => {
                            if !can_publish_data(&cipher, &in_flight) {
                                deferred_outbound = Some(outbound);
                                continue;
                            }
                            let batch = match self.boxcar_outbound(
                                outbound,
                                &mut cipher,
                                &mut deferred_outbound,
                                &mut in_flight,
                                &mut credit,
                            ).await {
                                Ok(batch) => batch,
                                Err(failure) => {
                                    failure.batch.ack_error(&failure.error);
                                    ack_deferred_outbound(&mut deferred_outbound, &failure.error);
                                    self.fail_stream(&mut in_flight, failure.error).await;
                                    return;
                                }
                            };
                            if let Err(failure) = self.publish_boxcar_batch(
                                &mut cipher,
                                batch,
                                &mut in_flight,
                            ).await {
                                failure.batch.ack_error(&failure.error);
                                ack_deferred_outbound(&mut deferred_outbound, &failure.error);
                                self.fail_stream(&mut in_flight, failure.error).await;
                                return;
                            }
                        }
                        None => {
                            outbound_closed = true;
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
        in_flight: &mut InflightPublishes,
        credit: &mut ReceiverCreditState,
    ) -> Result<BoxcarBatch, BoxcarFailure> {
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
                    let event = match event {
                        Ok(event) => event,
                        Err(error) => {
                            return Err(BoxcarFailure {
                                batch,
                                error: poll_error_to_disconnect(error),
                            });
                        }
                    };
                    if let Err(error) = self
                        .handle_stream_event(event, cipher, in_flight, credit)
                        .await
                    {
                        return Err(BoxcarFailure {
                            batch,
                            error,
                        });
                    }
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

    async fn publish_boxcar_batch(
        &mut self,
        cipher: &mut SessionCipher,
        batch: BoxcarBatch,
        in_flight: &mut InflightPublishes,
    ) -> Result<(), PublishBatchFailure> {
        let published = match self.publish_plaintext(cipher, &batch.plaintext).await {
            Ok(published) => published,
            Err(error) => {
                return Err(PublishBatchFailure { batch, error });
            }
        };
        in_flight.insert(InflightPublish::Data {
            token: published.token,
            counter: published.counter,
            plaintext_len: published.plaintext_len,
            frame: published.frame,
            quota_retries: 0,
            batch,
        });
        Ok(())
    }

    async fn fail_stream(&mut self, in_flight: &mut InflightPublishes, error: MqttTransportError) {
        in_flight.ack_error_all(&error);
        self.link.disconnect().await;
        send_inbound_error(self.inbound_tx.clone(), error).await;
    }

    async fn handle_ready_event(
        &mut self,
        event: LinkEvent,
        cipher: &mut SessionCipher,
        credit: &mut ReceiverCreditState,
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
                        self.handle_data_frame(counter, ciphertext_with_tag, cipher, credit)
                            .await
                    }
                    TransportFrame::Credit {
                        control_counter,
                        ciphertext_with_tag,
                    } => self.handle_credit_frame(control_counter, ciphertext_with_tag, cipher),
                }
            }
            LinkEvent::PubAck(ack) => ack.result,
            LinkEvent::Disconnect { reason } => Err(MqttTransportError::BrokerDisconnected {
                reason: format!("disconnect after session established: {reason}"),
            }),
            LinkEvent::SubAck { .. } | LinkEvent::Other => Ok(()),
        }
    }

    async fn handle_stream_event(
        &mut self,
        event: LinkEvent,
        cipher: &mut SessionCipher,
        in_flight: &mut InflightPublishes,
        credit: &mut ReceiverCreditState,
    ) -> Result<(), MqttTransportError> {
        match event {
            LinkEvent::PubAck(ack) => self.handle_publish_ack(ack, in_flight).await,
            other => self.handle_ready_event(other, cipher, credit).await,
        }?;
        self.publish_due_credit(cipher, in_flight, credit).await?;
        Ok(())
    }

    async fn publish_due_credit(
        &mut self,
        cipher: &mut SessionCipher,
        in_flight: &mut InflightPublishes,
        credit: &mut ReceiverCreditState,
    ) -> Result<(), MqttTransportError> {
        let Some(next_expected) = credit.due_credit() else {
            return Ok(());
        };
        if !in_flight.has_broker_capacity() {
            return Ok(());
        }
        let encrypted = cipher.encrypt_credit(next_expected)?;
        let frame = encode_credit_frame(encrypted.counter, &encrypted.ciphertext_with_tag);
        let token = self.publish_frame(frame.clone()).await?;
        credit.mark_published(next_expected);
        in_flight.insert(InflightPublish::Credit {
            token,
            next_expected,
            frame,
            quota_retries: 0,
        });
        tracing::debug!(
            role = ?self.config.role,
            control_counter = encrypted.counter,
            next_expected,
            "MQTT receiver credit publish enqueued"
        );
        Ok(())
    }

    async fn handle_publish_ack(
        &mut self,
        ack: PublishAck,
        in_flight: &mut InflightPublishes,
    ) -> Result<(), MqttTransportError> {
        if !in_flight.contains(ack.token) {
            return Err(MqttTransportError::PublishAckMismatch {
                packet_id: None,
                token: Some(ack.token.value()),
            });
        }

        match ack.result {
            Ok(()) => {
                let publish =
                    in_flight
                        .remove(ack.token)
                        .ok_or(MqttTransportError::PublishAckMismatch {
                            packet_id: None,
                            token: Some(ack.token.value()),
                        })?;
                match publish {
                    InflightPublish::Data {
                        counter,
                        plaintext_len,
                        batch,
                        ..
                    } => {
                        self.publish_pacer.record_success();
                        tracing::info!(
                            role = ?self.config.role,
                            counter,
                            plaintext_len,
                            "MQTT data publish accepted"
                        );
                        batch.ack_success();
                    }
                    InflightPublish::Credit { next_expected, .. } => {
                        self.publish_pacer.record_success();
                        tracing::debug!(
                            role = ?self.config.role,
                            next_expected,
                            "MQTT receiver credit publish accepted"
                        );
                    }
                }
                Ok(())
            }
            Err(error) => {
                self.publish_pacer.record_rejection(&error);
                if !publish_error_is_quota_exceeded(&error) {
                    return Err(error);
                }

                let Some(mut publish) = in_flight.remove(ack.token) else {
                    return Err(MqttTransportError::PublishAckMismatch {
                        packet_id: None,
                        token: Some(ack.token.value()),
                    });
                };
                let quota_retries = publish.quota_retries();
                if quota_retries >= PUBLISH_RETRY_ATTEMPTS {
                    in_flight.insert(publish);
                    return Err(error);
                }
                let frame = publish.frame().to_vec();
                let retry_number = quota_retries.saturating_add(1);
                tracing::warn!(
                    role = ?self.config.role,
                    retry_number,
                    max_retries = PUBLISH_RETRY_ATTEMPTS,
                    error = %error,
                    "retrying MQTT publish rejected by broker quota"
                );
                let token = match self.publish_frame(frame).await {
                    Ok(token) => token,
                    Err(retry_error) => {
                        publish.ack_error(&retry_error);
                        return Err(retry_error);
                    }
                };
                publish.requeue_after_quota_retry(token);
                in_flight.insert(publish);
                Ok(())
            }
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
    ) -> Result<PublishedFrame, MqttTransportError> {
        let encrypted = cipher.encrypt_next(plaintext)?;
        let counter = encrypted.counter;
        let plaintext_len = plaintext.len();
        let frame = encode_data_frame(encrypted.counter, &encrypted.ciphertext_with_tag);
        let token = self.publish_frame(frame.clone()).await?;
        tracing::debug!(
            role = ?self.config.role,
            counter,
            plaintext_len,
            in_flight_limit = MAX_DATA_QOS1_INFLIGHT,
            peer_credit_next_expected = cipher.peer_credit_next_expected(),
            credit_window = DATA_CREDIT_WINDOW,
            "MQTT data publish enqueued"
        );
        Ok(PublishedFrame {
            token,
            counter,
            plaintext_len,
            frame,
        })
    }

    async fn publish_local_salt(&mut self) -> Result<(), MqttTransportError> {
        let handshake = encode_handshake_frame(&self.local_salt);
        let mut retry = PublishRetryBackoff::new();
        loop {
            let token = match self.publish_frame(handshake.clone()).await {
                Ok(token) => token,
                Err(error) => {
                    if retryable_publish_error(&error) {
                        retry.sleep_after("enqueue handshake publish", &error).await;
                        continue;
                    }
                    return Err(error);
                }
            };

            // Keep session readiness behind the handshake PUBACK so the first data
            // chunk does not race an outstanding handshake publish.
            match self.await_publish_ack_before_session(token).await {
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

    async fn publish_frame(&mut self, frame: Vec<u8>) -> Result<PublishToken, MqttTransportError> {
        self.publish_pacer.wait_until_ready().await;
        let topic = self.outbound_topic.clone();
        self.link.publish(&topic, frame).await
    }

    async fn await_publish_ack_before_session(
        &mut self,
        expected: PublishToken,
    ) -> Result<(), MqttTransportError> {
        loop {
            match self.link.poll().await? {
                LinkEvent::PubAck(ack) if ack.token == expected => {
                    ack.result?;
                    return Ok(());
                }
                LinkEvent::PubAck(ack) => {
                    return Err(MqttTransportError::PublishAckMismatch {
                        packet_id: None,
                        token: Some(ack.token.value()),
                    });
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
                    TransportFrame::Credit {
                        control_counter,
                        ciphertext_with_tag,
                    } => {
                        self.defer_credit_frame(control_counter, ciphertext_with_tag);
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

    fn defer_credit_frame(&mut self, control_counter: u64, ciphertext_with_tag: Vec<u8>) {
        tracing::info!(
            role = ?self.config.role,
            control_counter,
            ciphertext_len = ciphertext_with_tag.len(),
            "MQTT receiver credit arrived before session was ready; deferring"
        );
        self.pending_credit_frames.push_back(PendingCreditFrame {
            control_counter,
            ciphertext_with_tag,
        });
    }

    async fn flush_pending_data_frames(
        &mut self,
        cipher: &mut SessionCipher,
        credit: &mut ReceiverCreditState,
    ) -> Result<(), MqttTransportError> {
        while let Some(frame) = self.pending_data_frames.pop_front() {
            self.handle_data_frame(frame.counter, frame.ciphertext_with_tag, cipher, credit)
                .await?;
        }
        Ok(())
    }

    async fn flush_pending_credit_frames(
        &mut self,
        cipher: &mut SessionCipher,
    ) -> Result<(), MqttTransportError> {
        while let Some(frame) = self.pending_credit_frames.pop_front() {
            self.handle_credit_frame(frame.control_counter, frame.ciphertext_with_tag, cipher)?;
        }
        Ok(())
    }

    async fn handle_data_frame(
        &mut self,
        counter: u64,
        ciphertext_with_tag: Vec<u8>,
        cipher: &mut SessionCipher,
        credit: &mut ReceiverCreditState,
    ) -> Result<(), MqttTransportError> {
        let delivered = cipher.decrypt_received(counter, &ciphertext_with_tag)?;
        if delivered.is_empty() {
            tracing::info!(
                role = ?self.config.role,
                counter,
                "MQTT data frame withheld (duplicate or awaiting earlier frame)"
            );
            return Ok(());
        }
        for plaintext in delivered {
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
        credit.note_delivered(cipher.local_next_expected_data_counter());
        Ok(())
    }

    fn handle_credit_frame(
        &mut self,
        control_counter: u64,
        ciphertext_with_tag: Vec<u8>,
        cipher: &mut SessionCipher,
    ) -> Result<(), MqttTransportError> {
        if let Some(next_expected) = cipher.decrypt_credit(control_counter, &ciphertext_with_tag)? {
            tracing::debug!(
                role = ?self.config.role,
                control_counter,
                next_expected,
                "MQTT receiver credit accepted"
            );
        }
        Ok(())
    }
}

pub(crate) struct PendingDataFrame {
    counter: u64,
    ciphertext_with_tag: Vec<u8>,
}

pub(crate) struct PendingCreditFrame {
    control_counter: u64,
    ciphertext_with_tag: Vec<u8>,
}

struct PublishedFrame {
    token: PublishToken,
    counter: u64,
    plaintext_len: usize,
    frame: Vec<u8>,
}

enum InflightPublish {
    Data {
        token: PublishToken,
        counter: u64,
        plaintext_len: usize,
        frame: Vec<u8>,
        quota_retries: u8,
        batch: BoxcarBatch,
    },
    Credit {
        token: PublishToken,
        next_expected: u64,
        frame: Vec<u8>,
        quota_retries: u8,
    },
}

impl InflightPublish {
    fn token(&self) -> PublishToken {
        match self {
            Self::Data { token, .. } | Self::Credit { token, .. } => *token,
        }
    }

    fn frame(&self) -> &[u8] {
        match self {
            Self::Data { frame, .. } | Self::Credit { frame, .. } => frame,
        }
    }

    fn quota_retries(&self) -> u8 {
        match self {
            Self::Data { quota_retries, .. } | Self::Credit { quota_retries, .. } => *quota_retries,
        }
    }

    fn requeue_after_quota_retry(&mut self, new_token: PublishToken) {
        match self {
            Self::Data {
                token,
                quota_retries,
                ..
            }
            | Self::Credit {
                token,
                quota_retries,
                ..
            } => {
                *token = new_token;
                *quota_retries = quota_retries.saturating_add(1);
            }
        }
    }

    fn ack_error(self, error: &MqttTransportError) {
        if let Self::Data { batch, .. } = self {
            batch.ack_error(error);
        }
    }
}

struct InflightPublishes {
    order: VecDeque<PublishToken>,
    by_token: HashMap<PublishToken, InflightPublish>,
}

impl InflightPublishes {
    fn new() -> Self {
        Self {
            order: VecDeque::new(),
            by_token: HashMap::new(),
        }
    }

    fn is_empty(&self) -> bool {
        self.by_token.is_empty()
    }

    fn data_len(&self) -> usize {
        self.by_token
            .values()
            .filter(|publish| matches!(publish, InflightPublish::Data { .. }))
            .count()
    }

    fn has_data_slot(&self) -> bool {
        self.data_len() < MAX_DATA_QOS1_INFLIGHT
    }

    fn has_broker_capacity(&self) -> bool {
        self.by_token.len() < MQTT_QOS1_WINDOW
    }

    fn contains(&self, token: PublishToken) -> bool {
        self.by_token.contains_key(&token)
    }

    fn insert(&mut self, publish: InflightPublish) {
        let token = publish.token();
        self.order.push_back(token);
        let replaced = self.by_token.insert(token, publish);
        debug_assert!(replaced.is_none());
    }

    fn remove(&mut self, token: PublishToken) -> Option<InflightPublish> {
        let publish = self.by_token.remove(&token)?;
        self.order.retain(|queued| *queued != token);
        Some(publish)
    }

    fn ack_error_all(&mut self, error: &MqttTransportError) {
        while let Some(token) = self.order.pop_front() {
            if let Some(publish) = self.by_token.remove(&token) {
                publish.ack_error(error);
            }
        }
        for (_, publish) in self.by_token.drain() {
            publish.ack_error(error);
        }
    }
}

struct ReceiverCreditState {
    last_published_next_expected: u64,
    pending_next_expected: Option<u64>,
    publish_after: Option<Instant>,
}

impl ReceiverCreditState {
    fn new() -> Self {
        Self {
            last_published_next_expected: 0,
            pending_next_expected: None,
            publish_after: None,
        }
    }

    fn note_delivered(&mut self, next_expected: u64) {
        let current_pending = self.pending_next_expected.unwrap_or(0);
        if next_expected <= self.last_published_next_expected && next_expected <= current_pending {
            return;
        }

        let current = self
            .pending_next_expected
            .unwrap_or(self.last_published_next_expected);
        if next_expected <= current {
            return;
        }

        self.pending_next_expected = Some(next_expected);
        let progress = next_expected.saturating_sub(self.last_published_next_expected);
        if progress >= CREDIT_EMIT_THRESHOLD {
            self.publish_after = Some(Instant::now());
        } else if self.publish_after.is_none() {
            self.publish_after = Some(Instant::now() + CREDIT_DEBOUNCE);
        }
    }

    fn due_credit(&self) -> Option<u64> {
        let next_expected = self.pending_next_expected?;
        if self.publish_after.is_some_and(|due| due <= Instant::now()) {
            Some(next_expected)
        } else {
            None
        }
    }

    fn next_publish_delay(&self) -> Option<Duration> {
        let due = self.publish_after?;
        self.pending_next_expected?;
        due.checked_duration_since(Instant::now())
    }

    fn mark_published(&mut self, next_expected: u64) {
        self.last_published_next_expected = self.last_published_next_expected.max(next_expected);
        if self.pending_next_expected == Some(next_expected) {
            self.pending_next_expected = None;
            self.publish_after = None;
        }
    }
}

fn has_receiver_credit(cipher: &SessionCipher) -> bool {
    cipher.next_send_data_counter()
        < cipher
            .peer_credit_next_expected()
            .saturating_add(DATA_CREDIT_WINDOW as u64)
}

fn can_publish_data(cipher: &SessionCipher, in_flight: &InflightPublishes) -> bool {
    in_flight.has_data_slot() && in_flight.has_broker_capacity() && has_receiver_credit(cipher)
}

struct BoxcarFailure {
    batch: BoxcarBatch,
    error: MqttTransportError,
}

struct PublishBatchFailure {
    batch: BoxcarBatch,
    error: MqttTransportError,
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

/// Generate a fresh random session salt. Shared by the native and wasm connect
/// entry points; `rand`'s `OsRng` maps to the OS CSPRNG on native and to the
/// WebCrypto-backed getrandom on wasm.
pub(crate) fn generate_session_salt() -> [u8; SESSION_SALT_LEN] {
    let mut salt = [0_u8; SESSION_SALT_LEN];
    OsRng.fill_bytes(&mut salt);
    salt
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

fn ack_deferred_outbound(
    deferred_outbound: &mut Option<OutboundChunk>,
    error: &MqttTransportError,
) {
    if let Some(outbound) = deferred_outbound.take() {
        let _send_result = outbound.ack.send(Err(error.to_string()));
    }
}

async fn send_inbound_error(inbound_tx: mpsc::Sender<InboundEvent>, error: MqttTransportError) {
    let _send_result = inbound_tx.send(InboundEvent::Error(Box::new(error))).await;
}

fn poll_error_to_disconnect(error: MqttTransportError) -> MqttTransportError {
    // The link wraps a poll failure as `BrokerConnect`; the original code
    // formatted the bare `ConnectionError` here, so unwrap the source to avoid
    // double-prefixing the resulting `BrokerDisconnected` reason string.
    match error {
        MqttTransportError::BrokerConnect { source } => MqttTransportError::BrokerDisconnected {
            reason: source.to_string(),
        },
        other => other,
    }
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
                let request = decode_open_request(&config.room, &config.psk, &publish.payload)?;
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
            LinkEvent::PubAck(ack) => ack.result?,
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
    let mut open_token = Some(link.publish(outbound_topic, open_frame.clone()).await?);
    let mut retry = interval_at(
        Instant::now() + RENDEZVOUS_RETRY_INTERVAL,
        RENDEZVOUS_RETRY_INTERVAL,
    );

    loop {
        tokio::select! {
            _ = retry.tick() => {
                if open_token.is_none() {
                    open_token = Some(link.publish(outbound_topic, open_frame.clone()).await?);
                }
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
                    LinkEvent::PubAck(ack) if open_token == Some(ack.token) => {
                        ack.result?;
                        open_token = None;
                    }
                    LinkEvent::PubAck(ack) => {
                        return Err(MqttTransportError::PublishAckMismatch {
                            packet_id: None,
                            token: Some(ack.token.value()),
                        });
                    }
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
    let token = link.publish(topic, frame).await?;
    await_publish_ack_before_stream(link, token).await
}

async fn await_publish_ack_before_stream<L: MqttLink>(
    link: &mut L,
    expected: PublishToken,
) -> Result<(), MqttTransportError> {
    loop {
        match link.poll().await? {
            LinkEvent::PubAck(ack) if ack.token == expected => {
                ack.result?;
                return Ok(());
            }
            LinkEvent::PubAck(ack) => {
                return Err(MqttTransportError::PublishAckMismatch {
                    packet_id: None,
                    token: Some(ack.token.value()),
                });
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
                return Err(MqttTransportError::Framing(
                    unexpected_publish_before_suback(&publish.topic),
                ));
            }
            LinkEvent::PubAck(ack) => ack.result?,
            LinkEvent::Other => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use std::error::Error;
    use std::time::Duration;

    use futures_channel::mpsc::{Sender, channel};
    use futures_util::SinkExt;
    use tokio::io::AsyncWriteExt;
    use tokio::sync::mpsc as tokio_mpsc;
    use tokio::task::JoinHandle;
    use tokio::time::timeout;

    use super::*;
    use crate::framing::{encode_credit_frame, encode_data_frame};
    use crate::stream::EnvelopeStream;
    use crate::types::{BrokerAuth, BrokerEndpoint};
    use protocol::BrokerUrl;

    struct PublishRecord {
        token: PublishToken,
        payload: Vec<u8>,
    }

    struct ScriptedLink {
        poll_rx: tokio_mpsc::UnboundedReceiver<Result<LinkEvent, MqttTransportError>>,
        publish_tx: tokio_mpsc::UnboundedSender<PublishRecord>,
        next_token: u64,
    }

    impl MqttLink for ScriptedLink {
        async fn subscribe(&mut self, _topic: &str) -> Result<(), MqttTransportError> {
            Ok(())
        }

        async fn publish(
            &mut self,
            _topic: &str,
            payload: Vec<u8>,
        ) -> Result<PublishToken, MqttTransportError> {
            let token = PublishToken::new(self.next_token);
            self.next_token += 1;
            self.publish_tx
                .send(PublishRecord { token, payload })
                .map_err(|_| MqttTransportError::ActorClosed)?;
            Ok(token)
        }

        async fn poll(&mut self) -> Result<LinkEvent, MqttTransportError> {
            self.poll_rx
                .recv()
                .await
                .ok_or(MqttTransportError::ActorClosed)?
        }

        async fn disconnect(&mut self) {}
    }

    struct DriverHarness {
        outbound_tx: Sender<OutboundChunk>,
        inbound_rx: mpsc::Receiver<InboundEvent>,
        poll_tx: tokio_mpsc::UnboundedSender<Result<LinkEvent, MqttTransportError>>,
        publish_rx: tokio_mpsc::UnboundedReceiver<PublishRecord>,
        driver: JoinHandle<()>,
    }

    fn test_room() -> RoomId {
        RoomId([0x44; 16])
    }

    fn test_psk() -> PreSharedKey {
        PreSharedKey([0x55; 32])
    }

    fn peer_cipher() -> Result<SessionCipher, MqttTransportError> {
        SessionCipher::new(
            &test_room(),
            &test_psk(),
            ParticipantRole::Client,
            &[0x11; SESSION_SALT_LEN],
            &[0x22; SESSION_SALT_LEN],
        )
        .map_err(MqttTransportError::Crypto)
    }

    fn incoming_publish(payload: Vec<u8>) -> LinkEvent {
        LinkEvent::Publish(IncomingPublish {
            topic: ParticipantRole::Host
                .inbound_topic(&test_room())
                .into_bytes(),
            payload,
            retain: false,
        })
    }

    fn incoming_data(
        peer: &mut SessionCipher,
        plaintext: &[u8],
    ) -> Result<LinkEvent, MqttTransportError> {
        let encrypted = peer.encrypt_next(plaintext)?;
        Ok(incoming_publish(encode_data_frame(
            encrypted.counter,
            &encrypted.ciphertext_with_tag,
        )))
    }

    fn incoming_credit(
        peer: &mut SessionCipher,
        next_expected: u64,
    ) -> Result<LinkEvent, MqttTransportError> {
        let encrypted = peer.encrypt_credit(next_expected)?;
        Ok(incoming_publish(encode_credit_frame(
            encrypted.counter,
            &encrypted.ciphertext_with_tag,
        )))
    }

    fn spawn_stream_driver() -> Result<DriverHarness, MqttTransportError> {
        let (outbound_tx, outbound_rx) = channel::<OutboundChunk>(64);
        let (inbound_tx, inbound_rx) = mpsc::channel::<InboundEvent>(64);
        let (poll_tx, poll_rx) =
            tokio_mpsc::unbounded_channel::<Result<LinkEvent, MqttTransportError>>();
        let (publish_tx, publish_rx) = tokio_mpsc::unbounded_channel::<PublishRecord>();
        let room = test_room();
        let psk = test_psk();
        let config = MqttConnectConfig {
            endpoint: BrokerEndpoint {
                url: BrokerUrl::new("wss://broker.example.test/mqtt").map_err(|error| {
                    MqttTransportError::Configuration {
                        message: error.to_string(),
                    }
                })?,
                auth: BrokerAuth::Anonymous,
            },
            room,
            psk: psk.clone(),
            role: ParticipantRole::Host,
        };
        let cipher = SessionCipher::new(
            &room,
            &psk,
            ParticipantRole::Host,
            &[0x11; SESSION_SALT_LEN],
            &[0x22; SESSION_SALT_LEN],
        )?;
        let driver = ProtocolDriver {
            config,
            link: ScriptedLink {
                poll_rx,
                publish_tx,
                next_token: 0,
            },
            inbound_topic: ParticipantRole::Host.inbound_topic(&room),
            outbound_topic: ParticipantRole::Host.outbound_topic(&room),
            local_salt: [0x11; SESSION_SALT_LEN],
            pending_peer_salt: None,
            established_peer_salt: Some([0x22; SESSION_SALT_LEN]),
            pending_data_frames: VecDeque::new(),
            pending_credit_frames: VecDeque::new(),
            outbound_rx,
            inbound_tx,
            ready_tx: None,
            publish_pacer: PublishPacer::new(),
            subscribe_barrier: None,
        };
        let driver = tokio::spawn(driver.run_stream(cipher, ReceiverCreditState::new()));
        Ok(DriverHarness {
            outbound_tx,
            inbound_rx,
            poll_tx,
            publish_rx,
            driver,
        })
    }

    fn full_chunk(byte: u8) -> (OutboundChunk, oneshot::Receiver<Result<(), String>>) {
        let (ack, ack_rx) = oneshot::channel();
        (
            OutboundChunk {
                bytes: vec![byte; MAX_PLAINTEXT_CHUNK_LEN],
                ack,
            },
            ack_rx,
        )
    }

    async fn send_full_chunk(
        outbound_tx: &mut Sender<OutboundChunk>,
        byte: u8,
    ) -> Result<oneshot::Receiver<Result<(), String>>, Box<dyn Error>> {
        let (chunk, ack_rx) = full_chunk(byte);
        outbound_tx.send(chunk).await?;
        Ok(ack_rx)
    }

    async fn next_publish(
        publish_rx: &mut tokio_mpsc::UnboundedReceiver<PublishRecord>,
    ) -> PublishRecord {
        timeout(Duration::from_secs(1), publish_rx.recv())
            .await
            .expect("timed out waiting for publish")
            .expect("publish channel closed")
    }

    async fn next_data_publish(
        publish_rx: &mut tokio_mpsc::UnboundedReceiver<PublishRecord>,
    ) -> PublishRecord {
        let publish = next_publish(publish_rx).await;
        assert!(
            matches!(
                decode_frame(&publish.payload),
                Ok(TransportFrame::Data { .. })
            ),
            "expected data publish"
        );
        publish
    }

    fn data_publish_counter(publish: &PublishRecord) -> u64 {
        match decode_frame(&publish.payload).expect("decode data publish") {
            TransportFrame::Data { counter, .. } => counter,
            other => panic!("expected data publish, got {other:?}"),
        }
    }

    fn credit_publish_counter(publish: &PublishRecord) -> u64 {
        match decode_frame(&publish.payload).expect("decode credit publish") {
            TransportFrame::Credit {
                control_counter, ..
            } => control_counter,
            other => panic!("expected credit publish, got {other:?}"),
        }
    }

    async fn next_credit_publish(
        publish_rx: &mut tokio_mpsc::UnboundedReceiver<PublishRecord>,
    ) -> PublishRecord {
        let publish = next_publish(publish_rx).await;
        assert!(
            matches!(
                decode_frame(&publish.payload),
                Ok(TransportFrame::Credit { .. })
            ),
            "expected credit publish"
        );
        publish
    }

    fn send_link_event(
        poll_tx: &tokio_mpsc::UnboundedSender<Result<LinkEvent, MqttTransportError>>,
        event: LinkEvent,
    ) {
        poll_tx.send(Ok(event)).expect("send link event");
    }

    fn send_poll_error(
        poll_tx: &tokio_mpsc::UnboundedSender<Result<LinkEvent, MqttTransportError>>,
        error: MqttTransportError,
    ) {
        poll_tx.send(Err(error)).expect("send poll error");
    }

    fn ack_success(
        poll_tx: &tokio_mpsc::UnboundedSender<Result<LinkEvent, MqttTransportError>>,
        token: PublishToken,
    ) {
        send_link_event(
            poll_tx,
            LinkEvent::PubAck(PublishAck {
                token,
                result: Ok(()),
            }),
        );
    }

    fn quota_rejection() -> MqttTransportError {
        MqttTransportError::PublishRejected {
            reason: crate::error::PublishRejection {
                code: crate::error::PUBACK_QUOTA_EXCEEDED,
                code_name: "QuotaExceeded".to_string(),
                reason_string: Some("test quota".to_string()),
            },
        }
    }

    fn non_quota_rejection() -> MqttTransportError {
        MqttTransportError::PublishRejected {
            reason: crate::error::PublishRejection {
                code: 0x80,
                code_name: "UnspecifiedError".to_string(),
                reason_string: Some("test rejection".to_string()),
            },
        }
    }

    async fn expect_ack_error(ack_rx: oneshot::Receiver<Result<(), String>>, expected: &str) {
        let result = timeout(Duration::from_secs(1), ack_rx)
            .await
            .expect("timed out waiting for ack")
            .expect("ack sender dropped");
        let message = result.expect_err("ack unexpectedly succeeded");
        assert!(
            message.contains(expected),
            "expected ack error to contain {expected:?}, got {message:?}"
        );
    }

    async fn assert_no_publish_yet(
        publish_rx: &mut tokio_mpsc::UnboundedReceiver<PublishRecord>,
        message: &str,
    ) {
        assert!(
            timeout(Duration::from_millis(25), publish_rx.recv())
                .await
                .is_err(),
            "{message}"
        );
    }

    #[tokio::test]
    async fn data_pipelines_up_to_receiver_credit_window() -> Result<(), Box<dyn Error>> {
        let mut harness = spawn_stream_driver()?;
        let mut ack_receivers = Vec::new();
        for index in 0..=DATA_CREDIT_WINDOW {
            ack_receivers.push(send_full_chunk(&mut harness.outbound_tx, index as u8).await?);
        }

        let mut tokens = Vec::new();
        for _ in 0..DATA_CREDIT_WINDOW {
            tokens.push(next_data_publish(&mut harness.publish_rx).await.token);
        }
        assert_no_publish_yet(
            &mut harness.publish_rx,
            "driver sent past the initial receiver credit window",
        )
        .await;

        for token in tokens.iter().copied() {
            ack_success(&harness.poll_tx, token);
        }
        assert_no_publish_yet(
            &mut harness.publish_rx,
            "broker PUBACK advanced receiver credit",
        )
        .await;

        let mut peer = peer_cipher()?;
        send_link_event(&harness.poll_tx, incoming_credit(&mut peer, 8)?);
        let next = next_data_publish(&mut harness.publish_rx).await.token;
        ack_success(&harness.poll_tx, next);

        for ack_rx in ack_receivers {
            timeout(Duration::from_secs(1), ack_rx)
                .await
                .expect("ack timed out")
                .expect("ack sender dropped")
                .expect("ack failed");
        }

        drop(harness.outbound_tx);
        timeout(Duration::from_secs(1), harness.driver)
            .await
            .expect("driver did not stop")?;
        Ok(())
    }

    #[tokio::test]
    async fn receiver_emits_standalone_cumulative_credit() -> Result<(), Box<dyn Error>> {
        let mut harness = spawn_stream_driver()?;
        let mut peer = peer_cipher()?;

        send_link_event(&harness.poll_tx, incoming_data(&mut peer, b"hello")?);
        match timeout(Duration::from_secs(1), harness.inbound_rx.recv()).await? {
            Some(InboundEvent::Data(bytes)) => assert_eq!(bytes, b"hello"),
            other => panic!("expected inbound data, got {other:?}"),
        }
        let credit = next_credit_publish(&mut harness.publish_rx).await;
        match decode_frame(&credit.payload)? {
            TransportFrame::Credit {
                control_counter, ..
            } => assert_eq!(control_counter, 0),
            other => panic!("expected credit frame, got {other:?}"),
        }

        ack_success(&harness.poll_tx, credit.token);
        drop(harness.outbound_tx);
        timeout(Duration::from_secs(1), harness.driver)
            .await
            .expect("driver did not stop")?;
        Ok(())
    }

    #[tokio::test]
    async fn credit_puback_does_not_complete_data_write_ack() -> Result<(), Box<dyn Error>> {
        let mut harness = spawn_stream_driver()?;
        let mut data_ack = Box::pin(send_full_chunk(&mut harness.outbound_tx, 1).await?);
        let data = next_data_publish(&mut harness.publish_rx).await;

        let mut peer = peer_cipher()?;
        send_link_event(&harness.poll_tx, incoming_data(&mut peer, b"inbound")?);
        let credit = next_credit_publish(&mut harness.publish_rx).await;
        ack_success(&harness.poll_tx, credit.token);
        assert!(
            timeout(Duration::from_millis(25), &mut data_ack)
                .await
                .is_err(),
            "credit PUBACK completed a data write ack"
        );

        ack_success(&harness.poll_tx, data.token);
        timeout(Duration::from_secs(1), &mut data_ack)
            .await
            .expect("data ack timed out")
            .expect("data ack sender dropped")
            .expect("data ack failed");
        drop(harness.outbound_tx);
        timeout(Duration::from_secs(1), harness.driver)
            .await
            .expect("driver did not stop")?;
        Ok(())
    }

    #[tokio::test]
    async fn quota_rejected_data_publish_is_retried_with_same_counter() -> Result<(), Box<dyn Error>>
    {
        let mut harness = spawn_stream_driver()?;
        let mut data_ack = Box::pin(send_full_chunk(&mut harness.outbound_tx, 1).await?);
        let first = next_data_publish(&mut harness.publish_rx).await;
        assert_eq!(data_publish_counter(&first), 0);

        send_link_event(
            &harness.poll_tx,
            LinkEvent::PubAck(PublishAck {
                token: first.token,
                result: Err(quota_rejection()),
            }),
        );
        let retry = next_data_publish(&mut harness.publish_rx).await;
        assert_eq!(
            data_publish_counter(&retry),
            0,
            "quota retry must retransmit the same encrypted data counter"
        );
        assert!(
            timeout(Duration::from_millis(25), &mut data_ack)
                .await
                .is_err(),
            "quota rejection completed the write before retry PUBACK"
        );

        ack_success(&harness.poll_tx, retry.token);
        timeout(Duration::from_secs(1), &mut data_ack)
            .await
            .expect("data ack timed out")
            .expect("data ack sender dropped")
            .expect("data ack failed after retry");
        drop(harness.outbound_tx);
        timeout(Duration::from_secs(1), harness.driver)
            .await
            .expect("driver did not stop")?;
        Ok(())
    }

    #[tokio::test]
    async fn quota_rejected_credit_publish_is_retried_with_same_counter()
    -> Result<(), Box<dyn Error>> {
        let mut harness = spawn_stream_driver()?;
        let mut peer = peer_cipher()?;
        send_link_event(&harness.poll_tx, incoming_data(&mut peer, b"inbound")?);
        let first = next_credit_publish(&mut harness.publish_rx).await;
        assert_eq!(credit_publish_counter(&first), 0);

        send_link_event(
            &harness.poll_tx,
            LinkEvent::PubAck(PublishAck {
                token: first.token,
                result: Err(quota_rejection()),
            }),
        );
        let retry = next_credit_publish(&mut harness.publish_rx).await;
        assert_eq!(
            credit_publish_counter(&retry),
            0,
            "quota retry must retransmit the same encrypted credit counter"
        );

        ack_success(&harness.poll_tx, retry.token);
        match timeout(Duration::from_secs(1), harness.inbound_rx.recv()).await? {
            Some(InboundEvent::Data(bytes)) => assert_eq!(bytes, b"inbound"),
            other => panic!("expected inbound data, got {other:?}"),
        }
        drop(harness.outbound_tx);
        timeout(Duration::from_secs(1), harness.driver)
            .await
            .expect("driver did not stop")?;
        Ok(())
    }

    #[tokio::test]
    async fn one_way_bulk_transfer_crosses_credit_windows() -> Result<(), Box<dyn Error>> {
        let mut harness = spawn_stream_driver()?;
        let mut stream = EnvelopeStream::new(harness.outbound_tx, harness.inbound_rx);
        stream
            .write_all(&vec![0x5a; MAX_PLAINTEXT_CHUNK_LEN * 40])
            .await?;
        let mut flush_task = tokio::spawn(async move { stream.flush().await });

        let mut first_window = Vec::new();
        for _ in 0..DATA_CREDIT_WINDOW {
            first_window.push(next_data_publish(&mut harness.publish_rx).await.token);
        }
        for token in first_window {
            ack_success(&harness.poll_tx, token);
        }
        assert!(
            timeout(Duration::from_millis(25), &mut flush_task)
                .await
                .is_err(),
            "flush completed before receiver credit"
        );

        let mut peer = peer_cipher()?;
        send_link_event(&harness.poll_tx, incoming_credit(&mut peer, 16)?);
        let mut second_window = Vec::new();
        for _ in 0..DATA_CREDIT_WINDOW {
            second_window.push(next_data_publish(&mut harness.publish_rx).await.token);
        }
        for token in second_window {
            ack_success(&harness.poll_tx, token);
        }
        assert!(
            timeout(Duration::from_millis(25), &mut flush_task)
                .await
                .is_err(),
            "flush completed before second receiver credit"
        );

        send_link_event(&harness.poll_tx, incoming_credit(&mut peer, 32)?);
        let mut final_window = Vec::new();
        for _ in 0..8 {
            final_window.push(next_data_publish(&mut harness.publish_rx).await.token);
        }
        for token in final_window {
            ack_success(&harness.poll_tx, token);
        }
        timeout(Duration::from_secs(1), &mut flush_task)
            .await
            .expect("flush timed out")
            .expect("flush task failed")?;
        timeout(Duration::from_secs(1), harness.driver)
            .await
            .expect("driver did not stop")?;
        Ok(())
    }

    #[tokio::test]
    async fn credit_blocked_sender_fails_explicitly_without_credit() -> Result<(), Box<dyn Error>> {
        let mut harness = spawn_stream_driver()?;
        let mut stream = EnvelopeStream::new(harness.outbound_tx, harness.inbound_rx);
        stream
            .write_all(&vec![
                0x5b;
                MAX_PLAINTEXT_CHUNK_LEN * (DATA_CREDIT_WINDOW + 1)
            ])
            .await?;
        let mut flush_task = tokio::spawn(async move { stream.flush().await });

        let mut tokens = Vec::new();
        for _ in 0..DATA_CREDIT_WINDOW {
            tokens.push(next_data_publish(&mut harness.publish_rx).await.token);
        }
        for token in tokens {
            ack_success(&harness.poll_tx, token);
        }

        let error = timeout(Duration::from_secs(1), &mut flush_task)
            .await
            .expect("flush timed out")
            .expect("flush task failed")
            .expect_err("flush unexpectedly succeeded");
        let message = error.to_string();
        assert!(
            message.contains("receiver credit"),
            "unexpected credit-blocked error: {message}"
        );
        timeout(Duration::from_secs(1), harness.driver)
            .await
            .expect("driver did not stop")?;
        Ok(())
    }

    #[tokio::test]
    async fn credit_publish_rejection_fails_stream() -> Result<(), Box<dyn Error>> {
        let mut harness = spawn_stream_driver()?;
        let mut peer = peer_cipher()?;
        send_link_event(&harness.poll_tx, incoming_data(&mut peer, b"inbound")?);
        let credit = next_credit_publish(&mut harness.publish_rx).await;

        send_link_event(
            &harness.poll_tx,
            LinkEvent::PubAck(PublishAck {
                token: credit.token,
                result: Err(non_quota_rejection()),
            }),
        );
        match timeout(Duration::from_secs(1), harness.inbound_rx.recv()).await? {
            Some(InboundEvent::Data(bytes)) => assert_eq!(bytes, b"inbound"),
            other => panic!("expected inbound data before error, got {other:?}"),
        }
        match timeout(Duration::from_secs(1), harness.inbound_rx.recv()).await? {
            Some(InboundEvent::Error(error)) => {
                assert!(
                    error.to_string().contains("UnspecifiedError"),
                    "unexpected inbound error: {error}"
                );
            }
            other => panic!("expected inbound error, got {other:?}"),
        }
        timeout(Duration::from_secs(1), harness.driver)
            .await
            .expect("driver did not stop")?;
        Ok(())
    }

    #[tokio::test]
    async fn puback_rejection_fails_all_inflight_data_and_closes() -> Result<(), Box<dyn Error>> {
        let mut harness = spawn_stream_driver()?;
        let ack_a = send_full_chunk(&mut harness.outbound_tx, 1).await?;
        let ack_b = send_full_chunk(&mut harness.outbound_tx, 2).await?;
        let first = next_data_publish(&mut harness.publish_rx).await.token;
        let _second = next_data_publish(&mut harness.publish_rx).await.token;

        send_link_event(
            &harness.poll_tx,
            LinkEvent::PubAck(PublishAck {
                token: first,
                result: Err(non_quota_rejection()),
            }),
        );

        expect_ack_error(ack_a, "UnspecifiedError").await;
        expect_ack_error(ack_b, "UnspecifiedError").await;
        match timeout(Duration::from_secs(1), harness.inbound_rx.recv()).await? {
            Some(InboundEvent::Error(error)) => {
                assert!(
                    error.to_string().contains("UnspecifiedError"),
                    "unexpected inbound error: {error}"
                );
            }
            other => panic!("expected inbound error, got {other:?}"),
        }
        timeout(Duration::from_secs(1), harness.driver)
            .await
            .expect("driver did not stop")?;
        Ok(())
    }

    #[tokio::test]
    async fn disconnect_fails_all_inflight_data_and_closes() -> Result<(), Box<dyn Error>> {
        let mut harness = spawn_stream_driver()?;
        let ack_a = send_full_chunk(&mut harness.outbound_tx, 1).await?;
        let ack_b = send_full_chunk(&mut harness.outbound_tx, 2).await?;
        let _first = next_data_publish(&mut harness.publish_rx).await.token;
        let _second = next_data_publish(&mut harness.publish_rx).await.token;

        send_link_event(
            &harness.poll_tx,
            LinkEvent::Disconnect {
                reason: "test disconnect".to_string(),
            },
        );

        expect_ack_error(ack_a, "test disconnect").await;
        expect_ack_error(ack_b, "test disconnect").await;
        match timeout(Duration::from_secs(1), harness.inbound_rx.recv()).await? {
            Some(InboundEvent::Error(error)) => {
                assert!(
                    error.to_string().contains("test disconnect"),
                    "unexpected inbound error: {error}"
                );
            }
            other => panic!("expected inbound error, got {other:?}"),
        }
        timeout(Duration::from_secs(1), harness.driver)
            .await
            .expect("driver did not stop")?;
        Ok(())
    }

    #[tokio::test]
    async fn wasm_unknown_puback_mismatch_fails_all_inflight_flush_acks()
    -> Result<(), Box<dyn Error>> {
        let mut harness = spawn_stream_driver()?;
        let mut stream = EnvelopeStream::new(harness.outbound_tx, harness.inbound_rx);
        stream
            .write_all(&vec![0x5b; MAX_PLAINTEXT_CHUNK_LEN * 2])
            .await?;
        let mut flush_task = tokio::spawn(async move { stream.flush().await });

        let _first = next_data_publish(&mut harness.publish_rx).await.token;
        let _second = next_data_publish(&mut harness.publish_rx).await.token;
        assert!(
            timeout(Duration::from_millis(25), &mut flush_task)
                .await
                .is_err(),
            "flush completed before PUBACKs"
        );

        send_poll_error(
            &harness.poll_tx,
            MqttTransportError::PublishAckMismatch {
                packet_id: Some(99),
                token: None,
            },
        );
        let error = timeout(Duration::from_secs(1), &mut flush_task)
            .await
            .expect("flush timed out")
            .expect("flush task failed")
            .expect_err("flush unexpectedly succeeded");
        let message = error.to_string();
        assert!(
            message.contains("packet id Some(99)"),
            "unexpected flush error: {message}"
        );

        timeout(Duration::from_secs(1), harness.driver)
            .await
            .expect("driver did not stop")?;
        Ok(())
    }
}
