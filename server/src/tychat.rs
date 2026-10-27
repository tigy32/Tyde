//! Host-owned Tychat persistence and transport boundary. No transport or crypto lives here.
use std::collections::{HashMap, VecDeque};
use std::future::Future;
use std::path::PathBuf;
use std::pin::Pin;
use std::sync::Arc;

use blake2::{Blake2s256, Digest};
use protocol::*;
use serde::{Deserialize, Serialize};
use tokio::sync::{mpsc, oneshot, watch};

use crate::store::permissions::{atomic_write_owner_only, enforce_owner_only_file};

#[derive(Clone, Serialize, Deserialize)]
#[serde(transparent)]
pub struct SecretBotState(pub Vec<u8>);

impl std::fmt::Debug for SecretBotState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("SecretBotState(<redacted>)")
    }
}

#[derive(Clone, Serialize, Deserialize)]
pub(crate) struct Pairing {
    pub generation: TychatPairingId,
    pub secret: SecretBotState,
    pub api_base_url: String,
    pub fingerprints: TychatFingerprints,
}

#[derive(Clone, Serialize, Deserialize)]
pub(crate) struct Session {
    pub agent_id: AgentId,
    pub session_id: SessionId,
    pub settings: TychatSettings,
    pub launch_profile: Option<LaunchProfile>,
}

#[derive(Clone, Serialize, Deserialize)]
pub(crate) struct PendingOwnerMessage {
    pub message: TychatOwnerMessage,
    pub at: tychat_bot::MessageRef,
}

#[derive(Clone, Default, Serialize, Deserialize)]
pub(crate) struct Journal {
    pub pairing: Option<Pairing>,
    pub session: Option<Session>,
    pub turn_sequence: u64,
    pub outbox: Vec<TychatOutboundMessage>,
    #[serde(default)]
    pub inbox: Vec<PendingOwnerMessage>,
    pub deliveries: HashMap<TychatMessageId, TychatDeliveryReceipt>,
}

struct State {
    path: PathBuf,
    journal: Journal,
    snapshot: TychatStatePayload,
    typing: bool,
    process_lock: Option<std::fs::File>,
    lock_path: PathBuf,
}

impl State {
    pub fn commit(&mut self, journal: Journal) -> Result<(), String> {
        let bytes = serde_json::to_vec(&journal)
            .map_err(|_| "Cannot encode Tychat secret journal".to_owned())?;
        atomic_write_owner_only(&self.path, &bytes)?;
        // The existing secret writer syncs the file; also fence its rename.
        #[cfg(unix)]
        if let Some(parent) = self.path.parent() {
            std::fs::File::open(parent)
                .and_then(|directory| directory.sync_all())
                .map_err(|_| "Cannot sync Tychat secret directory".to_owned())?;
        }
        self.journal = journal;
        Ok(())
    }
}

/// v0.9.5-beta.10 persisted the API origin and access mode inside the
/// session's settings copy, and that session ran without a custom agent.
fn decode_journal(bytes: &[u8]) -> Result<Journal, String> {
    let mut value: serde_json::Value =
        serde_json::from_slice(bytes).map_err(|_| "Invalid Tychat secret journal".to_owned())?;
    if let Some(settings) = value
        .pointer_mut("/session/settings")
        .and_then(serde_json::Value::as_object_mut)
    {
        settings.remove("api_base_url");
        settings.remove("access_mode");
        settings
            .entry("custom_agent_id")
            .or_insert(serde_json::Value::Null);
    }
    serde_json::from_value(value).map_err(|_| "Invalid Tychat secret journal".to_owned())
}

#[derive(Clone)]
pub(crate) struct TychatService {
    commands: mpsc::UnboundedSender<Command>,
    snapshot: watch::Receiver<TychatStatePayload>,
    changed: watch::Receiver<u64>,
    pub bridge: Arc<crate::tychat_bridge::BridgeHandle>,
    pub api_base: url::Url,
}

