use std::{future::Future, pin::Pin, sync::Mutex as StdMutex, time::Duration};

use blake2::{Blake2s256, Digest};
use protocol::*;
use tokio::sync::{Mutex, mpsc, oneshot, watch};
use tokio_util::sync::CancellationToken;
use tychat_bot::{BotClient, BotError, BotEvent, BotState, PauseReason};

use crate::{
    HostHandle,
    tychat::{PendingOwnerMessage, SecretBotState, TychatService},
};

const RETRY: Duration = Duration::from_secs(5);
const MAX_TEXT_UTF16: usize = 8000;
type Work<T> = Pin<Box<dyn Future<Output = Result<T, Failure>> + Send>>;

#[derive(Default)]
pub(crate) struct BridgeHandle {
    stopped: CancellationToken,
    task: Mutex<Option<tokio::task::JoinHandle<()>>>,
    disconnect: StdMutex<Option<mpsc::UnboundedSender<oneshot::Sender<()>>>>,
}

impl BridgeHandle {
    pub async fn start(&self, host: HostHandle, service: TychatService) {
        let mut task = self.task.lock().await;
        if task.is_some() || self.stopped.is_cancelled() {
            return;
        }
        let (tx, rx) = mpsc::unbounded_channel();
        *self.disconnect.lock().expect("Tychat bridge sender lock") = Some(tx);
        let stop = self.stopped.clone();
        *task = Some(tokio::spawn(async move {
            if let Err(reason) = run(host, service.clone(), rx, stop).await {
                tracing::error!(
                    "Tychat bridge stopped after a host persistence or lifecycle failure"
                );
                service.fail(reason).await;
            }
        }));
    }

    pub async fn disconnect(&self) -> Result<(), String> {
        if self
            .task
            .lock()
            .await
            .as_ref()
            .is_none_or(|task| task.is_finished())
        {
            return Ok(());
        }
        let tx = self
            .disconnect
            .lock()
            .map_err(|_| "Tychat bridge sender lock failed")?
            .clone();
        if let Some(tx) = tx {
            let (reply, done) = oneshot::channel();
            tx.send(reply).map_err(|_| "Tychat bridge is stopped")?;
            done.await
                .map_err(|_| "Tychat disconnect was not acknowledged")?;
        }
        Ok(())
    }

    pub async fn shutdown(&self) {
        self.stopped.cancel();
        let mut task = self.task.lock().await;
        if let Some(task) = task.as_mut()
            && task.await.is_err()
        {
            tracing::error!("Tychat bridge task failed during shutdown");
        }
        task.take();
    }

    pub async fn abort(&self) {
        self.stopped.cancel();
        if let Some(task) = self.task.lock().await.take() {
            task.abort();
            let _ = task.await;
        }
    }
}

enum Failure {
    Host(String),
    Sdk(BotError),
}
impl From<String> for Failure {
    fn from(value: String) -> Self {
        Self::Host(value)
    }
}
impl From<BotError> for Failure {
    fn from(value: BotError) -> Self {
        Self::Sdk(value)
    }
}

pub(crate) fn encode(state: &BotState) -> Result<SecretBotState, String> {
    serde_json::to_vec(state)
        .map(SecretBotState)
        .map_err(|_| "Cannot encode Tychat bot state".into())
}

pub(crate) fn error_status(error: &BotError) -> TychatBridgeStatus {
    match error {
        BotError::Paused(reason) => TychatBridgeStatus::Paused {
            reason: match reason {
                PauseReason::OwnerIdentityReplaced => {
                    "Owner identity was replaced; unpair and pair a new bot"
                }
                PauseReason::OwnerIdentityUnverifiable => {
                    "Owner identity cannot be verified; unpair and pair a new bot"
                }
            }
            .into(),
        },
        _ => TychatBridgeStatus::Failed {
            reason: match error {
                BotError::Network(_) => "Tychat network request failed; retrying",
                BotError::Http { status: 401, .. } | BotError::Revoked => {
                    "Owner revoked this bot; pair a new bot"
                }
                BotError::Http { .. } => "Tychat server refused the request",
                BotError::InvalidPairingCode => "Pairing code is invalid, expired or already used",
                BotError::NotConfirmed => "Owner has not confirmed the bot",
                BotError::MessageIdConflict => {
                    "Tychat rejected a conflicting outbound message identity"
                }
                BotError::MessageTooLong => {
                    "Tychat rejected a reply chunk; the full reply remains saved"
                }
                BotError::Protocol(_) => "Tychat protocol response was invalid",
                BotError::Crypto => "Tychat verification or decryption failed",
                BotError::InvalidState(_) => "Saved Tychat bot state is invalid; re-pair",
                BotError::Closed => "Tychat client closed",
                BotError::Paused(_) => unreachable!("handled above"),
            }
            .into(),
        },
    }
}

