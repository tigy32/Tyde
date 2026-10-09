//! Host-owned Tychat persistence and transport boundary. No transport or crypto lives here.
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex as StdMutex};

use blake2::{Blake2s256, Digest};
use protocol::*;
use serde::{Deserialize, Serialize};
use tokio::sync::{Mutex, watch};

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

pub(crate) struct State {
    path: PathBuf,
    pub journal: Journal,
    pub snapshot: TychatStatePayload,
    pub typing: bool,
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
    pub state: Arc<Mutex<State>>,
    pub lifecycle: Arc<Mutex<()>>,
    changed: watch::Sender<u64>,
    pub bridge: Arc<crate::tychat_bridge::BridgeHandle>,
    /// Where new pairings are redeemed; an existing pairing keeps its own origin.
    pub api_base: url::Url,
    #[cfg(feature = "test-support")]
    pub outbound_ack_gate: Arc<Mutex<Option<Arc<crate::host::SpawnOperationTestGateInner>>>>,
    process_lock: Arc<StdMutex<Option<std::fs::File>>>,
    lock_path: PathBuf,
}

impl TychatService {
    pub fn load(path: PathBuf, api_base: url::Url) -> Result<Self, String> {
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
        let journal = match std::fs::read(&path) {
            Ok(bytes) => {
                enforce_owner_only_file(&path)?;
                decode_journal(&bytes)?
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Journal::default(),
            Err(_) => return Err("Cannot read Tychat secret journal".to_owned()),
        };
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
        let (changed, _) = watch::channel(0);
        Ok(Self {
            state: Arc::new(Mutex::new(State {
                path,
                journal,
                snapshot,
                typing: false,
            })),
            lifecycle: Arc::new(Mutex::new(())),
            changed,
            bridge: Arc::new(crate::tychat_bridge::BridgeHandle::default()),
            api_base,
            #[cfg(feature = "test-support")]
            outbound_ack_gate: Arc::new(Mutex::new(None)),
            process_lock: Arc::new(StdMutex::new(process_lock)),
            lock_path,
        })
    }

    pub fn acquire_process_lock(&self) -> Result<(), String> {
        let mut lock = self
            .process_lock
            .lock()
            .map_err(|_| "Tychat process lock failed")?;
        if lock.is_some() {
            return Ok(());
        }
        let file = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(&self.lock_path)
            .map_err(|_| "Cannot open Tychat process lock")?;
        fs2::FileExt::try_lock_exclusive(&file)
            .map_err(|_| "Another host owns this Tychat state")?;
        match std::fs::read(self.lock_path.with_extension("json")) {
            Ok(bytes) => {
                let journal = decode_journal(&bytes)?;
                if journal.pairing.is_some() {
                    return Err(
                        "Another host saved a pairing; restart this host before continuing".into(),
                    );
                }
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(_) => return Err("Cannot read Tychat secret journal".into()),
        }
        *lock = Some(file);
        Ok(())
    }

    pub fn release_process_lock(&self) {
        match self.process_lock.lock() {
            Ok(mut lock) => {
                lock.take();
            }
            Err(_) => tracing::error!("Tychat process lock could not be released"),
        }
    }

    pub async fn enqueue_owner(
        &self,
        generation: &TychatPairingId,
        pending: PendingOwnerMessage,
    ) -> Result<(), String> {
        let mut state = self.state.lock().await;
        if !state
            .journal
            .pairing
            .as_ref()
            .is_some_and(|pairing| &pairing.generation == generation)
        {
            return Err("Stale Tychat pairing".into());
        }
        let mut journal = state.journal.clone();
        if !journal
            .inbox
            .iter()
            .any(|entry| entry.message.message_id == pending.message.message_id)
        {
            journal.inbox.push(pending);
            state.commit(journal)?;
        }
        Ok(())
    }

    pub async fn acknowledge_owner(
        &self,
        generation: &TychatPairingId,
        id: &TychatMessageId,
    ) -> Result<(), String> {
        let mut state = self.state.lock().await;
        if !state
            .journal
            .pairing
            .as_ref()
            .is_some_and(|pairing| &pairing.generation == generation)
        {
            return Err("Stale Tychat pairing".into());
        }
        let mut journal = state.journal.clone();
        journal
            .inbox
            .retain(|entry| &entry.message.message_id != id);
        state.commit(journal)
    }

    pub fn subscribe(&self) -> watch::Receiver<u64> {
        self.changed.subscribe()
    }

    pub fn notify(&self) {
        self.changed
            .send_modify(|revision| *revision = revision.wrapping_add(1));
    }

    pub async fn fail(&self, reason: String) {
        self.state.lock().await.snapshot.status = TychatBridgeStatus::Failed { reason };
        self.notify();
    }

    pub async fn begin_turn(&self) -> Result<TychatTurnId, String> {
        let mut state = self.state.lock().await;
        let mut journal = state.journal.clone();
        journal.turn_sequence = journal
            .turn_sequence
            .checked_add(1)
            .ok_or_else(|| "Tychat turn sequence exhausted".to_owned())?;
        let id = TychatTurnId(journal.turn_sequence);
        state.commit(journal)?;
        Ok(id)
    }

    pub async fn append(
        &self,
        agent: &AgentId,
        turn: TychatTurnId,
        question: Option<&str>,
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
        if let Some(question) = question {
            hash.update(question.as_bytes());
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
        let mut state = self.state.lock().await;
        if state.journal.pairing.is_none() {
            return Err("Tychat is unpaired".into());
        }
        let mut journal = state.journal.clone();
        if !journal
            .outbox
            .iter()
            .any(|entry| entry.message_id == message.message_id)
        {
            journal.outbox.push(message);
            state.commit(journal)?;
        }
        drop(state);
        self.notify();
        Ok(())
    }

    pub async fn activity(&self, activity: AgentActivity) {
        let mut state = self.state.lock().await;
        let typing = activity == AgentActivity::Thinking;
        if state.typing != typing {
            state.typing = typing;
            drop(state);
            self.notify();
        }
    }
}

pub(crate) fn steering_capabilities() -> Vec<BackendSteeringCapability> {
    crate::backend::SUPPORTED_BACKENDS
        .into_iter()
        .map(|kind| BackendSteeringCapability {
            backend_kind: kind,
            mid_turn: if crate::backend::capabilities_for_backend_kind(kind)
                .contains(tyde_agent_adapter::BackendCapability::MidTurnSteering)
            {
                MidTurnSteeringCapability::Supported
            } else {
                MidTurnSteeringCapability::Unsupported
            },
        })
        .collect()
}

pub(crate) const STARTUP_MESSAGE: &str = "Your Tychat session is ready. Greet the owner in one short sentence, then wait for their message. Do not start any tasks until asked.";

pub(crate) const INSTRUCTIONS: &str = "## Tychat\n\nYou are the Tychat agent: the owner messages you from their phone through Tychat, and your final message each turn is sent back to them. Keep replies short and phone-readable. Unless the owner asks you to do something yourself, hand work to independent top-level agents rather than doing it in this session, and report back when it finishes. Agent-control MCP with global: true lets you spawn, list, read, await and steer every agent on the host. Never expose credentials or private keys.";

pub(crate) fn answer(request: &ToolRequest, text: &str) -> Result<SendMessageToolResponse, String> {
    match request.tool_type {
        ToolRequestType::AskUserQuestion { .. } => Ok(SendMessageToolResponse::AskUserQuestion {
            tool_call_id: request.tool_call_id.clone(),
            answer: text.into(),
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