pub(crate) enum Update {
    InstallPairing(Pairing),
    PersistBot {
        generation: TychatPairingId,
        secret: SecretBotState,
    },
    BridgeStatus {
        generation: TychatPairingId,
        status: TychatBridgeStatus,
    },
    Fail(String),
    PairFailed(TychatBridgeStatus),
    EnqueueOwner {
        generation: TychatPairingId,
        pending: PendingOwnerMessage,
    },
    AcknowledgeOwner {
        generation: TychatPairingId,
        id: TychatMessageId,
    },
    AcknowledgeOutbound {
        generation: TychatPairingId,
        id: TychatOutboundId,
    },
    Append(TychatOutboundMessage),
    Delivery(TychatDeliveryReceipt),
    Application {
        settings: Option<TychatSettings>,
        application: TychatSettingsApplication,
    },
    ClearAgent {
        reset: bool,
    },
    Admit {
        session: Session,
        application: TychatSettingsApplication,
    },
    Forget,
    AcquireProcessLock,
    ReleaseProcessLock,
}

pub(crate) enum Lifecycle {
    Reconcile {
        reset: bool,
        reply: oneshot::Sender<Result<(), String>>,
    },
    Install {
        secret: SecretBotState,
        fingerprints: TychatFingerprints,
        reply: oneshot::Sender<Result<TychatPairingId, String>>,
    },
    Command {
        command: TychatCommandPayload,
        disconnected: Option<crate::tychat_bridge::DisconnectedTransport>,
        reply: oneshot::Sender<Result<(), String>>,
    },
    Forget {
        generation: Option<TychatPairingId>,
        reply: oneshot::Sender<Result<(), String>>,
    },
    Deliver {
        generation: TychatPairingId,
        message: TychatOwnerMessage,
        reply: oneshot::Sender<Result<TychatDeliveryReceipt, String>>,
    },
}

impl Lifecycle {
    async fn run(self, host: crate::HostHandle) {
        let stopped = host.tychat_lifecycle_stop();
        match self {
            Self::Reconcile { reset, reply } => {
                let _ =
                    reply.send(until_stopped(&stopped, host.reconcile_tychat_owned(reset)).await);
            }
            Self::Install {
                secret,
                fingerprints,
                reply,
            } => {
                let _ = reply.send(
                    until_stopped(
                        &stopped,
                        host.install_tychat_pairing_owned(secret, fingerprints),
                    )
                    .await,
                );
            }
            Self::Command {
                command,
                disconnected,
                reply,
            } => {
                let result = until_stopped(&stopped, host.tychat_command_owned(command)).await;
                drop(disconnected);
                let _ = reply.send(result);
            }
            Self::Forget { generation, reply } => {
                let _ = reply.send(
                    until_stopped(
                        &stopped,
                        host.forget_tychat_pairing_owned(generation.as_ref()),
                    )
                    .await,
                );
            }
            Self::Deliver {
                generation,
                message,
                reply,
            } => {
                let _ = reply.send(
                    until_stopped(
                        &stopped,
                        host.deliver_tychat_message_owned(&generation, message),
                    )
                    .await,
                );
            }
        }
    }
}

async fn until_stopped<T>(
    stopped: &tokio_util::sync::CancellationToken,
    operation: impl Future<Output = Result<T, String>>,
) -> Result<T, String> {
    tokio::select! {
        biased;
        () = stopped.cancelled() => Err("Host stopped during Tychat operation".into()),
        result = operation => result,
    }
}

pub(crate) enum DeliveryTarget {
    Agent(AgentId),
    Receipt(TychatDeliveryReceipt),
}

#[cfg(feature = "test-support")]
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub enum TychatTestBoundary {
    StatePublication,
    OutboundAcknowledgement,
    PairingInstall,
    UnpairDisconnected,
}

enum Command {
    Update(Update, oneshot::Sender<Result<(), String>>),
    Pairing(oneshot::Sender<Option<Pairing>>),
    ReconcileState(oneshot::Sender<(Option<Session>, Option<AgentId>)>),
    PendingOwner(oneshot::Sender<Option<PendingOwnerMessage>>),
    Outbound(
        TychatPairingId,
        oneshot::Sender<Result<TychatOutboundSnapshot, String>>,
    ),
    Delivery(
        TychatPairingId,
        TychatOwnerMessage,
        oneshot::Sender<Result<DeliveryTarget, String>>,
    ),
    BeginTurn(oneshot::Sender<Result<TychatTurnId, String>>),
    Activity(AgentActivity),
    SettingsChanged,
    Lifecycle(crate::HostHandle, Lifecycle),
    #[cfg(feature = "test-support")]
    InstallTestGate(
        TychatTestBoundary,
        Arc<crate::host::SpawnOperationTestGateInner>,
    ),
    #[cfg(feature = "test-support")]
    TakeTestGate(
        TychatTestBoundary,
        oneshot::Sender<Option<Arc<crate::host::SpawnOperationTestGateInner>>>,
    ),
}