fn retryable(error: &BotError) -> bool {
    matches!(
        error,
        BotError::Network(_)
            | BotError::Http {
                status: 408 | 429 | 500..=599,
                ..
            }
    )
}

async fn persist(
    host: &HostHandle,
    generation: &TychatPairingId,
    state: &BotState,
) -> Result<(), String> {
    host.persist_tychat_bot_state(generation, encode(state)?)
        .await
}

async fn run(
    host: HostHandle,
    service: TychatService,
    mut disconnect: mpsc::UnboundedReceiver<oneshot::Sender<()>>,
    stop: CancellationToken,
) -> Result<(), String> {
    let mut changed = service.subscribe();
    let mut held = None;
    let mut retry_at = tokio::time::Instant::now();
    loop {
        if stop.is_cancelled() {
            return Ok(());
        }
        let settings = host.tychat_settings().await?;
        let pairing = host.tychat_bot_state().await;
        let identity = pairing.as_ref().map(|(id, _, _)| id.clone());
        if !settings.enabled || identity.is_none() {
            held = None;
        } else if held.as_ref() != identity.as_ref() && tokio::time::Instant::now() >= retry_at {
            let (generation, base, secret) = pairing.ok_or("Tychat pairing disappeared")?;
            let state: BotState = match serde_json::from_slice(&secret.0) {
                Ok(state) => state,
                Err(_) => {
                    host.set_tychat_bridge_status(
                        &generation,
                        error_status(&BotError::InvalidState("decode")),
                    )
                    .await?;
                    held = Some(generation);
                    continue;
                }
            };
            let url = url::Url::parse(&base).map_err(|_| "Invalid saved Tychat API origin")?;
            host.set_tychat_bridge_status(&generation, TychatBridgeStatus::Connecting)
                .await?;
            let observed_cursor = state.cursor_seq();
            let connection = tokio::select! {
                biased;
                _ = stop.cancelled() => return Ok(()),
                Some(reply) = disconnect.recv() => { held = Some(generation); let _ = reply.send(()); continue; },
                result = BotClient::connect(&url, state) => result,
            };
            match connection {
                Ok((client, events)) => {
                    let result = connected(
                        SessionContext {
                            host: &host,
                            service: &service,
                            generation: &generation,
                            changed: &mut changed,
                            disconnect: &mut disconnect,
                            stop: &stop,
                            observed_cursor,
                        },
                        &client,
                        events,
                    )
                    .await;
                    // Every exit persists the SDK's final snapshot before the last client drops.
                    // connected drains owner events through that snapshot's cursor first.
                    drop(client);
                    match result? {
                        Exit::Wake => {}
                        Exit::Stopped => return Ok(()),
                        Exit::Hold => held = Some(generation),
                        Exit::Unpair(reply) => {
                            held = Some(generation);
                            let _ = reply.send(());
                        }
                    }
                }
                Err(error) => {
                    if matches!(
                        error,
                        BotError::Revoked | BotError::Http { status: 401, .. }
                    ) {
                        host.forget_tychat_pairing(Some(&generation)).await?;
                    } else {
                        host.set_tychat_bridge_status(&generation, error_status(&error))
                            .await?;
                        if !retryable(&error) {
                            held = Some(generation);
                        }
                    }
                }
            }
            retry_at = tokio::time::Instant::now() + RETRY;
            continue;
        }
        tokio::select! {
            _ = stop.cancelled() => return Ok(()),
            Some(reply) = disconnect.recv() => { held = identity; let _ = reply.send(()); },
            result = changed.changed() => { if result.is_err() { return Ok(()); } },
            _ = tokio::time::sleep_until(retry_at), if settings.enabled && identity.is_some() && held.as_ref() != identity.as_ref() => {}
        }
    }
}

enum Exit {
    Wake,
    Stopped,
    Hold,
    Unpair(oneshot::Sender<()>),
}

struct SessionContext<'a> {
    host: &'a HostHandle,
    service: &'a TychatService,
    generation: &'a TychatPairingId,
    changed: &'a mut watch::Receiver<u64>,
    disconnect: &'a mut mpsc::UnboundedReceiver<oneshot::Sender<()>>,
    stop: &'a CancellationToken,
    observed_cursor: u64,
}

