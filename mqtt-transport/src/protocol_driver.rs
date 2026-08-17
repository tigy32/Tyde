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
use std::future::Future;
use std::str;
use std::time::Duration;

use futures_channel::mpsc::Receiver as OutboundReceiver;
use futures_util::StreamExt;
use futures_util::stream::FuturesUnordered;
use rand::RngCore;
use rand::rngs::OsRng;
use tokio::sync::{mpsc, oneshot};

use crate::time::{Instant, interval_at, sleep};

use crate::chunking::MAX_PLAINTEXT_CHUNK_LEN;
use crate::config::{MqttConnectConfig, ParticipantRole};
use crate::error::{FramingError, MqttTransportError, WriteAckError};
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
/// PUBACK stall watchdog: fail the stream when publishes are outstanding and no
/// PUBACK has arrived for this long. This is a *progress* deadline, not a
/// per-publish age limit: AWS IoT throttles connections that burst past its
/// 512 KiB/s per-connection cap by delaying acks, and a per-publish deadline
/// measured from local enqueue turned that ordinary backpressure into a
/// reconnect loop (each retry re-sending the full bootstrap and re-tripping the
/// throttle). As long as acks keep arriving the deadline keeps extending; a
/// dead link is still caught by MQTT keep-alive (10 s) well before this fires.
const PUBLISH_ACK_TIMEOUT: Duration = Duration::from_secs(30);
/// Outbound managed-service data budget: sustained rate and burst allowance for
/// encrypted data publishes. AWS IoT enforces a fixed 512 KiB/s per-connection
/// throughput cap (inbound + outbound) and delays traffic beyond it; publishing
/// under the cap keeps PUBACKs flowing instead of relying on the stall watchdog.
/// Loopback brokers have no managed-service quota and bypass this budget. The
/// budget is post-charged at whole-batch granularity, so a single batch may
/// overshoot by up to one 64 KiB chunk before pacing kicks in.
const OUTBOUND_BUDGET_BYTES_PER_SEC: f64 = 384.0 * 1024.0;
const OUTBOUND_BUDGET_BURST_BYTES: f64 = 256.0 * 1024.0;
const RENDEZVOUS_RETRY_INTERVAL: Duration = Duration::from_millis(250);
const MAX_RENDEZVOUS_CANDIDATES: usize = 3;
const CREDIT_EMIT_THRESHOLD: u64 = (DATA_CREDIT_WINDOW / 2) as u64;
const CREDIT_DEBOUNCE: Duration = Duration::from_millis(25);
const CREDIT_BLOCK_TIMEOUT: Duration = Duration::from_secs(10);

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
    pub(crate) outbound_budget: OutboundByteBudget,
    pub(crate) session_renewal_after: Option<Duration>,
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
        let mut last_publish_ack_at: Option<Instant> = None;
        let session_renewal_timer = sleep(
            self.session_renewal_after
                .unwrap_or(Duration::from_secs(365 * 24 * 60 * 60)),
        );
        tokio::pin!(session_renewal_timer);
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
                && self.outbound_budget.is_ready()
                && let Some(outbound) = deferred_outbound.take()
            {
                credit_blocked_since = None;
                let batch = self.boxcar_outbound(outbound, &mut deferred_outbound);
                if let Err(failure) = self
                    .publish_boxcar_batch(&mut cipher, batch, &mut in_flight)
                    .await
                {
                    failure.batch.ack_error(&failure.error);
                    ack_deferred_outbound(&mut deferred_outbound, &failure.error);
                    self.fail_stream(&mut in_flight, failure.error).await;
                    return;
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

            let publish_ack_delay = in_flight.next_ack_timeout_delay(last_publish_ack_at);
            let publish_ack_timer = sleep(publish_ack_delay.unwrap_or(PUBLISH_ACK_TIMEOUT));
            tokio::pin!(publish_ack_timer);

            // Wake when the outbound byte budget refills so paced data resumes
            // without waiting for an unrelated link event.
            let budget_delay = (deferred_outbound.is_some()
                && can_publish_data(&cipher, &in_flight))
            .then(|| self.outbound_budget.delay_until_ready())
            .filter(|delay| !delay.is_zero());
            let budget_timer = sleep(budget_delay.unwrap_or(CREDIT_DEBOUNCE));
            tokio::pin!(budget_timer);

            let can_accept_outbound = !outbound_closed
                && deferred_outbound.is_none()
                && in_flight.has_data_slot()
                && in_flight.has_broker_capacity();
            tokio::select! {
                _ = &mut budget_timer, if budget_delay.is_some() => {
                    continue;
                }
                _ = &mut session_renewal_timer, if self.session_renewal_after.is_some() => {
                    let error = MqttTransportError::ManagedSessionExpired;
                    ack_deferred_outbound(&mut deferred_outbound, &error);
                    self.fail_stream(&mut in_flight, error).await;
                    return;
                }
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
                _ = &mut publish_ack_timer, if publish_ack_delay.is_some() => {
                    let token = in_flight
                        .oldest_token()
                        .expect("PUBACK timer requires an in-flight publish");
                    let error = MqttTransportError::PublishAckTimeout {
                        token: token.value(),
                        timeout_ms: PUBLISH_ACK_TIMEOUT.as_millis() as u64,
                    };
                    ack_deferred_outbound(&mut deferred_outbound, &error);
                    self.fail_stream(&mut in_flight, error).await;
                    return;
                }
                event = self.link.poll() => {
                    match event {
                        Ok(event) => {
                            if matches!(event, LinkEvent::PubAck(_)) {
                                last_publish_ack_at = Some(Instant::now());
                            }
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
                            if !can_publish_data(&cipher, &in_flight)
                                || !self.outbound_budget.is_ready()
                            {
                                deferred_outbound = Some(outbound);
                                continue;
                            }
                            let batch = self.boxcar_outbound(outbound, &mut deferred_outbound);
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

    fn boxcar_outbound(
        &mut self,
        first: OutboundChunk,
        deferred_outbound: &mut Option<OutboundChunk>,
    ) -> BoxcarBatch {
        let mut batch = BoxcarBatch::new(first);
        while batch.plaintext.len() < MAX_PLAINTEXT_CHUNK_LEN {
            match self.outbound_rx.try_recv() {
                Ok(next) => {
                    if !append_or_defer(&mut batch, next, deferred_outbound) {
                        break;
                    }
                }
                Err(_) => break,
            }
        }
        batch
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
        self.outbound_budget.charge(published.frame.len());
        in_flight.insert(InflightPublish::Data {
            token: published.token,
            enqueued_at: Instant::now(),
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
        // Credit frames are flow control and never wait on the byte budget, but
        // their (tiny) cost still counts against it.
        self.outbound_budget.charge(frame.len());
        credit.mark_published(next_expected);
        in_flight.insert(InflightPublish::Credit {
            token,
            enqueued_at: Instant::now(),
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
        enqueued_at: Instant,
        counter: u64,
        plaintext_len: usize,
        frame: Vec<u8>,
        quota_retries: u8,
        batch: BoxcarBatch,
    },
    Credit {
        token: PublishToken,
        enqueued_at: Instant,
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

    fn enqueued_at(&self) -> Instant {
        match self {
            Self::Data { enqueued_at, .. } | Self::Credit { enqueued_at, .. } => *enqueued_at,
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
                enqueued_at,
                quota_retries,
                ..
            }
            | Self::Credit {
                token,
                enqueued_at,
                quota_retries,
                ..
            } => {
                *token = new_token;
                *enqueued_at = Instant::now();
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

    fn oldest_token(&self) -> Option<PublishToken> {
        self.by_token
            .values()
            .min_by_key(|publish| publish.enqueued_at())
            .map(InflightPublish::token)
    }

    /// Remaining time before the PUBACK stall watchdog fires. The deadline is
    /// anchored to whichever is later: the oldest outstanding publish or the
    /// last PUBACK the link delivered — so a broker that is acking slowly (for
    /// example while enforcing its per-connection throughput cap) extends the
    /// deadline with every ack instead of being misread as dead.
    fn next_ack_timeout_delay(&self, last_publish_ack_at: Option<Instant>) -> Option<Duration> {
        let oldest_enqueued_at = self
            .by_token
            .values()
            .map(InflightPublish::enqueued_at)
            .min()?;
        let anchor = match last_publish_ack_at {
            Some(acked_at) => oldest_enqueued_at.max(acked_at),
            None => oldest_enqueued_at,
        };
        Some(
            PUBLISH_ACK_TIMEOUT
                .checked_sub(Instant::now().duration_since(anchor))
                .unwrap_or(Duration::ZERO),
        )
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

/// Token-bucket byte budget for outbound data publishes. Post-charged: a batch
/// is allowed whenever the balance is positive and its full cost is deducted
/// afterwards (possibly driving the balance negative), so pacing never splits a
/// batch but bounds sustained throughput to the configured rate.
pub(crate) struct OutboundByteBudget {
    rate_bytes_per_sec: f64,
    burst_bytes: f64,
    tokens: f64,
    last_refill: Instant,
}

impl OutboundByteBudget {
    pub(crate) fn new() -> Self {
        Self::with_rate(OUTBOUND_BUDGET_BYTES_PER_SEC, OUTBOUND_BUDGET_BURST_BYTES)
    }

    pub(crate) fn for_endpoint(endpoint: &protocol::BrokerUrl) -> Self {
        let is_loopback = url::Url::parse(endpoint.as_str())
            .ok()
            .and_then(|parsed| parsed.host_str().map(str::to_owned))
            .is_some_and(|host| {
                host.eq_ignore_ascii_case("localhost")
                    || host
                        .parse::<std::net::IpAddr>()
                        .is_ok_and(|address| address.is_loopback())
            });
        if is_loopback {
            Self::with_rate(1e12, 1e12)
        } else {
            Self::new()
        }
    }

    fn with_rate(rate_bytes_per_sec: f64, burst_bytes: f64) -> Self {
        Self {
            rate_bytes_per_sec,
            burst_bytes,
            tokens: burst_bytes,
            last_refill: Instant::now(),
        }
    }

    fn refill(&mut self) {
        let now = Instant::now();
        let elapsed = now.duration_since(self.last_refill);
        self.last_refill = now;
        self.tokens =
            (self.tokens + elapsed.as_secs_f64() * self.rate_bytes_per_sec).min(self.burst_bytes);
    }

    fn is_ready(&mut self) -> bool {
        self.refill();
        self.tokens > 0.0
    }

    fn charge(&mut self, bytes: usize) {
        self.refill();
        self.tokens -= bytes as f64;
    }

    fn delay_until_ready(&mut self) -> Duration {
        self.refill();
        if self.tokens > 0.0 {
            Duration::ZERO
        } else {
            Duration::from_secs_f64((1.0 - self.tokens) / self.rate_bytes_per_sec)
        }
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
    match error {
        MqttTransportError::PublishRejected { reason } => !reason.is_not_authorized(),
        MqttTransportError::BrokerConnect { .. }
        | MqttTransportError::Publish { .. }
        | MqttTransportError::BrokerDisconnected { .. } => true,
        _ => false,
    }
}

struct BoxcarBatch {
    plaintext: Vec<u8>,
    acks: Vec<oneshot::Sender<Result<(), WriteAckError>>>,
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
        let ack_error = WriteAckError::from_error(error);
        for ack in self.acks {
            let _send_result = ack.send(Err(ack_error.clone()));
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
        let _send_result = outbound.ack.send(Err(WriteAckError::from_error(error)));
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

fn decode_host_open_request(
    config: &MqttConnectConfig,
    payload: &[u8],
) -> Result<Option<OpenRequest>, MqttTransportError> {
    match decode_open_request(&config.room, &config.psk, payload) {
        Ok(request) => Ok(Some(request)),
        Err(FramingError::UnknownTag { .. } | FramingError::VersionMismatch { .. }) => Ok(None),
        Err(error) => Err(MqttTransportError::Framing(error)),
    }
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

pub(crate) async fn accept_ephemeral_data_connections<L, F, Fut, T>(
    config: &MqttConnectConfig,
    inbound_topic: &str,
    outbound_topic: &str,
    link: &mut L,
    mut connect_candidate: F,
) -> Result<T, MqttTransportError>
where
    L: MqttLink,
    F: FnMut(ConnectionId, EphemeralDataRoom) -> Fut,
    Fut: Future<Output = (ConnectionId, Result<T, MqttTransportError>)>,
{
    link.subscribe(inbound_topic).await?;
    await_suback(link, "rendezvous subscribe").await?;

    let mut candidates = FuturesUnordered::new();
    let mut accepts = HashMap::<ConnectionId, OpenAccept>::new();
    loop {
        tokio::select! {
            candidate = candidates.next(), if !candidates.is_empty() => {
                if let Some((connection_id, result)) = candidate {
                    accepts.remove(&connection_id);
                    if let Ok(connected) = result {
                        link.disconnect().await;
                        return Ok(connected);
                    }
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
                        let request = match decode_host_open_request(config, &publish.payload)? {
                            Some(request) => request,
                            None => continue,
                        };
                        if request.proposed_data_room == config.room {
                            return Err(MqttTransportError::Framing(FramingError::InvalidTopic {
                                message: "rendezvous request proposed the rendezvous room as its data room"
                                    .to_owned(),
                            }));
                        }

                        if let Some(accept) = accepts.get(&request.connection_id) {
                            if accept.client_nonce != request.client_nonce
                                || accept.data_room != request.proposed_data_room
                            {
                                return Err(MqttTransportError::Framing(
                                    FramingError::InvalidTopic {
                                        message: "rendezvous connection id was reused with different parameters"
                                            .to_owned(),
                                    },
                                ));
                            }
                            let frame = encode_open_accept(&config.room, &config.psk, accept)?;
                            publish_control_frame(link, outbound_topic, frame).await?;
                            continue;
                        }
                        if candidates.len() >= MAX_RENDEZVOUS_CANDIDATES {
                            tracing::warn!(
                                candidate_count = candidates.len(),
                                "ignoring MQTT rendezvous request while candidate limit is full"
                            );
                            continue;
                        }

                        let accept = OpenAccept {
                            connection_id: request.connection_id,
                            client_nonce: request.client_nonce,
                            server_nonce: random_nonce(),
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
                        let connection_id = accept.connection_id;
                        let data = EphemeralDataRoom {
                            room: accept.data_room,
                            psk,
                        };
                        accepts.insert(connection_id, accept);
                        candidates.push(connect_candidate(connection_id, data));
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
                let Some(request) = decode_host_open_request(config, &publish.payload)? else {
                    continue;
                };
                if request.proposed_data_room == config.room {
                    return Err(MqttTransportError::Framing(FramingError::InvalidTopic {
                        message: "rendezvous request proposed the rendezvous room as its data room"
                            .to_owned(),
                    }));
                }
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
                        if accept.data_room != request.proposed_data_room
                            || accept.data_room == config.room
                        {
                            return Err(MqttTransportError::Framing(FramingError::InvalidTopic {
                                message: "rendezvous accept selected an invalid data room"
                                    .to_owned(),
                            }));
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