type LifecycleWork = Pin<Box<dyn Future<Output = ()> + Send>>;

impl TychatService {
    pub fn load(
        path: PathBuf,
        api_base: url::Url,
    ) -> Result<(Self, impl Future<Output = ()> + Send + 'static), String> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).map_err(|_| "Cannot create Tychat secret directory")?;
        }
        let lock_path = path.with_extension("lock");
        let mut options = std::fs::OpenOptions::new();
        options.read(true).write(true).create(true).truncate(false);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let file = options
            .open(&lock_path)
            .map_err(|_| "Cannot open Tychat process lock")?;
        enforce_owner_only_file(&lock_path)?;
        fs2::FileExt::try_lock_exclusive(&file)
            .map_err(|_| "Another host owns this Tychat state")?;
        let mut journal = match std::fs::read(&path) {
            Ok(bytes) => {
                enforce_owner_only_file(&path)?;
                decode_journal(&bytes)?
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Journal::default(),
            Err(_) => return Err("Cannot read Tychat secret journal".to_owned()),
        };
        // The SDK owns the fingerprint format; older SDKs saved 60-digit forms.
        if let Some(pairing) = journal.pairing.as_mut()
            && let Ok(bot) = serde_json::from_slice::<tychat_bot::BotState>(&pairing.secret.0)
        {
            pairing.fingerprints = TychatFingerprints {
                bot: bot.bot_fingerprint().to_owned(),
                owner: bot.owner_fingerprint(),
            };
        }
        let process_lock = journal.pairing.as_ref().map(|_| file);
        let snapshot = TychatStatePayload {
            status: if journal.pairing.is_some() {
                TychatBridgeStatus::Connecting
            } else {
                TychatBridgeStatus::Unpaired
            },
            fingerprints: journal
                .pairing
                .as_ref()
                .map(|pairing| pairing.fingerprints.clone()),
            backend_capabilities: steering_capabilities(),
            ..Default::default()
        };
        let (changed_tx, changed) = watch::channel(0_u64);
        let (snapshot_tx, snapshot_rx) = watch::channel(snapshot.clone());
        let (commands, mut incoming) = mpsc::unbounded_channel();
        let mut state = State {
            path,
            journal,
            snapshot,
            typing: false,
            process_lock,
            lock_path,
        };
        let owner = async move {
            let mut pending = VecDeque::new();
            let mut active: Option<LifecycleWork> = None;
            #[cfg(feature = "test-support")]
            let mut test_gates = HashMap::new();
            loop {
                if active.is_none()
                    && let Some((host, operation)) = pending.pop_front()
                {
                    active = Some(Box::pin(Lifecycle::run(operation, host)));
                }
                // External agent work may await persistence commands on this mailbox.
                let command = tokio::select! {
                    command = incoming.recv() => match command { Some(command) => command, None => break },
                    () = async { if let Some(work) = active.as_mut() { work.await } }, if active.is_some() => {
                        active = None;
                        continue;
                    }
                };
                let publish = match command {
                    Command::Update(update, reply) => {
                        let result = state.update(update);
                        if let Ok(publish) = &result {
                            snapshot_tx.send_replace(state.snapshot.clone());
                            if *publish {
                                changed_tx
                                    .send_modify(|revision| *revision = revision.wrapping_add(1));
                            }
                        }
                        let _ = reply.send(result.map(|_| ()));
                        false
                    }
                    Command::Pairing(reply) => {
                        let _ = reply.send(state.journal.pairing.clone());
                        false
                    }
                    Command::ReconcileState(reply) => {
                        let _ = reply.send((
                            state.journal.session.clone(),
                            state.snapshot.agent_id.clone(),
                        ));
                        false
                    }
                    Command::PendingOwner(reply) => {
                        let _ = reply.send(state.journal.inbox.first().cloned());
                        false
                    }
                    Command::Outbound(generation, reply) => {
                        let _ = reply.send(state.check_generation(&generation).map(|()| {
                            TychatOutboundSnapshot {
                                typing: state.typing,
                                pending: state.journal.outbox.clone(),
                            }
                        }));
                        false
                    }
                    Command::Delivery(generation, message, reply) => {
                        let _ = reply.send(state.delivery_target(&generation, &message));
                        false
                    }
                    Command::BeginTurn(reply) => {
                        let result = state.begin_turn();
                        let _ = reply.send(result);
                        false
                    }
                    Command::Activity(activity) => {
                        let typing = activity == AgentActivity::Thinking;
                        let changed = state.typing != typing;
                        state.typing = typing;
                        changed
                    }
                    Command::SettingsChanged => state.journal.pairing.is_some(),
                    Command::Lifecycle(host, operation) => {
                        pending.push_back((host, operation));
                        false
                    }
                    #[cfg(feature = "test-support")]
                    Command::InstallTestGate(boundary, gate) => {
                        test_gates.insert(boundary, gate);
                        false
                    }
                    #[cfg(feature = "test-support")]
                    Command::TakeTestGate(boundary, reply) => {
                        let _ = reply.send(test_gates.remove(&boundary));
                        false
                    }
                };
                if publish {
                    snapshot_tx.send_replace(state.snapshot.clone());
                    changed_tx.send_modify(|revision| *revision = revision.wrapping_add(1));
                }
            }
        };
        Ok((
            Self {
                commands,
                snapshot: snapshot_rx,
                changed,
                bridge: Arc::new(crate::tychat_bridge::BridgeHandle::default()),
                api_base,
            },
            owner,
        ))
    }