async fn connected(
    context: SessionContext<'_>,
    client: &BotClient,
    mut events: mpsc::Receiver<BotEvent>,
) -> Result<Exit, String> {
    let SessionContext {
        host,
        service,
        generation,
        changed,
        disconnect,
        stop,
        mut observed_cursor,
    } = context;
    let mut ready = false;
    let mut outbound: Option<Work<bool>> = None;
    let mut inbound: Option<Work<()>> = None;
    let mut typing = None;
    let mut retry_at = tokio::time::Instant::now();
    let mut revoked = false;
    let exit = loop {
        if stop.is_cancelled() {
            break Exit::Stopped;
        }
        if !host.tychat_settings().await?.enabled {
            break Exit::Wake;
        }
        if !host
            .tychat_bot_state()
            .await
            .is_some_and(|(id, _, _)| &id == generation)
        {
            break Exit::Wake;
        }
        if ready && tokio::time::Instant::now() >= retry_at {
            let snapshot = host.tychat_outbound(generation).await?;
            if outbound.is_none()
                && (typing != Some(snapshot.typing) || !snapshot.pending.is_empty())
            {
                let host = host.clone();
                let client = client.clone();
                let generation = generation.clone();
                outbound = Some(Box::pin(async move {
                    client.set_typing(snapshot.typing).await?;
                    if let Some(message) = snapshot.pending.first() {
                        send_reply(&client, message).await?;
                        #[cfg(feature = "test-support")]
                        host.wait_tychat_outbound_ack_for_test().await;
                        host.acknowledge_tychat_outbound(&generation, message.message_id)
                            .await?;
                    }
                    Ok(snapshot.typing)
                }));
            }
            let state = host.tychat_state().await;
            if inbound.is_none()
                && state.agent_id.is_some()
                && state.status == TychatBridgeStatus::Connected
                && let Some(pending) = service.state.lock().await.journal.inbox.first().cloned()
            {
                let host = host.clone();
                let client = client.clone();
                let service = service.clone();
                let generation = generation.clone();
                inbound = Some(Box::pin(async move {
                    host.deliver_tychat_message(&generation, pending.message.clone())
                        .await?;
                    client.mark_read(pending.at).await?;
                    service
                        .acknowledge_owner(&generation, &pending.message.message_id)
                        .await?;
                    Ok(())
                }));
            }
        }
        tokio::select! {
            biased;
            _ = stop.cancelled() => break Exit::Stopped,
            Some(reply) = disconnect.recv() => break Exit::Unpair(reply),
            event = events.recv() => match event {
                Some(BotEvent::StateChanged(state)) => { persist(host, generation, &state).await?; observed_cursor = observed_cursor.max(state.cursor_seq()); }
                Some(BotEvent::OwnerMessage { message_id, text, at }) => {
                    service.enqueue_owner(generation, PendingOwnerMessage { message: TychatOwnerMessage { message_id: TychatMessageId(message_id), text }, at }).await?;
                }
                Some(BotEvent::Ready) => { ready = true; host.set_tychat_bridge_status(generation, TychatBridgeStatus::Connected).await?; }
                Some(BotEvent::AwaitingOwnerConfirmation) => { ready = false; host.set_tychat_bridge_status(generation, TychatBridgeStatus::AwaitingOwnerConfirmation).await?; }
                Some(BotEvent::Disconnected { .. }) => {
                    tracing::info!(dropped_envelopes = client.dropped_envelopes(), "Tychat disconnected; SDK owns reconnect");
                    host.set_tychat_bridge_status(generation, TychatBridgeStatus::Connecting).await?;
                    typing = None; retry_at = tokio::time::Instant::now() + RETRY;
                }
                Some(BotEvent::Paused { reason }) => { host.set_tychat_bridge_status(generation, error_status(&BotError::Paused(reason))).await?; break Exit::Hold; }
                Some(BotEvent::Revoked) => { revoked = true; break Exit::Hold; }
                None => { host.set_tychat_bridge_status(generation, error_status(&BotError::Closed)).await?; break Exit::Hold; }
            },
            result = async { outbound.as_mut().expect("guarded outbound operation").await }, if outbound.is_some() => {
                outbound = None;
                match result {
                    Ok(value) => { typing = Some(value); host.set_tychat_bridge_status(generation, TychatBridgeStatus::Connected).await?; }
                    Err(Failure::Host(reason)) => return Err(reason),
                    Err(Failure::Sdk(error)) => {
                        if matches!(error, BotError::Revoked | BotError::Http { status: 401, .. }) { revoked = true; break Exit::Hold; }
                        host.set_tychat_bridge_status(generation, error_status(&error)).await?;
                        if !retryable(&error) { break Exit::Hold; }
                        typing = None;
                        retry_at = tokio::time::Instant::now() + RETRY;
                    }
                }
            },
            result = async { inbound.as_mut().expect("guarded inbound operation").await }, if inbound.is_some() => {
                inbound = None;
                match result {
                    Ok(()) => {},
                    Err(Failure::Host(reason)) => { host.set_tychat_bridge_status(generation, TychatBridgeStatus::Failed { reason }).await?; break Exit::Hold; },
                    Err(Failure::Sdk(error)) => {
                        if matches!(error, BotError::Revoked | BotError::Http { status: 401, .. }) { revoked = true; break Exit::Hold; }
                        host.set_tychat_bridge_status(generation, error_status(&error)).await?;
                        if !retryable(&error) { break Exit::Hold; }
                        typing = None;
                        retry_at = tokio::time::Instant::now() + RETRY;
                    }
                }
            },
            result = changed.changed() => { if result.is_err() { break Exit::Stopped; } },
            _ = tokio::time::sleep_until(retry_at), if ready && tokio::time::Instant::now() < retry_at => {}
        }
    };
    drop(outbound);
    drop(inbound);
    // Snapshot before draining, so messages read later by the SDK replay after restart.
    // OwnerMessage precedes its cursor's StateChanged in the SDK event contract.
    let mut state = client.state().await;
    while observed_cursor < state.cursor_seq() {
        match events.recv().await {
            Some(BotEvent::OwnerMessage {
                message_id,
                text,
                at,
            }) => {
                service
                    .enqueue_owner(
                        generation,
                        PendingOwnerMessage {
                            message: TychatOwnerMessage {
                                message_id: TychatMessageId(message_id),
                                text,
                            },
                            at,
                        },
                    )
                    .await?
            }
            Some(BotEvent::StateChanged(observed)) => {
                persist(host, generation, &observed).await?;
                observed_cursor = observed_cursor.max(observed.cursor_seq());
                if observed.cursor_seq() >= state.cursor_seq() {
                    state = observed;
                }
            }
            Some(BotEvent::Revoked) => {
                revoked = true;
                break;
            }
            Some(_) => {}
            None => return Err("Tychat stopped before its shutdown cursor was journaled".into()),
        }
    }
    persist(host, generation, &state).await?;
    if ready && !revoked && !matches!(exit, Exit::Hold) {
        match tokio::time::timeout(Duration::from_secs(2), client.set_typing(false)).await {
            Ok(Ok(())) => {}
            _ => tracing::warn!("Tychat typing stop was not acknowledged; SDK renewal is stopping"),
        }
    }
    if revoked {
        host.forget_tychat_pairing(Some(generation)).await?;
    }
    tracing::info!(
        dropped_envelopes = client.dropped_envelopes(),
        "Tychat transport stopped"
    );
    Ok(exit)
}