    async fn request<T>(
        &self,
        command: impl FnOnce(oneshot::Sender<T>) -> Command,
    ) -> Result<T, String> {
        let (reply, result) = oneshot::channel();
        self.commands
            .send(command(reply))
            .map_err(|_| "Tychat owner is stopped")?;
        result
            .await
            .map_err(|_| "Tychat owner stopped before completion".into())
    }

    pub async fn update(&self, update: Update) -> Result<(), String> {
        self.request(|reply| Command::Update(update, reply)).await?
    }

    pub async fn lifecycle<T>(
        &self,
        host: crate::HostHandle,
        operation: impl FnOnce(oneshot::Sender<T>) -> Lifecycle,
    ) -> Result<T, String> {
        self.request(|reply| Command::Lifecycle(host, operation(reply)))
            .await
    }

    pub fn snapshot(&self) -> TychatStatePayload {
        self.snapshot.borrow().clone()
    }
    pub fn subscribe(&self) -> watch::Receiver<u64> {
        let mut changed = self.changed.clone();
        changed.mark_unchanged();
        changed
    }
    pub async fn pairing(&self) -> Result<Option<Pairing>, String> {
        self.request(Command::Pairing).await
    }
    pub async fn reconcile_state(&self) -> Result<(Option<Session>, Option<AgentId>), String> {
        self.request(Command::ReconcileState).await
    }
    pub async fn pending_owner(&self) -> Result<Option<PendingOwnerMessage>, String> {
        self.request(Command::PendingOwner).await
    }
    pub async fn outbound(
        &self,
        generation: &TychatPairingId,
    ) -> Result<TychatOutboundSnapshot, String> {
        self.request(|reply| Command::Outbound(generation.clone(), reply))
            .await?
    }
    pub async fn delivery_target(
        &self,
        generation: &TychatPairingId,
        message: &TychatOwnerMessage,
    ) -> Result<DeliveryTarget, String> {
        self.request(|reply| Command::Delivery(generation.clone(), message.clone(), reply))
            .await?
    }
    pub async fn enqueue_owner(
        &self,
        generation: &TychatPairingId,
        pending: PendingOwnerMessage,
    ) -> Result<(), String> {
        self.update(Update::EnqueueOwner {
            generation: generation.clone(),
            pending,
        })
        .await
    }
    pub async fn acknowledge_owner(
        &self,
        generation: &TychatPairingId,
        id: &TychatMessageId,
    ) -> Result<(), String> {
        self.update(Update::AcknowledgeOwner {
            generation: generation.clone(),
            id: id.clone(),
        })
        .await
    }
    pub async fn fail(&self, reason: String) {
        if self.update(Update::Fail(reason)).await.is_err() {
            tracing::error!("Tychat owner stopped before failure publication");
        }
    }
    pub async fn acquire_process_lock(&self) -> Result<(), String> {
        self.update(Update::AcquireProcessLock).await
    }
    pub async fn release_process_lock(&self) {
        if self.update(Update::ReleaseProcessLock).await.is_err() {
            tracing::error!("Tychat owner stopped before process-lock release");
        }
    }
    pub async fn begin_turn(&self) -> Result<TychatTurnId, String> {
        self.request(Command::BeginTurn).await?
    }
    pub fn activity(&self, activity: AgentActivity) {
        if self.commands.send(Command::Activity(activity)).is_err() {
            tracing::error!("Tychat owner stopped before activity projection");
        }
    }
    pub fn settings_changed(&self) {
        if self.commands.send(Command::SettingsChanged).is_err() {
            tracing::error!("Tychat owner stopped before settings notification");
        }
    }
    #[cfg(feature = "test-support")]
    pub fn install_test_gate(
        &self,
        boundary: TychatTestBoundary,
        gate: Arc<crate::host::SpawnOperationTestGateInner>,
    ) {
        if self
            .commands
            .send(Command::InstallTestGate(boundary, gate))
            .is_err()
        {
            panic!("test Tychat owner stopped before gate installation");
        }
    }
    #[cfg(feature = "test-support")]
    pub async fn take_test_gate(
        &self,
        boundary: TychatTestBoundary,
    ) -> Option<Arc<crate::host::SpawnOperationTestGateInner>> {
        self.request(|reply| Command::TakeTestGate(boundary, reply))
            .await
            .expect("test Tychat owner available")
    }

    pub async fn append(
        &self,
        agent: &AgentId,
        turn: TychatTurnId,
        part: TychatPart<'_>,
        text: String,
    ) -> Result<(), String> {
        if text.trim().is_empty() {
            return Ok(());
        }
        let mut hash = Blake2s256::new();
        hash.update(b"tyde.tychat.outbound.v1\0");
        hash.update((agent.0.len() as u64).to_be_bytes());
        hash.update(agent.0.as_bytes());
        hash.update(turn.0.to_be_bytes());
        match part {
            TychatPart::Final => {}
            TychatPart::Question(tool_call_id) => hash.update(tool_call_id.as_bytes()),
            TychatPart::Send(index) => {
                hash.update(b"\0send\0");
                hash.update(index.to_be_bytes());
            }
        }
        let digest = hash.finalize();
        let mut bytes = [0; 16];
        bytes.copy_from_slice(&digest[..16]);
        let message = TychatOutboundMessage {
            message_id: TychatOutboundId(bytes),
            agent_id: agent.clone(),
            turn_id: turn,
            text,
        };
        self.update(Update::Append(message)).await
    }
}

impl State {
    fn check_generation(&self, generation: &TychatPairingId) -> Result<(), String> {
        if self
            .journal
            .pairing
            .as_ref()
            .is_some_and(|pairing| &pairing.generation == generation)
        {
            Ok(())
        } else {
            Err("Stale Tychat pairing".into())
        }
    }

    fn delivery_target(
        &self,
        generation: &TychatPairingId,
        message: &TychatOwnerMessage,
    ) -> Result<DeliveryTarget, String> {
        self.check_generation(generation)?;
        if !matches!(self.snapshot.status, TychatBridgeStatus::Connected) {
            return Err("Tychat bridge is not connected".into());
        }
        if let Some(receipt) = self.journal.deliveries.get(&message.message_id) {
            return Ok(DeliveryTarget::Receipt(receipt.clone()));
        }
        self.snapshot
            .agent_id
            .clone()
            .map(DeliveryTarget::Agent)
            .ok_or_else(|| "Tychat agent is unavailable".into())
    }

    fn begin_turn(&mut self) -> Result<TychatTurnId, String> {
        let mut journal = self.journal.clone();
        journal.turn_sequence = journal
            .turn_sequence
            .checked_add(1)
            .ok_or("Tychat turn sequence exhausted")?;
        let turn = TychatTurnId(journal.turn_sequence);
        self.commit(journal)?;
        Ok(turn)
    }