async fn send_reply(client: &BotClient, message: &TychatOutboundMessage) -> Result<(), BotError> {
    if message.text.encode_utf16().count() <= MAX_TEXT_UTF16 {
        return client.send_text(message.message_id.0, &message.text).await;
    }
    let mut start = 0;
    let mut index = 0u64;
    while start < message.text.len() {
        // A labelled part also carries whitespace-only runs, which the SDK otherwise refuses.
        let prefix = format!("[Part {}]\n", index + 1);
        let capacity = MAX_TEXT_UTF16 - prefix.encode_utf16().count();
        let mut units = 0;
        let mut end = start;
        for ch in message.text[start..].chars() {
            if units + ch.len_utf16() > capacity {
                break;
            }
            units += ch.len_utf16();
            end += ch.len_utf8();
        }
        let text = format!("{prefix}{}", &message.text[start..end]);
        client
            .send_text(chunk_id(message.message_id, index), &text)
            .await?;
        start = end;
        index += 1;
    }
    Ok(())
}

fn chunk_id(message: TychatOutboundId, index: u64) -> [u8; 16] {
    let digest = Blake2s256::new()
        .chain_update(b"tyde.tychat.chunk.v1\0")
        .chain_update(message.0)
        .chain_update(index.to_be_bytes())
        .finalize();
    let mut id = [0; 16];
    id.copy_from_slice(&digest[..16]);
    id
}