    fn acquire_process_lock(&mut self) -> Result<(), String> {
        if self.process_lock.is_some() {
            return Ok(());
        }
        let file = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(&self.lock_path)
            .map_err(|_| "Cannot open Tychat process lock")?;
        fs2::FileExt::try_lock_exclusive(&file)
            .map_err(|_| "Another host owns this Tychat state")?;
        match std::fs::read(&self.path) {
            Ok(bytes) => {
                if decode_journal(&bytes)?.pairing.is_some() {
                    return Err(
                        "Another host saved a pairing; restart this host before continuing".into(),
                    );
                }
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(_) => return Err("Cannot read Tychat secret journal".into()),
        }
        self.process_lock = Some(file);
        Ok(())
    }

    fn update(&mut self, update: Update) -> Result<bool, String> {
        let previous = self.snapshot.clone();
        let previous_typing = self.typing;
        let outbound_changed = matches!(
            &update,
            Update::Append(_) | Update::AcknowledgeOutbound { .. }
        );
        let mut journal = self.journal.clone();
        match update {
            Update::AcquireProcessLock => return self.acquire_process_lock().map(|()| false),
            Update::ReleaseProcessLock => {
                self.process_lock.take();
                return Ok(false);
            }
            Update::InstallPairing(pairing) => {
                if journal.pairing.is_some() {
                    return Err("Unpair before pairing another bot".into());
                }
                self.acquire_process_lock()?;
                let fingerprints = pairing.fingerprints.clone();
                journal.pairing = Some(pairing);
                self.commit(journal)?;
                self.snapshot.fingerprints = Some(fingerprints);
                self.snapshot.status = TychatBridgeStatus::AwaitingOwnerConfirmation;
            }
            Update::PersistBot { generation, secret } => {
                self.check_generation(&generation)?;
                let bot: tychat_bot::BotState =
                    serde_json::from_slice(&secret.0).map_err(|_| "Invalid Tychat bot state")?;
                let fingerprints = TychatFingerprints {
                    bot: bot.bot_fingerprint().to_owned(),
                    owner: bot.owner_fingerprint(),
                };
                let pairing = journal.pairing.as_mut().ok_or("Stale Tychat pairing")?;
                pairing.secret = secret;
                pairing.fingerprints = fingerprints.clone();
                self.commit(journal)?;
                self.snapshot.fingerprints = Some(fingerprints);
            }
            Update::BridgeStatus { generation, status } => {
                self.check_generation(&generation)?;
                if matches!(status, TychatBridgeStatus::Unpaired) {
                    return Err("Use Unpair to remove a Tychat pairing".into());
                }
                self.snapshot.status = status;
            }
            Update::Fail(reason) => self.snapshot.status = TychatBridgeStatus::Failed { reason },
            Update::PairFailed(status) => {
                self.process_lock.take();
                self.snapshot.status = status;
            }
            Update::EnqueueOwner {
                generation,
                pending,
            } => {
                self.check_generation(&generation)?;
                if !journal
                    .inbox
                    .iter()
                    .any(|entry| entry.message.message_id == pending.message.message_id)
                {
                    journal.inbox.push(pending);
                    self.commit(journal)?;
                }
            }
            Update::AcknowledgeOwner { generation, id } => {
                self.check_generation(&generation)?;
                journal.inbox.retain(|entry| entry.message.message_id != id);
                self.commit(journal)?;
            }
            Update::AcknowledgeOutbound { generation, id } => {
                self.check_generation(&generation)?;
                journal.outbox.retain(|entry| entry.message_id != id);
                self.commit(journal)?;
            }
            Update::Append(message) => {
                if journal.pairing.is_none() {
                    return Err("Tychat is unpaired".into());
                }
                if !journal
                    .outbox
                    .iter()
                    .any(|entry| entry.message_id == message.message_id)
                {
                    journal.outbox.push(message);
                    self.commit(journal)?;
                }
            }
            Update::Delivery(receipt) => {
                journal
                    .deliveries
                    .insert(receipt.message_id.clone(), receipt.clone());
                self.commit(journal)?;
                self.snapshot.last_delivery = Some(receipt);
            }
            Update::Application {
                settings,
                application,
            } => {
                if let Some(settings) = settings {
                    let session = journal
                        .session
                        .as_mut()
                        .ok_or("Tychat live session has no durable binding")?;
                    session.settings = settings;
                    self.commit(journal)?;
                }
                self.snapshot.settings_application = application;
            }
            Update::ClearAgent { reset } => {
                if reset {
                    journal.session = None;
                    self.commit(journal)?;
                }
                self.snapshot.agent_id = None;
                self.snapshot.settings_application = TychatSettingsApplication::Inactive;
                self.typing = false;
            }
            Update::Admit {
                session,
                application,
            } => {
                let id = session.agent_id.clone();
                journal.session = Some(session);
                self.commit(journal)?;
                self.snapshot.agent_id = Some(id);
                self.snapshot.settings_application = application;
            }
            Update::Forget => {
                self.commit(Journal::default())?;
                self.snapshot = TychatStatePayload {
                    backend_capabilities: steering_capabilities(),
                    ..Default::default()
                };
                self.typing = false;
                self.process_lock.take();
            }
        }
        Ok(outbound_changed || self.snapshot != previous || self.typing != previous_typing)
    }
}

/// Which outbound message of a turn is being journaled; it keys the
/// deterministic message id so retries never duplicate a delivery.
#[derive(Clone, Copy)]
pub enum TychatPart<'a> {
    Final,
    Question(&'a str),
    Send(u32),
}

pub(crate) fn steering_capabilities() -> Vec<BackendSteeringCapability> {
    crate::backend::SUPPORTED_BACKENDS
        .into_iter()
        .map(|kind| BackendSteeringCapability {
            backend_kind: kind,
            mid_turn: if crate::backend::supports_running_redirect(kind) {
                MidTurnSteeringCapability::Supported
            } else {
                MidTurnSteeringCapability::Unsupported
            },
        })
        .collect()
}

pub(crate) const STARTUP_MESSAGE: &str = "Your Tychat session is ready. Greet the owner in one short sentence, then wait for their message. Do not start any tasks until asked.";

pub(crate) const INSTRUCTIONS: &str = "## Tychat\n\nYou are the Tychat agent: the owner messages you from their phone through Tychat. Keep replies short and phone-readable. Use the tyde-tychat tychat_send_message tool to message the owner during a turn: acknowledge a request before starting work that takes more than a moment (for example \"On it!\"), then send progress and results the same way. Your final message is skipped only when tychat_send_message was your last tool call; after any other tool call it is sent as the reply, so finish with the results the owner should read. Unless the owner asks you to do something yourself, hand work to independent top-level agents rather than doing it in this session, and report back when it finishes. Agent-control MCP with global: true lets you spawn, list, read, await and steer every agent on the host. Never expose credentials or private keys.";

pub(crate) fn answer(request: &ToolRequest, text: &str) -> Result<SendMessageToolResponse, String> {
    match request.tool_type {
        ToolRequestType::AskUserQuestion { .. } => Ok(SendMessageToolResponse::AskUserQuestion {
            tool_call_id: request.tool_call_id.clone(),
            answer: text.into(),

            structured_answers: None,
        }),
        ToolRequestType::ExitPlanMode { .. } => {
            let decision = match text.trim().to_ascii_lowercase().as_str() {
                "1" | "approve" => ExitPlanModeDecision::Approve,
                "2" | "reject" => ExitPlanModeDecision::Reject,
                _ => return Err("Reply 1 to approve or 2 to reject the pending plan".into()),
            };
            Ok(SendMessageToolResponse::ExitPlanMode {
                tool_call_id: request.tool_call_id.clone(),
                decision,
                feedback: None,
            })
        }
        _ => Err("Pending interaction is not answerable through Tychat".into()),
    }
}

pub(crate) fn question_text(request: &ToolRequest) -> Option<String> {
    match &request.tool_type {
        ToolRequestType::AskUserQuestion { questions, .. } => Some(
            questions
                .iter()
                .enumerate()
                .map(|(index, question)| {
                    let mut text = format!("{}. {}", index + 1, question.question);
                    for (option, choice) in question.options.iter().enumerate() {
                        text.push_str(&format!("\n   {}. {}", option + 1, choice.label));
                    }
                    text
                })
                .collect::<Vec<_>>()
                .join("\n\n"),
        ),
        ToolRequestType::ExitPlanMode { plan, .. } => Some(format!(
            "{}\n\n1. Approve\n2. Reject",
            plan.as_deref().unwrap_or("Approve this plan?")
        )),
        _ => None,
    }
}
